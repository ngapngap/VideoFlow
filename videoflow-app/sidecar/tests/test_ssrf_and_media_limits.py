from __future__ import annotations

import asyncio
import base64
import ipaddress
import socket
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock

import anyio
import anyio.abc
import anyio.streams.tls
import pytest
from starlette.datastructures import FormData, Headers, UploadFile

from muse2api.api.routes.images import MAX_REFERENCE_IMAGES, edit_images, generate_images
from muse2api.api.schemas import ImageGenerationRequest
from muse2api.core import media
from muse2api.core.media import (
    _EXT_BY_MIME,
    _MAX_B64_LEN,
    _MAX_FETCH_BYTES,
    _MAX_REDIRECTS,
    MediaStore,
    _assert_public_host,
    _assert_public_url,
    _is_public_ip,
    _resolve_public_ips,
    load_image_ref,
)
from muse2api.drivers.base import MediaResult
from muse2api.errors import InvalidRequest


class _DummyRawSocket:
    def setsockopt(self, *args, **kwargs) -> None:
        return None


class _FakeByteStream(anyio.abc.ByteStream):
    """Minimal in-memory ByteStream speaking raw HTTP/1.1 to httpcore."""

    def __init__(self, responses: list[bytes], recorded_requests: list[bytes]) -> None:
        self._responses = responses
        self._recorded_requests = recorded_requests
        self._buf = bytearray()
        self._raw_socket = _DummyRawSocket()
        self._closed = False

    async def send(self, item: bytes) -> None:
        self._recorded_requests.append(bytes(item))
        if not self._buf and self._responses:
            self._buf.extend(self._responses.pop(0))

    async def receive(self, max_bytes: int = 65536) -> bytes:
        if self._closed:
            raise anyio.EndOfStream
        if not self._buf:
            if self._responses:
                self._buf.extend(self._responses.pop(0))
            else:
                raise anyio.EndOfStream
        chunk = bytes(self._buf[:max_bytes])
        del self._buf[:max_bytes]
        return chunk

    async def send_eof(self) -> None:
        return None

    async def aclose(self) -> None:
        self._closed = True


def _http_response(
    status: int,
    reason: str,
    headers: dict[str, str],
    body: bytes = b"",
) -> bytes:
    lines = [f"HTTP/1.1 {status} {reason}"]
    for k, v in headers.items():
        lines.append(f"{k}: {v}")
    lines.append("")
    lines.append("")
    return "\r\n".join(lines).encode("ascii") + body


def test_avif_mime_mapping_and_media_store_save(tmp_path: Path) -> None:
    assert _EXT_BY_MIME["image/avif"] == ".avif"
    store = MediaStore(tmp_path)
    name = store.save(b"avif-payload", "image/avif")
    assert name.endswith(".avif")
    assert (tmp_path / name).read_bytes() == b"avif-payload"

    name_with_params = store.save(b"avif-payload-2", "IMAGE/AVIF; charset=utf-8")
    assert name_with_params.endswith(".avif")


@pytest.mark.parametrize(
    "raw_ip",
    [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "::1",
        "::",
        "fc00::1",
        "fd12:3456:789a::1",
        "fe80::1",
        "fe80::1%eth0",
        "2606:4700:4700::1111%eth0",
        "ff02::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "::ffff:100.64.0.1",
        "64:ff9b::127.0.0.1",
        "64:ff9b::8.8.8.8",
        "64:ff9b:1::127.0.0.1",
        "64:ff9b:1::8.8.8.8",
        "2002:7f00:0001::",
        "2002:0808:0808::",
        "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        "2001::",
        "::127.0.0.1",
        "::10.0.0.1",
        "::8.8.8.8",
        "fec0::1",
    ],
)
def test_is_public_ip_blocks_special_and_transition_ipv6(raw_ip: str) -> None:
    ip = ipaddress.ip_address(raw_ip)
    assert _is_public_ip(ip) is False


