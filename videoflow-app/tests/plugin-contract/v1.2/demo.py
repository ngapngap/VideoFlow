"""Loopback contract test provider. Its only side effects are in --data-dir."""
import argparse
import hashlib
import json
import os
import sqlite3
import threading
from email import policy
from email.parser import BytesParser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--data-dir", type=Path, required=True)
parser.add_argument("--config-file", type=Path, required=True)
parser.add_argument("--port", type=int, required=True)
args = parser.parse_args()
args.data_dir.mkdir(parents=True, exist_ok=True)
config = json.loads(args.config_file.read_text("utf-8-sig"))
samples = json.loads(Path(__file__).with_name("responses.json").read_text("utf-8"))
db = args.data_dir / "operations.db"
with sqlite3.connect(db) as connection:
    connection.execute("CREATE TABLE IF NOT EXISTS operations (id TEXT PRIMARY KEY, fingerprint TEXT, state TEXT, kind TEXT)")
lock = threading.Lock()

class Handler(BaseHTTPRequestHandler):
    def reply(self, status, data):
        raw = data if isinstance(data, bytes) else json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "video/mp4" if isinstance(data, bytes) else "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def fail(self, status, kind, code):
        self.reply(status, {"status":"failed","error":{"kind":kind,"code":code,"scope":"operation","message":code,"retryable":False}})

    def dispatch(self):
        if self.headers.get("Authorization") != "Bearer " + os.environ["VF_PLUGIN_TOKEN"]:
            return self.fail(401, "auth", "PLUGIN_TOKEN_INVALID")
        path = self.path
        if path == "/healthz":
            return self.reply(200, {"status":"ok","checks":{},"message":"ready"})
        if path == "/vf-plugin/v1/capabilities":
            return self.reply(200, samples["capabilities"])
        if path == "/vf-plugin/v1/quota":
            return self.reply(200, {"limits":[]})
        with lock, sqlite3.connect(db) as conn:
            if self.command == "POST" and path in ("/v1/videos", "/v1/images/edits"):
                key = self.headers["Idempotency-Key"]
                if not key:
                    return self.fail(400,"business","MISSING_KEY")
                raw = self.rfile.read(int(self.headers["Content-Length"]))
                kind = "image" if path.endswith("edits") else "video"
                if kind == "image":
                    header = self.headers["Content-Type"].encode()
                    form = BytesParser(policy=policy.default).parsebytes(b"Content-Type: " + header + b"\r\n\r\n" + raw)
                    values = {p.get_param("name",header="content-disposition"):p.get_payload(decode=True).hex() for p in form.iter_parts()}
                else:
                    values = json.loads(raw)
                digest = hashlib.sha256(json.dumps([kind,values], sort_keys=True).encode()).hexdigest()
                row = conn.execute("SELECT fingerprint,state FROM operations WHERE id=?", (key,)).fetchone()
                if row and row[1] == "cancelled":
                    return self.fail(409,"business","OPERATION_CANCELLED")
                if row and row[0] != digest:
                    return self.fail(409,"business","INPUT_CHANGED")
                if not row:
                    conn.execute("INSERT INTO operations VALUES (?,?,'succeeded',?)", (key,digest,kind))
                    conn.commit()
                    if config.get("drop_response"):
                        self.close_connection = True
                        return
                if kind == "image":
                    return self.reply(200, {"data":samples["image_lookup_200"]["data"]})
                return self.reply(202, {"task_id":key,"id":key,"status":"processing"})
            key = path.rsplit("/", 1)[-1]
            if path.startswith("/vf-plugin/v1/operations/") or path.startswith("/v1/videos/"):
                if self.command == "DELETE" and path.startswith("/vf-plugin/v1/operations/"):
                    conn.execute("INSERT OR IGNORE INTO operations VALUES (?,NULL,'cancelled',NULL)", (key,))
                    conn.commit()
                row = conn.execute("SELECT state,kind FROM operations WHERE id=?", (key,)).fetchone()
                if not row:
                    return self.fail(404,"not_found","UNKNOWN_OPERATION")
                body = {"operation_id":key,"status":row[0]}
                if row[1] == "image":
                    body.update(media_kind="image", data=samples["image_lookup_200"]["data"])
                elif row[1] == "video":
                    body.update(media_kind="video", task_id=key, media_name="sample.mp4")
                return self.reply(200,body)
            if path == "/v1/media/sample.mp4":
                return self.reply(200, Path(__file__).with_name("sample.mp4").read_bytes())
            return self.fail(404,"not_found","UNKNOWN_ROUTE")

    do_GET = do_POST = do_DELETE = dispatch

ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
