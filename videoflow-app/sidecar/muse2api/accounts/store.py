"""Account persistence. ``AccountStore`` is the interface; ``JsonAccountStore`` is the default.

A SQLite/Redis store can be added by implementing the same protocol.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import tempfile
from pathlib import Path
from typing import Protocol

from .model import Account

log = logging.getLogger(__name__)


class AccountStore(Protocol):
    async def load(self) -> list[Account]: ...
    async def save(self, accounts: list[Account]) -> None: ...


class JsonAccountStore:
    def __init__(self, path: Path) -> None:
        self.path = path
        self._lock = asyncio.Lock()

    async def load(self) -> list[Account]:
        if not self.path.is_file():
            return []
        raw = await asyncio.to_thread(self.path.read_text, encoding="utf-8")
        if not raw.strip():
            return []
        try:
            items = json.loads(raw)
        except json.JSONDecodeError:
            backup = self.path.with_suffix(self.path.suffix + ".corrupt")
            log.warning("accounts file %s corrupted; backing up to %s", self.path, backup)
            await asyncio.to_thread(os.replace, self.path, backup)
            return []
        return [Account.model_validate(item) for item in items]

    async def save(self, accounts: list[Account]) -> None:
        async with self._lock:
            payload = json.dumps(
                [a.model_dump(mode="json") for a in accounts], ensure_ascii=False, indent=2
            )
            await asyncio.to_thread(self._atomic_write, payload)

    def _atomic_write(self, payload: str) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=self.path.parent, prefix=".accounts.", suffix=".tmp")
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                f.write(payload)
                f.flush()
                os.fsync(f.fileno())
            os.chmod(tmp, 0o600)
            os.replace(tmp, self.path)
        except BaseException:
            if os.path.exists(tmp):
                os.unlink(tmp)
            raise