@pytest.mark.parametrize(
    ("raw_ip", "expected"),
    [
        ("8.8.8.8", "8.8.8.8"),
        ("93.184.216.34", "93.184.216.34"),
        ("2606:4700:4700::1111", "2606:4700:4700::1111"),
        ("::ffff:93.184.216.34", "93.184.216.34"),
    ],
)
async def test_resolve_public_ips_allows_public_and_unwraps_v4_mapped(
    raw_ip: str, expected: str
) -> None:
    ip = ipaddress.ip_address(raw_ip)
    assert _is_public_ip(ip) is True
    assert await _resolve_public_ips(raw_ip) == [expected]


async def test_resolve_public_ips_rejects_mixed_public_and_private_records(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def fake_getaddrinfo(host, port, **kwargs):
        return [
            (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.216.34", 0)),
            (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("127.0.0.1", 0)),
        ]

    monkeypatch.setattr(asyncio.get_running_loop(), "getaddrinfo", fake_getaddrinfo)
    with pytest.raises(InvalidRequest, match="public IP"):
        await _assert_public_host("dual.example")


async def test_assert_public_url_rejects_bad_port_scheme_and_zone_id() -> None:
    with pytest.raises(InvalidRequest, match="public http\\(s\\) URL"):
        _assert_public_url("ftp://93.184.216.34/img.png")
    with pytest.raises(InvalidRequest, match="public http\\(s\\) URL"):
        _assert_public_url("https://example.com:99999/img.png")
    with pytest.raises(InvalidRequest, match="public IP"):
        await _resolve_public_ips("2606:4700:4700::1111%25eth0")


async def test_fetch_public_image_pins_ipv4_and_prevents_dns_rebinding(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    dns_calls = 0
    connect_calls: list[tuple[str, int]] = []
    sni_hosts: list[str | None] = []
    sent_requests: list[bytes] = []

    async def fake_getaddrinfo(host, port, **kwargs):
        nonlocal dns_calls
        dns_calls += 1
        if dns_calls == 1:
            return [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.216.34", 0))]
        return [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("127.0.0.1", 0))]

    async def fake_connect_tcp(remote_host, remote_port, **kwargs):
        connect_calls.append((str(remote_host), int(remote_port)))
        resp = _http_response(
            200,
            "OK",
            {"Content-Type": "image/png", "Content-Length": "4"},
            b"\x89PNG",
        )
        return _FakeByteStream([resp], sent_requests)

    async def fake_tls_wrap(stream, *, ssl_context=None, hostname=None, **kwargs):
        sni_hosts.append(hostname)
        return stream

    monkeypatch.setattr(asyncio.get_running_loop(), "getaddrinfo", fake_getaddrinfo)
    monkeypatch.setattr(anyio, "connect_tcp", fake_connect_tcp)
    monkeypatch.setattr(anyio.streams.tls.TLSStream, "wrap", fake_tls_wrap)

    data, mime = await load_image_ref("https://example.com/a.png")
    assert data == b"\x89PNG"
    assert mime == "image/png"
    assert dns_calls == 1
    assert connect_calls == [("93.184.216.34", 443)]
    assert sni_hosts == ["example.com"]
    raw_req = b"".join(sent_requests).decode("ascii")
    assert "host: example.com\r\n" in raw_req.lower()


async def test_fetch_public_image_pins_ipv6_with_port_host_and_sni(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    dns_calls = 0
    connect_calls: list[tuple[str, int]] = []
    sni_hosts: list[str | None] = []
    sent_requests: list[bytes] = []

    async def fake_getaddrinfo(host, port, **kwargs):
        nonlocal dns_calls
        dns_calls += 1
        return [(socket.AF_INET6, socket.SOCK_STREAM, 6, "", ("2606:4700:4700::1111", 0, 0, 0))]

    async def fake_connect_tcp(remote_host, remote_port, **kwargs):
        connect_calls.append((str(remote_host), int(remote_port)))
        resp = _http_response(
            200,
            "OK",
            {"Content-Type": "image/avif; charset=utf-8", "Content-Length": "4"},
            b"AVIF",
        )
        return _FakeByteStream([resp], sent_requests)

    async def fake_tls_wrap(stream, *, ssl_context=None, hostname=None, **kwargs):
        sni_hosts.append(hostname)
        return stream

    monkeypatch.setattr(asyncio.get_running_loop(), "getaddrinfo", fake_getaddrinfo)
    monkeypatch.setattr(anyio, "connect_tcp", fake_connect_tcp)
    monkeypatch.setattr(anyio.streams.tls.TLSStream, "wrap", fake_tls_wrap)

    data, mime = await load_image_ref("https://v6.example:8443/pic.avif")
    assert data == b"AVIF"
    assert mime == "image/avif"
    assert dns_calls == 1
    assert connect_calls == [("2606:4700:4700::1111", 8443)]
    assert sni_hosts == ["v6.example"]
    raw_req = b"".join(sent_requests).decode("ascii")
    assert "host: v6.example:8443\r\n" in raw_req.lower()


async def test_fetch_public_image_relative_redirect_catches_rebound_dns(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    dns_calls = 0
    connect_calls: list[tuple[str, int]] = []
    sent_requests: list[bytes] = []

    async def fake_getaddrinfo(host, port, **kwargs):
        nonlocal dns_calls
        dns_calls += 1
        if dns_calls == 1:
            return [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.216.34", 0))]
        return [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("127.0.0.1", 0))]

    async def fake_connect_tcp(remote_host, remote_port, **kwargs):
        connect_calls.append((str(remote_host), int(remote_port)))
        resp = _http_response(
            302,
            "Found",
            {"Location": "/step2", "Content-Length": "0"},
        )
        return _FakeByteStream([resp], sent_requests)

    async def fake_tls_wrap(stream, *, ssl_context=None, hostname=None, **kwargs):
        return stream

    monkeypatch.setattr(asyncio.get_running_loop(), "getaddrinfo", fake_getaddrinfo)
    monkeypatch.setattr(anyio, "connect_tcp", fake_connect_tcp)
    monkeypatch.setattr(anyio.streams.tls.TLSStream, "wrap", fake_tls_wrap)

    with pytest.raises(InvalidRequest, match="public IP"):
        await load_image_ref("https://good.example/step1")
    assert dns_calls == 2
    assert len(connect_calls) == 1


async def test_fetch_public_image_blocks_redirect_to_metadata_and_redirect_loop(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    connect_calls: list[tuple[str, int]] = []

    async def fake_getaddrinfo(host, port, **kwargs):
        return [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.216.34", 0))]

    async def fake_connect_tcp_meta(remote_host, remote_port, **kwargs):
        connect_calls.append((str(remote_host), int(remote_port)))
        resp = _http_response(
            302,
            "Found",
            {"Location": "http://169.254.169.254/latest/meta-data", "Content-Length": "0"},
        )
        return _FakeByteStream([resp], [])

    monkeypatch.setattr(asyncio.get_running_loop(), "getaddrinfo", fake_getaddrinfo)
    monkeypatch.setattr(anyio, "connect_tcp", fake_connect_tcp_meta)

    with pytest.raises(InvalidRequest, match="public IP"):
        await load_image_ref("http://good.example/start")
    assert len(connect_calls) == 1

    connect_calls.clear()

    async def fake_connect_tcp_loop(remote_host, remote_port, **kwargs):
        connect_calls.append((str(remote_host), int(remote_port)))
        resp = _http_response(
            302,
            "Found",
            {"Location": f"/hop{len(connect_calls)}", "Content-Length": "0"},
        )
        return _FakeByteStream([resp], [])

    monkeypatch.setattr(anyio, "connect_tcp", fake_connect_tcp_loop)

    with pytest.raises(InvalidRequest, match="redirect limit"):
        await load_image_ref("http://good.example/start")
    assert len(connect_calls) == _MAX_REDIRECTS + 1


async def test_fetch_public_image_rejects_oversized_content_length_and_streaming_body(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def fake_getaddrinfo(host, port, **kwargs):
        return [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.216.34", 0))]

    class _FailIfBodyReadStream(_FakeByteStream):
        def __init__(self, responses: list[bytes]) -> None:
            super().__init__(responses, [])
            self._reads = 0

        async def receive(self, max_bytes: int = 65536) -> bytes:
            self._reads += 1
            if self._reads > 1:
                raise AssertionError("body must not be streamed when Content-Length exceeds cap")
            return await super().receive(max_bytes)

    async def fake_connect_tcp_cl(remote_host, remote_port, **kwargs):
        # Send header only in first read; second read raises if streaming attempts to read body
        resp = _http_response(
            200,
            "OK",
            {"Content-Type": "image/png", "Content-Length": str(_MAX_FETCH_BYTES + 1)},
            b"",
        )
        return _FailIfBodyReadStream([resp])

    monkeypatch.setattr(asyncio.get_running_loop(), "getaddrinfo", fake_getaddrinfo)
    monkeypatch.setattr(anyio, "connect_tcp", fake_connect_tcp_cl)

    with pytest.raises(InvalidRequest, match="exceeds 20 MiB"):
        await load_image_ref("http://good.example/huge.png")

    async def fake_connect_tcp_stream(remote_host, remote_port, **kwargs):
        header = (
            b"HTTP/1.1 200 OK\r\n"
            b"Content-Type: image/png\r\n"
            b"Connection: close\r\n\r\n"
        )
        chunk = b"x" * (1024 * 1024)
        chunks = [header] + [chunk] * 21
        return _FakeByteStream(chunks, [])

    monkeypatch.setattr(anyio, "connect_tcp", fake_connect_tcp_stream)
    with pytest.raises(InvalidRequest, match="exceeds 20 MiB"):
        await load_image_ref("http://good.example/stream-huge.png")


async def test_load_image_ref_base64_precheck_blocks_before_b64decode(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    spy = MagicMock(side_effect=AssertionError("base64.b64decode must not be called"))
    monkeypatch.setattr(media.base64, "b64decode", spy)

    oversized_cases = [
        "data:image/png;base64," + ("A" * (_MAX_B64_LEN + 4)),
        "data:image/png;base64," + ("A" * _MAX_B64_LEN),  # 20_971_521 decoded bytes (no '=')
        "A" * (_MAX_B64_LEN + 4),
        "data:image/png;base64," + (" " * (_MAX_B64_LEN * 2 + 10)) + "AAAA",
        "AAAA" + (" " * (_MAX_B64_LEN * 2 + 10)) + "AAAA",
    ]
    for case in oversized_cases:
        with pytest.raises(InvalidRequest, match="exceeds 20 MiB"):
            await load_image_ref(case)
        assert spy.call_count == 0

    with pytest.raises(InvalidRequest, match="invalid base64 image data"):
        await load_image_ref("data:image/png;base64")
    assert spy.call_count == 0


async def test_load_image_ref_valid_base64_and_exact_limit_boundary() -> None:
    payload = base64.b64encode(b"avif-bytes").decode("ascii")
    raw, mime = await load_image_ref(f"data:image/avif;base64,{payload}")
    assert raw == b"avif-bytes"
    assert mime == "image/avif"

    raw_bare, mime_bare = await load_image_ref(payload)
    assert raw_bare == b"avif-bytes"
    assert mime_bare == "image/png"

    exact_limit_b64 = base64.b64encode(b"z" * _MAX_FETCH_BYTES).decode("ascii")
    assert len(exact_limit_b64) == _MAX_B64_LEN
    assert exact_limit_b64.endswith("=")
    raw_max, _ = await load_image_ref(f"data:image/png;base64,{exact_limit_b64}")
    assert len(raw_max) == _MAX_FETCH_BYTES


def _fake_request_with_files(files: list[UploadFile], prompt: str = "draw") -> MagicMock:
    form = FormData([("prompt", prompt), *[("image", f) for f in files]])
    req = MagicMock()
    req.form = AsyncMock(return_value=form)
    req.state = SimpleNamespace()
    req.base_url = "http://localhost:8000/"
    req.headers = {}
    return req


def _fake_services() -> MagicMock:
    svc = MagicMock()
    svc.settings.image_timeout = 30.0
    svc.gateway.generate_image = AsyncMock(
        return_value=[MediaResult(data=b"out", mime="image/png", kind="image", revised_prompt="ok")]
    )
    svc.media.save = MagicMock(return_value="img_abc.png")
    return svc


async def test_edit_images_rejects_excess_file_count_and_oversized_before_reading() -> None:
    too_many = []
    for i in range(MAX_REFERENCE_IMAGES + 1):
        uf = UploadFile(filename=f"f{i}.png", file=MagicMock(), headers=Headers({"content-type": "image/png"}))
        uf.read = AsyncMock(return_value=b"png")  # type: ignore[method-assign]
        too_many.append(uf)

    with pytest.raises(InvalidRequest, match="at most 4"):
        await edit_images(_fake_request_with_files(too_many), _fake_services())
    for uf in too_many:
        assert uf.read.await_count == 0

    oversized = UploadFile(
        filename="big.png",
        file=MagicMock(),
        size=_MAX_FETCH_BYTES + 1,
        headers=Headers({"content-type": "image/png"}),
    )
    oversized.read = AsyncMock(return_value=b"png")  # type: ignore[method-assign]

    with pytest.raises(InvalidRequest, match="exceeds 20 MiB"):
        await edit_images(_fake_request_with_files([oversized]), _fake_services())
    assert oversized.read.await_count == 0


async def test_edit_images_fallback_read_cap_when_size_is_none() -> None:
    unknown_oversized = UploadFile(
        filename="stream.png",
        file=MagicMock(),
        size=None,
        headers=Headers({"content-type": "image/png"}),
    )
    unknown_oversized.read = AsyncMock(return_value=b"x" * (_MAX_FETCH_BYTES + 1))  # type: ignore[method-assign]

    with pytest.raises(InvalidRequest, match="exceeds 20 MiB"):
        await edit_images(_fake_request_with_files([unknown_oversized]), _fake_services())
    unknown_oversized.read.assert_awaited_once_with(_MAX_FETCH_BYTES + 1)

    valid_file = UploadFile(
        filename="ok.avif",
        file=MagicMock(),
        size=4,
        headers=Headers({"content-type": "image/avif"}),
    )
    valid_file.read = AsyncMock(return_value=b"AVIF")  # type: ignore[method-assign]
    svc = _fake_services()
    res = await edit_images(_fake_request_with_files([valid_file]), svc)
    assert len(res["data"]) == 1
    sent_req = svc.gateway.generate_image.await_args.args[0]
    assert sent_req.reference_images[0].mime == "image/avif"
    assert sent_req.reference_images[0].data == b"AVIF"


async def test_generate_images_rejects_excess_refs_before_fetching(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    loader_spy = AsyncMock(side_effect=AssertionError("load_image_ref must not be called"))
    monkeypatch.setattr("muse2api.api.routes.images.load_image_ref", loader_spy)

    body = ImageGenerationRequest(
        prompt="hello",
        image=["https://example.com/1.png"] * (MAX_REFERENCE_IMAGES + 1),
    )
    with pytest.raises(InvalidRequest, match="at most 4"):
        await generate_images(body, _fake_request_with_files([]), _fake_services())
    assert loader_spy.await_count == 0
