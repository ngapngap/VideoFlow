"""Local media storage for generated images/videos and helpers for image inputs."""

from __future__ import annotations

import asyncio
import base64
import ipaddress
import mimetypes
import re
import socket
import time
import urllib.parse
import uuid
from pathlib import Path

import httpx

from ..errors import InvalidRequest, NotFound

_EXT_BY_MIME = {
    "image/png": ".png",
    "image/jpeg": ".jpg",
    "image/webp": ".webp",
    "image/gif": ".gif",
    "image/avif": ".avif",
    "video/mp4": ".mp4",
    "video/webm": ".webm",
    "video/quicktime": ".mov",
}
_SAFE_NAME = re.compile(r"^[A-Za-z0-9_\-]+\.[A-Za-z0-9]+$")


class MediaStore:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.root.mkdir(parents=True, exist_ok=True)

    def save(self, data: bytes, mime: str, prefix: str = "m") -> str:
        ext = _EXT_BY_MIME.get(mime.split(";")[0].strip().lower(), ".bin")
        name = f"{prefix}_{uuid.uuid4().hex[:16]}{ext}"
        partial_path = self.root / f"{name}.partial"
        final_path = self.root / name
        try:
            partial_path.write_bytes(data)
            partial_path.replace(final_path)
        except Exception:
            if partial_path.is_file():
                try:
                    partial_path.unlink()
                except OSError:
                    pass
            raise
        return name

    def path_of(self, name: str) -> Path:
        if not _SAFE_NAME.match(name):
            raise NotFound("media not found")
        p = self.root / name
        if not p.is_file():
            raise NotFound("media not found")
        return p

    def prune(self, max_age_seconds: float) -> int:
        """Delete stored media files older than ``max_age_seconds``.

        Only removes files whose names match the store's safe pattern, so a
        foreign file dropped into the directory is never touched. Returns the
        number of deleted files.
        """
        if max_age_seconds <= 0:
            return 0
        cutoff = time.time() - max_age_seconds
        removed = 0
        for child in self.root.iterdir():
            if (
                child.is_file()
                and _SAFE_NAME.match(child.name)
                and child.stat().st_mtime < cutoff
            ):
                child.unlink()
                removed += 1
        return removed

    @staticmethod
    def mime_of(name: str) -> str:
        return mimetypes.guess_type(name)[0] or "application/octet-stream"


