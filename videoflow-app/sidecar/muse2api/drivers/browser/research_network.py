"""Capture only a completed same-request HTTP response whose POST body has this op marker."""

from __future__ import annotations

import base64
from urllib.parse import urlsplit

from .cdp import CDPError, CDPSession

MAX_HTTP_BODY_BYTES = 2 * 1024 * 1024


class ResearchHttpCapture:
    def __init__(self, operation_id: str, stage: str) -> None:
        self.marker = f"VFCTX BEGIN {operation_id} {stage}"
        self.matches: dict[str, dict] = {}
        self._callbacks = {
            "Network.requestWillBeSent": self._request,
            "Network.responseReceived": self._response,
            "Network.loadingFinished": self._finished,
            "Network.loadingFailed": self._failed,
        }

    def attach(self, session: CDPSession) -> None:
        for method, callback in self._callbacks.items():
            session.on(method, callback)

    def detach(self, session: CDPSession) -> None:
        for method, callback in self._callbacks.items():
            session.off(method, callback)

    def _request(self, params: dict) -> None:
        request = params.get("request") or {}
        url = request.get("url") or ""
        try:
            host = (urlsplit(url).hostname or "").lower()
        except ValueError:
            return
        if (request.get("method") != "POST" or host != "muse.ai"
                or self.marker not in str(request.get("postData") or "")):
            return
        request_id = params.get("requestId")
        if isinstance(request_id, str):
            self.matches[request_id] = {"url": url, "status": None, "finished": False,
                                        "failed": False, "error": None}

    def _response(self, params: dict) -> None:
        entry = self.matches.get(params.get("requestId"))
        if entry is not None:
            response = params.get("response") or {}
            try:
                entry["status"] = int(response.get("status"))
            except (TypeError, ValueError):
                entry["status"] = None

    def _finished(self, params: dict) -> None:
        entry = self.matches.get(params.get("requestId"))
        if entry is not None:
            entry["finished"] = True

    def _failed(self, params: dict) -> None:
        entry = self.matches.get(params.get("requestId"))
        if entry is not None:
            entry["failed"] = True
            entry["error"] = "network_loading_failed"

    async def snapshot(self, session: CDPSession) -> dict:
        if len(self.matches) > 1:
            return {"http_available": False, "http_ambiguous": True,
                    "http_status": None, "http_body": None, "http_complete": False}
        if not self.matches:
            return {"http_available": False, "http_ambiguous": False,
                    "http_status": None, "http_body": None, "http_complete": False}
        request_id, entry = next(iter(self.matches.items()))
        receipt = {"http_available": True, "http_ambiguous": False,
                   "http_status": entry["status"], "http_body": None,
                   "http_complete": False}
        if entry["failed"]:
            receipt["http_error"] = entry["error"]
            return receipt
        if not entry["finished"]:
            return receipt
        try:
            result = await session.send("Network.getResponseBody", {"requestId": request_id}, timeout=5.0)
            body = result.get("body", "")
            raw = base64.b64decode(body) if result.get("base64Encoded") else str(body).encode("utf-8")
            if len(raw) > MAX_HTTP_BODY_BYTES:
                receipt["http_body"] = raw[:MAX_HTTP_BODY_BYTES].decode("utf-8", errors="replace")
                receipt["http_error"] = "response_body_too_large"
                return receipt
            receipt["http_body"] = raw.decode("utf-8", errors="replace")
            receipt["http_complete"] = True
        except (CDPError, ValueError, TypeError):
            receipt["http_error"] = "response_body_unavailable"
        return receipt
