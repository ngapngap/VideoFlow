"""Durable SQLite store for VideoFlow research sessions and prompt batches."""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import tempfile
import time
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


class ResearchStore:
    def __init__(self, root: Path):
        self.root = root
        self.db_path = root / "reels.sqlite3"
        self.assets_dir = root / "assets"
        self.assets_dir.mkdir(parents=True, exist_ok=True)
        root.mkdir(parents=True, exist_ok=True)
        self._init_db()

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        conn = sqlite3.connect(self.db_path, timeout=15, isolation_level=None)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA foreign_keys=ON")
        conn.execute("PRAGMA busy_timeout=15000")
        try:
            yield conn
        finally:
            conn.close()

    def _init_db(self) -> None:
        with self.db() as c:
            c.execute("PRAGMA journal_mode=WAL")
            c.executescript("""
                CREATE TABLE IF NOT EXISTS sessions(
                    session_id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE,
                    request_hash TEXT NOT NULL DEFAULT '',
                    topic TEXT NOT NULL, filters_json TEXT NOT NULL, status TEXT NOT NULL,
                    seq INTEGER NOT NULL DEFAULT 0, source_selection_version INTEGER NOT NULL DEFAULT 0,
                    reported_total INTEGER,
                    raw_reply TEXT, error_json TEXT, created_at REAL NOT NULL, updated_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS sources(
                    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                    reel_id TEXT NOT NULL, url TEXT NOT NULL, thumb TEXT, caption TEXT,
                    coverage TEXT NOT NULL DEFAULT 'unknown', likes TEXT, views TEXT,
                    country TEXT, market_evidence TEXT, buy_url TEXT, cta TEXT, idea TEXT,
                    filter_status TEXT NOT NULL DEFAULT 'missing', missing_json TEXT NOT NULL DEFAULT '[]',
                    accepted_missing_reason TEXT, selected INTEGER NOT NULL DEFAULT 0,
                    created_at REAL NOT NULL, PRIMARY KEY(session_id,reel_id)
                );
                CREATE TABLE IF NOT EXISTS selections(
                    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                    idea_id TEXT, revision INTEGER, version INTEGER NOT NULL DEFAULT 0, updated_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS ideas(
                    idea_id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                    reel_id TEXT NOT NULL, ordinal INTEGER NOT NULL DEFAULT 0,
                    current_revision INTEGER NOT NULL DEFAULT 0,
                    status TEXT NOT NULL, error_json TEXT, created_at REAL NOT NULL,
                    UNIQUE(session_id,reel_id)
                );
                CREATE TABLE IF NOT EXISTS idea_revisions(
                    idea_id TEXT NOT NULL REFERENCES ideas(idea_id) ON DELETE CASCADE,
                    revision INTEGER NOT NULL, asset_hash TEXT, asset_mime TEXT, title TEXT,
                    content TEXT, visual_brief_json TEXT, source_image_hash TEXT, source_meta_json TEXT,
                    created_at REAL NOT NULL, PRIMARY KEY(idea_id,revision)
                );
                CREATE TABLE IF NOT EXISTS batches(
                    batch_id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE,
                    request_hash TEXT NOT NULL DEFAULT '',
                    session_id TEXT NOT NULL REFERENCES sessions(session_id), idea_id TEXT NOT NULL,
                    revision INTEGER NOT NULL, n INTEGER NOT NULL, status TEXT NOT NULL,
                    provider_json TEXT NOT NULL, prompt_language TEXT NOT NULL,
                    constraints_json TEXT NOT NULL, seq INTEGER NOT NULL DEFAULT 0,
                    raw_generate TEXT, raw_check TEXT, error_json TEXT,
                    repair_count INTEGER NOT NULL DEFAULT 0,
                    created_at REAL NOT NULL, updated_at REAL NOT NULL,
                    FOREIGN KEY(idea_id,revision) REFERENCES idea_revisions(idea_id,revision)
                );
                CREATE TABLE IF NOT EXISTS variants(
                    batch_id TEXT NOT NULL REFERENCES batches(batch_id) ON DELETE CASCADE,
                    ordinal INTEGER NOT NULL, title TEXT, conflict TEXT, reveal TEXT, payoff TEXT,
                    image_prompt TEXT, video_prompt TEXT, status TEXT NOT NULL DEFAULT 'pending',
                    check_result_json TEXT, attempt INTEGER NOT NULL DEFAULT 0,
                    edit_version INTEGER NOT NULL DEFAULT 0, qa_soft_notes_json TEXT,
                    PRIMARY KEY(batch_id,ordinal)
                );
                CREATE TABLE IF NOT EXISTS operations(
                    operation_id TEXT PRIMARY KEY, parent_id TEXT NOT NULL, stage TEXT NOT NULL,
                    attempt INTEGER NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL,
                    binding_json TEXT, raw_http TEXT, raw_ui_textcontent TEXT,
                    source_path TEXT, normalized_json TEXT, error_json TEXT,
                    stop_requested_at REAL, created_at REAL NOT NULL, updated_at REAL NOT NULL,
                    UNIQUE(parent_id,stage,attempt)
                );
                CREATE TABLE IF NOT EXISTS transfers(
                    request_id TEXT PRIMARY KEY, batch_id TEXT NOT NULL,
                    ordinals_json TEXT NOT NULL, project TEXT NOT NULL,
                    status TEXT NOT NULL, added INTEGER, existing INTEGER,
                    payload_hash TEXT, created_at REAL NOT NULL, updated_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS idempotency(
                    request_id TEXT PRIMARY KEY, action TEXT NOT NULL,
                    payload_hash TEXT NOT NULL, entity_id TEXT NOT NULL,
                    created_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS events(
                    event_id INTEGER PRIMARY KEY AUTOINCREMENT, entity_type TEXT NOT NULL,
                    entity_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT NOT NULL,
                    created_at REAL NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_sources_session_status ON sources(session_id,filter_status,reel_id);
                CREATE INDEX IF NOT EXISTS idx_sessions_created ON sessions(created_at DESC,session_id DESC);
                CREATE INDEX IF NOT EXISTS idx_batches_session ON batches(session_id,created_at DESC);
            """)
            for table, column, declaration in (
                ("sessions", "request_hash", "TEXT NOT NULL DEFAULT ''"),
                ("sessions", "source_selection_version", "INTEGER NOT NULL DEFAULT 0"),
                ("batches", "request_hash", "TEXT NOT NULL DEFAULT ''"),
                ("batches", "repair_count", "INTEGER NOT NULL DEFAULT 0"),
                ("ideas", "ordinal", "INTEGER NOT NULL DEFAULT 0"),
            ):
                columns = {
                    row["name"] for row in c.execute(f"PRAGMA table_info({table})").fetchall()
                }
                if column not in columns:
                    c.execute(f"ALTER TABLE {table} ADD COLUMN {column} {declaration}")
            c.execute("PRAGMA user_version=2")

    @staticmethod
    def new_id(prefix: str) -> str:
        return prefix + "_" + uuid.uuid4().hex

    def save_asset(self, data: bytes, mime: str) -> tuple[str, str, int]:
        digest = hashlib.sha256(data).hexdigest()
        ext = {"image/jpeg": ".jpg", "image/png": ".png"}.get(mime.lower())
        if not ext:
            raise ValueError("asset must be a JPEG, PNG, or WebP image")
        dest = self.assets_dir / f"{digest}{ext}"
        if not dest.exists():
            fd, tmp = tempfile.mkstemp(prefix=".asset-", suffix=".tmp", dir=self.assets_dir)
            try:
                with os.fdopen(fd, "wb") as f:
                    f.write(data)
                    f.flush()
                    os.fsync(f.fileno())
                if hashlib.sha256(Path(tmp).read_bytes()).hexdigest() != digest:
                    raise ValueError("asset digest mismatch")
                os.replace(tmp, dest)
            finally:
                if os.path.exists(tmp):
                    os.unlink(tmp)
        return digest, mime.lower(), len(data)

    def event(
        self,
        conn: sqlite3.Connection,
        entity_type: str,
        entity_id: str,
        seq: int,
        event: dict[str, Any],
    ) -> None:
        conn.execute(
            "INSERT INTO events(entity_type,entity_id,seq,event_json,created_at) VALUES(?,?,?,?,?)",
            (entity_type, entity_id, seq, _json(event), time.time()),
        )