_MAX_FETCH_BYTES = 20 * 1024 * 1024  # 20 MiB cap on server-side image fetches
_MAX_B64_LEN = ((_MAX_FETCH_BYTES + 2) // 3) * 4
_MAX_REDIRECTS = 5
_BLOCKED_V6_NETWORKS = (
    ipaddress.IPv6Network("64:ff9b::/96"),    # NAT64 well-known (RFC 6052)
    ipaddress.IPv6Network("64:ff9b:1::/48"),  # NAT64 local-use (RFC 8215)
    ipaddress.IPv6Network("2002::/16"),       # 6to4 (RFC 3056)
    ipaddress.IPv6Network("2001::/32"),       # Teredo (RFC 4380)
    ipaddress.IPv6Network("::/96"),           # Deprecated IPv4-compatible (RFC 4291)
)


def _assert_public_url(url: str) -> str:
    """Reject non-http(s) URLs and malformed ports/hosts (SSRF guard)."""
    parsed = urllib.parse.urlparse(url)
    try:
        port = parsed.port
        httpx_url = httpx.URL(url)
    except (httpx.InvalidURL, ValueError) as exc:
        raise InvalidRequest("image_url must be a public http(s) URL") from exc
    if parsed.scheme not in ("http", "https") or not parsed.hostname or not httpx_url.raw_host:
        raise InvalidRequest("image_url must be a public http(s) URL")
    if port is not None and not (1 <= port <= 65535):
        raise InvalidRequest("image_url port must be between 1 and 65535")
    return url


def _is_public_ip(ip: ipaddress.IPv4Address | ipaddress.IPv6Address) -> bool:
    """Return True only if ``ip`` is a globally routable unicast address."""
    if ip.version == 6:
        if ip.scope_id is not None:
            return False
        if ip.ipv4_mapped is not None:
            return _is_public_ip(ip.ipv4_mapped)
        if ip.sixtofour is not None or ip.teredo is not None or ip.is_site_local:
            return False
        if any(ip in net for net in _BLOCKED_V6_NETWORKS):
            return False
    return (
        ip.is_global
        and not ip.is_private
        and not ip.is_loopback
        and not ip.is_link_local
        and not ip.is_multicast
        and not ip.is_reserved
        and not ip.is_unspecified
    )


async def _resolve_public_ips(hostname: str) -> list[str]:
    """Resolve hostname once and return validated public IP strings (unwrapping v4-mapped)."""
    if not hostname or "%" in hostname:
        raise InvalidRequest("image_url must resolve to a public IP address")
    try:
        literal = ipaddress.ip_address(hostname)
    except ValueError:
        literal = None
    if literal is not None:
        if not _is_public_ip(literal):
            raise InvalidRequest("image_url must resolve to a public IP address")
        if literal.version == 6 and literal.ipv4_mapped is not None:
            literal = literal.ipv4_mapped
        return [literal.compressed]

    try:
        infos = await asyncio.get_running_loop().getaddrinfo(
            hostname, None, type=socket.SOCK_STREAM
        )
    except socket.gaierror as exc:
        raise InvalidRequest("image_url host does not resolve") from exc
    if not infos:
        raise InvalidRequest("image_url host does not resolve")

    resolved: list[str] = []
    for info in infos:
        try:
            ip = ipaddress.ip_address(info[4][0])
        except ValueError as exc:
            raise InvalidRequest("image_url must resolve to a public IP address") from exc
        if not _is_public_ip(ip):
            raise InvalidRequest("image_url must resolve to a public IP address")
        if ip.version == 6 and ip.ipv4_mapped is not None:
            ip = ip.ipv4_mapped
        s = ip.compressed
        if s not in resolved:
            resolved.append(s)
    return resolved


async def _assert_public_host(hostname: str) -> None:
    """Reject hostnames resolving to non-public IPs (SSRF guard)."""
    await _resolve_public_ips(hostname)


def _is_ip_literal(host: str) -> bool:
    clean = host.strip("[]").split("%", 1)[0]
    try:
        ipaddress.ip_address(clean)
        return True
    except ValueError:
        return False


async def _fetch_public_image(url: str, *, timeout: float) -> tuple[bytes, str]:
    """Fetch an image URL with single-shot DNS IP pinning, manual redirects, and a size cap."""
    deadline = time.monotonic() + timeout
    try:
        async with httpx.AsyncClient(
            timeout=timeout,
            follow_redirects=False,
            trust_env=False,
            limits=httpx.Limits(max_keepalive_connections=0),
        ) as client:
            for hop in range(_MAX_REDIRECTS + 1):
                url = _assert_public_url(url)
                parsed = httpx.URL(url)
                raw_host = parsed.raw_host.decode("ascii")
                lookup_host = raw_host.strip("[]")
                public_ips = await _resolve_public_ips(lookup_host)
                headers = {
                    "Host": parsed.netloc.decode("ascii"),
                    "Accept-Encoding": "identity",
                }
                extensions = {} if _is_ip_literal(lookup_host) else {"sni_hostname": raw_host}

                redirect_next: str | None = None
                last_connect_exc: httpx.HTTPError | None = None
                for idx, pinned_ip in enumerate(public_ips):
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise httpx.TimeoutException("image_url fetch deadline exceeded")
                    pinned_url = str(parsed.copy_with(username="", password="", host=pinned_ip))
                    try:
                        async with client.stream(
                            "GET",
                            pinned_url,
                            headers=headers,
                            extensions=extensions,
                            timeout=remaining,
                        ) as resp:
                            if resp.status_code in (301, 302, 303, 307, 308):
                                if hop >= _MAX_REDIRECTS:
                                    raise InvalidRequest("image_url exceeded redirect limit")
                                location = resp.headers.get("location")
                                if not location:
                                    raise InvalidRequest("image_url redirect missing location")
                                redirect_next = urllib.parse.urljoin(url, location)
                                break
                            resp.raise_for_status()
                            ce = (resp.headers.get("content-encoding") or "").strip().lower()
                            if ce and ce != "identity":
                                raise InvalidRequest("compressed image responses are not supported")
                            content_length = resp.headers.get("content-length")
                            if content_length is not None:
                                try:
                                    declared_len = int(content_length.strip())
                                except ValueError:
                                    declared_len = None
                                if declared_len is not None and declared_len > _MAX_FETCH_BYTES:
                                    raise InvalidRequest("image_url response exceeds 20 MiB")
                            mime = (
                                resp.headers.get("content-type", "image/png").split(";")[0].strip()
                                or "image/png"
                            )
                            body = bytearray()
                            async for chunk in resp.aiter_bytes(65536):
                                if time.monotonic() > deadline:
                                    raise httpx.TimeoutException("image_url fetch deadline exceeded")
                                body.extend(chunk)
                                if len(body) > _MAX_FETCH_BYTES:
                                    raise InvalidRequest("image_url response exceeds 20 MiB")
                            return bytes(body), mime
                    except (httpx.ConnectError, httpx.ConnectTimeout) as exc:
                        last_connect_exc = exc
                        if idx < len(public_ips) - 1:
                            continue
                        raise
                if redirect_next is not None:
                    url = redirect_next
                    continue
                if last_connect_exc is not None:
                    raise last_connect_exc
            raise InvalidRequest("image_url exceeded redirect limit")
    except httpx.HTTPError as exc:
        raise InvalidRequest(f"failed to fetch image_url: {exc}") from exc


def _decode_base64_bounded(payload: str, *, err_msg: str) -> bytes:
    """Reject oversized base64 strings before invoking ``base64.b64decode``."""
    if len(payload) > _MAX_B64_LEN * 2:
        raise InvalidRequest("image exceeds 20 MiB")
    compact = "".join(payload.split())
    if len(compact) > _MAX_B64_LEN:
        raise InvalidRequest("image exceeds 20 MiB")
    if compact and len(compact) % 4 == 0:
        pad = 2 if compact.endswith("==") else 1 if compact.endswith("=") else 0
        if (len(compact) // 4) * 3 - pad > _MAX_FETCH_BYTES:
            raise InvalidRequest("image exceeds 20 MiB")
    try:
        raw = base64.b64decode(compact, validate=True)
    except ValueError as exc:
        raise InvalidRequest(err_msg) from exc
    if len(raw) > _MAX_FETCH_BYTES:
        raise InvalidRequest("image exceeds 20 MiB")
    if not raw:
        raise InvalidRequest(err_msg)
    return raw


async def load_image_ref(ref: str, *, timeout: float = 20.0) -> tuple[bytes, str]:
    """Decode a data URL or download an http(s) URL. Returns ``(bytes, mime)``."""
    ref = ref.strip()
    if ref.startswith("data:"):
        header, sep, payload = ref.partition(",")
        if not sep:
            raise InvalidRequest("invalid base64 image data")
        mime = header[5:].split(";")[0].strip() or "image/png"
        return _decode_base64_bounded(payload, err_msg="invalid base64 image data"), mime
    if ref.startswith(("http://", "https://")):
        return await _fetch_public_image(ref, timeout=timeout)
    return _decode_base64_bounded(
        ref, err_msg="image must be a data URL, http(s) URL or base64 string"
    ), "image/png"
