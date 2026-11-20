"""Regression tests for Round 2 Sidecar audit fixes (#25.1 - #25.7)."""

from __future__ import annotations

import asyncio
import json
import re
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from httpx import ASGITransport, AsyncClient

from muse2api.accounts.model import Account, AccountStatus
from muse2api.api.deps import require_admin_key
from muse2api.api.routes.images import MAX_REFERENCE_IMAGES
from muse2api.app import create_app
from muse2api.config import Settings
from muse2api.core.media import _MAX_FETCH_BYTES
from muse2api.drivers.base import ChatRequest
from muse2api.drivers.browser.cdp import CDPError
from muse2api.drivers.browser.chromium import _build_wmi_filter
from muse2api.drivers.browser.driver import (
    _PORT_OWNERS,
    _USED_PORTS,
    BrowserDriver,
    _Tab,
    allocate_cdp_port,
    release_cdp_port,
)
from muse2api.drivers.browser.proxy_relay import LocalProxyRelay
from muse2api.errors import NoAccountAvailable, NotFound

_VALID_COOKIES = {
    "hatch_sess": "sess_1",
    "hatch_native_auth_device": "dev_1",
}


def _eval_powershell_wmi_filter(flt: str, cmd: str) -> bool:
    """Evaluate PowerShell `-and` / `-or` / `-imatch` clauses in `_build_wmi_filter`."""
    tokens = re.split(r"(\(|\)| -or | -and )", flt)
    pos = 0

    def _skip_ws() -> None:
        nonlocal pos
        while pos < len(tokens) and not tokens[pos].strip():
            pos += 1

    def _eval_atom(expr: str) -> bool:
        m_dir = re.search(
            r"--user-data-dir=\"\?'\s*\+\s*\[regex\]::Escape\('([^']+)'\)\s*\+\s*'([^']+)'",
            expr,
        )
        if m_dir:
            escaped_path = m_dir.group(1).replace("''", "'")
            suffix_pat = m_dir.group(2)
            pat = r'--user-data-dir="?' + re.escape(escaped_path) + suffix_pat
            return re.search(pat, cmd, re.IGNORECASE) is not None
        m_port = re.search(r"-imatch '(--remote-debugging-port=\d+\(\\s\|\$\))'", expr)
        if m_port:
            return re.search(m_port.group(1), cmd, re.IGNORECASE) is not None
        raise AssertionError(f"unexpected WMI filter atom: {expr!r}")

    def _parse_or() -> bool:
        nonlocal pos
        val = _parse_and()
        while True:
            _skip_ws()
            if pos < len(tokens) and tokens[pos] == " -or ":
                pos += 1
                rhs = _parse_and()
                val = val or rhs
            else:
                return val

    def _parse_and() -> bool:
        nonlocal pos
        val = _parse_primary()
        while True:
            _skip_ws()
            if pos < len(tokens) and tokens[pos] == " -and ":
                pos += 1
                rhs = _parse_primary()
                val = val and rhs
            else:
                return val

    def _parse_primary() -> bool:
        nonlocal pos
        _skip_ws()
        if tokens[pos] == "(":
            pos += 1
            # Parenthesized atom like `($_.CommandLine -imatch ...)` vs nested group
            depth = 1
            raw_parts: list[str] = []
            lookahead = pos
            has_logical = False
            while lookahead < len(tokens) and depth > 0:
                tk = tokens[lookahead]
                if tk == "(":
                    depth += 1
                elif tk == ")":
                    depth -= 1
                    if depth == 0:
                        break
                elif tk in (" -or ", " -and "):
                    has_logical = True
                raw_parts.append(tk)
                lookahead += 1
            if not has_logical:
                pos = lookahead + 1
                return _eval_atom("".join(raw_parts))
            val = _parse_or()
            _skip_ws()
            assert tokens[pos] == ")"
            pos += 1
            return val
        raise AssertionError(f"unexpected token at {pos}: {tokens[pos:]}")

    return _parse_or()


# ── #25.1: _build_wmi_filter & failed startup port reservation ───────────────


