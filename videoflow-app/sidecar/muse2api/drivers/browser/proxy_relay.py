"""Lightweight local HTTP proxy relay for upstream HTTP/HTTPS/SOCKS5 proxies.

Chromium/Edge does not accept ``user:pass@`` credentials in ``--proxy-server``
and cannot natively use ``socks5h://`` remote-DNS or authenticated SOCKS5 proxies.
This relay listens on ``127.0.0.1:<port>`` and forwards both ``CONNECT`` tunnels
(HTTPS/WSS) and plain HTTP requests to the upstream proxy (injecting
``Proxy-Authorization: Basic ...`` on every HTTP/HTTPS proxy request or performing
an RFC 1928/1929 SOCKS5 handshake with remote DNS ``ATYP=0x03``).
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import ipaddress
import logging
import re
import struct
import time
from dataclasses import dataclass
from urllib.parse import unquote, urlsplit

log = logging.getLogger(__name__)

_HEADER_LIMIT = 65536
_COPY_CHUNK = 65536
_DEFAULT_IDLE_TIMEOUT = 120.0
_DEFAULT_CLOSE_TIMEOUT = 2.0
_MAX_LEADING_EMPTY_LINES = 8


class _HttpValidationError(Exception):
    """Raised on malformed or smuggling-prone HTTP framing from a client."""


class _UpstreamProxyError(Exception):
    """Raised when connecting to or authenticating with the upstream proxy fails."""


class _UpstreamTimeoutError(_UpstreamProxyError):
    """Raised when the upstream proxy times out."""


@dataclass(slots=True)
class _ParsedHeaders:
    content_length: int | None
    is_chunked: bool
    has_transfer_encoding: bool
    host: str | None
    connection_tokens: set[str]
    proxy_connection_tokens: set[str]
    expect_100: bool


def mask_proxy(proxy: str | None) -> str:
    """Return proxy URL with password redacted for safe logging."""
    if not proxy:
        return "direct"
    raw = proxy.strip()
    if not raw:
        return "direct"
    try:
        parsed = urlsplit(raw)
        port = parsed.port
    except ValueError:
        return "***"
    if "@" in parsed.path or "@" in parsed.query or "@" in parsed.fragment:
        return "***"
    if parsed.username is None and parsed.password is None:
        if "@" in raw:
            return "***"
        return raw
    host = parsed.hostname or ""
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    port_part = f":{port}" if port is not None else ""
    user_part = f"{parsed.username}:***" if parsed.username is not None else "***"
    scheme = f"{parsed.scheme}://" if parsed.scheme else ""
    path_part = "" if parsed.path == "/" else parsed.path
    return f"{scheme}{user_part}@{host}{port_part}{path_part}"


def needs_proxy_relay(proxy: str | None) -> bool:
    """Return True when ``proxy`` needs the local relay (auth proxies or socks5h)."""
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
    if not parsed.hostname:
        return False
    has_creds = parsed.username is not None or parsed.password is not None
    if scheme in ("http", "https", "socks5"):
        return has_creds
    if scheme == "socks5h":
        return True
    return False


async def _read_http_head(reader: asyncio.StreamReader, *, is_request: bool = True) -> bytes:
    """Read until ``\\r\\n\\r\\n`` (HTTP header terminator), bounded to ``_HEADER_LIMIT``."""
    err_cls = _HttpValidationError if is_request else _UpstreamProxyError
    buf = bytearray()
    leading_empty = 0
    while True:
        try:
            chunk = await reader.readuntil(b"\r\n")
        except asyncio.IncompleteReadError as exc:
            if not buf and not exc.partial:
                return b""
            if not is_request:
                raise _UpstreamProxyError(
                    "Upstream closed with truncated HTTP response headers"
                ) from exc
            raise
        except asyncio.LimitOverrunError as exc:
            raise err_cls("HTTP header line exceeds limit") from exc
        line_body = chunk[:-2]
        if b"\r" in line_body or b"\n" in line_body or b"\x00" in line_body:
            raise err_cls("Invalid control character in HTTP header")
        if not buf and chunk == b"\r\n":
            leading_empty += 1
            if leading_empty > _MAX_LEADING_EMPTY_LINES:
                raise err_cls("Too many leading empty lines before HTTP head")
            continue
        buf.extend(chunk)
        if len(buf) > _HEADER_LIMIT:
            raise err_cls("HTTP header exceeds limit")
        if buf.endswith(b"\r\n\r\n"):
            return bytes(buf)


async def _pipe(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    *,
    idle_timeout: float = _DEFAULT_IDLE_TIMEOUT,
    last_activity: list[float] | None = None,
) -> bool:
    """Copy ``reader`` to ``writer``. Returns True on clean EOF, False on error/timeout."""
    slice_timeout = max(0.05, min(5.0, idle_timeout))
    if last_activity is None:
        last_activity = [time.monotonic()]
    clean_eof = False
    try:
        while True:
            try:
                data = await asyncio.wait_for(reader.read(_COPY_CHUNK), timeout=slice_timeout)
            except asyncio.TimeoutError:
                if time.monotonic() - last_activity[0] >= idle_timeout:
                    raise
                continue
            if not data:
                clean_eof = True
                break
            last_activity[0] = time.monotonic()
            writer.write(data)
            await asyncio.wait_for(writer.drain(), timeout=idle_timeout)
            last_activity[0] = time.monotonic()
    except (asyncio.TimeoutError, ConnectionError, OSError):
        clean_eof = False
    finally:
        if clean_eof:
            with contextlib.suppress(Exception):
                if writer.can_write_eof():
                    writer.write_eof()
    return clean_eof


async def _pipe_pair(
    reader_a: asyncio.StreamReader,
    writer_b: asyncio.StreamWriter,
    reader_b: asyncio.StreamReader,
    writer_a: asyncio.StreamWriter,
    *,
    idle_timeout: float,
) -> None:
    last_activity = [time.monotonic()]
    t1 = asyncio.create_task(
        _pipe(reader_a, writer_b, idle_timeout=idle_timeout, last_activity=last_activity)
    )
    t2 = asyncio.create_task(
        _pipe(reader_b, writer_a, idle_timeout=idle_timeout, last_activity=last_activity)
    )
    tasks = {t1: writer_b, t2: writer_a}
    try:
        done, pending = await asyncio.wait(tasks.keys(), return_when=asyncio.FIRST_COMPLETED)
        for finished in done:
            clean_eof = (
                not finished.cancelled()
                and finished.exception() is None
                and bool(finished.result())
            )
            if not clean_eof or not tasks[finished].can_write_eof():
                for p in pending:
                    p.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
    finally:
        remaining = [t for t in (t1, t2) if not t.done()]
        for t in remaining:
            t.cancel()
        if remaining:
            await asyncio.gather(*remaining, return_exceptions=True)


def _parse_http_headers(head: bytes, *, is_request: bool) -> _ParsedHeaders:
    err_cls = _HttpValidationError if is_request else _UpstreamProxyError
    raw_lines = head.split(b"\r\n")
    for raw_line in raw_lines[:-2]:
        if b"\r" in raw_line or b"\n" in raw_line or b"\x00" in raw_line:
            raise err_cls("Invalid control character in HTTP header")
    lines = raw_lines[1:-2]
    content_lengths: list[int] = []
    te_values: list[str] = []
    host: str | None = None
    conn_tokens: set[str] = set()
    proxy_conn_tokens: set[str] = set()
    expect_100 = False

    for line in lines:
        if b":" not in line:
            if is_request:
                raise _HttpValidationError("Malformed HTTP header line")
            continue
        name_b, val_b = line.split(b":", 1)
        if is_request and (not name_b or name_b != name_b.strip()):
            raise _HttpValidationError("Invalid whitespace in HTTP header name")
        name = name_b.strip().lower().decode("latin-1")
        val = val_b.strip().decode("latin-1")
        if name == "content-length":
            parts = [p.strip() for p in val.split(",")]
            if not parts or any(not (p.isascii() and p.isdigit()) for p in parts):
                raise err_cls("Invalid Content-Length header")
            content_lengths.extend(int(p) for p in parts)
        elif name == "transfer-encoding":
            te_values.extend(t.strip().lower() for t in val.split(",") if t.strip())
        elif name == "host":
            if host is None:
                host = val
            elif is_request:
                raise _HttpValidationError("Duplicate Host header")
        elif name == "connection":
            conn_tokens.update(t.strip().lower() for t in val.split(",") if t.strip())
        elif name == "proxy-connection":
            proxy_conn_tokens.update(t.strip().lower() for t in val.split(",") if t.strip())
        elif name == "expect":
            if "100-continue" in val.lower():
                expect_100 = True

    if len(set(content_lengths)) > 1:
        raise err_cls("Conflicting Content-Length headers")
    cl = content_lengths[0] if content_lengths else None
    has_te = bool(te_values)
    if is_request and has_te and te_values != ["chunked"]:
        raise _HttpValidationError("Unsupported request Transfer-Encoding")
    is_chunked = has_te and te_values[-1] == "chunked"
    if cl is not None and has_te:
        raise err_cls("Conflicting Content-Length and Transfer-Encoding headers")

    return _ParsedHeaders(
        content_length=cl,
        is_chunked=is_chunked,
        has_transfer_encoding=has_te,
        host=host,
        connection_tokens=conn_tokens,
        proxy_connection_tokens=proxy_conn_tokens,
        expect_100=expect_100,
    )


def _parse_authority(authority: str, default_port: int) -> tuple[str, int]:
    raw = authority.strip()
    if not raw:
        raise _HttpValidationError("Empty host authority")
    if raw.startswith("["):
        end = raw.find("]")
        if end == -1:
            raise _HttpValidationError("Unclosed IPv6 bracket in authority")
        host = raw[1:end]
        rest = raw[end + 1 :]
        if not rest:
            port = default_port
        elif rest.startswith(":") and rest[1:].isascii() and rest[1:].isdigit():
            port = int(rest[1:])
        else:
            raise _HttpValidationError("Invalid port in IPv6 authority")
    elif ":" in raw:
        host, port_s = raw.rsplit(":", 1)
        if not (port_s.isascii() and port_s.isdigit()):
            raise _HttpValidationError("Invalid port in authority")
        port = int(port_s)
    else:
        host = raw
        port = default_port
    if not host or not (1 <= port <= 65535):
        raise _HttpValidationError("Invalid host or port in authority")
    return host, port


def _parse_http_target(target: str, host_header: str | None) -> tuple[str, str, int, str]:
    if "://" in target:
        try:
            parsed = urlsplit(target)
            port_val = parsed.port
        except ValueError as exc:
            raise _HttpValidationError("Invalid target URL") from exc
        scheme = (parsed.scheme or "").lower()
        if scheme not in ("http", "https") or not parsed.hostname:
            raise _HttpValidationError("Unsupported proxy target URL")
        port = port_val if port_val is not None else (443 if scheme == "https" else 80)
        if not (1 <= port <= 65535):
            raise _HttpValidationError("Invalid target port")
        path = parsed.path or "/"
        origin_path = f"{path}?{parsed.query}" if parsed.query else path
        return scheme, parsed.hostname, port, origin_path
    if not host_header:
        raise _HttpValidationError("Missing Host header for origin-form request")
    host, port = _parse_authority(host_header, 80)
    origin_path = target or "/"
    return "http", host, port, origin_path


def _format_host_header(host: str, port: int, default_port: int = 80) -> str:
    base = f"[{host}]" if ":" in host and not host.startswith("[") else host
    return base if port == default_port else f"{base}:{port}"


def _rewrite_head_for_origin(
    head: bytes,
    method_b: bytes,
    origin_path: str,
    version_b: bytes,
    default_host_hdr: str,
) -> bytes:
    lines = head.split(b"\r\n")
    rebuilt = bytearray()
    rebuilt.extend(method_b + b" " + origin_path.encode("latin-1") + b" " + version_b + b"\r\n")
    has_host = False
    has_connection = False
    proxy_conn_close = False
    for line in lines[1:]:
        if not line:
            continue
        low = line.lower()
        if low.startswith(b"proxy-authorization:"):
            continue
        if low.startswith(b"proxy-connection:"):
            val = line.split(b":", 1)[1].strip().lower()
            if b"close" in val:
                proxy_conn_close = True
            continue
        if low.startswith(b"host:"):
            has_host = True
        elif low.startswith(b"connection:"):
            has_connection = True
        rebuilt.extend(line)
        rebuilt.extend(b"\r\n")
    if not has_host:
        rebuilt.extend(f"Host: {default_host_hdr}\r\n".encode("latin-1"))
    if not has_connection and proxy_conn_close:
        rebuilt.extend(b"Connection: close\r\n")
    rebuilt.extend(b"\r\n")
    return bytes(rebuilt)


def _strip_connection_close(resp_head: bytes) -> bytes:
    lines = resp_head.split(b"\r\n")
    if not lines:
        return resp_head
    rebuilt = bytearray(lines[0] + b"\r\n")
    for line in lines[1:]:
        if not line:
            continue
        low = line.lower()
        if low.startswith((b"connection:", b"proxy-connection:")):
            hdr_name = line.split(b":", 1)[0].strip()
            tokens = [t.strip() for t in line.split(b":", 1)[1].split(b",")]
            kept = [t for t in tokens if t and t.lower() != b"close"]
            if not kept:
                continue
            rebuilt.extend(hdr_name + b": " + b", ".join(kept) + b"\r\n")
            continue
        rebuilt.extend(line + b"\r\n")
    rebuilt.extend(b"\r\n")
    return bytes(rebuilt)


async def _relay_exact(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    length: int,
    *,
    timeout: float,
) -> None:
    remaining = length
    while remaining > 0:
        chunk = await asyncio.wait_for(
            reader.read(min(remaining, _COPY_CHUNK)),
            timeout=timeout,
        )
        if not chunk:
            raise EOFError("Unexpected EOF during fixed-length HTTP body")
        writer.write(chunk)
        await asyncio.wait_for(writer.drain(), timeout=timeout)
        remaining -= len(chunk)


async def _relay_chunked(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    *,
    timeout: float,
) -> None:
    while True:
        size_line = await asyncio.wait_for(reader.readuntil(b"\r\n"), timeout=timeout)
        size_token = size_line[:-2].split(b";", 1)[0].strip()
        if not re.fullmatch(rb"[0-9A-Fa-f]+", size_token):
            raise _HttpValidationError("Invalid chunked transfer size")
        chunk_size = int(size_token, 16)
        writer.write(size_line)
        if chunk_size == 0:
            trailer_bytes = 0
            while True:
                trailer = await asyncio.wait_for(reader.readuntil(b"\r\n"), timeout=timeout)
                trailer_bytes += len(trailer)
                if trailer_bytes > _HEADER_LIMIT:
                    raise _HttpValidationError("Chunked trailer exceeds limit")
                writer.write(trailer)
                if trailer == b"\r\n":
                    break
            await asyncio.wait_for(writer.drain(), timeout=timeout)
            return
        await _relay_exact(reader, writer, chunk_size, timeout=timeout)
        crlf = await asyncio.wait_for(reader.readexactly(2), timeout=timeout)
        if crlf != b"\r\n":
            raise _HttpValidationError("Missing CRLF after chunk payload")
        writer.write(crlf)
        await asyncio.wait_for(writer.drain(), timeout=timeout)


class LocalProxyRelay:
    """Local unauthenticated HTTP proxy that bridges HTTP/HTTPS/SOCKS5 upstream proxies."""

    def __init__(
        self,
        upstream_url: str,
        *,
        connect_timeout: float = 15.0,
        idle_timeout: float = _DEFAULT_IDLE_TIMEOUT,
        close_timeout: float = _DEFAULT_CLOSE_TIMEOUT,
    ) -> None:
        cleaned = upstream_url.strip()
        parsed = urlsplit(cleaned)
        scheme = (parsed.scheme or "http").lower()
        if scheme not in ("http", "https", "socks5", "socks5h"):
            raise ValueError(f"Unsupported upstream proxy scheme: {scheme}")
        if not parsed.hostname:
            raise ValueError(f"Invalid upstream proxy URL: {mask_proxy(upstream_url)}")
        self.upstream_url = cleaned
        self.upstream_scheme = scheme
        self.upstream_host = parsed.hostname
        default_port = 443 if scheme == "https" else (1080 if scheme in ("socks5", "socks5h") else 80)
        port_val = parsed.port
        self.upstream_port = port_val if port_val is not None else default_port
        if not (1 <= self.upstream_port <= 65535):
            raise ValueError(f"Invalid upstream proxy port: {self.upstream_port}")
        self.connect_timeout = connect_timeout
        self.idle_timeout = idle_timeout
        self.close_timeout = close_timeout

        self._is_socks5 = scheme in ("socks5", "socks5h")
        self._has_auth = parsed.username is not None or parsed.password is not None
        user = unquote(parsed.username or "")
        password = unquote(parsed.password or "")
        self._user_bytes = user.encode("utf-8")
        self._pass_bytes = password.encode("utf-8")
        if self._is_socks5 and self._has_auth and (
            len(self._user_bytes) > 255 or len(self._pass_bytes) > 255
        ):
            raise ValueError("SOCKS5 username/password must not exceed 255 bytes")

        if self._has_auth:
            token = base64.b64encode(f"{user}:{password}".encode()).decode("ascii")
            self._auth_header = f"Proxy-Authorization: Basic {token}\r\n".encode("ascii")
        else:
            self._auth_header = b""

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
        tasks = list(self._tasks)
        self._tasks.clear()
        for t in tasks:
            t.cancel()
        if tasks:
            with contextlib.suppress(Exception):
                await asyncio.wait_for(
                    asyncio.gather(*tasks, return_exceptions=True),
                    timeout=self.close_timeout + 1.0,
                )
        if server is not None:
            with contextlib.suppress(Exception):
                await asyncio.wait_for(server.wait_closed(), timeout=self.close_timeout)
        self.local_port = None

    def _accept(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        task = asyncio.create_task(self._handle_client(reader, writer))
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)

    async def _close_writer(self, writer: asyncio.StreamWriter | None) -> None:
        if writer is None:
            return
        with contextlib.suppress(Exception):
            writer.close()
            await asyncio.wait_for(writer.wait_closed(), timeout=self.close_timeout)

    async def _open_upstream(self) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        ssl_arg = True if self.upstream_scheme == "https" else None
        try:
            return await asyncio.wait_for(
                asyncio.open_connection(self.upstream_host, self.upstream_port, ssl=ssl_arg),
                timeout=self.connect_timeout,
            )
        except asyncio.TimeoutError as exc:
            raise _UpstreamTimeoutError("Timed out connecting to upstream proxy") from exc
        except (OSError, Exception) as exc:
            if isinstance(exc, asyncio.CancelledError):
                raise
            raise _UpstreamProxyError(f"Failed connecting to upstream proxy: {exc}") from exc

    async def _socks5_connect(
        self, target_host: str, target_port: int
    ) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        if not (1 <= target_port <= 65535):
            raise ValueError(f"Invalid SOCKS5 target port: {target_port}")
        clean_host = target_host.strip()
        if clean_host.startswith("[") and clean_host.endswith("]"):
            clean_host = clean_host[1:-1]
        if not clean_host:
            raise ValueError("Empty SOCKS5 target host")

        addr_payload: bytes
        try:
            ip_obj = ipaddress.ip_address(clean_host.split("%", 1)[0])
        except ValueError:
            host_bytes = clean_host.encode("idna")
            if not (1 <= len(host_bytes) <= 255):
                raise ValueError("SOCKS5 domain length must be 1..255 bytes") from None
            addr_payload = b"\x03" + bytes([len(host_bytes)]) + host_bytes
        else:
            if ip_obj.version == 4:
                addr_payload = b"\x01" + ip_obj.packed
            else:
                addr_payload = b"\x04" + ip_obj.packed

        upstream_reader, upstream_writer = await self._open_upstream()
        try:
            await asyncio.wait_for(
                self._socks5_handshake(
                    upstream_reader, upstream_writer, addr_payload, target_port
                ),
                timeout=self.connect_timeout,
            )
            return upstream_reader, upstream_writer
        except _UpstreamProxyError:
            await self._close_writer(upstream_writer)
            raise
        except asyncio.TimeoutError as exc:
            await self._close_writer(upstream_writer)
            raise _UpstreamTimeoutError("SOCKS5 handshake timed out") from exc
        except asyncio.CancelledError:
            await self._close_writer(upstream_writer)
            raise
        except Exception as exc:
            await self._close_writer(upstream_writer)
            raise _UpstreamProxyError(f"SOCKS5 handshake failed: {exc}") from exc

    async def _socks5_handshake(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        addr_payload: bytes,
        target_port: int,
    ) -> None:
        if self._has_auth:
            writer.write(b"\x05\x02\x00\x02")
        else:
            writer.write(b"\x05\x01\x00")
        await writer.drain()

        greeting_resp = await reader.readexactly(2)
        ver, method = greeting_resp[0], greeting_resp[1]
        if ver != 0x05:
            raise _UpstreamProxyError(f"Invalid SOCKS5 greeting version: {ver}")
        if method == 0x02:
            if not self._has_auth:
                raise _UpstreamProxyError("SOCKS5 upstream demanded username/password auth")
            auth_pkt = (
                b"\x01"
                + bytes([len(self._user_bytes)])
                + self._user_bytes
                + bytes([len(self._pass_bytes)])
                + self._pass_bytes
            )
            writer.write(auth_pkt)
            await writer.drain()
            auth_resp = await reader.readexactly(2)
            if auth_resp[0] != 0x01 or auth_resp[1] != 0x00:
                raise _UpstreamProxyError(f"SOCKS5 auth rejected (status={auth_resp[1]})")
        elif method != 0x00:
            raise _UpstreamProxyError(f"SOCKS5 no acceptable auth method (method={method:#x})")

        connect_req = b"\x05\x01\x00" + addr_payload + struct.pack("!H", target_port)
        writer.write(connect_req)
        await writer.drain()

        reply_hdr = await reader.readexactly(4)
        r_ver, rep, _rsv, bnd_atyp = reply_hdr[0], reply_hdr[1], reply_hdr[2], reply_hdr[3]
        if r_ver != 0x05:
            raise _UpstreamProxyError(f"Invalid SOCKS5 reply version: {r_ver}")
        if rep != 0x00:
            raise _UpstreamProxyError(f"SOCKS5 CONNECT failed with reply={rep:#x}")
        if bnd_atyp == 0x01:
            await reader.readexactly(6)
        elif bnd_atyp == 0x04:
            await reader.readexactly(18)
        elif bnd_atyp == 0x03:
            bnd_len = (await reader.readexactly(1))[0]
            await reader.readexactly(bnd_len + 2)
        else:
            raise _UpstreamProxyError(f"Unsupported SOCKS5 BND ATYP: {bnd_atyp:#x}")

    def _inject_auth(self, head: bytes) -> bytes:
        """Replace or append Proxy-Authorization header in raw HTTP request headers."""
        if not self._has_auth:
            return head
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

    async def _handle_connect(
        self,
        client_reader: asyncio.StreamReader,
        client_writer: asyncio.StreamWriter,
        head: bytes,
        target: str,
    ) -> None:
        upstream_writer: asyncio.StreamWriter | None = None
        try:
            if self._is_socks5:
                target_host, target_port = _parse_authority(target, 443)
                upstream_reader, upstream_writer = await self._socks5_connect(target_host, target_port)
                client_writer.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                await asyncio.wait_for(client_writer.drain(), timeout=self.idle_timeout)
            else:
                upstream_reader, upstream_writer = await self._open_upstream()
                try:
                    upstream_writer.write(self._inject_auth(head))
                    await asyncio.wait_for(upstream_writer.drain(), timeout=self.connect_timeout)
                    resp_head = await asyncio.wait_for(
                        _read_http_head(upstream_reader, is_request=False),
                        timeout=self.connect_timeout,
                    )
                except asyncio.TimeoutError as exc:
                    raise _UpstreamTimeoutError("Upstream HTTP CONNECT timed out") from exc
                except (OSError, asyncio.IncompleteReadError) as exc:
                    raise _UpstreamProxyError(f"Upstream HTTP CONNECT failed: {exc}") from exc
                if not resp_head:
                    raise _UpstreamProxyError("Upstream closed during HTTP CONNECT")
                client_writer.write(resp_head)
                await asyncio.wait_for(client_writer.drain(), timeout=self.idle_timeout)
                status_line = resp_head.split(b"\r\n", 1)[0]
                parts = status_line.split(b" ", 2)
                if len(parts) < 2 or parts[1] != b"200":
                    return

            await _pipe_pair(
                client_reader,
                upstream_writer,
                upstream_reader,
                client_writer,
                idle_timeout=self.idle_timeout,
            )
        finally:
            await self._close_writer(upstream_writer)

    async def _handle_client(
        self,
        client_reader: asyncio.StreamReader,
        client_writer: asyncio.StreamWriter,
    ) -> None:
        upstream_reader: asyncio.StreamReader | None = None
        upstream_writer: asyncio.StreamWriter | None = None
        upstream_target: tuple[str, int] | None = None
        response_started = False

        try:
            while True:
                response_started = False
                try:
                    head = await asyncio.wait_for(
                        _read_http_head(client_reader),
                        timeout=self.idle_timeout,
                    )
                except asyncio.TimeoutError:
                    return
                if not head:
                    return

                first_line = head.split(b"\r\n", 1)[0]
                req_parts = first_line.split(b" ")
                if len(req_parts) != 3 or not all(req_parts):
                    raise _HttpValidationError("Malformed HTTP request line")
                method_b, target_b, version_b = req_parts
                method_u = method_b.upper()
                version_u = version_b.upper()
                if not version_u.startswith(b"HTTP/1."):
                    raise _HttpValidationError("Unsupported HTTP version")
                target_str = target_b.decode("latin-1")

                if method_u == b"CONNECT":
                    await self._close_writer(upstream_writer)
                    upstream_writer = None
                    upstream_reader = None
                    upstream_target = None
                    await self._handle_connect(
                        client_reader, client_writer, head, target_str
                    )
                    return

                req_hdrs = _parse_http_headers(head, is_request=True)
                has_req_body = req_hdrs.is_chunked or (
                    req_hdrs.content_length is not None and req_hdrs.content_length > 0
                )

                if self._is_socks5:
                    scheme, target_host, target_port, origin_path = _parse_http_target(
                        target_str, req_hdrs.host
                    )
                    if scheme == "https":
                        client_writer.write(
                            b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n"
                        )
                        await asyncio.wait_for(client_writer.drain(), timeout=self.close_timeout)
                        return
                    target_key = (target_host, target_port)
                    if upstream_writer is not None and (
                        upstream_target != target_key
                        or upstream_writer.is_closing()
                        or (upstream_reader is not None and upstream_reader.at_eof())
                    ):
                        await self._close_writer(upstream_writer)
                        upstream_writer = None
                        upstream_reader = None
                        upstream_target = None
                    if upstream_writer is None or upstream_reader is None:
                        upstream_reader, upstream_writer = await self._socks5_connect(
                            target_host, target_port
                        )
                        upstream_target = target_key
                    default_host_hdr = _format_host_header(target_host, target_port, 80)
                    outgoing_head = _rewrite_head_for_origin(
                        head, method_b, origin_path, version_b, default_host_hdr
                    )
                else:
                    if upstream_writer is not None and (
                        upstream_writer.is_closing()
                        or (upstream_reader is not None and upstream_reader.at_eof())
                    ):
                        await self._close_writer(upstream_writer)
                        upstream_writer = None
                        upstream_reader = None
                    if upstream_writer is None or upstream_reader is None:
                        upstream_reader, upstream_writer = await self._open_upstream()
                    outgoing_head = self._inject_auth(head)

                upstream_writer.write(outgoing_head)
                await asyncio.wait_for(upstream_writer.drain(), timeout=self.idle_timeout)

                prefetched_resp: bytes | None = None
                skip_req_body = False
                if req_hdrs.expect_100 and has_req_body:
                    wait_100 = min(1.5, self.idle_timeout)
                    while True:
                        try:
                            interim = await asyncio.wait_for(
                                _read_http_head(upstream_reader, is_request=False),
                                timeout=wait_100,
                            )
                        except asyncio.TimeoutError:
                            break
                        if not interim:
                            raise EOFError("Upstream closed during Expect: 100-continue")
                        s_parts = interim.split(b"\r\n", 1)[0].split(b" ", 2)
                        status_code = (
                            int(s_parts[1])
                            if len(s_parts) >= 2 and s_parts[1].isascii() and s_parts[1].isdigit()
                            else 0
                        )
                        if status_code == 100:
                            response_started = True
                            client_writer.write(interim)
                            await asyncio.wait_for(client_writer.drain(), timeout=self.idle_timeout)
                            break
                        if 100 < status_code < 200:
                            response_started = True
                            client_writer.write(interim)
                            await asyncio.wait_for(client_writer.drain(), timeout=self.idle_timeout)
                            continue
                        prefetched_resp = interim
                        skip_req_body = True
                        break

                if not skip_req_body:
                    if req_hdrs.is_chunked:
                        await _relay_chunked(
                            client_reader, upstream_writer, timeout=self.idle_timeout
                        )
                    elif req_hdrs.content_length is not None and req_hdrs.content_length > 0:
                        await _relay_exact(
                            client_reader,
                            upstream_writer,
                            req_hdrs.content_length,
                            timeout=self.idle_timeout,
                        )

                while True:
                    if prefetched_resp is not None:
                        resp_head = prefetched_resp
                        prefetched_resp = None
                    else:
                        resp_head = await asyncio.wait_for(
                            _read_http_head(upstream_reader, is_request=False),
                            timeout=self.idle_timeout,
                        )
                    if not resp_head:
                        raise EOFError("Upstream closed before sending HTTP response")

                    status_line = resp_head.split(b"\r\n", 1)[0]
                    s_parts = status_line.split(b" ", 2)
                    if len(s_parts) < 2 or not (s_parts[1].isascii() and s_parts[1].isdigit()):
                        raise _UpstreamProxyError("Invalid HTTP response status line from upstream")
                    resp_ver = s_parts[0].upper()
                    status_code = int(s_parts[1])

                    if status_code == 101:
                        response_started = True
                        client_writer.write(resp_head)
                        await asyncio.wait_for(client_writer.drain(), timeout=self.idle_timeout)
                        await _pipe_pair(
                            client_reader,
                            upstream_writer,
                            upstream_reader,
                            client_writer,
                            idle_timeout=self.idle_timeout,
                        )
                        return
                    if 100 <= status_code < 200:
                        response_started = True
                        client_writer.write(resp_head)
                        await asyncio.wait_for(client_writer.drain(), timeout=self.idle_timeout)
                        continue
                    break

                resp_hdrs = _parse_http_headers(resp_head, is_request=False)
                if version_u == b"HTTP/1.0":
                    client_wants_ka = (
                        "keep-alive" in req_hdrs.connection_tokens
                        or "keep-alive" in req_hdrs.proxy_connection_tokens
                    )
                else:
                    client_wants_ka = (
                        "close" not in req_hdrs.connection_tokens
                        and "close" not in req_hdrs.proxy_connection_tokens
                    )

                upstream_close = (
                    "close" in resp_hdrs.connection_tokens
                    or "close" in resp_hdrs.proxy_connection_tokens
                    or (
                        resp_ver == b"HTTP/1.0"
                        and "keep-alive" not in resp_hdrs.connection_tokens
                        and "keep-alive" not in resp_hdrs.proxy_connection_tokens
                    )
                )

                if method_u == b"HEAD" or status_code in (204, 304):
                    body_mode = "none"
                    framed = True
                elif resp_hdrs.is_chunked:
                    body_mode = "chunked"
                    framed = True
                elif resp_hdrs.has_transfer_encoding:
                    body_mode = "eof"
                    framed = False
                elif resp_hdrs.content_length is not None:
                    body_mode = "length"
                    framed = True
                else:
                    body_mode = "eof"
                    framed = False

                if skip_req_body:
                    client_keep_alive = False
                    upstream_keep_alive = False
                elif not framed:
                    client_keep_alive = False
                    upstream_keep_alive = False
                else:
                    client_keep_alive = client_wants_ka
                    upstream_keep_alive = not upstream_close

                out_resp_head = (
                    _strip_connection_close(resp_head)
                    if (client_keep_alive and upstream_close)
                    else resp_head
                )
                response_started = True
                client_writer.write(out_resp_head)
                await asyncio.wait_for(client_writer.drain(), timeout=self.idle_timeout)

                if body_mode == "chunked":
                    await _relay_chunked(
                        upstream_reader, client_writer, timeout=self.idle_timeout
                    )
                elif body_mode == "length" and resp_hdrs.content_length:
                    await _relay_exact(
                        upstream_reader,
                        client_writer,
                        resp_hdrs.content_length,
                        timeout=self.idle_timeout,
                    )
                elif body_mode == "eof":
                    await _pipe(upstream_reader, client_writer, idle_timeout=self.idle_timeout)

                if not upstream_keep_alive:
                    await self._close_writer(upstream_writer)
                    upstream_writer = None
                    upstream_reader = None
                    upstream_target = None

                if not client_keep_alive:
                    return
        except _HttpValidationError as exc:
            log.debug("proxy relay bad request: %s", exc)
            if not response_started:
                with contextlib.suppress(Exception):
                    client_writer.write(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                    await asyncio.wait_for(client_writer.drain(), timeout=self.close_timeout)
        except _UpstreamTimeoutError as exc:
            log.warning("upstream proxy timeout (%s): %s", mask_proxy(self.upstream_url), exc)
            if not response_started:
                with contextlib.suppress(Exception):
                    client_writer.write(b"HTTP/1.1 504 Gateway Timeout\r\nConnection: close\r\n\r\n")
                    await asyncio.wait_for(client_writer.drain(), timeout=self.close_timeout)
        except _UpstreamProxyError as exc:
            log.warning("upstream proxy error (%s): %s", mask_proxy(self.upstream_url), exc)
            if not response_started:
                with contextlib.suppress(Exception):
                    client_writer.write(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                    await asyncio.wait_for(client_writer.drain(), timeout=self.close_timeout)
        except (asyncio.CancelledError, asyncio.IncompleteReadError, ConnectionError, OSError):
            pass
        except Exception as exc:  # noqa: BLE001
            log.debug("proxy relay error: %s", exc)
            if not response_started:
                with contextlib.suppress(Exception):
                    client_writer.write(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                    await asyncio.wait_for(client_writer.drain(), timeout=self.close_timeout)
        finally:
            await self._close_writer(upstream_writer)
            await self._close_writer(client_writer)
