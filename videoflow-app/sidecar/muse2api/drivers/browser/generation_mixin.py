"""Chat, image, and video generation mixin for BrowserDriver."""

from __future__ import annotations

import asyncio
import base64
import contextlib
import logging
import sys
from collections.abc import AsyncIterator, Callable
from typing import TYPE_CHECKING, Any

from ...accounts.model import Account, AccountStatus
from ...core.prompt import followup_text
from ...errors import (
    UpstreamAuthError,
    UpstreamError,
    UpstreamQuotaError,
    UpstreamRefused,
    UpstreamTimeout,
    UpstreamVideoLocked,
    UpstreamVmDisabled,
)
from ...upstream import muse
from ..base import (
    ChatRequest,
    ImageRequest,
    InputImage,
    MediaResult,
    VideoRequest,
)
from . import dom
from .cdp import CDPError
from .media_verify import _infer_mime_from_bytes, is_thread_url, verify_and_save_download
from .research_dom import capture_turn_expression, stop_expression
from .research_network import ResearchHttpCapture

if TYPE_CHECKING:
    from .driver import _Tab

log = logging.getLogger(__name__)


def _drv() -> Any:
    return sys.modules["muse2api.drivers.browser.driver"]


class GenerationMixin:
    async def _state(self, tab: _Tab) -> dict:
        state = await tab.session.evaluate(dom.CHAT_STATE) or {}
        tail = state.get("tail", "")
        low = tail.lower()
        if hit := next((h for h in dom.QUOTA_HINTS if h in low), None):
            i = low.find(hit)
            snippet = " ".join(tail[max(0, i - 120):i + 120].split())
            # The page tail also holds the prompt and the thread-history sidebar, and
            # matching it cooled down accounts whose images muse.ai went on to deliver.
            # Only a finished agent reply that says so counts; anything else is logged.
            reply = state.get("lastText", "")
            if not state.get("generating") and hit in reply.lower():
                log.warning("quota hint %r in reply on %s: %s", hit,
                            tab.thread_url or "new thread", snippet)
                raise UpstreamQuotaError(f"muse.ai reports the account is out of quota: …{snippet}…")
            if snippet != tab.quota_note:
                tab.quota_note = snippet
                log.info("ignoring quota hint %r outside the reply on %s: %s", hit,
                         tab.thread_url or "new thread", snippet)
        return state

    async def _snapshot_media_urls(self, tab: _Tab) -> set[str]:
        """Snapshot current media URLs on DOM before prompt is sent (P1-3)."""
        try:
            urls = await tab.session.evaluate(dom.MEDIA_URLS_SNAPSHOT) or []
            return {u for u in urls if u}
        except Exception as exc:
            log.debug("could not snapshot media urls: %s", exc)
            return set()

    async def _prepare(self, tab: _Tab, prompt: str, images: list[InputImage]) -> dict:
        await self._new_thread(tab)
        base = await self._state(tab)
        await self._attach(tab, images)
        existing = await self._snapshot_media_urls(tab)
        existing.update({a["src"] for a in base.get("attachments", []) if a.get("src")})
        base["existing_urls"] = existing
        await self._send(tab, prompt)
        return base

    async def _deny_approvals(self, tab: _Tab) -> None:
        denied = await tab.session.evaluate(dom.DENY_APPROVALS) or []
        for what in denied:
            log.info("denied muse.ai %s", what.lower())

    # ------------------------------------------------------------ chat
    async def _live_continuation(self, tab: _Tab) -> bool:
        """This tab already shows the stored conversation, even if its URL is still /thread/new."""
        last_user = next((text for role, text in reversed(tab.turns) if role == "user"), "")
        if not last_user:
            return False
        try:
            page = await tab.session.evaluate(dom.PAGE_STATE) or {}
            if not page.get("hasInput"):
                return False
            state = await self._state(tab)
            present = await tab.session.evaluate(
                f"!!(document.body && document.body.innerText.includes({dom.q(last_user[:80])}))"
            )
        except CDPError:
            return False
        if state.get("generating"):
            return False
        if any(hint in state.get("tail", "") for hint in dom.STALL_HINTS):
            return False
        if (state.get("agentCount") or 0) >= _drv().HOT_BUBBLE_LIMIT:
            return False
        return bool(present)

    async def _open_saved(self, tab: _Tab) -> None:
        tab.warm = False
        await tab.session.send("Page.navigate", {"url": tab.thread_url})
        deadline = _drv().time.monotonic() + self.settings.page_ready_timeout
        nav_retries = 0
        max_nav_retries = 2
        retry_margin = min(5.0, self.settings.page_ready_timeout * 0.25)
        while _drv().time.monotonic() < deadline:
            if getattr(tab.session, "closed", False) is True:
                raise CDPError("cdp connection closed")
            await _drv().asyncio.sleep(0.25)
            if await self._on_thread(tab):
                return
            try:
                href = await self._href(tab)
            except TimeoutError:
                href = ""
            _, nav_retries = await self._maybe_retry_error_nav(
                tab,
                href=href,
                target_url=tab.thread_url,
                nav_retries=nav_retries,
                max_nav_retries=max_nav_retries,
                deadline=deadline,
                retry_margin=retry_margin,
                label="saved conversation navigation",
            )
        raise UpstreamTimeout("saved conversation did not become ready")

    async def _begin_chat(self, account: Account, req: ChatRequest,
                          before_send: Callable[[_Tab], None] | None = None,
                          cancel: asyncio.Event | None = None,
                          after_send_setup_failure: Callable[[_Tab], None] | None = None):
        """Send ``req`` on the saved conversation when it continues that page."""
        hint = req.conversation_hint or ""
        if hint:
            # A named conversation reuses the tab already holding it; a new one takes
            # a warm thread rather than opening a cold tab.
            def owned(t: _Tab) -> bool:
                return t.has_state and (t.hint or "") == hint

            has_own = any(owned(t) for t in self._tabs.get(account.id, []))
            tab = await self._checkout(account, prefer=owned if has_own else (lambda t: owned(t) or t.warm))
        else:
            # No identity (no `user` field): never latch onto a tab another
            # anonymous request was using, or two clients without `user`
            # would share one thread. Prefer a fresh/stateless tab instead.
            tab = await self._checkout(account, prefer=lambda t: not t.has_state)
        prompt, images = req.prompt, req.images
        try:
            # The saved conversation (restored after a restart) may only go into a
            # tab while no other tab of the account already holds a conversation.
            if hint and not any(t.has_state for t in self._tabs.get(account.id, []) if t is not tab):
                self._load_hot(tab, account)
            # An anonymous request never continues a thread, even if the tab
            # it got (e.g. the at-limit fallback, or a restored hot page)
            # happens to hold one: without an identity there is no way to
            # know it is the same client. It opens a new thread instead and
            # sends the full prompt, which keeps the model in context.
            same_owner = bool(hint) and (tab.hint or "") == hint
            follow = followup_text(tab.turns, req.turns) if tab.turns and same_owner else None
            if follow and len(req.images) >= tab.image_count and (
                tab.thread_url or await self._live_continuation(tab)
            ):
                if await self._on_thread(tab) or await self._live_continuation(tab):
                    log.info("reusing hot page %s", tab.thread_url or "open page")
                elif tab.thread_url:
                    log.info("reopening %s", tab.thread_url)
                    await self._open_saved(tab)
                prompt = follow
                images = req.images[tab.image_count:]
            else:
                saved = account.meta.get("hot_page")
                if isinstance(saved, dict) and (saved.get("hint") or "") == hint:
                    account.meta.pop("hot_page", None)
                if tab.warm:
                    log.info("using a warm thread")
                    self._forget(tab)
                else:
                    log.info("opening a new thread")
                    await self._new_thread(tab)
            base = await self._state(tab)
            await self._attach(tab, images)
            if before_send is not None:
                before_send(tab)
            await self._send(tab, prompt, cancel=cancel)
            return tab, base
        except CDPError as exc:
            if after_send_setup_failure is not None:
                after_send_setup_failure(tab)
            await self._close_tab(tab)
            await self._checkin(tab)
            raise UpstreamError(f"browser error: {exc}") from exc
        except BaseException:
            if after_send_setup_failure is not None:
                after_send_setup_failure(tab)
            self._forget(tab)
            await self._checkin(tab)
            raise

    async def chat_stream(self, account: Account, req: ChatRequest) -> AsyncIterator[str]:
        if self.account is None:
            drv = await self._ensure_account_driver(account)
            async with contextlib.aclosing(drv.chat_stream(account, req)) as inner:
                async for delta in inner:
                    yield delta
            return
        tab, base = await self._begin_chat(account, req)
        base_count, base_text = base.get("agentCount", 0), base.get("lastText", "")
        started = _drv().time.monotonic()
        emitted, last, stable, got_first = "", None, 0, False
        finished = False
        errored = False
        try:
            while True:
                if req.cancel and req.cancel.is_set():
                    return
                elapsed = _drv().time.monotonic() - started
                if elapsed > req.timeout:
                    raise UpstreamTimeout("assistant reply timed out")
                if not got_first and elapsed > req.first_token_timeout:
                    raise UpstreamTimeout("no first token from assistant")
                await _drv().asyncio.sleep(_drv().POLL_INTERVAL)
                st = await self._state(tab)
                if st.get("approval"):
                    await self._deny_approvals(tab)
                    continue
                text = st.get("lastText", "")
                is_new = st.get("agentCount", 0) > base_count or (text and text != base_text)
                if not is_new or not text:
                    if elapsed > _drv().STALL_GIVE_UP and any(h in st.get("tail", "") for h in dom.STALL_HINTS):
                        raise UpstreamTimeout("upstream workspace is stuck connecting")
                    continue
                got_first = True
                if text != last:
                    delta = text[len(emitted):] if text.startswith(emitted) else text
                    if delta:
                        emitted = text
                        yield delta
                    last, stable = text, 0
                else:
                    stable += 1
                    if (not st.get("generating") and stable >= _drv().STABLE_POLLS_DONE) or stable >= _drv().STABLE_POLLS_FORCE:
                        finished = True
                        return
        except CDPError as exc:
            errored = True
            await self._close_tab(tab)
            raise UpstreamError(f"browser error: {exc}") from exc
        except BaseException:
            errored = True
            raise
        finally:
            # Only a named conversation is kept: an anonymous one can never be
            # continued, so holding it would just keep the tab from counting as fresh.
            try:
                if finished and req.conversation_hint:
                    url = ""
                    for _ in range(10):
                        url = await self._href(tab)
                        if is_thread_url(url):
                            break
                        await _drv().asyncio.sleep(0.2)
                    self._save_hot(tab, account, req, url if is_thread_url(url) else "")
                    if tab.thread_url:
                        log.info("saved hot page %s", tab.thread_url)
                    else:
                        log.info("reply finished but the thread url was not ready yet")
                else:
                    self._forget(tab)
            except BaseException:
                self._forget(tab)
                raise
            finally:
                await asyncio.shield(self._checkin(tab))
            if finished and not errored:
                self.prewarm(account)

    async def research_reply(
        self,
        account: Account,
        *,
        prompt: str,
        operation_id: str,
        stage: str,
        images: list[InputImage] | None = None,
        timeout: float = 900.0,
        conversation_hint: str | None = None,
        cancel: asyncio.Event | None = None,
    ) -> dict[str, Any]:
        """Submit one research prompt and return its bound raw assistant code node.

        This intentionally bypasses chat_stream's flattened text deltas and hot-page
        retry behavior. Once sent, this operation is observed on its exact turn only.
        """
        if self.account is None:
            drv = await self._ensure_account_driver(account)
            return await drv.research_reply(
                account, prompt=prompt, operation_id=operation_id, stage=stage,
                images=images, timeout=timeout, conversation_hint=conversation_hint, cancel=cancel,
            )
        if cancel and cancel.is_set():
            return {"status": "stopped", "operation_id": operation_id, "stage": stage,
                    "http_status": None, "http_body": None,
                    "raw_ui_code_textcontent": None, "dom_turn": None}
        req = ChatRequest(
            prompt=prompt,
            model="muse-chat",
            images=list(images or []),
            timeout=timeout,
            first_token_timeout=min(timeout, 480.0),
            conversation_hint=f"research:{operation_id}:{conversation_hint or ''}",
            turns=[("user", prompt)],
            cancel=cancel,
        )
        started = _drv().time.monotonic()
        http_capture = ResearchHttpCapture(operation_id, stage)
        tab, _base = await asyncio.wait_for(
            self._begin_chat(account, req, before_send=http_capture.attach, cancel=cancel,
                             after_send_setup_failure=lambda t: http_capture.detach(t.session)),
            timeout=timeout)
        expression = capture_turn_expression(account.id, operation_id, stage)
        capture: dict[str, Any] = {"pending": True, "reason": "waiting-for-assistant"}
        try:
            while True:
                if cancel and cancel.is_set():
                    with contextlib.suppress(CDPError, TimeoutError):
                        await tab.session.evaluate(stop_expression(operation_id, stage), timeout=5.0)
                    return {
                        "status": "stopped", "operation_id": operation_id, "stage": stage,
                        "http_status": None, "http_body": None,
                        "raw_ui_code_textcontent": capture.get("raw_ui_code_textcontent"),
                        "dom_turn": capture if capture.get("owner_id") else None,
                    }
                elapsed = _drv().time.monotonic() - started
                if elapsed >= timeout:
                    raise UpstreamTimeout("research assistant reply timed out")
                if elapsed >= min(timeout, 480.0) and capture.get("reason") == "waiting-for-assistant":
                    raise UpstreamTimeout("no first research response from assistant")
                state = await self._state(tab)
                if state.get("approval"):
                    await self._deny_approvals(tab)
                capture = await tab.session.evaluate(expression, timeout=10.0) or capture
                if capture.get("pending"):
                    await _drv().asyncio.sleep(_drv().POLL_INTERVAL)
                    continue
                if capture.get("completed"):
                    binding = {key: capture.get(key) for key in (
                        "owner_id", "thread_id", "user_message_id", "assistant_message_id",
                        "user_turn_id", "assistant_turn_id",
                    )}
                    http_receipt = await http_capture.snapshot(tab.session)
                    return {
                        "status": "completed" if not capture.get("error") else "protocol_error",
                        "operation_id": operation_id,
                        "stage": stage,
                        **http_receipt,
                        "raw_ui_code_textcontent": capture.get("raw_ui_code_textcontent"),
                        "dom_turn": capture,
                        "binding": binding,
                        "capture_error": capture.get("error"),
                    }
                raise UpstreamError(f"research DOM capture failed: {capture.get('error', 'unknown')}")
        except BaseException:
            raise
        finally:
            http_capture.detach(tab.session)
            self._forget(tab)
            await asyncio.shield(self._checkin(tab))

    # ------------------------------------------------------------ warm threads
    _WARM_REPLY_TIMEOUT = 90.0
    _WARM_RETRY_DELAY = 60.0
    _WARM_MAX_FAILED_ROUNDS = 3

    @staticmethod
    def _is_prewarm_eligible(account: Account) -> bool:
        return (
            account.enabled
            and account.status == AccountStatus.ACTIVE
            and not account.quota_exhausted()
            and not muse.missing_session_cookies(account.cookies)
        )

    def _can_prewarm(self, account: Account) -> bool:
        if self._closed or self.settings.warm_threads <= 0:
            return False
        if not self._is_prewarm_eligible(account):
            return False
        if self._account_blocked_cb is not None and self._account_blocked_cb(account.id):
            return False
        if self._account_lookup is not None:
            latest = self._account_lookup(account.id)
            if latest is None or not self._is_prewarm_eligible(latest):
                return False
        return True

    def prewarm(self, account: Account) -> None:
        """Top up ``account``'s warm threads in the background (``MUSE2API_WARM_THREADS``)."""
        if not self._can_prewarm(account):
            return
        if self.account is None:
            existing = self._prewarm_tasks.get(account.id)
            if existing and not existing.done():
                return
            async def _kick() -> None:
                try:
                    if not self._can_prewarm(account):
                        return
                    drv = await self._ensure_account_driver(account)
                    if not drv._can_prewarm(account):
                        return
                    if not drv.browser_alive:
                        if drv._chromium is None and drv._browser is None:
                            async with drv._recovery_lock:
                                if drv._closed or not drv._can_prewarm(account):
                                    return
                                if drv._chromium is None and drv._browser is None:
                                    await drv.startup()
                        if not drv._closed and not drv.browser_alive and drv._can_prewarm(account):
                            await drv._recover_browser()
                    if drv._can_prewarm(account):
                        drv.prewarm(account)
                except asyncio.CancelledError:
                    raise
                except Exception as exc:  # noqa: BLE001
                    log.warning("prewarm kick failed for %s: %s", account.id, exc)
            self._prewarm_tasks[account.id] = asyncio.create_task(_kick())
            return
        if self._browser is None:
            return
        task = self._warming.get(account.id)
        if task and not task.done():
            return
        self._warming[account.id] = asyncio.create_task(self._fill_warm(account))

    async def _fill_warm(self, account: Account) -> None:
        """Warm the missing threads in parallel, round after round, until the account has
        enough. A failed round (a cold page load often times out) is retried after a
        pause, up to ``_WARM_MAX_FAILED_ROUNDS`` in a row."""
        failed = 0
        while self._can_prewarm(account):
            batch: list[_Tab] = []
            try:
                while self._can_prewarm(account):
                    tab = await self._checkout_for_warming(account, len(batch))
                    if tab is None:
                        break
                    batch.append(tab)
            except BaseException as exc:
                for t in batch:
                    t.busy = False
                    t.warming = False
                cancelled: asyncio.CancelledError | None = (
                    exc if isinstance(exc, asyncio.CancelledError) else None
                )
                for t in batch:
                    try:
                        await asyncio.shield(self._checkin(t))
                    except asyncio.CancelledError as c_exc:
                        if cancelled is None:
                            cancelled = c_exc
                    except Exception:  # noqa: BLE001
                        pass
                if cancelled is not None:
                    if cancelled is exc:
                        raise
                    raise cancelled from exc
                log.warning("prewarm checkout aborted for %s: %s", account.id, exc)
                return
            if not batch:
                return
            if not self._can_prewarm(account):
                for t in batch:
                    t.busy = False
                    t.warming = False
                await asyncio.shield(self._notify_tabs())
                return
            results = await asyncio.gather(*(self._warm_one(account, t) for t in batch))
            if any(r is None for r in results):
                return
            if all(results):
                failed = 0
                continue
            failed += 1
            if failed >= self._WARM_MAX_FAILED_ROUNDS:
                return
            if not self._can_prewarm(account):
                return
            await _drv().asyncio.sleep(self._WARM_RETRY_DELAY)

    async def _warm_one(self, account: Account, tab: _Tab) -> bool | None:
        try:
            if not self._can_prewarm(account):
                self._forget(tab)
                return None
            await self._warm_up(tab)
        except (UpstreamAuthError, UpstreamVmDisabled, UpstreamQuotaError) as exc:
            self._forget(tab)
            log.warning("warming a thread for %s aborted (non-transient): %s", account.id, exc)
            return None
        except Exception as exc:  # noqa: BLE001
            self._forget(tab)
            if isinstance(exc, CDPError):
                await self._close_tab(tab)
            log.warning("warming a thread for %s failed: %s", account.id, exc)
            return False
        finally:
            await asyncio.shield(self._checkin(tab))
        log.info("warm thread ready for account %s", account.id)
        return True

    async def _checkout_for_warming(self, account: Account, pending: int = 0) -> _Tab | None:
        """A tab to warm, but only while the account has warm threads missing (counting
        the ``pending`` ones being warmed) and a slot is still left for a real request."""
        limit = max(1, self.settings.account_max_concurrency)
        async with self._tab_cond:
            if not self._can_prewarm(account):
                return None
            tabs = [t for t in self._tabs.get(account.id, []) if not t.closed]
            if sum(t.warm for t in tabs) + pending >= self.settings.warm_threads:
                return None
            idle = [t for t in tabs if not (t.busy or t.stale or t.session.closed)]
            free = len(idle) + limit - len(tabs) - self._opening.get(account.id, 0)
            if free < 2:
                return None
        return await self._checkout(
            account,
            prefer=lambda t: not t.has_state and not t.warm,
            warming=True,
        )

    async def _warm_up(self, tab: _Tab) -> None:
        await self._new_thread(tab)
        base = (await self._state(tab)).get("agentCount", 0)
        await self._send(tab, self.settings.warm_prompt)
        deadline = _drv().time.monotonic() + self._WARM_REPLY_TIMEOUT
        last, stable = None, 0
        while _drv().time.monotonic() < deadline:
            await _drv().asyncio.sleep(_drv().POLL_INTERVAL)
            st = await self._state(tab)
            if st.get("approval"):
                await self._deny_approvals(tab)
                continue
            text = st.get("lastText", "")
            if st.get("agentCount", 0) <= base or not text:
                continue
            if text != last:
                last, stable = text, 0
            elif not st.get("generating") and (stable := stable + 1) >= _drv().STABLE_POLLS_DONE:
                tab.warm = True
                return
        raise UpstreamTimeout("no reply to the warm-up message")

    # ------------------------------------------------------------ media
    @staticmethod
    def _media_prompt(prompt: str, kind: str, size: str | None, duration: int | None = None) -> str:
        hints = []
        if size:
            hints.append(f"aspect ratio {size}")
        if duration:
            hints.append(f"{duration} seconds long")
        verb = "Generate an image" if kind == "image" else "Generate a video"
        suffix = f" ({', '.join(hints)})" if hints else ""
        return f"{verb}{suffix}: {prompt}"

    # muse.ai posts a completion sentence ("Here's your video…") a few seconds
    # before the media attachment's blob src is ready, so a reply that is "text
    # only" right now may still be finalising. Keep waiting this long for the
    # attachment to appear/load before treating the reply as a refusal.
    _MEDIA_GRACE = {"video": 90.0, "image": 8.0}
    # The timeout is soft while muse.ai is visibly still working (stop button
    # shown or attachment loading): giving up then threw away images that landed
    # a minute later, and the failover started the whole generation over.
    _OVERRUN_FACTOR = 2.5

    async def _wait_video_mount(
        self, tab: _Tab, timeout: float = 6.0, existing_urls: set[str] | None = None
    ) -> bool:
        """Poll every 100ms up to 6s for a NEW <video> to mount src or data-hatch-video-src (P1-2)."""
        res = await tab.session.evaluate(
            dom.wait_video_mount(
                timeout_ms=int(timeout * 1000),
                interval_ms=100,
                existing_urls=existing_urls,
            ),
            await_promise=True,
            timeout=int(timeout + 2),
        )
        mounted = bool((res or {}).get("mounted"))
        if not mounted:
            log.warning("video element did not mount src within %.1fs; continuing with fallback", timeout)
        return mounted

    async def _wait_media(self, tab: _Tab, base: dict, kind: str, timeout: float,
                          on_progress, cancel: asyncio.Event | None) -> dict:
        base_atts = len(base.get("attachments", []))
        base_count = base.get("agentCount", 0)
        existing_urls = set(base.get("existing_urls") or [])
        grace = self._MEDIA_GRACE.get(kind, 8.0)
        started = _drv().time.monotonic()
        text_done_at: float | None = None
        denied = asked_to_attach = busy = False
        while True:
            elapsed = _drv().time.monotonic() - started
            if elapsed >= timeout and not (busy and elapsed < timeout * self._OVERRUN_FACTOR):
                break
            if cancel and cancel.is_set():
                raise asyncio.CancelledError
            await _drv().asyncio.sleep(0.6)
            st = await self._state(tab)
            if st.get("approval"):
                await self._deny_approvals(tab)
                denied, text_done_at = True, None
                continue
            new_atts = st.get("attachments", [])[base_atts:]

            # P1-2: If kind is video and any video attachment is missing src or has old src, wait for mount
            if kind == "video":
                unmounted = any(
                    a.get("kind") == "video" and (not a.get("src") or a.get("src") in existing_urls)
                    for a in new_atts
                )
                if unmounted:
                    await self._wait_video_mount(tab, timeout=6.0, existing_urls=existing_urls)
                    st = await self._state(tab)
                    new_atts = st.get("attachments", [])[base_atts:]

            # P1-3: Filter attachments by kind and NOT in existing_urls
            fresh = [
                a for a in new_atts
                if a.get("src") and a.get("src") not in existing_urls and a.get("kind") == kind
            ]
            if fresh:
                return fresh[-1]

            # P1-4: Detect Muse video lock response ONLY on a new assistant reply bubble
            if (
                kind == "video"
                and not st.get("generating")
                and not fresh
                and st.get("agentCount", 0) > base_count
            ):
                text_low = (st.get("lastText") or "").lower()
                if any(h in text_low for h in dom.VIDEO_LOCKED_HINTS):
                    matched_hint = next(h for h in dom.VIDEO_LOCKED_HINTS if h in text_low)
                    raw_text = st.get("lastText") or matched_hint
                    log.warning(
                        "video locked detected on account %s (matched %r): %.200s",
                        tab.account_id, matched_hint, raw_text,
                    )
                    raise UpstreamVideoLocked(
                        f"muse.ai reports video generation is locked: {raw_text[:200]}"
                    )

            busy = bool(st.get("generating")) or any(kind in (a.get("tid") or "") for a in new_atts)
            elapsed = _drv().time.monotonic() - started
            if on_progress:
                on_progress(min(95, int(elapsed / timeout * 100)))
            # An attachment node for this kind is on the page but its (blob) src
            # has not loaded yet -> media is still finalising, keep waiting.
            if any(kind in (a.get("tid") or "") for a in new_atts):
                text_done_at = None
                continue
            # No attachment yet. The reply may be genuinely text-only (a refusal),
            # or the attachment may simply lag the completion sentence.
            text = st.get("lastText", "")
            if st.get("agentCount", 0) > base_count and text and not st.get("generating"):
                if text_done_at is None:
                    text_done_at = _drv().time.monotonic()
                elif _drv().time.monotonic() - text_done_at >= grace:
                    if kind == "video" and not asked_to_attach:
                        await self._wait_video_mount(tab, timeout=6.0, existing_urls=existing_urls)
                        st = await self._state(tab)
                        new_atts = st.get("attachments", [])[base_atts:]
                        fresh = [
                            a for a in new_atts
                            if a.get("src") and a.get("src") not in existing_urls and a.get("kind") == kind
                        ]
                        if fresh:
                            return fresh[-1]

                    if not asked_to_attach:
                        # The agent often finishes with the file only in its workspace:
                        # after a denied upload, after post-processing that it shows as
                        # an image gallery (raw, QC crops and final mixed together), or
                        # when it recognises a repeat request and points at the earlier
                        # file. Asking once gets the final file attached in chat.
                        log.info("%s not attached (%s); asking for it in chat: %.120s", kind,
                                 "denied approval" if denied else "text-only reply", text)
                        await self._send(tab, f"Please attach the final {kind} file here in "
                                              "the chat" + (" instead of uploading it." if denied
                                                            else ", as a single attachment."))
                        asked_to_attach, text_done_at = True, None
                        base_count = st.get("agentCount", 0)
                        continue
                    # Last resort: media delivered as a link in the text bubble.
                    probe = await tab.session.evaluate(dom.LAST_BUBBLE_MEDIA) or {}
                    cand_links = [u for u in probe.get("links", []) if u not in existing_urls]
                    url = self._pick_media_url(cand_links, kind)
                    if url:
                        log.info("%s delivered as link in text bubble: %s", kind, url)
                        return {"src": url, "kind": kind}
                    log.info("%s: no media after %.0fs grace; links=%s html=%.300s",
                             kind, grace, probe.get("links", []), probe.get("html", ""))
                    if kind == "video" and any(h in text.lower() for h in dom.VIDEO_LOCKED_HINTS):
                        raise UpstreamVideoLocked(f"muse.ai reports video generation is locked: {text[:200]}")
                    raise UpstreamRefused(f"upstream replied with text only: {text[:200]}")
            else:
                text_done_at = None
        if busy:
            log.warning("%s still generating after %.0fs; giving up", kind, elapsed)
        raise UpstreamTimeout(f"{kind} generation timed out")

    _VIDEO_EXT = (".mp4", ".webm", ".mov", ".m4v")
    _IMAGE_EXT = (".png", ".jpg", ".jpeg", ".webp", ".gif", ".avif")
    _MEDIA_HOSTS = ("videodelivery.net", "cloudflarestream.com", "cloudflarestorage.com",
                    "amazonaws.com", "storage.googleapis.com", "blob.core.windows.net",
                    "musecdn", "cdn.muse", "muse-cdn")
    _MEDIA_PATHS = ("/media/", "/download", "/dl/", "/video", "/attachment", "/files/")

    @classmethod
    def _pick_media_url(cls, links: list[str], kind: str) -> str | None:
        """Best media URL from a reply bubble, or None if nothing looks like one."""
        exts = cls._VIDEO_EXT if kind == "video" else cls._IMAGE_EXT

        def score(u: str) -> int:
            low = u.lower()
            path = low.split("?", 1)[0].split("#", 1)[0]
            if u.startswith("blob:") or path.endswith(exts):
                return 3
            if any(h in low for h in cls._MEDIA_HOSTS):
                return 2
            if any(seg in low for seg in cls._MEDIA_PATHS):
                return 1
            return 0

        cands = [u for u in links if u and not u.startswith(("mailto:", "javascript:"))]
        cands.sort(key=score, reverse=True)
        return cands[0] if cands and score(cands[0]) > 0 else None

    async def _download(self, tab: _Tab, att: dict, kind: str) -> MediaResult:
        res = await tab.session.evaluate(dom.fetch_as_base64(att["src"]), await_promise=True,
                                         timeout=300)
        if not (res or {}).get("ok"):
            raise UpstreamError(f"failed to download generated {kind}: {(res or {}).get('err')}")
        raw_b64 = (res or {}).get("b64") or ""
        try:
            raw_bytes = base64.b64decode(raw_b64)
        except Exception as exc:
            raise UpstreamError(f"corrupt base64 data for {kind}: {exc}") from exc

        verified_data = await verify_and_save_download(raw_bytes, kind)
        raw_mime = (res.get("mime") or "").strip()
        base_mime = raw_mime.split(";")[0].strip().lower()
        if not base_mime or base_mime == "application/octet-stream":
            mime = _infer_mime_from_bytes(verified_data, kind)
        else:
            mime = raw_mime
        return MediaResult(data=verified_data, mime=mime, kind=kind,
                           width=att.get("w") or None, height=att.get("h") or None)

    async def _generate(self, account: Account, prompt: str, images: list[InputImage],
                        kind: str, timeout: float, on_progress, cancel) -> MediaResult:
        # Media always starts a fresh thread, so keep tabs holding a chat (or a warm
        # thread) for chats.
        tab = await self._checkout(account, prefer=lambda t: not t.has_state and not t.warm)
        try:
            base = await self._prepare(tab, prompt, images)
            att = await self._wait_media(tab, base, kind, timeout, on_progress, cancel)
            return await self._download(tab, att, kind)
        except CDPError as exc:
            await self._close_tab(tab)
            raise UpstreamError(f"browser error: {exc}") from exc
        finally:
            await self._checkin(tab)

    async def generate_image(self, account: Account, req: ImageRequest) -> list[MediaResult]:
        if self.account is None:
            drv = await self._ensure_account_driver(account)
            return await drv.generate_image(account, req)
        prompt = self._media_prompt(req.prompt, "image", req.size)
        results = []
        for _ in range(max(1, req.n)):
            r = await self._generate(account, prompt, req.reference_images, "image",
                                     req.timeout, req.on_progress, req.cancel)
            r.revised_prompt = req.prompt
            results.append(r)
        return results

    @staticmethod
    async def _acquire_video_lock(
        lock: asyncio.Lock,
        *,
        deadline: float,
        cancel: asyncio.Event | None,
    ) -> None:
        if cancel and cancel.is_set():
            raise asyncio.CancelledError
        remaining = deadline - _drv().time.monotonic()
        if remaining <= 0:
            raise UpstreamTimeout("video generation timed out while waiting for account video lock")
        acquire_task = asyncio.create_task(lock.acquire())
        cancel_task = asyncio.create_task(cancel.wait()) if cancel is not None else None
        wait_tasks: set[asyncio.Task[Any]] = {acquire_task}
        if cancel_task is not None:
            wait_tasks.add(cancel_task)
        try:
            done, _pending = await asyncio.wait(
                wait_tasks,
                timeout=remaining,
                return_when=asyncio.FIRST_COMPLETED,
            )
            if acquire_task in done and not acquire_task.cancelled():
                acquire_task.result()
                if cancel and cancel.is_set():
                    raise asyncio.CancelledError
                return
            if cancel_task is not None and cancel_task in done:
                raise asyncio.CancelledError
            raise UpstreamTimeout("video generation timed out while waiting for account video lock")
        except BaseException:
            if not acquire_task.done():
                acquire_task.cancel()
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await acquire_task
            if (
                acquire_task.done()
                and not acquire_task.cancelled()
                and acquire_task.exception() is None
            ):
                lock.release()
            raise
        finally:
            if cancel_task is not None and not cancel_task.done():
                cancel_task.cancel()
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await cancel_task

    def _check_video_account_ready(self, account: Account) -> None:
        if self._closed:
            raise UpstreamError("browser driver is closed")
        if account.status == AccountStatus.VIDEO_LOCKED:
            raise UpstreamVideoLocked(f"account '{account.id}' video generation is locked")
        if account.status == AccountStatus.INVALID:
            raise UpstreamAuthError(f"account '{account.id}' is invalid")
        if not account.enabled:
            raise UpstreamError(f"account '{account.id}' is disabled")
        if account.quota_exhausted():
            raise UpstreamQuotaError(f"account '{account.id}' is out of quota")
        if account.status == AccountStatus.COOLING and not account.is_available():
            raise UpstreamTimeout(f"account '{account.id}' is cooling down")
        if missing := muse.missing_session_cookies(account.cookies):
            raise UpstreamAuthError(
                f"account '{account.id}' is missing session cookies: {', '.join(missing)}"
            )

    async def generate_video(self, account: Account, req: VideoRequest) -> MediaResult:
        if req.cancel and req.cancel.is_set():
            raise asyncio.CancelledError
        if self.account is None:
            drv = await self._ensure_account_driver(account)
            return await drv.generate_video(account, req)
        prompt = self._media_prompt(req.prompt, "video", req.size, req.duration)
        images = [req.first_frame] if req.first_frame else []
        deadline = _drv().time.monotonic() + max(0.0, req.timeout)
        lock = self._video_locks.setdefault(account.id, asyncio.Lock())
        await self._acquire_video_lock(lock, deadline=deadline, cancel=req.cancel)
        try:
            remaining = deadline - _drv().time.monotonic()
            if remaining <= 0:
                raise UpstreamTimeout("video generation timed out while waiting for account video lock")
            self._check_video_account_ready(account)
            return await self._generate(account, prompt, images, "video", remaining,
                                        req.on_progress, req.cancel)
        finally:
            lock.release()