def test_build_wmi_filter_never_matches_sibling_account_even_on_same_port(
    tmp_path: Path,
) -> None:
    profiles = tmp_path / "profiles"
    p1 = profiles / "acc_1"
    p2 = profiles / "acc_2"
    p10 = profiles / "acc_10"
    p1.mkdir(parents=True)
    p2.mkdir(parents=True)

    flt = _build_wmi_filter(p1, port=19212)
    # Parent dir must never appear without the account's own directory name
    assert f"[regex]::Escape('{profiles}')" not in flt

    cmd_acc1 = f'msedge.exe --remote-debugging-port=19212 --user-data-dir="{p1}" --headless=new'
    cmd_acc2_same_port = (
        f'msedge.exe --remote-debugging-port=19212 --user-data-dir="{p2}" --headless=new'
    )
    cmd_acc10_same_port = (
        f'msedge.exe --remote-debugging-port=19212 --user-data-dir="{p10}" --headless=new'
    )

    assert _eval_powershell_wmi_filter(flt, cmd_acc1) is True
    assert _eval_powershell_wmi_filter(flt, cmd_acc2_same_port) is False
    assert _eval_powershell_wmi_filter(flt, cmd_acc10_same_port) is False

    # Short 8.3 path variant is also matched when distinct on Windows
    short_p1 = str(tmp_path / "PROFILES~1" / "ACC_1~1")
    with patch("muse2api.drivers.browser.chromium._win_short_path", return_value=short_p1):
        flt_short = _build_wmi_filter(p1, port=19212)
    cmd_short = (
        f'msedge.exe --remote-debugging-port=19212 --user-data-dir="{short_p1}" --headless=new'
    )
    assert _eval_powershell_wmi_filter(flt_short, cmd_short) is True
    assert _eval_powershell_wmi_filter(flt_short, cmd_acc2_same_port) is False


async def test_failed_startup_keeps_port_reserved_across_multi_account_allocation(
    tmp_path: Path,
) -> None:
    settings = Settings(data_dir=tmp_path / "data", cdp_port=19620)
    root = BrowserDriver(settings)
    acc1 = Account(id="acc_p1", cookies=dict(_VALID_COOKIES))
    acc2 = Account(id="acc_p2", cookies=dict(_VALID_COOKIES))
    acc3 = Account(id="acc_p3", cookies=dict(_VALID_COOKIES))

    drv1 = await root._ensure_account_driver(acc1)
    p1 = drv1.port
    assert p1 in _USED_PORTS
    assert _PORT_OWNERS.get(p1) is drv1

    with (
        patch("muse2api.drivers.browser.driver.find_chromium", return_value="msedge.exe"),
        patch(
            "muse2api.drivers.browser.driver.ChromiumProcess.start",
            AsyncMock(return_value=f"ws://127.0.0.1:{p1}/b"),
        ),
        patch("muse2api.drivers.browser.driver.ChromiumProcess.stop", AsyncMock()),
        patch(
            "muse2api.drivers.browser.driver.CDPSession.connect",
            AsyncMock(side_effect=CDPError("boom")),
        ),
        pytest.raises(CDPError, match="boom"),
    ):
        await drv1.startup()

    # drv1 remains in _account_drivers and MUST keep p1 reserved so acc2 gets a distinct port
    assert p1 in _USED_PORTS
    assert _PORT_OWNERS.get(p1) is drv1
    drv2 = await root._ensure_account_driver(acc2)
    assert drv2.port != p1

    # If p1 were externally freed and reused by drv3, drv1.startup() re-allocates a fresh port
    release_cdp_port(p1)
    reused_port = allocate_cdp_port(p1)
    assert reused_port == p1
    drv3 = BrowserDriver(settings, account=acc3, port=reused_port)
    _PORT_OWNERS[reused_port] = drv3
    drv3._port_reserved = True
    root._account_drivers[acc3.id] = drv3

    mock_browser = MagicMock(closed=False, close=AsyncMock())

    async def fake_start_any(self):
        return f"ws://127.0.0.1:{self.port}/b"

    with (
        patch("muse2api.drivers.browser.driver.find_chromium", return_value="msedge.exe"),
        patch("muse2api.drivers.browser.driver.ChromiumProcess.start", fake_start_any),
        patch("muse2api.drivers.browser.driver.ChromiumProcess.stop", AsyncMock()),
        patch(
            "muse2api.drivers.browser.driver.CDPSession.connect",
            AsyncMock(return_value=mock_browser),
        ),
    ):
        await drv1.startup()

    assert drv1.port != p1
    assert drv1.port in _USED_PORTS
    assert _PORT_OWNERS.get(drv1.port) is drv1
    assert _PORT_OWNERS.get(p1) is drv3

    await root.shutdown()
    assert p1 not in _USED_PORTS
    assert drv2.port not in _USED_PORTS
    assert drv1.port not in _USED_PORTS


# ── #25.2: _kick() recovers crashed browser & respects deleted accounts ──────


