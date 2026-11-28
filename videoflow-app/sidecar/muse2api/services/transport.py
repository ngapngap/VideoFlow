"""Idempotent, single-attempt Muse research transport with bound browser receipts."""

from __future__ import annotations

import asyncio
import base64
import binascii
import hashlib
import json
import re
import time
from dataclasses import dataclass, field
from typing import Any

from ..accounts.pool import AccountPool
from ..config import Settings
from ..drivers.base import InputImage, MuseDriver
from ..errors import UpstreamError

_STAGES = {"SOURCE", "CARD", "GENERATE", "CHECK", "REPAIR"}
_MAX_PROMPT_BYTES = 2 * 1024 * 1024
_MAX_IMAGE_BYTES = 20 * 1024 * 1024
_MAX_IMAGES = 4


@dataclass
class _ResearchOperation:
    operation_id: str
    stage: str | None
    payload_hash: str | None
    status: str
    created_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    stop_requested: bool = False
    cancel: asyncio.Event = field(default_factory=asyncio.Event)
    result: dict[str, Any] = field(default_factory=dict)
    task: asyncio.Task | None = None

    def snapshot(self) -> dict[str, Any]:
        return {
            "operation_id": self.operation_id,
            "stage": self.stage,
            "status": self.status,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "stop_requested": self.stop_requested,
            **self.result,
        }


