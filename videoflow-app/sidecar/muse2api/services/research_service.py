"""Application service for persisted VideoFlow research workflows."""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import re
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from ..core.media import load_image_ref
from ..errors import InvalidRequest, NotFound
from .research_store import ResearchStore, _json

_STAGES = {"SOURCE", "CARD", "GENERATE", "CHECK", "REPAIR"}
_MIME_EXT = {"image/jpeg": ".jpg", "image/png": ".png"}


class ResearchService:
    def __init__(self, root: Path, services: Any):
        self.store = ResearchStore(root)
        self.services = services
        self._jobs: dict[str, asyncio.Task] = {}
        self._stops: dict[str, asyncio.Event] = {}
        self._lock = asyncio.Lock()
        with self.store.db() as db:
            for table in ("batches", "sessions"):
                db.execute(
                    f"UPDATE {table} SET status='needs_review' WHERE status IN ('queued','watching','checking','submitting','repairing','searching','parsing')"
                )
            db.execute("UPDATE ideas SET status='needs_content' WHERE status='preparing'")

    async def shutdown(self):
        for event in self._stops.values():
            event.set()
        for task in self._jobs.values():
            task.cancel()
        await asyncio.gather(*self._jobs.values(), return_exceptions=True)

    @staticmethod
    def _required(data: dict[str, Any], *keys: str) -> None:
        missing = [k for k in keys if data.get(k) in (None, "")]
        if missing:
            raise InvalidRequest(f"missing required field(s): {', '.join(missing)}")

    @staticmethod
    def _payload_hash(value: Any) -> str:
        return hashlib.sha256(_json(value).encode("utf-8")).hexdigest()

    def _idempotency_lookup(self, c, request_id: str, action: str, payload_hash: str) -> str | None:
        row = c.execute(
            "SELECT action,payload_hash,entity_id FROM idempotency WHERE request_id=?",
            (request_id,),
        ).fetchone()
        if not row:
            return None
        if row["action"] != action or row["payload_hash"] != payload_hash:
            raise InvalidRequest("request_id payload conflict")
        return row["entity_id"]

    def _idempotency_save(
        self, c, request_id: str, action: str, payload_hash: str, entity_id: str
    ) -> None:
        c.execute(
            "INSERT INTO idempotency(request_id,action,payload_hash,entity_id,created_at) VALUES(?,?,?,?,?)",
            (request_id, action, payload_hash, entity_id, time.time()),
        )

    @staticmethod
    def _row(row) -> dict[str, Any] | None:
        return dict(row) if row else None

    def _source_dict(self, row) -> dict[str, Any]:
        d = dict(row)
        d["source_id"] = d["reel_id"]
        d["thumb_url"] = d.get("thumb")
        d["thumb_asset_hash"] = None
        d["thumb_asset_mime"] = None
        d["missing_fields"] = json.loads(d.pop("missing_json") or "[]")
        d["selected"] = bool(d["selected"])
        d["accept_missing_reason"] = d.get("accepted_missing_reason")
        return d

    @staticmethod
    def _exact_metric(value: str | None) -> int | None:
        if value is None:
            return None
        text = str(value).strip()
        return int(text) if re.fullmatch(r"[0-9]+", text) else None

    @staticmethod
    def _canonical_reel_url(value: str | None) -> tuple[str, str] | None:
        if not value or len(value) > 4096:
            return None
        try:
            p = urlsplit(value.strip())
            if p.scheme.lower() not in ("http", "https") or not p.hostname:
                return None
            host = p.hostname.lower()
            if host not in (
                "facebook.com",
                "www.facebook.com",
                "m.facebook.com",
                "web.facebook.com",
            ):
                return None
            match = re.fullmatch(r"/reel/([0-9]+)/?", p.path, re.IGNORECASE)
            if not match:
                return None
            reel_id = match.group(1)
            canonical = f"https://www.facebook.com/reel/{reel_id}/"
            return reel_id, canonical
        except ValueError:
            return None

    def _classify_source(
        self, row: dict[str, Any], filters: dict[str, Any]
    ) -> tuple[str, list[str], str | None, str | None]:
        missing: list[str] = []
        rejected: list[str] = []
        likes = self._exact_metric(row.get("likes"))
        views = self._exact_metric(row.get("views"))
        like_threshold = filters.get("likes_above", filters.get("min_likes", 500))
        view_threshold = filters.get("views_above", filters.get("min_views", 30000))
        try:
            like_threshold = int(like_threshold) if like_threshold not in (None, "") else None
            view_threshold = int(view_threshold) if view_threshold not in (None, "") else None
        except (TypeError, ValueError):
            raise InvalidRequest("like/view thresholds must be integers") from None
        if like_threshold is not None:
            if likes is None:
                missing.append("likes")
            elif likes <= like_threshold:
                rejected.append("likes_not_above_threshold")
        if view_threshold is not None:
            if views is None:
                missing.append("views")
            elif views <= view_threshold:
                rejected.append("views_not_above_threshold")
        market = filters.get("country", filters.get("target_market", filters.get("market")))
        if market not in (None, "", "all", "ALL"):
            evidence = str(row.get("market_evidence") or "").strip()
            country = str(row.get("country") or "").strip().upper()
            if not evidence or not re.fullmatch(r"[A-Z]{2}", country):
                missing.append("market")
            elif country != str(market).upper():
                rejected.append("market_mismatch")
        if filters.get("require_buy_link", filters.get("require_caption_buy_link", False)):
            caption = row.get("caption")
            full = row.get("coverage") == "full" and isinstance(caption, str)
            buy_url, cta = str(row.get("buy_url") or "").strip(), str(row.get("cta") or "").strip()
            if (
                isinstance(caption, str)
                and buy_url
                and urlsplit(buy_url).scheme in ("http", "https")
                and cta
                and buy_url in caption
                and cta.casefold() in caption.casefold()
            ):
                pass
            elif not full:
                missing.extend(x for x in ("buy_url", "cta") if x not in missing)
            elif not buy_url or urlsplit(buy_url).scheme not in ("https", "http") or not cta:
                rejected.append("buy_link_or_cta_absent")
            elif buy_url not in caption or cta.casefold() not in caption.casefold():
                rejected.append("buy_link_or_cta_not_in_caption")
        if rejected:
            return (
                "rejected",
                sorted(set(missing + rejected)),
                None,
                "; ".join(sorted(set(rejected))),
            )
        if missing:
            return "missing", sorted(set(missing)), None, None
        return "qualified", [], None, None

    def _commit_source_records(
        self, session_id: str, records: list[dict[str, Any]], total: int | None
    ) -> None:
        now = time.time()
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            s = c.execute(
                "SELECT filters_json,status,seq FROM sessions WHERE session_id=?", (session_id,)
            ).fetchone()
            if not s:
                raise NotFound("research session not found")
            filters = json.loads(s["filters_json"])
            grouped = {}
            for item in records:
                canonical = self._canonical_reel_url(item.get("url"))
                if canonical:
                    grouped.setdefault(canonical[0], []).append(item)
            for observations in grouped.values():
                item = {
                    k: v
                    for observation in observations
                    for k, v in observation.items()
                    if v is not None
                }
                for metric in ("likes", "views"):
                    values = {r.get(metric) for r in observations if r.get(metric) is not None}
                    if len(values) > 1:
                        item[metric] = None
                result = self._canonical_reel_url(item.get("url"))
                if not result:
                    continue
                reel_id, url = result
                coverage = (
                    item.get("coverage")
                    if item.get("coverage") in ("full", "excerpt", "unknown")
                    else "unknown"
                )
                row = {**item, "url": url, "coverage": coverage}
                status, reasons, _, _ = self._classify_source(row, filters)
                c.execute(
                    """INSERT INTO sources(session_id,reel_id,url,thumb,caption,coverage,likes,views,country,
                    market_evidence,buy_url,cta,idea,filter_status,missing_json,created_at)
                    VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                    ON CONFLICT(session_id,reel_id) DO UPDATE SET thumb=COALESCE(excluded.thumb,sources.thumb),
                    caption=COALESCE(excluded.caption,sources.caption),coverage=excluded.coverage,
                    likes=COALESCE(excluded.likes,sources.likes),views=COALESCE(excluded.views,sources.views),
                    country=COALESCE(excluded.country,sources.country),market_evidence=COALESCE(excluded.market_evidence,sources.market_evidence),
                    buy_url=COALESCE(excluded.buy_url,sources.buy_url),cta=COALESCE(excluded.cta,sources.cta),idea=COALESCE(excluded.idea,sources.idea),
                    filter_status=excluded.filter_status,missing_json=excluded.missing_json""",
                    (
                        session_id,
                        reel_id,
                        url,
                        row.get("thumb"),
                        row.get("caption"),
                        coverage,
                        row.get("likes"),
                        row.get("views"),
                        row.get("country"),
                        row.get("market_evidence"),
                        row.get("buy_url"),
                        row.get("cta"),
                        row.get("idea"),
                        status,
                        _json(reasons),
                        now,
                    ),
                )
            seq = s["seq"] + 1
            c.execute(
                "UPDATE sessions SET status='completed',reported_total=?,seq=?,updated_at=?,error_json=NULL WHERE session_id=?",
                (total, seq, now, session_id),
            )
            self.store.event(
                c, "session", session_id, seq, {"status": "completed", "records": len(records)}
            )
            c.execute("COMMIT")

    def _idea_dict(self, row, conn) -> dict[str, Any]:
        d = dict(row)
        rev = conn.execute(
            "SELECT * FROM idea_revisions WHERE idea_id=? AND revision=?",
            (d["idea_id"], d["current_revision"]),
        ).fetchone()
        d["source_id"] = d["reel_id"]
        d["revision"] = d.pop("current_revision")
        if rev:
            r = dict(rev)
            d.update(
                {
                    "asset_hash": r["asset_hash"],
                    "asset_mime": r["asset_mime"],
                    "title": r["title"],
                    "content": r["content"],
                    "visual_brief": json.loads(r["visual_brief_json"] or "null"),
                    "source_image_hash": r["source_image_hash"],
                }
            )
        else:
            d.update(asset_hash=None, asset_mime=None, title=None, content=None, visual_brief=None)
        d["error"] = (json.loads(d.pop("error_json") or "{}") or {}).get("message")
        return d

    def _variant_dict(self, row) -> dict[str, Any]:
        d = dict(row)
        d["version"] = d["edit_version"]
        d["check_result"] = json.loads(d.pop("check_result_json") or "null")
        d["qa_soft_notes"] = json.loads(d.pop("qa_soft_notes_json") or "null")
        return d

    def _batch_dict(self, row, conn) -> dict[str, Any]:
        d = dict(row)
        d["constraints"] = json.loads(d.pop("constraints_json") or "{}")
        # Provider metadata is deliberately filtered: credentials are never persisted.
        d["provider"] = json.loads(d.pop("provider_json") or "{}")
        d["error"] = (json.loads(d.pop("error_json") or "{}") or {}).get("message")
        checks = json.loads(d.pop("raw_check") or "[]")
        d["check_summary"] = (
            f"{sum(item['verdict'] == 'PASS' for item in checks)}/{len(checks)} cặp đạt CHECK"
            if checks
            else None
        )
        d["prompt_source"], d["prompt_model"] = d["provider"]["source"], d["provider"].get("model")
        pinned = conn.execute(
            "SELECT asset_hash,asset_mime,title,content FROM idea_revisions WHERE idea_id=? AND revision=?",
            (d["idea_id"], d["revision"]),
        ).fetchone()
        d["card"] = dict(pinned) if pinned else None
        d.pop("raw_generate", None)
        d["variants"] = [
            self._variant_dict(v)
            for v in conn.execute(
                "SELECT * FROM variants WHERE batch_id=? ORDER BY ordinal", (d["batch_id"],)
            ).fetchall()
        ]
        return d

    def _session_snapshot(self, session_id: str) -> dict[str, Any]:
        with self.store.db() as c:
            s = c.execute("SELECT * FROM sessions WHERE session_id=?", (session_id,)).fetchone()
            if not s:
                raise NotFound("research session not found")
            counts = {
                r["filter_status"]: r["n"]
                for r in c.execute(
                    "SELECT filter_status,COUNT(*) n FROM sources WHERE session_id=? GROUP BY filter_status",
                    (session_id,),
                ).fetchall()
            }
            src = c.execute(
                "SELECT reel_id FROM sources WHERE session_id=? AND selected=1 ORDER BY reel_id",
                (session_id,),
            ).fetchall()
            ideas = [
                self._idea_dict(r, c)
                for r in c.execute(
                    "SELECT * FROM ideas WHERE session_id=? ORDER BY created_at,idea_id",
                    (session_id,),
                ).fetchall()
            ]
            selection = c.execute(
                "SELECT * FROM selections WHERE session_id=?", (session_id,)
            ).fetchone()
            batches = [
                self._batch_dict(r, c)
                for r in c.execute(
                    "SELECT * FROM batches WHERE session_id=? ORDER BY created_at DESC",
                    (session_id,),
                ).fetchall()
            ]
            return {
                "session_id": session_id,
                "seq": s["seq"],
                "status": s["status"],
                "topic": s["topic"],
                "filters": json.loads(s["filters_json"]),
                "reported_total": s["reported_total"],
                "counts": counts,
                "source_selection_ids": [r["reel_id"] for r in src],
                "selection_version": s["source_selection_version"],
                "selection_version_current": selection["version"] if selection else 0,
                "selection": (
                    {
                        "idea_id": selection["idea_id"],
                        "revision": selection["revision"],
                        "version": selection["version"],
                    }
                    if selection and selection["idea_id"]
                    else None
                ),
                "ideas": ideas,
                "batches": batches,
                "error": json.loads(s["error_json"] or "null"),
                "created_at": s["created_at"],
                "updated_at": s["updated_at"],
            }

    def search_start(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "request_id", "topic")
        topic = str(data["topic"]).strip()
        if not topic or len(topic) > 1000:
            raise InvalidRequest("topic must contain 1-1000 characters")
        filters = data.get("filters") or {}
        if not isinstance(filters, dict):
            raise InvalidRequest("filters must be an object")
        now = time.time()
        sid = self.store.new_id("research")
        request_hash = self._payload_hash({"topic": topic, "filters": filters})
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            existing_id = self._idempotency_lookup(
                c, data["request_id"], "search_start", request_hash
            )
            if existing_id:
                c.execute("ROLLBACK")
                return self._session_snapshot(existing_id)
            c.execute(
                "INSERT INTO sessions(session_id,request_id,request_hash,topic,filters_json,status,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?)",
                (sid, data["request_id"], request_hash, topic, _json(filters), "queued", now, now),
            )
            self._idempotency_save(c, data["request_id"], "search_start", request_hash, sid)
            c.execute("INSERT INTO selections(session_id,updated_at) VALUES(?,?)", (sid, now))
            self.store.event(c, "session", sid, 0, {"status": "queued"})
            c.execute("COMMIT")
        self._start_job(sid, "SOURCE", {"topic": topic, "filters": filters}, None)
        return self._session_snapshot(sid)

    def search_list(self, cursor: str | None = None, limit: int = 20) -> dict[str, Any]:
        limit = max(1, min(int(limit), 100))
        with self.store.db() as c:
            if cursor:
                row = c.execute(
                    "SELECT created_at,session_id FROM sessions WHERE session_id=?", (cursor,)
                ).fetchone()
                if not row:
                    raise InvalidRequest("invalid history cursor")
                rows = c.execute(
                    "SELECT * FROM sessions WHERE (created_at,session_id)<(?,?) ORDER BY created_at DESC,session_id DESC LIMIT ?",
                    (row["created_at"], row["session_id"], limit + 1),
                ).fetchall()
            else:
                rows = c.execute(
                    "SELECT * FROM sessions ORDER BY created_at DESC,session_id DESC LIMIT ?",
                    (limit + 1,),
                ).fetchall()
            page, more = rows[:limit], len(rows) > limit
            items = []
            for row in page:
                counts = {
                    x["filter_status"]: x["n"]
                    for x in c.execute(
                        "SELECT filter_status,COUNT(*) n FROM sources WHERE session_id=? GROUP BY filter_status",
                        (row["session_id"],),
                    ).fetchall()
                }
                items.append(
                    {
                        "session_id": row["session_id"],
                        "topic": row["topic"],
                        "status": row["status"],
                        "created_at": row["created_at"],
                        "counts": counts,
                    }
                )
            return {
                "items": items,
                "next_cursor": page[-1]["session_id"] if more and page else None,
            }

    def search_sources(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "session_id")
        limit = max(1, min(int(data.get("limit", 20)), 100))
        bucket = data.get("bucket", "all")
        valid = {"all", "qualified", "missing"}
        if bucket not in valid:
            raise InvalidRequest("bucket must be all, eligible, or missing")
        with self.store.db() as c:
            if not c.execute(
                "SELECT 1 FROM sessions WHERE session_id=?", (data["session_id"],)
            ).fetchone():
                raise NotFound("research session not found")
            total = c.execute(
                "SELECT COUNT(*) FROM sources WHERE session_id=?"
                + (" AND filter_status=?" if bucket != "all" else ""),
                (data["session_id"], bucket) if bucket != "all" else (data["session_id"],),
            ).fetchone()[0]
            where = "session_id=?" + (" AND filter_status=?" if bucket != "all" else "")
            params = (data["session_id"], bucket) if bucket != "all" else (data["session_id"],)
            rows = [
                self._source_dict(r)
                for r in c.execute(f"SELECT * FROM sources WHERE {where}", params).fetchall()
            ]
            rows.sort(
                key=lambda r: (
                    self._exact_metric(r.get("views")) is None,
                    -(self._exact_metric(r.get("views")) or 0),
                    self._exact_metric(r.get("likes")) is None,
                    -(self._exact_metric(r.get("likes")) or 0),
                    r["reel_id"],
                )
            )
            index = 0
            token = data.get("cursor")
            if token:
                try:
                    decoded = json.loads(
                        base64.urlsafe_b64decode(token + "=" * (-len(token) % 4)).decode("utf-8")
                    )
                except Exception:
                    raise InvalidRequest("invalid source cursor") from None
                if (
                    decoded.get("session_id") != data["session_id"]
                    or decoded.get("bucket") != bucket
                ):
                    raise InvalidRequest("source cursor does not match session/filter")
                found = next(
                    (i for i, r in enumerate(rows) if r["reel_id"] == decoded.get("last_reel_id")),
                    None,
                )
                if found is None:
                    raise InvalidRequest("source cursor is stale")
                index = found + 1
            page = rows[index : index + limit]
            more = index + limit < len(rows)
            next_cursor = None
            if more and page:
                next_cursor = (
                    base64.urlsafe_b64encode(
                        _json(
                            {
                                "session_id": data["session_id"],
                                "bucket": bucket,
                                "last_reel_id": page[-1]["reel_id"],
                            }
                        ).encode()
                    )
                    .decode()
                    .rstrip("=")
                )
            return {"items": page, "total": total, "next_cursor": next_cursor}

    def set_sources(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "session_id")
        selections = data.get("source_ids")
        if not isinstance(selections, list):
            raise InvalidRequest("source_ids must be a list")
        sid = data["session_id"]
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            sess = c.execute(
                "SELECT source_selection_version,seq FROM sessions WHERE session_id=?", (sid,)
            ).fetchone()
            if not sess:
                raise NotFound("research session not found")
            expected = data.get("expected_selection_version")
            if expected is not None and int(expected) != sess["source_selection_version"]:
                c.execute("ROLLBACK")
                raise InvalidRequest("stale selection version")
            chosen = {}
            for item in selections:
                if not isinstance(item, dict) or not item.get("source_id"):
                    raise InvalidRequest("each source selection needs source_id")
                src = c.execute(
                    "SELECT filter_status FROM sources WHERE session_id=? AND reel_id=?",
                    (sid, item["source_id"]),
                ).fetchone()
                if not src:
                    raise InvalidRequest("source does not belong to session")
                reason = str(item.get("reason") or "").strip()
                accept = bool(item.get("accept_missing"))
                if src["filter_status"] == "missing" and (not accept or not reason):
                    raise InvalidRequest(
                        "missing-data sources require explicit acceptance and reason"
                    )
                if src["filter_status"] == "qualified":
                    chosen[item["source_id"]] = None
                elif src["filter_status"] == "missing" and accept:
                    chosen[item["source_id"]] = reason
            c.execute(
                "UPDATE sources SET selected=0,accepted_missing_reason=NULL WHERE session_id=?",
                (sid,),
            )
            for rid, reason in chosen.items():
                c.execute(
                    "UPDATE sources SET selected=1,accepted_missing_reason=? WHERE session_id=? AND reel_id=?",
                    (reason, sid, rid),
                )
            version, seq = sess["source_selection_version"] + 1, sess["seq"] + 1
            c.execute(
                "UPDATE sessions SET source_selection_version=?,seq=?,updated_at=? WHERE session_id=?",
                (version, seq, time.time(), sid),
            )
            self.store.event(c, "session", sid, seq, {"source_selection_version": version})
            c.execute("COMMIT")
        return {"source_selection_ids": list(chosen), "selection_version": version}

    def _start_job(
        self, parent_id: str, stage: str, input_data: dict[str, Any], provider: dict | None
    ):
        job_key = f"{parent_id}:{stage}"
        old = self._jobs.get(job_key)
        if old and not old.done():
            return
        stop = self._stops[job_key] = asyncio.Event()
        task = asyncio.create_task(self._run_job(parent_id, stage, input_data, provider, stop))
        self._jobs[job_key] = task

    async def _run_job(
        self,
        parent_id: str,
        stage: str,
        input_data: dict[str, Any],
        provider: dict | None,
        stop: asyncio.Event,
    ):
        # Provider execution is delegated to the transport service; this method
        # persists its operation and terminal result before publishing a snapshot.
        try:
            await self._execute_stage(parent_id, stage, input_data, provider, stop)
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # keep task boundary and expose a filtered error
            self._mark_failed(parent_id, stage, str(exc))

    def _mark_failed(self, parent_id: str, stage: str, message: str) -> None:
        if self._stopped(parent_id, stage):
            return
        if stage == "CARD":
            return
        table, key = ("sessions", "session_id") if stage == "SOURCE" else ("batches", "batch_id")
        status = "needs_review" if "review" in message.lower() else "failed"
        with self.store.db() as db:
            db.execute(
                f"UPDATE {table} SET status=?,error_json=?,seq=seq+1,updated_at=? WHERE {key}=? AND status!='stopped'",
                (
                    status,
                    _json({"code": "research_failed", "message": message[:500]}),
                    time.time(),
                    parent_id,
                ),
            )

    from .research_batch import run_batch as _run_batch

    async def _execute_stage(
        self,
        parent_id: str,
        stage: str,
        input_data: dict[str, Any],
        provider: dict | None,
        stop: asyncio.Event,
    ):
        if stage == "SOURCE":
            await self._run_source(parent_id, input_data, stop)
        elif stage == "CARD":
            await self._run_cards(parent_id, input_data, stop)
        elif stage == "GENERATE":
            await self._run_batch(parent_id, "GENERATE", provider or {}, stop)
        elif stage == "CHECK":
            await self._run_batch(parent_id, "CHECK", provider or {}, stop)
        elif stage == "REPAIR":
            await self._run_batch(parent_id, "REPAIR", provider or {}, stop)
        else:
            raise InvalidRequest("unsupported research stage")

    def _record_operation(
        self,
        op_id: str,
        parent_id: str,
        stage: str,
        attempt: int,
        payload_hash: str,
        status: str,
        *,
        snapshot: dict | None = None,
        source_path: str | None = None,
        normalized: Any = None,
        error: dict | None = None,
        stop_requested: bool = False,
    ) -> None:
        now = time.time()
        snap = snapshot or {}
        binding = snap.get("binding") or (snap.get("dom_turn") or {}).get("binding")
        with self.store.db() as c:
            c.execute(
                """INSERT INTO operations(operation_id,parent_id,stage,attempt,payload_hash,status,binding_json,
                raw_http,raw_ui_textcontent,source_path,normalized_json,error_json,stop_requested_at,created_at,updated_at)
                VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(operation_id) DO UPDATE SET status=excluded.status,
                binding_json=COALESCE(excluded.binding_json,operations.binding_json),
                raw_http=COALESCE(excluded.raw_http,operations.raw_http),
                raw_ui_textcontent=COALESCE(excluded.raw_ui_textcontent,operations.raw_ui_textcontent),
                source_path=COALESCE(excluded.source_path,operations.source_path),
                normalized_json=COALESCE(excluded.normalized_json,operations.normalized_json),
                error_json=excluded.error_json,stop_requested_at=COALESCE(excluded.stop_requested_at,operations.stop_requested_at),
                updated_at=excluded.updated_at""",
                (
                    op_id,
                    parent_id,
                    stage,
                    attempt,
                    payload_hash,
                    status,
                    _json(binding) if binding else None,
                    snap.get("http_body") or snap.get("raw_http"),
                    snap.get("raw_ui_code_textcontent"),
                    source_path,
                    _json(normalized) if normalized is not None else None,
                    _json(error) if error else None,
                    now if stop_requested else None,
                    now,
                    now,
                ),
            )

    async def _call_stage(
        self,
        parent_id: str,
        stage: str,
        prompt_data: dict[str, Any],
        provider_input: dict | None,
        *,
        count: int | None = None,
        ordinals: list[int] | None = None,
        attempt: int = 1,
    ) -> tuple[Any, dict[str, Any]]:
        import uuid

        from ..research.protocol import select_reply
        from .research_prompts import build_prompt

        identity = self._payload_hash([parent_id, stage, attempt, prompt_data])
        op_id = str(uuid.uuid5(uuid.NAMESPACE_URL, identity))
        with self.store.db() as db:
            prior = db.execute("SELECT * FROM operations WHERE operation_id=?", (op_id,)).fetchone()
        if prior:
            if prior["status"] == "completed" and prior["normalized_json"]:
                value = json.loads(prior["normalized_json"])
                raw = (
                    prior["raw_ui_textcontent"]
                    if prior["source_path"] == "dom_textContent"
                    else prior["raw_http"]
                )
                return value, {"raw": raw, "operation_id": op_id}
            raise InvalidRequest("needs_review: previous submission has no confirmed result")
        built = build_prompt(stage, op_id, prompt_data)
        prompt, images = built["prompt"], built.get("images") or []
        provider_input = provider_input or {"source": "internal", "timeout_s": 900}
        safe_provider = self._provider_config(provider_input)
        hashed = self._payload_hash(
            {
                "stage": stage,
                "prompt": prompt,
                "images": [
                    hashlib.sha256(base64.b64decode(i["data_b64"])).hexdigest() for i in images
                ],
                "provider": safe_provider,
            }
        )
        self._record_operation(op_id, parent_id, stage, attempt, hashed, "queued")
        if safe_provider["source"] == "gateway":
            from .providers import run_gateway_text

            result = await run_gateway_text(provider_input, prompt, safe_provider["timeout_s"])
            http_body = result.get("content") if result.get("complete") else result.get("raw_http")
            snapshot = {
                "http_status": result.get("http_status"),
                "http_body": http_body,
                "http_complete": bool(result.get("complete")),
                "status": "completed" if result.get("complete") else "failed",
                "error": result.get("error"),
            }
            snapshot["http_body"] = result.get("raw_http")
            self._record_operation(
                op_id, parent_id, stage, attempt, hashed, "received", snapshot=snapshot
            )
            if not result.get("complete") or not 200 <= (result.get("http_status") or 0) < 300:
                raise InvalidRequest(
                    f"Gateway HTTP {result.get('http_status')}: {result.get('error')}"
                )
            try:
                from ..research.protocol import extract_http_payload

                payload = extract_http_payload(http_body or "", stage)
                value = self._read_stage(payload, stage, op_id, count, ordinals)
                selected = {"via": "http", "raw": http_body, "payload": payload, "value": value}
            except Exception as exc:
                error = {"code": "protocol_error", "message": str(exc)[:300]}
                self._record_operation(
                    op_id,
                    parent_id,
                    stage,
                    attempt,
                    hashed,
                    "protocol_error",
                    snapshot=snapshot,
                    error=error,
                )
                raise InvalidRequest(
                    f"{stage} response failed protocol validation: {error['message']}"
                ) from None
        else:
            transport = self.services.research_transport
            if transport is None:
                raise InvalidRequest("Muse research transport is unavailable")
            input_data = {
                "prompt": prompt,
                "images": images,
                "timeout_s": safe_provider["timeout_s"],
                "conversation_hint": prompt_data.get("conversation_hint"),
            }
            await transport.submit_muse(op_id, stage, input_data)
            deadline = time.monotonic() + safe_provider["timeout_s"]
            snapshot = None
            while time.monotonic() < deadline:
                if self._stopped(parent_id, stage):
                    await transport.stop_muse(op_id)
                    snap = await transport.get_muse(op_id)
                    self._record_operation(
                        op_id,
                        parent_id,
                        stage,
                        attempt,
                        hashed,
                        "stopped",
                        snapshot=snap,
                        stop_requested=True,
                    )
                    raise InvalidRequest("research operation stopped")
                snapshot = await transport.get_muse(op_id)
                if snapshot and snapshot.get("status") in {
                    "completed",
                    "failed",
                    "stopped",
                    "protocol_error",
                }:
                    break
                await asyncio.sleep(0.5)
            if not snapshot or snapshot.get("status") not in {
                "completed",
                "failed",
                "stopped",
                "protocol_error",
            }:
                await transport.stop_muse(op_id)
                self._record_operation(
                    op_id,
                    parent_id,
                    stage,
                    attempt,
                    hashed,
                    "needs_review",
                    snapshot=snapshot,
                    error={"code": "timeout", "message": "operation deadline elapsed"},
                )
                raise InvalidRequest("Muse response deadline elapsed; result requires review")
            if snapshot.get("status") != "completed":
                error = snapshot.get("error") or {
                    "code": snapshot.get("status"),
                    "message": "Muse operation did not complete",
                }
                self._record_operation(
                    op_id,
                    parent_id,
                    stage,
                    attempt,
                    hashed,
                    snapshot["status"],
                    snapshot=snapshot,
                    error=error,
                )
                raise InvalidRequest(f"Muse operation {snapshot.get('status')}")
            self._record_operation(
                op_id, parent_id, stage, attempt, hashed, "received", snapshot=snapshot
            )
            http = snapshot.get("http_body")
            if http and snapshot.get("http_complete"):
                try:
                    http = json.loads(http)["choices"][0]["message"]["content"]
                except (ValueError, KeyError, TypeError, IndexError):
                    pass
            binding = snapshot.get("binding") or {}
            try:
                selected = select_reply(
                    http,
                    snapshot.get("dom_turn"),
                    binding,
                    lambda text: self._read_stage(text, stage, op_id, count, ordinals),
                    stage=stage,
                    http_status=snapshot.get("http_status"),
                    http_complete=bool(snapshot.get("http_complete")),
                )
            except Exception as exc:
                error = {"code": "protocol_error", "message": str(exc)[:300]}
                self._record_operation(
                    op_id,
                    parent_id,
                    stage,
                    attempt,
                    hashed,
                    "protocol_error",
                    snapshot=snapshot,
                    error=error,
                )
                raise InvalidRequest(
                    f"{stage} response failed protocol/context validation: {error['message']}"
                ) from None
        selected["stage"] = stage
        self._record_operation(
            op_id,
            parent_id,
            stage,
            attempt,
            hashed,
            "completed",
            snapshot=snapshot,
            source_path=selected["via"],
            normalized=selected["value"],
        )
        return selected["value"], {**snapshot, **selected, "operation_id": op_id}

    def _stopped(self, parent_id: str, stage: str) -> bool:
        for key in (f"{parent_id}:{stage}", f"{parent_id}:GENERATE"):
            event = self._stops.get(key)
            if event and event.is_set():
                return True
        return False

    @staticmethod
    def _read_stage(text, stage, op_id, count, ordinals):
        from ..research.protocol import extract_context, read_source_page, validate_stage

        if stage == "SOURCE":
            return read_source_page(extract_context(text, op_id, stage))
        return validate_stage(text, stage, op_id, count=count, ordinals=ordinals)

    async def _run_source(self, session_id, data, stop):
        self.session_status(session_id, "searching")
        page, _ = await self._call_stage(session_id, "SOURCE", data, None)
        if not stop.is_set():
            self._commit_source_records(session_id, page["items"], page["reported_total"])

    async def _run_cards(self, session_id: str, data: dict[str, Any], stop: asyncio.Event) -> None:
        idea_ids = data["idea_ids"]
        for iid in idea_ids:
            if stop.is_set() or self._stopped(session_id, "CARD"):
                break
            try:
                await self._run_card(session_id, iid)
            except Exception as exc:
                with self.store.db() as c:
                    c.execute(
                        "UPDATE ideas SET status=?,error_json=? WHERE idea_id=?",
                        (
                            "needs_content",
                            _json({"code": "card_failed", "message": str(exc)[:300]}),
                            iid,
                        ),
                    )

    async def _run_card(self, session_id: str, idea_id: str) -> None:
        with self.store.db() as c:
            idea = c.execute(
                "SELECT * FROM ideas WHERE session_id=? AND idea_id=?", (session_id, idea_id)
            ).fetchone()
            if not idea:
                raise NotFound("idea not found")
            source = self._source_dict(
                c.execute(
                    "SELECT * FROM sources WHERE session_id=? AND reel_id=?",
                    (session_id, idea["reel_id"]),
                ).fetchone()
            )
            if idea["status"] == "ready":
                return
            rev = c.execute(
                "SELECT asset_hash,asset_mime FROM idea_revisions WHERE idea_id=? AND revision=?",
                (idea_id, idea["current_revision"]),
            ).fetchone()
        thumb = source.get("thumb")
        if rev and rev["asset_hash"]:
            saved = self._image_path(rev["asset_hash"], rev["asset_mime"]).read_bytes()
            thumb = (
                "data:" + rev["asset_mime"] + ";base64," + base64.b64encode(saved).decode("ascii")
            )
        if not thumb:
            with self.store.db() as c:
                c.execute(
                    "UPDATE ideas SET status='needs_image',error_json=? WHERE idea_id=?",
                    (
                        _json({"code": "missing_image", "message": "source has no image URL"}),
                        idea_id,
                    ),
                )
            return
        image_data, image_mime = await self._decode_asset(thumb)
        digest, image_mime, _ = await asyncio.to_thread(
            self.store.save_asset, image_data, image_mime
        )
        image = {"data_b64": base64.b64encode(image_data).decode("ascii"), "mime": image_mime}
        card, receipt = await self._call_stage(
            session_id, "CARD", {"source": source, "images": [image]}, None
        )
        # Publish only after the image is durable and the complete CARD validated.
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            current = c.execute(
                "SELECT current_revision FROM ideas WHERE idea_id=?", (idea_id,)
            ).fetchone()
            if current["current_revision"] != idea["current_revision"]:
                return
            next_revision = int(current["current_revision"]) + 1
            visual = {
                "formula": card["formula"],
                "visual": card["visual"],
                "print_side": card["print_side"],
                "print_text": card["print_text"],
                "source_image_hash": digest,
            }
            c.execute(
                "INSERT INTO idea_revisions(idea_id,revision,asset_hash,asset_mime,title,content,visual_brief_json,source_image_hash,source_meta_json,created_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
                (
                    idea_id,
                    next_revision,
                    digest,
                    image_mime,
                    card["title"],
                    card["content"] + "\n\n" + card["formula"],
                    _json(visual),
                    digest,
                    _json(source),
                    time.time(),
                ),
            )
            c.execute(
                "UPDATE ideas SET current_revision=?,status='ready',error_json=NULL WHERE idea_id=?",
                (next_revision, idea_id),
            )
            c.execute("COMMIT")

    def session_status(
        self,
        session_id: str,
        status: str,
        *,
        error: dict | None = None,
        reported_total: int | None = None,
    ) -> None:
        with self.store.db() as c:
            row = c.execute("SELECT seq FROM sessions WHERE session_id=?", (session_id,)).fetchone()
            if not row:
                return
            seq = row["seq"] + 1
            c.execute(
                "UPDATE sessions SET status=?,seq=?,reported_total=COALESCE(?,reported_total),error_json=?,updated_at=? WHERE session_id=?",
                (
                    status,
                    seq,
                    reported_total,
                    _json(error) if error else None,
                    time.time(),
                    session_id,
                ),
            )
            self.store.event(c, "session", session_id, seq, {"status": status, "error": error})

    def search_get(self, session_id: str) -> dict[str, Any]:
        return self._session_snapshot(session_id)

    def _image_path(self, digest: str, mime: str) -> Path:
        ext = _MIME_EXT.get(mime.lower())
        if not ext or not re.fullmatch(r"[a-f0-9]{64}", digest):
            raise InvalidRequest("invalid image asset reference")
        return self.store.assets_dir / f"{digest}{ext}"

    async def _decode_asset(self, ref: str, mime_hint: str | None = None) -> tuple[bytes, str]:
        try:
            data, mime = await load_image_ref(ref, timeout=25)
        except Exception as exc:
            raise InvalidRequest(f"image could not be safely loaded: {str(exc)[:160]}") from None
        if len(data) > 20 * 1024 * 1024:
            raise InvalidRequest("image exceeds 20 MiB")
        try:
            from io import BytesIO

            from PIL import Image

            with Image.open(BytesIO(data)) as im:
                im.verify()
                fmt = im.format
            if fmt == "WEBP":
                with Image.open(BytesIO(data)) as im:
                    converted = BytesIO()
                    im.convert("RGBA" if "A" in im.getbands() else "RGB").save(
                        converted, format="PNG", optimize=False
                    )
                    data = converted.getvalue()
                fmt = "PNG"
        except Exception:
            raise InvalidRequest("image bytes are not a decodable image") from None
        normalized = {"JPEG": "image/jpeg", "PNG": "image/png", "WEBP": "image/webp"}.get(fmt)
        if not normalized or (
            mime_hint and mime_hint.lower() not in (normalized, "application/octet-stream")
        ):
            raise InvalidRequest("image MIME does not match decoded image")
        return data, normalized

    def _source_by_id(self, c, sid: str, rid: str):
        row = c.execute(
            "SELECT * FROM sources WHERE session_id=? AND reel_id=?", (sid, rid)
        ).fetchone()
        if not row:
            raise NotFound("source not found")
        return row

    def prepare_ideas(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "session_id", "request_id")
        sid = data["session_id"]
        ids = data.get("source_ids")
        if not isinstance(ids, list) or not ids or len(set(map(str, ids))) != len(ids):
            raise InvalidRequest("source_ids must be a non-empty unique list")
        created = []
        request_hash = self._payload_hash({"session_id": sid, "source_ids": ids})
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            if not c.execute("SELECT 1 FROM sessions WHERE session_id=?", (sid,)).fetchone():
                raise NotFound("research session not found")
            old_request = self._idempotency_lookup(
                c, data["request_id"], "prepare_ideas", request_hash
            )
            for rid in ids:
                src = self._source_by_id(c, sid, str(rid))
                if not src["selected"]:
                    raise InvalidRequest("all requested sources must be selected")
                row = c.execute(
                    "SELECT idea_id FROM ideas WHERE session_id=? AND reel_id=?", (sid, rid)
                ).fetchone()
                if row:
                    created.append(row["idea_id"])
                    continue
                iid = self.store.new_id("idea")
                c.execute(
                    "INSERT INTO ideas(idea_id,session_id,reel_id,status,created_at) VALUES(?,?,?,?,?)",
                    (iid, sid, rid, "preparing", time.time()),
                )
                c.execute(
                    "INSERT INTO idea_revisions(idea_id,revision,source_meta_json,created_at) VALUES(?,?,?,?)",
                    (iid, 0, _json(dict(src)), time.time()),
                )
                created.append(iid)
            if old_request is None:
                self._idempotency_save(c, data["request_id"], "prepare_ideas", request_hash, sid)
            c.execute(
                "UPDATE sessions SET seq=seq+1,updated_at=? WHERE session_id=?", (time.time(), sid)
            )
            c.execute("COMMIT")
        op = self.store.new_id("op")
        if not old_request:
            self._start_job(
                sid,
                "CARD",
                {"idea_ids": created, "request_id": data["request_id"], "operation_id": op},
                None,
            )
        with self.store.db() as c:
            ideas = [
                self._idea_dict(
                    c.execute("SELECT * FROM ideas WHERE idea_id=?", (iid,)).fetchone(), c
                )
                for iid in created
            ]
        return {"operation_id": op, "status": "preparing", "ideas": ideas}

    def idea_get(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "session_id", "idea_id")
        with self.store.db() as c:
            row = c.execute(
                "SELECT * FROM ideas WHERE session_id=? AND idea_id=?",
                (data["session_id"], data["idea_id"]),
            ).fetchone()
            if not row:
                raise NotFound("idea not found")
            current = self._idea_dict(row, c)
            current["revisions"] = []
            for rev in c.execute(
                "SELECT * FROM idea_revisions WHERE idea_id=? ORDER BY revision DESC",
                (data["idea_id"],),
            ).fetchall():
                item = dict(rev)
                item["visual_brief"] = json.loads(item.pop("visual_brief_json") or "null")
                item["source_meta"] = json.loads(item.pop("source_meta_json") or "null")
                current["revisions"].append(item)
            return {"idea": current}

    async def idea_update(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "session_id", "idea_id", "expected_revision", "request_id")
        sid, iid = data["session_id"], data["idea_id"]
        update_hash = self._payload_hash(
            {k: data[k] for k in sorted(data) if k not in ("request_id", "_provider")}
        )
        with self.store.db() as c:
            prior = self._idempotency_lookup(c, data["request_id"], "idea_update", update_hash)
            if prior:
                return self.idea_get({"session_id": sid, "idea_id": iid})
            row = c.execute(
                "SELECT * FROM ideas WHERE session_id=? AND idea_id=?", (sid, iid)
            ).fetchone()
            if not row:
                raise NotFound("idea not found")
            current_rev = int(row["current_revision"])
            if current_rev != int(data["expected_revision"]):
                raise InvalidRequest("stale idea revision")
            prev = c.execute(
                "SELECT * FROM idea_revisions WHERE idea_id=? AND revision=?", (iid, current_rev)
            ).fetchone()
        image_data = None
        image_mime = prev["asset_mime"] if prev else None
        if data.get("image_b64"):
            image_data, image_mime = await self._decode_asset(
                data["image_b64"], data.get("image_mime")
            )
        elif data.get("image_url"):
            image_data, image_mime = await self._decode_asset(data["image_url"])
        elif data.get("image_ref"):
            image_data, image_mime = await self._decode_asset(
                data["image_ref"], data.get("image_mime")
            )
        if image_data is not None:
            digest, image_mime, size = await asyncio.to_thread(
                self.store.save_asset, image_data, image_mime
            )
        else:
            digest = prev["asset_hash"] if prev else None
        title = data.get("title", prev["title"] if prev else None)
        content = data.get("content", prev["content"] if prev else None)
        visual = data.get(
            "visual_brief", json.loads(prev["visual_brief_json"] or "null") if prev else None
        )
        image_changed = image_data is not None and digest != (prev["asset_hash"] if prev else None)
        next_rev = current_rev + 1
        now = time.time()
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            latest = c.execute(
                "SELECT current_revision FROM ideas WHERE session_id=? AND idea_id=?", (sid, iid)
            ).fetchone()
            if not latest or latest["current_revision"] != current_rev:
                raise InvalidRequest("stale idea revision")
            c.execute(
                "INSERT INTO idea_revisions(idea_id,revision,asset_hash,asset_mime,title,content,visual_brief_json,source_image_hash,source_meta_json,created_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
                (
                    iid,
                    next_rev,
                    digest,
                    image_mime,
                    title,
                    content,
                    _json(visual) if visual is not None else None,
                    digest,
                    prev["source_meta_json"] if prev else None,
                    now,
                ),
            )
            reviewed_changed_image = (not image_changed) or (
                "content" in data and "visual_brief" in data
            )
            status = (
                "ready"
                if digest and content and visual is not None and reviewed_changed_image
                else "needs_image"
                if not digest
                else "needs_content"
            )
            c.execute(
                "UPDATE ideas SET current_revision=?,status=? WHERE idea_id=?",
                (next_rev, status, iid),
            )
            # Selection is invalidated when its selected revision changes.
            c.execute(
                "UPDATE selections SET idea_id=NULL,revision=NULL,version=version+1,updated_at=? WHERE session_id=? AND idea_id=?",
                (now, sid, iid),
            )
            self._idempotency_save(c, data["request_id"], "idea_update", update_hash, iid)
            c.execute("COMMIT")
        if image_changed and not reviewed_changed_image:
            with self.store.db() as db:
                db.execute("UPDATE ideas SET status='preparing' WHERE idea_id=?", (iid,))
            self._start_job(
                sid, "CARD", {"idea_ids": [iid], "request_id": data["request_id"]}, None
            )
        return self.idea_get({"session_id": sid, "idea_id": iid})

    def select_idea(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "session_id", "idea_id", "revision")
        sid, iid, rev = data["session_id"], data["idea_id"], int(data["revision"])
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            idea = c.execute(
                "SELECT status,current_revision FROM ideas WHERE session_id=? AND idea_id=?",
                (sid, iid),
            ).fetchone()
            if not idea:
                raise NotFound("idea not found")
            if idea["status"] != "ready" or idea["current_revision"] != rev:
                raise InvalidRequest("idea revision is not ready/current")
            sel = c.execute("SELECT version FROM selections WHERE session_id=?", (sid,)).fetchone()
            version = sel["version"] if sel else 0
            if (
                data.get("expected_version") is not None
                and int(data["expected_version"]) != version
            ):
                raise InvalidRequest("stale idea selection version")
            version += 1
            c.execute(
                "INSERT INTO selections(session_id,idea_id,revision,version,updated_at) VALUES(?,?,?,?,?) ON CONFLICT(session_id) DO UPDATE SET idea_id=excluded.idea_id,revision=excluded.revision,version=excluded.version,updated_at=excluded.updated_at",
                (sid, iid, rev, version, time.time()),
            )
            c.execute("COMMIT")
        return {"selection": {"idea_id": iid, "revision": rev}, "version": version}

    def develop_start(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "request_id", "session_id", "idea_id", "revision", "n", "_provider")
        sid, iid, rev = data["session_id"], data["idea_id"], int(data["revision"])
        n = int(data["n"])
        if not 2 <= n <= 10:
            raise InvalidRequest("n must be between 2 and 10")
        provider = self._provider_config(data["_provider"])
        request_hash = self._payload_hash(
            {
                "session_id": sid,
                "idea_id": iid,
                "revision": rev,
                "n": n,
                "provider": provider,
                "prompt_language": data.get("prompt_language", "vi"),
                "constraints": data.get("constraints") or {},
            }
        )
        now, bid = time.time(), self.store.new_id("batch")
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            old_id = self._idempotency_lookup(c, data["request_id"], "develop_start", request_hash)
            if old_id:
                c.execute("ROLLBACK")
                return self.batch_get({"batch_id": old_id})
            idea = c.execute(
                "SELECT status,current_revision FROM ideas WHERE session_id=? AND idea_id=?",
                (sid, iid),
            ).fetchone()
            sel = c.execute(
                "SELECT idea_id,revision FROM selections WHERE session_id=?", (sid,)
            ).fetchone()
            revrow = c.execute(
                "SELECT * FROM idea_revisions WHERE idea_id=? AND revision=?", (iid, rev)
            ).fetchone()
            if (
                not idea
                or not revrow
                or idea["status"] != "ready"
                or idea["current_revision"] != rev
            ):
                raise InvalidRequest("selected idea revision is not ready/current")
            if not sel or sel["idea_id"] != iid or sel["revision"] != rev:
                raise InvalidRequest("the exact ready idea revision must be selected")
            if not revrow["asset_hash"] or not revrow["content"]:
                raise InvalidRequest("idea image and content are required")
            asset_path = self._image_path(revrow["asset_hash"], revrow["asset_mime"])
            if (
                not asset_path.is_file()
                or hashlib.sha256(asset_path.read_bytes()).hexdigest() != revrow["asset_hash"]
            ):
                raise InvalidRequest("idea image asset is missing or corrupt")
            c.execute(
                "INSERT INTO batches(batch_id,request_id,request_hash,session_id,idea_id,revision,n,status,provider_json,prompt_language,constraints_json,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    bid,
                    data["request_id"],
                    request_hash,
                    sid,
                    iid,
                    rev,
                    n,
                    "queued",
                    _json(provider),
                    data.get("prompt_language", "vi"),
                    _json(data.get("constraints") or {}),
                    now,
                    now,
                ),
            )
            self._idempotency_save(c, data["request_id"], "develop_start", request_hash, bid)
            for ordinal in range(1, n + 1):
                c.execute(
                    "INSERT INTO variants(batch_id,ordinal,status) VALUES(?,?,?)",
                    (bid, ordinal, "pending"),
                )
            c.execute("COMMIT")
        self._start_job(bid, "GENERATE", {"request_id": data["request_id"]}, data["_provider"])
        return {"batch_id": bid, "status": "queued"}

    @staticmethod
    def _provider_config(raw: Any) -> dict[str, Any]:
        if not isinstance(raw, dict) or raw.get("source") not in ("internal", "gateway"):
            raise InvalidRequest("_provider.source must be internal or gateway")
        timeout = float(raw.get("timeout_s", 900))
        if not 5 <= timeout <= 3600:
            raise InvalidRequest("provider timeout_s must be 5-3600")
        if raw["source"] == "gateway":
            for key in ("base_url", "model", "api_key"):
                if not isinstance(raw.get(key), str) or not raw[key].strip():
                    raise InvalidRequest(f"gateway provider requires {key}")
            parsed = urlsplit(raw["base_url"].strip())
            if (
                parsed.scheme.lower() not in ("https", "http")
                or not parsed.hostname
                or parsed.username
                or parsed.password
            ):
                raise InvalidRequest(
                    "gateway URL must be an HTTP(S) URL without embedded credentials"
                )
        else:
            if raw.get("api_key"):
                raise InvalidRequest("internal Muse provider does not accept api_key")
        return {
            "source": raw["source"],
            "base_url": raw.get("base_url"),
            "model": raw.get("model"),
            "timeout_s": timeout,
        }

    def batch_get(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "batch_id")
        with self.store.db() as c:
            row = c.execute(
                "SELECT * FROM batches WHERE batch_id=?", (data["batch_id"],)
            ).fetchone()
            if not row:
                raise NotFound("batch not found")
            return {"batch": self._batch_dict(row, c)}

    def batch_stop(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "batch_id")
        bid = data["batch_id"]
        now = time.time()
        job_key = f"{bid}:GENERATE"
        if job_key in self._stops:
            self._stops[job_key].set()
        with self.store.db() as c:
            row = c.execute("SELECT status,seq FROM batches WHERE batch_id=?", (bid,)).fetchone()
            if not row:
                raise NotFound("batch not found")
            if row["status"] not in ("ready", "partial", "failed", "stopped"):
                seq = row["seq"] + 1
                c.execute(
                    "UPDATE batches SET status='stopped',seq=?,updated_at=? WHERE batch_id=?",
                    (seq, now, bid),
                )
                c.execute(
                    "UPDATE operations SET stop_requested_at=?,status='stopped',updated_at=? WHERE parent_id=? AND status IN ('queued','running')",
                    (now, now, bid),
                )
            else:
                seq = row["seq"]
        return self.batch_get({"batch_id": bid})

    def search_stop(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "session_id")
        sid = data["session_id"]
        for stage in ("SOURCE", "CARD"):
            stop = self._stops.get(f"{sid}:{stage}")
            if stop:
                stop.set()
        with self.store.db() as c:
            row = c.execute("SELECT status FROM sessions WHERE session_id=?", (sid,)).fetchone()
            if not row:
                raise NotFound("research session not found")
            if row["status"] not in ("completed", "failed", "stopped"):
                c.execute(
                    "UPDATE sessions SET status='stopped',seq=seq+1,updated_at=? WHERE session_id=?",
                    (time.time(), sid),
                )
        return self.search_get(sid)

    def search_resume(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "session_id")
        sid = data["session_id"]
        with self.store.db() as c:
            row = c.execute(
                "SELECT request_id,topic,filters_json,status FROM sessions WHERE session_id=?",
                (sid,),
            ).fetchone()
            if not row:
                raise NotFound("research session not found")
            if row["status"] not in ("stopped", "failed", "needs_review"):
                return self.search_get(sid)
            c.execute(
                "UPDATE sessions SET status='searching',error_json=NULL,seq=seq+1,updated_at=? WHERE session_id=?",
                (time.time(), sid),
            )
            filters = json.loads(row["filters_json"])
        self._start_job(sid, "SOURCE", {"topic": row["topic"], "filters": filters}, None)
        return self.search_get(sid)

    def batch_resume(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "batch_id", "_provider")
        bid = data["batch_id"]
        provider = self._provider_config(data["_provider"])
        with self.store.db() as c:
            row = c.execute(
                "SELECT status,provider_json FROM batches WHERE batch_id=?", (bid,)
            ).fetchone()
            if not row:
                raise NotFound("batch not found")
            if json.loads(row["provider_json"]) != provider:
                raise InvalidRequest("provider snapshot mismatch for this batch")
            if row["status"] in ("ready", "partial", "failed"):
                return self.batch_get({"batch_id": bid})
            c.execute(
                "UPDATE batches SET status='watching',error_json=NULL,seq=seq+1,updated_at=? WHERE batch_id=?",
                (time.time(), bid),
            )
        self._start_job(bid, "GENERATE", {"resume": True}, data["_provider"])
        return self.batch_get({"batch_id": bid})

    def variant_edit(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "batch_id", "ordinal", "expected_version", "_provider")
        bid, ordinal = data["batch_id"], int(data["ordinal"])
        provider = self._provider_config(data["_provider"])
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            batch = c.execute(
                "SELECT provider_json FROM batches WHERE batch_id=?", (bid,)
            ).fetchone()
            if not batch:
                raise NotFound("batch not found")
            if json.loads(batch["provider_json"]) != provider:
                raise InvalidRequest("provider snapshot mismatch for this batch")
            request_id = data.get("request_id") or self.store.new_id("edit")
            edit_hash = self._payload_hash(
                {k: data[k] for k in sorted(data) if k not in ("request_id", "_provider")}
            )
            prior = self._idempotency_lookup(c, request_id, "variant_edit", edit_hash)
            if prior:
                c.execute("ROLLBACK")
                with self.store.db() as read:
                    old_variant = read.execute(
                        "SELECT * FROM variants WHERE batch_id=? AND ordinal=?", (bid, ordinal)
                    ).fetchone()
                    return {"variant": self._variant_dict(old_variant)}
            row = c.execute(
                "SELECT edit_version,status FROM variants WHERE batch_id=? AND ordinal=?",
                (bid, ordinal),
            ).fetchone()
            if not row:
                raise NotFound("variant not found")
            if row["edit_version"] != int(data["expected_version"]):
                raise InvalidRequest("stale variant edit version")
            sets, params = [], []
            for key in ("image_prompt", "video_prompt"):
                if key in data:
                    if not isinstance(data[key], str) or not data[key].strip():
                        raise InvalidRequest(f"{key} must be non-empty text")
                    sets.append(f"{key}=?")
                    params.append(data[key])
            if not sets:
                raise InvalidRequest("at least one prompt field is required")
            sets.extend(
                ["edit_version=edit_version+1", "status='pending'", "check_result_json=NULL"]
            )
            params.extend([bid, ordinal])
            c.execute(
                f"UPDATE variants SET {','.join(sets)} WHERE batch_id=? AND ordinal=?", params
            )
            self._idempotency_save(c, request_id, "variant_edit", edit_hash, f"{bid}:{ordinal}")
            c.execute(
                "UPDATE batches SET status='checking',seq=seq+1,updated_at=? WHERE batch_id=?",
                (time.time(), bid),
            )
            c.execute("COMMIT")
        self._start_job(
            bid,
            "CHECK",
            {"ordinals": [ordinal], "request_id": data.get("request_id")},
            data["_provider"],
        )
        with self.store.db() as c:
            return {
                "variant": self._variant_dict(
                    c.execute(
                        "SELECT * FROM variants WHERE batch_id=? AND ordinal=?", (bid, ordinal)
                    ).fetchone()
                )
            }

    def develop_repair(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "batch_id", "request_id", "_provider")
        bid = data["batch_id"]
        provider = self._provider_config(data["_provider"])
        repair_hash = self._payload_hash({"batch_id": bid, "provider": provider})
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            prior = self._idempotency_lookup(c, data["request_id"], "develop_repair", repair_hash)
            if prior:
                c.execute("ROLLBACK")
                return self.batch_get({"batch_id": bid})
            row = c.execute(
                "SELECT repair_count,status,provider_json FROM batches WHERE batch_id=?", (bid,)
            ).fetchone()
            if not row:
                raise NotFound("batch not found")
            if json.loads(row["provider_json"]) != provider:
                raise InvalidRequest("provider snapshot mismatch for this batch")
            if row["repair_count"] >= 1:
                raise InvalidRequest("automatic repair limit reached")
            if row["status"] not in ("partial", "failed", "ready"):
                raise InvalidRequest("batch is not in a repairable state")
            c.execute(
                "UPDATE batches SET repair_count=repair_count+1,status='repairing',seq=seq+1,updated_at=? WHERE batch_id=?",
                (time.time(), bid),
            )
            self._idempotency_save(c, data["request_id"], "develop_repair", repair_hash, bid)
            c.execute("COMMIT")
        self._start_job(bid, "REPAIR", {"request_id": data["request_id"]}, data["_provider"])
        return self.batch_get({"batch_id": bid})

    def transfer_prepare(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "batch_id", "ordinals", "request_id", "project")
        bid, request_id = data["batch_id"], data["request_id"]
        ordinals = [int(x) for x in data["ordinals"]]
        if not ordinals or len(set(ordinals)) != len(ordinals):
            raise InvalidRequest("ordinals must be a non-empty unique list")
        with self.store.db() as c:
            batch = c.execute("SELECT * FROM batches WHERE batch_id=?", (bid,)).fetchone()
            if not batch:
                raise NotFound("batch not found")
            existing_transfer = c.execute(
                "SELECT * FROM transfers WHERE request_id=?", (request_id,)
            ).fetchone()
            if existing_transfer:
                if (
                    existing_transfer["batch_id"] != bid
                    or existing_transfer["ordinals_json"] != _json(ordinals)
                    or existing_transfer["project"] != data["project"]
                ):
                    raise InvalidRequest("transfer request_id payload conflict")
                if existing_transfer["status"] == "committed":
                    return {
                        k: existing_transfer[k]
                        for k in ("batch_id", "request_id", "status", "added", "existing")
                    }
            if batch["status"] not in ("ready", "partial"):
                raise InvalidRequest("batch verification is incomplete")
            rows = []
            for ordinal in ordinals:
                row = c.execute(
                    "SELECT * FROM variants WHERE batch_id=? AND ordinal=?", (bid, ordinal)
                ).fetchone()
                if (
                    not row
                    or row["status"] != "pass"
                    or not row["image_prompt"]
                    or not row["video_prompt"]
                ):
                    raise InvalidRequest(f"ordinal {ordinal} is not a validated PASS variant")
                rows.append(
                    {
                        "ordinal": ordinal,
                        "image_prompt": row["image_prompt"],
                        "video_prompt": row["video_prompt"],
                    }
                )
            idea_rev = c.execute(
                "SELECT r.asset_hash,r.asset_mime FROM idea_revisions r WHERE r.idea_id=? AND r.revision=?",
                (batch["idea_id"], batch["revision"]),
            ).fetchone()
            path = self._image_path(idea_rev["asset_hash"], idea_rev["asset_mime"])
            image = path.read_bytes()
            digest = hashlib.sha256(image).hexdigest()
            if digest != idea_rev["asset_hash"]:
                raise InvalidRequest("reference image failed SHA-256 validation")
            payload_hash = hashlib.sha256(
                _json(
                    {
                        "batch_id": bid,
                        "ordinals": ordinals,
                        "project": data["project"],
                        "image": digest,
                        "rows": rows,
                    }
                ).encode()
            ).hexdigest()
            if existing_transfer and existing_transfer["payload_hash"] != payload_hash:
                raise InvalidRequest("transfer request_id payload conflict")
            now = time.time()
            c.execute(
                "INSERT INTO transfers(request_id,batch_id,ordinals_json,project,status,payload_hash,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(request_id) DO NOTHING",
                (
                    request_id,
                    bid,
                    _json(ordinals),
                    data["project"],
                    "prepared",
                    payload_hash,
                    now,
                    now,
                ),
            )
            return {
                "batch_id": bid,
                "request_id": request_id,
                "project": data["project"],
                "ordinals": ordinals,
                "filename": f"{digest}{_MIME_EXT[idea_rev['asset_mime']]}",
                "image_b64": base64.b64encode(image).decode("ascii"),
                "image_sha256": digest,
                "rows": rows,
                "payload_hash": payload_hash,
                "transfer_status": existing_transfer["status"] if existing_transfer else "prepared",
            }

    def transfer_commit(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "batch_id", "ordinals", "request_id", "project", "added", "existing")
        bid, request_id = data["batch_id"], data["request_id"]
        with self.store.db() as c:
            c.execute("BEGIN IMMEDIATE")
            row = c.execute("SELECT * FROM transfers WHERE request_id=?", (request_id,)).fetchone()
            if (
                not row
                or row["batch_id"] != bid
                or row["project"] != data["project"]
                or row["ordinals_json"] != _json([int(x) for x in data["ordinals"]])
            ):
                raise InvalidRequest("no matching prepared transfer")
            if row["status"] == "committed":
                if row["added"] != int(data["added"]) or row["existing"] != int(data["existing"]):
                    raise InvalidRequest("transfer commit replay payload conflict")
            else:
                c.execute(
                    "UPDATE transfers SET status='committed',added=?,existing=?,updated_at=? WHERE request_id=?",
                    (int(data["added"]), int(data["existing"]), time.time(), request_id),
                )
            c.execute("COMMIT")
        return {
            "batch_id": bid,
            "request_id": request_id,
            "added": int(data["added"]),
            "existing": int(data["existing"]),
            "status": "committed",
        }

    async def prompt_test(self, data: dict[str, Any]) -> dict[str, Any]:
        self._required(data, "_provider")
        provider = self._provider_config(data["_provider"])
        if provider["source"] == "internal":
            return {
                "ok": True,
                "source": "internal",
                "model": provider.get("model") or "muse-chat",
                "message": "Muse session available; no prompt data sent by this check",
            }
        from .providers import run_gateway_text

        result = await run_gateway_text(
            data["_provider"], "Reply with the single word OK.", provider["timeout_s"]
        )
        if (
            not result.get("complete")
            or int(result.get("http_status") or 0) < 200
            or int(result.get("http_status") or 0) >= 300
        ):
            return {
                "ok": False,
                "source": "gateway",
                "model": provider["model"],
                "http_status": result.get("http_status"),
                "error": result.get("error"),
            }
        return {
            "ok": True,
            "source": "gateway",
            "model": provider["model"],
            "http_status": result.get("http_status"),
            "response": str(result.get("content", ""))[:80],
        }