async def test_prewarm_kick_recovers_crashed_browser_and_skips_deleted_account(
    tmp_path: Path,
) -> None:
    settings = Settings(
        data_dir=tmp_path / "data",
        driver="browser",
        warm_threads=1,
        account_max_concurrency=2,
        defer_prewarm=True,
    )
    app = create_app(settings)
    svc = app.state.services
    root = svc.driver
    assert isinstance(root, BrowserDriver)

    acc = Account(id="acc_crash", cookies=dict(_VALID_COOKIES))
    await svc.pool.upsert(acc)

    sub = await root._ensure_account_driver(acc)
    old_chromium = MagicMock()
    old_chromium.stop = AsyncMock()
    sub._chromium = old_chromium
    sub._browser = MagicMock(closed=True, close=AsyncMock())
    sub._contexts[acc.id] = "dead_ctx"
    stale_sess = MagicMock(closed=True, close=AsyncMock())
    sub._tabs[acc.id] = [_Tab(acc.id, "dead_ctx", "dead_t", stale_sess)]

    startup_calls = 0

    async def fake_startup() -> None:
        nonlocal startup_calls
        startup_calls += 1
        sub._browser = MagicMock(closed=False, close=AsyncMock())
        sub._chromium = MagicMock(stop=AsyncMock())

    with (
        patch.object(sub, "startup", side_effect=fake_startup),
        patch.object(sub, "prewarm") as sub_prewarm_spy,
        patch("muse2api.drivers.browser.driver._kill_stale_browsers", return_value=0),
        patch("muse2api.drivers.browser.driver._is_port_in_use", return_value=False),
    ):
        root.prewarm(acc)
        kick_task = root._prewarm_tasks[acc.id]
        await kick_task

    old_chromium.stop.assert_awaited_once()
    assert "dead_ctx" not in sub._contexts.values()
    assert sub._tabs == {}
    assert startup_calls == 1
    sub_prewarm_spy.assert_called_once_with(acc)

    # Deleted account: _ensure_account_driver raises NotFound and registers nothing
    acc_del = Account(id="acc_gone", cookies=dict(_VALID_COOKIES))
    with pytest.raises(NotFound):
        await root._ensure_account_driver(acc_del)
    assert acc_del.id not in root._account_drivers

    await root.shutdown()


# ── #25.3: account_quota reservation vs DELETE / PATCH / open-login / acquire ─


async def test_account_quota_serializes_against_delete_patch_open_login_and_acquire(
    tmp_path: Path,
) -> None:
    settings = Settings(
        data_dir=tmp_path / "data",
        media_dir=tmp_path / "media",
        driver="browser",
        defer_prewarm=True,
    )
    app = create_app(settings)
    app.dependency_overrides[require_admin_key] = lambda: None
    svc = app.state.services
    root = svc.driver
    assert isinstance(root, BrowserDriver)

    acc1 = Account(id="acc_q1", label="Q1", cookies=dict(_VALID_COOKIES))
    acc2 = Account(id="acc_q2", label="Q2", cookies=dict(_VALID_COOKIES))
    await svc.pool.upsert(acc1)
    await svc.pool.upsert(acc2)

    entered_ensure = asyncio.Event()
    release_ensure = asyncio.Event()

    mock_sub = MagicMock()
    mock_sub.proxy = ""
    mock_sub.is_account_busy.return_value = False
    mock_sub.quota = AsyncMock(return_value={"plan": "Pro", "weekly_used_pct": 10, "found": True})
    mock_sub.shutdown = AsyncMock()

    async def slow_ensure(account: Account):
        entered_ensure.set()
        await release_ensure.wait()
        root._account_drivers[account.id] = mock_sub
        return mock_sub

    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        with patch.object(root, "_ensure_account_driver", side_effect=slow_ensure):
            quota_task = asyncio.create_task(client.get(f"/admin/accounts/{acc1.id}/quota"))
            await asyncio.wait_for(entered_ensure.wait(), timeout=2.0)

            # While quota is in flight (even before _ensure_account_driver finishes),
            # DELETE, PATCH proxy, open-login, and second quota return 409
            r_del = await client.delete(f"/admin/accounts/{acc1.id}")
            assert r_del.status_code == 409
            assert svc.pool.get(acc1.id) is not None

            r_patch = await client.patch(
                f"/admin/accounts/{acc1.id}",
                json={"proxy": "http://127.0.0.1:8888"},
            )
            assert r_patch.status_code == 409

            r_login = await client.post(f"/admin/accounts/{acc1.id}/open-login")
            assert r_login.status_code == 409

            r_q2 = await client.get(f"/admin/accounts/{acc1.id}/quota")
            assert r_q2.status_code == 409

            # Concurrent pool.acquire() must skip acc1 and choose acc2
            leased = await svc.pool.acquire()
            assert leased.id == acc2.id
            await svc.pool.release(leased)

            release_ensure.set()
            r_ok = await quota_task
            assert r_ok.status_code == 200
            assert r_ok.json()["quota"]["plan"] == "Pro"

        # After quota completes, reservation is released and DELETE succeeds
        r_del_ok = await client.delete(f"/admin/accounts/{acc1.id}")
        assert r_del_ok.status_code == 200
        assert svc.pool.get(acc1.id) is None

    await root.shutdown()