class MuseResearchTransport:
    """One POST per operation ID, observed on that submission's exact browser turn.

    Durable intent and lifecycle history belong to the caller's ResearchService;
    this component owns only the in-process provider task and its raw receipt.
    It never retries an operation after any side effect may have started.
    """

    def __init__(self, settings: Settings, pool: AccountPool, driver: MuseDriver) -> None:
        self.settings = settings
        self.pool = pool
        self.driver = driver
        self._operations: dict[str, _ResearchOperation] = {}
        self._lock = asyncio.Lock()

    @staticmethod
    def _input(input_data: dict[str, Any]) -> tuple[str, list[InputImage], float, str | None, str]:
        if not isinstance(input_data, dict):
            raise ValueError("input must be an object")
        prompt = input_data.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("input.prompt must be a non-empty string")
        if len(prompt.encode("utf-8")) > _MAX_PROMPT_BYTES:
            raise ValueError("prompt exceeds 2 MiB")
        try:
            timeout = float(input_data.get("timeout_s", 900))
        except (TypeError, ValueError) as exc:
            raise ValueError("timeout_s must be numeric") from exc
        if not 5 <= timeout <= 3600:
            raise ValueError("timeout_s must be between 5 and 3600")
        hint = input_data.get("conversation_hint")
        if hint is not None and (not isinstance(hint, str) or not hint.strip()):
            raise ValueError("conversation_hint must be a non-empty string or null")
        raw_images = input_data.get("images", [])
        if not isinstance(raw_images, list) or len(raw_images) > _MAX_IMAGES:
            raise ValueError("images must be a list of at most four items")
        images: list[InputImage] = []
        hashes: list[str] = []
        for i, item in enumerate(raw_images):
            if not isinstance(item, dict):
                raise ValueError(f"images[{i}] must be an object")
            mime = item.get("mime")
            encoded = item.get("data_b64")
            if not isinstance(mime, str) or mime not in {"image/jpeg", "image/png", "image/webp"}:
                raise ValueError(f"images[{i}].mime is unsupported")
            if not isinstance(encoded, str) or not encoded:
                raise ValueError(f"images[{i}].data_b64 is required")
            try:
                data = base64.b64decode(encoded, validate=True)
            except (ValueError, binascii.Error) as exc:
                raise ValueError(f"images[{i}].data_b64 is invalid") from exc
            if not data or len(data) > _MAX_IMAGE_BYTES:
                raise ValueError(f"images[{i}] is empty or exceeds 20 MiB")
            signatures = {
                "image/jpeg": lambda b: b.startswith(b"\xff\xd8\xff"),
                "image/png": lambda b: b.startswith(b"\x89PNG\r\n\x1a\n"),
                "image/webp": lambda b: len(b) >= 12 and b[:4] == b"RIFF" and b[8:12] == b"WEBP",
            }
            if not signatures[mime](data):
                raise ValueError(f"images[{i}] bytes do not match MIME")
            images.append(InputImage(data=data, mime=mime))
            hashes.append(hashlib.sha256(data).hexdigest())
        hash_payload = json.dumps(
            {"prompt": prompt, "images": hashes, "timeout_s": timeout,
             "conversation_hint": hint},
            ensure_ascii=False, sort_keys=True, separators=(",", ":"),
        ).encode("utf-8")
        return prompt, images, timeout, hint, hashlib.sha256(hash_payload).hexdigest()

    async def submit_muse(self, operation_id: str, stage: str,
                          input_data: dict[str, Any]) -> dict[str, Any]:
        if not isinstance(operation_id, str) or not operation_id.strip():
            raise ValueError("operation_id is required")
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", operation_id):
            raise ValueError("operation_id contains unsupported characters")
        if not isinstance(stage, str) or stage not in _STAGES:
            raise ValueError("unsupported research stage")
        prompt, images, timeout, hint, payload_hash = self._input(input_data)
        if f"VFCTX BEGIN {operation_id} {stage}" not in prompt or f"VFCTX END {operation_id} {stage}" not in prompt:
            raise ValueError("prompt must include the exact operation/stage envelope markers")
        async with self._lock:
            existing = self._operations.get(operation_id)
            if existing:
                if existing.stop_requested or existing.status == "stopped":
                    return existing.snapshot()
                if existing.payload_hash != payload_hash or existing.stage != stage:
                    raise ValueError("operation_id payload conflict")
                return existing.snapshot()
            operation = _ResearchOperation(
                operation_id=operation_id, stage=stage, payload_hash=payload_hash, status="running")
            self._operations[operation_id] = operation
            operation.task = asyncio.create_task(
                self._run(operation, prompt, images, timeout, hint),
                name=f"research:{operation_id}",
            )
            return operation.snapshot()

    async def _run(self, operation: _ResearchOperation, prompt: str, images: list[InputImage],
                   timeout: float, hint: str | None) -> None:
        try:
            if operation.cancel.is_set():
                operation.status = "stopped"
                return
            async with self.pool.lease(hint=f"research:{operation.operation_id}") as lease:
                method = getattr(self.driver, "research_reply", None)
                if method is None:
                    raise UpstreamError("selected driver does not support bound research capture")
                receipt = await method(
                    lease.account,
                    prompt=prompt,
                    operation_id=operation.operation_id,
                    stage=operation.stage,
                    images=images,
                    timeout=timeout,
                    conversation_hint=hint,
                    cancel=operation.cancel,
                )
            operation.result = self._safe_receipt(receipt)
            operation.status = operation.result.pop("status", "completed")
        except asyncio.CancelledError:
            operation.status = "stopped" if operation.stop_requested else "failed"
            if not operation.stop_requested:
                operation.result = {"error": {"code": "interrupted", "message": "provider task interrupted"}}
        except Exception as exc:  # noqa: BLE001
            operation.status = "stopped" if operation.stop_requested else "failed"
            if not operation.stop_requested:
                operation.result = {"error": {"code": "transport_error", "message": type(exc).__name__}}
        finally:
            operation.updated_at = time.time()

    @staticmethod
    def _safe_receipt(receipt: Any) -> dict[str, Any]:
        if not isinstance(receipt, dict):
            return {"status": "failed", "error": {"code": "invalid_receipt", "message": "provider returned invalid receipt"}}
        allowed = {
                "status", "http_status", "http_body", "raw_ui_code_textcontent", "dom_turn",
            "binding", "capture_error", "complete", "thread_id", "user_message_id",
            "assistant_message_id", "user_turn_id", "assistant_turn_id", "error",
            "http_available", "http_ambiguous", "http_complete", "http_error",
                "http_body_complete",
        }
        return {k: v for k, v in receipt.items() if k in allowed}

    async def get_muse(self, operation_id: str) -> dict[str, Any] | None:
        async with self._lock:
            operation = self._operations.get(operation_id)
            return operation.snapshot() if operation else None

    async def stop_muse(self, operation_id: str) -> dict[str, Any]:
        if not isinstance(operation_id, str) or not operation_id.strip():
            raise ValueError("operation_id is required")
        async with self._lock:
            operation = self._operations.get(operation_id)
            if operation is None:
                operation = _ResearchOperation(
                    operation_id=operation_id, stage=None, payload_hash=None, status="stopped",
                    stop_requested=True,
                )
                operation.cancel.set()
                operation.updated_at = time.time()
                self._operations[operation_id] = operation
                return operation.snapshot()
            operation.stop_requested = True
            operation.cancel.set()
            if operation.status not in {"completed", "failed", "protocol_error", "stopped"}:
                operation.status = "stop_requested"
            operation.updated_at = time.time()
            return operation.snapshot()

    async def shutdown(self) -> None:
        async with self._lock:
            tasks = []
            for operation in self._operations.values():
                if operation.task and not operation.task.done():
                    operation.stop_requested = True
                    operation.cancel.set()
                    tasks.append(operation.task)
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
