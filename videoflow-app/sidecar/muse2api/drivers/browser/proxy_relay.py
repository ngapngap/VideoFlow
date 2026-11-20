"""Lightweight local HTTP proxy relay for upstream proxies requiring Basic Auth.

Chromium/Edge does not accept ``user:pass@`` credentials in ``--proxy-server``.
When an account (or global setting) configures an authenticated HTTP/HTTPS proxy
such as ``http://user:pass@host:port``, this relay listens on ``127.0.0.1:<port>``
and forwards both ``CONNECT`` tunnels (HTTPS/WSS) and plain HTTP requests to the
upstream proxy with ``Proxy-Authorization: Basic ...`` injected.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import logging
from urllib.parse import unquote, urlsplit

log = logging.getLogger(__name__)

_HEADER_LIMIT = 65536
_COPY_CHUNK = 65536


def mask_proxy(proxy: str | None) -> str:
    """Return proxy URL with password redacted for safe logging."""
    if not proxy:
        return "direct"
    raw = proxy.strip()
    if not raw:
        return "direct"
    try:
        parsed = urlsplit(raw)
        if parsed.username is None and parsed.password is None:
            return raw
        host = parsed.hostname or ""
        if ":" in host and not host.startswith("["):
            host = f"[{host}]"
        port_part = f":{parsed.port}" if parsed.port else ""
        user_part = f"{parsed.username}:***" if parsed.username is not None else "***"
        scheme = f"{parsed.scheme}://" if parsed.scheme else ""
        return f"{scheme}{user_part}@{host}{port_part}{parsed.path}"
    except Exception:  # noqa: BLE001
        return "***"


def needs_proxy_relay(proxy: str | None) -> bool:
    """Return True when ``proxy`` is an HTTP/HTTPS proxy URL carrying credentials."""
    if not proxy:
        return False
    raw = proxy.strip()
    if not raw:
        return False
    try:
        parsed = urlsplit(raw)
    except ValueError:
        return False
    scheme = (parsed.scheme or "").lower()
    if scheme not in ("http", "https"):
        return False
    return bool(parsed.hostname) and (parsed.username is not None or parsed.password is not None)


async def _read_http_head(reader: asyncio.StreamReader) -> bytes:
    """Read until ``\\r\\n\\r\\n`` (HTTP header terminator), bounded to ``_HEADER_LIMIT``."""
    buf = bytearray()
    while True:
        chunk = await reader.readuntil(b"\r\n")
        buf.extend(chunk)
        if len(buf) > _HEADER_LIMIT:
            raise ValueError("HTTP header exceeds limit")
        if buf.endswith(b"\r\n\r\n") or chunk == b"\r\n":
            return bytes(buf)


async def _pipe(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    try:
        while True:
            data = await reader.read(_COPY_CHUNK)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except (asyncio.CancelledError, ConnectionError, OSError):
        pass
    finally:
        with contextlib.suppress(Exception):
            if writer.can_write_eof():
                writer.write_eof()


class LocalProxyRelay:
    """Local unauthenticated HTTP proxy that injects Basic Auth for an upstream proxy."""

    def __init__(self, upstream_url: str, *, connect_timeout: float = 15.0) -> None:
        parsed = urlsplit(upstream_url.strip())
        if not parsed.hostname:
            raise ValueError(f"Invalid upstream proxy URL: {mask_proxy(upstream_url)}")
        self.upstream_url = upstream_url.strip()
        self.upstream_scheme = (parsed.scheme or "http").lower()
        self.upstream_host = parsed.hostname
        self.upstream_port = parsed.port or (443 if self.upstream_scheme == "https" else 80)
        self.connect_timeout = connect_timeout

        user = unquote(parsed.username or "")
        password = unquote(parsed.password or "")
        token = base64.b64encode(f"{user}:{password}".encode()).decode("ascii")
        self._auth_header = f"Proxy-Authorization: Basic {token}\r\n".encode("ascii")

        self._server: asyncio.AbstractServer | None = None
        self._tasks: set[asyncio.Task[None]] = set()
        self.local_port: int | None = None

    @property
    def local_url(self) -> str:
        if self.local_port is None:
            raise RuntimeError("LocalProxyRelay has not been started")
        return f"http://127.0.0.1:{self.local_port}"

    async def start(self) -> str:
        if self._server is not None and self.local_port is not None:
            return self.local_url
        self._server = await asyncio.start_server(
            self._accept,
            host="127.0.0.1",
            port=0,
        )
        sock = self._server.sockets[0]
        self.local_port = int(sock.getsockname()[1])
        log.info(
            "started local proxy relay on 127.0.0.1:%d -> %s",
            self.local_port,
            mask_proxy(self.upstream_url),
        )
        return self.local_url

    async def stop(self) -> None:
        server = self._server
        self._server = None
        if server is not None:
            server.close()
            with contextlib.suppress(Exception):
                await server.wait_closed()
        tasks = list(self._tasks)
        self._tasks.clear()
        for t in tasks:
            t.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
        self.local_port = None

    def _accept(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        task = asyncio.create_task(self._handle_client(reader, writer))
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)

    async def _open_upstream(self) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        ssl_arg = True if self.upstream_scheme == "https" else None
        return await asyncio.wait_for(
            asyncio.open_connection(self.upstream_host, self.upstream_port, ssl=ssl_arg),
            timeout=self.connect_timeout,
        )

    def _inject_auth(self, head: bytes) -> bytes:
        """Replace or append Proxy-Authorization header in raw HTTP request headers."""
        lines = head.split(b"\r\n")
        if not lines:
            return head
        request_line = lines[0]
        filtered = [
            line
            for line in lines[1:]
            if line and not line.lower().startswith(b"proxy-authorization:")
        ]
        rebuilt = bytearray()
        rebuilt.extend(request_line)
        rebuilt.extend(b"\r\n")
        for line in filtered:
            rebuilt.extend(line)
            rebuilt.extend(b"\r\n")
        rebuilt.extend(self._auth_header)
        rebuilt.extend(b"\r\n")
        return bytes(rebuilt)

    async def _handle_client(
        self,
        client_reader: asyncio.StreamReader,
        client_writer: asyncio.StreamWriter,
    ) -> None:
        upstream_writer: asyncio.StreamWriter | None = None
        try:
            head = await asyncio.wait_for(_read_http_head(client_reader), timeout=30.0)
            if not head:
                return
            upstream_reader, upstream_writer = await self._open_upstream()
            first_line = head.split(b"\r\n", 1)[0]
            method = first_line.split(b" ", 1)[0].upper()

            upstream_writer.write(self._inject_auth(head))
            await upstream_writer.drain()

            if method == b"CONNECT":
                resp_head = await asyncio.wait_for(
                    _read_http_head(upstream_reader), timeout=self.connect_timeout
                )
                client_writer.write(resp_head)
                await client_writer.drain()
                status_line = resp_head.split(b"\r\n", 1)[0]
                parts = status_line.split(b" ", 2)
                if len(parts) < 2 or parts[1] != b"200":
                    return

            await asyncio.gather(
                _pipe(client_reader, upstream_writer),
                _pipe(upstream_reader, client_writer),
                return_exceptions=True,
            )
        except (asyncio.CancelledError, asyncio.IncompleteReadError, ConnectionError, OSError):
            pass
        except Exception as exc:  # noqa: BLE001
            log.debug("proxy relay error: %s", exc)
            with contextlib.suppress(Exception):
                client_writer.write(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                await client_writer.drain()
        finally:
            if upstream_writer is not None:
                with contextlib.suppress(Exception):
                    upstream_writer.close()
                    await upstream_writer.wait_closed()
            with contextlib.suppress(Exception):
                client_writer.close()
                await client_writer.wait_closed()