# ── #25.4: chat_stream cancel during post-reply _href() still checks in tab ──


async def test_chat_stream_cancel_during_post_reply_href_checks_in_tab(tmp_path: Path) -> None:
    acc = Account(id="acc_href", cookies=dict(_VALID_COOKIES))
    settings = Settings(data_dir=tmp_path / "data")
    drv = BrowserDriver(settings, account=acc)
    tab = _Tab(acc.id, "ctx", "tgt", MagicMock(closed=False), busy=True)
    drv._tabs[acc.id] = [tab]

    href_entered = asyncio.Event()

    async def hanging_href(_tab: _Tab) -> str:
        href_entered.set()
        await asyncio.Future()
        return ""

    async def consume_all() -> None:
        req = ChatRequest(prompt="hi", model="muse-chat", conversation_hint="conv-key")
        chunks = [c async for c in drv.chat_stream(acc, req)]
        assert chunks == ["done"]

    with (
        patch("muse2api.drivers.browser.driver.POLL_INTERVAL", 0.0),
        patch("muse2api.drivers.browser.driver.STABLE_POLLS_DONE", 1),
        patch.object(
            drv,
            "_begin_chat",
            AsyncMock(return_value=(tab, {"agentCount": 0, "lastText": ""})),
        ),
        patch.object(
            drv,
            "_state",
            AsyncMock(return_value={"agentCount": 1, "lastText": "done", "generating": False}),
        ),
        patch.object(drv, "_href", side_effect=hanging_href),
    ):
        task = asyncio.create_task(consume_all())
        await asyncio.wait_for(href_entered.wait(), timeout=2.0)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

    assert tab.busy is False
    assert tab.thread_url == ""
    assert drv.is_account_busy(acc.id) is False


# ── #25.5: chunked /v1/images/edits upload capped without Content-Length ─────


