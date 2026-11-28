"""Text-only provider calls for research; provider credentials stay transient."""

from __future__ import annotations

import asyncio
import json
from typing import Any
from urllib.parse import urlsplit, urlunsplit

import httpx

_MAX_GATEWAY_RESPONSE_BYTES = 2 * 1024 * 1024


def _completion_url(base_url: str) -> str:
    if not isinstance(base_url, str) or not base_url.strip():
        raise ValueError("gateway base_url is required")
    parts = urlsplit(base_url.strip())
    if parts.scheme not in {"http", "https"} or not parts.hostname or parts.username or parts.password:
        raise ValueError("gateway base_url must be an HTTP(S) URL without embedded credentials")
    if parts.query or parts.fragment:
        raise ValueError("gateway base_url cannot include query or fragment")
    path = parts.path.rstrip("/")
    endpoint = path if path.endswith("/chat/completions") else path + "/chat/completions"
    return urlunsplit((parts.scheme, parts.netloc, endpoint, "", ""))


async def run_gateway_text(config: dict[str, Any], prompt: str, timeout: float | None = None) -> dict[str, Any]:
    """Send one non-streaming chat completion and return its raw receipt.

    The caller owns transient configuration. This function never logs or returns
    the key, and it leaves response text untouched for the stage reader.
    """
    if not isinstance(config, dict):
        raise ValueError("provider config must be an object")
    if not isinstance(prompt, str) or not prompt.strip():
        raise ValueError("prompt must be a non-empty string")
    model = config.get("model")
    api_key = config.get("api_key")
    if not isinstance(model, str) or not model.strip():
        raise ValueError("gateway model is required")
    if not isinstance(api_key, str) or not api_key.strip():
        raise ValueError("gateway api_key is required")
    endpoint = _completion_url(config.get("base_url"))
    try:
        timeout_s = float(timeout if timeout is not None else config.get("timeout_s", 900))
    except (TypeError, ValueError) as exc:
        raise ValueError("gateway timeout must be numeric") from exc
    if not 5 <= timeout_s <= 3600:
        raise ValueError("gateway timeout must be between 5 and 3600 seconds")

    body = {"model": model, "messages": [{"role": "user", "content": prompt}], "stream": False}
    raw = ""
    status: int | None = None
    try:
        async with httpx.AsyncClient(timeout=httpx.Timeout(timeout_s), follow_redirects=False,
                                     trust_env=False) as client:
            async def request_and_read() -> tuple[int, bytes, bool]:
                nonlocal status
                async with client.stream(
                    "POST", endpoint,
                    headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"},
                    json=body,
                ) as response:
                    status = response.status_code
                    parts: list[bytes] = []
                    received = 0
                    async for chunk in response.aiter_bytes():
                        remaining = _MAX_GATEWAY_RESPONSE_BYTES + 1 - received
                        if remaining <= 0:
                            return status, b"".join(parts), False
                        parts.append(chunk[:remaining])
                        received += min(len(chunk), remaining)
                        if received > _MAX_GATEWAY_RESPONSE_BYTES:
                            return status, b"".join(parts), False
                    return status, b"".join(parts), True

            status, response_bytes, body_complete = await asyncio.wait_for(
                request_and_read(), timeout=timeout_s)
            raw = response_bytes.decode("utf-8", errors="replace")
            if not body_complete:
                return {"http_status": status, "raw_http": raw, "content": None, "complete": False,
                        "error": {"code": "gateway_response_too_large",
                                  "message": "Gateway response exceeded 2 MiB"}}
    except TimeoutError:
        return {"http_status": status, "raw_http": raw, "content": None, "complete": False,
                "error": {"code": "gateway_timeout", "message": "Gateway request exceeded overall timeout"}}
    except httpx.HTTPError as exc:
        return {"http_status": status, "raw_http": raw, "content": None, "complete": False,
                "error": {"code": "gateway_transport_error", "message": type(exc).__name__}}

    if status is None or not 200 <= status < 300:
        return {"http_status": status, "raw_http": raw, "content": None, "complete": False,
                "error": {"code": "gateway_http_error", "message": f"Gateway returned HTTP {status}"}}
    try:
        parsed = json.loads(raw)
        content = parsed["choices"][0]["message"]["content"]
    except (json.JSONDecodeError, KeyError, IndexError, TypeError):
        return {"http_status": status, "raw_http": raw, "content": None, "complete": False,
                "error": {"code": "gateway_invalid_response", "message": "Gateway response has no chat content"}}
    if not isinstance(content, str) or not content:
        return {"http_status": status, "raw_http": raw, "content": None, "complete": False,
                "error": {"code": "gateway_invalid_response", "message": "Gateway chat content is empty or invalid"}}
    return {"http_status": status, "raw_http": raw, "content": content, "complete": True, "error": None}


class GatewayTextProvider:
    async def run(self, config: dict[str, Any], prompt: str,
                  timeout: float | None = None) -> dict[str, Any]:
        return await run_gateway_text(config, prompt, timeout)


class MuseResearchProvider:
    """Thin provider facade over the idempotent Muse research transport."""

    def __init__(self, transport) -> None:
        self.transport = transport

    async def submit(self, operation_id: str, stage: str, input_data: dict[str, Any]):
        return await self.transport.submit_muse(operation_id, stage, input_data)

    async def get(self, operation_id: str):
        return await self.transport.get_muse(operation_id)

    async def stop(self, operation_id: str):
        return await self.transport.stop_muse(operation_id)