async def test_edit_images_chunked_upload_exceeding_max_body_aborts_immediately(
    tmp_path: Path,
) -> None:
    settings = Settings(data_dir=tmp_path / "data", api_key="k", driver="mock")
    app = create_app(settings)
    max_body = (MAX_REFERENCE_IMAGES + 1) * _MAX_FETCH_BYTES
    chunk_size = 10 * 1024 * 1024
    max_chunks_expected = (max_body // chunk_size) + 2

    chunks_sent = 0
    header = (
        b"--BOUND\r\n"
        b'Content-Disposition: form-data; name="prompt"\r\n\r\nhi\r\n'
        b"--BOUND\r\n"
        b'Content-Disposition: form-data; name="image"; filename="big.png"\r\n'
        b"Content-Type: image/png\r\n\r\n"
    )
    payload_chunk = b"A" * chunk_size

    async def receive():
        nonlocal chunks_sent
        chunks_sent += 1
        if chunks_sent == 1:
            return {"type": "http.request", "body": header, "more_body": True}
        if chunks_sent > max_chunks_expected + 5:
            raise AssertionError("route buffered past max_body limit without aborting")
        return {"type": "http.request", "body": payload_chunk, "more_body": True}

    responses: list[dict] = []

    async def send(msg):
        responses.append(msg)

    scope = {
        "type": "http",
        "asgi": {"version": "3.0"},
        "http_version": "1.1",
        "method": "POST",
        "scheme": "http",
        "path": "/v1/images/edits",
        "raw_path": b"/v1/images/edits",
        "query_string": b"",
        "headers": [
            (b"authorization", b"Bearer k"),
            (b"transfer-encoding", b"chunked"),
            (b"content-type", b"multipart/form-data; boundary=BOUND"),
        ],
        "client": ("127.0.0.1", 12345),
        "server": ("test", 80),
    }

    async with app.router.lifespan_context(app):
        await app(scope, receive, send)

    start_msg = next(m for m in responses if m["type"] == "http.response.start")
    body_bytes = b"".join(m.get("body", b"") for m in responses if m["type"] == "http.response.body")
    assert start_msg["status"] == 400
    parsed = json.loads(body_bytes.decode())
    assert parsed["error"]["code"] == "invalid_request"
    assert "too large" in parsed["error"]["message"]
    assert chunks_sent <= max_chunks_expected


# ── #25.6: proxy_relay truncated upstream head -> 502 & dual CL+TE -> 502 ───


async def test_proxy_relay_truncated_upstream_head_and_dual_cl_te_return_502() -> None:
    mode = "truncated"

    async def fake_upstream(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            await reader.readuntil(b"\r\n\r\n")
            if mode == "truncated":
                writer.write(b"HTTP/1.1 200 OK\r\nX-Half: abc")
                await writer.drain()
            elif mode == "expect_truncated":
                writer.write(b"HTTP/1.1 100 Continue\r\nX-Half: abc")
                await writer.drain()
            elif mode == "dual_cl_te":
                writer.write(
                    b"HTTP/1.1 200 OK\r\n"
                    b"Content-Length: 5\r\n"
                    b"Transfer-Encoding: chunked\r\n\r\n"
                    b"5\r\nhello\r\n0\r\n\r\n"
                )
                await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    up_srv = await asyncio.start_server(fake_upstream, "127.0.0.1", 0)
    up_port = up_srv.sockets[0].getsockname()[1]
    relay = LocalProxyRelay(f"http://u:p@127.0.0.1:{up_port}")
    await relay.start()
    try:
        # 1. Truncated response headers -> 502
        mode = "truncated"
        r1, w1 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        w1.write(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
        await w1.drain()
        resp1 = await asyncio.wait_for(r1.read(), timeout=2.0)
        assert resp1.startswith(b"HTTP/1.1 502 Bad Gateway")
        w1.close()
        await w1.wait_closed()

        # 2. Truncated interim Expect: 100-continue response -> 502
        mode = "expect_truncated"
        r2, w2 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        w2.write(
            b"POST http://example.com/ HTTP/1.1\r\n"
            b"Host: example.com\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n"
        )
        await w2.drain()
        resp2 = await asyncio.wait_for(r2.read(), timeout=2.0)
        assert resp2.startswith(b"HTTP/1.1 502 Bad Gateway")
        w2.close()
        await w2.wait_closed()

        # 3. Upstream response carrying both Content-Length and Transfer-Encoding -> 502
        mode = "dual_cl_te"
        r3, w3 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        w3.write(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
        await w3.drain()
        resp3 = await asyncio.wait_for(r3.read(), timeout=2.0)
        assert resp3.startswith(b"HTTP/1.1 502 Bad Gateway")
        w3.close()
        await w3.wait_closed()
    finally:
        await relay.stop()
        up_srv.close()
        await up_srv.wait_closed()


# ── #25.7: POST /admin/accounts & import mark partial cookies INVALID ────────


async def test_create_and_import_partial_cookies_marks_invalid_and_excluded_from_pool(
    tmp_path: Path,
) -> None:
    settings = Settings(data_dir=tmp_path / "data", api_key="k", admin_key="k", driver="mock")
    app = create_app(settings)
    app.state.services.pool.acquire_timeout = 0.2
    headers = {"Authorization": "Bearer k"}

    async with app.router.lifespan_context(app):
        async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
            r_create = await client.post(
                "/admin/accounts",
                headers=headers,
                json={"label": "Partial1", "cookies": {"hatch_sess": "only_sess"}},
            )
            assert r_create.status_code == 200
            acc1 = r_create.json()["account"]
            assert acc1["status"] == "invalid"
            assert "hatch_native_auth_device" in (acc1["last_error"] or "")
            assert r_create.json()["missing_cookies"] == ["hatch_native_auth_device"]

            r_import = await client.post(
                "/admin/accounts/import",
                headers=headers,
                json={
                    "accounts": [
                        {"label": "Partial2", "cookies": {"hatch_native_auth_device": "only_dev"}}
                    ]
                },
            )
            assert r_import.status_code == 200
            item0 = r_import.json()["data"][0]
            assert item0["ok"] is True
            assert item0["account"]["status"] == "invalid"
            assert "hatch_sess" in (item0["account"]["last_error"] or "")

        assert app.state.services.pool.get(acc1["id"]).status == AccountStatus.INVALID
        with pytest.raises(NoAccountAvailable):
            await app.state.services.pool.acquire()
