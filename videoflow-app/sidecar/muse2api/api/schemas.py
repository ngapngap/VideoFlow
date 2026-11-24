"""OpenAI-compatible request schemas. Unknown fields are accepted and ignored so that
clients sending newer parameters keep working."""

from __future__ import annotations

import ipaddress
from typing import Any, Literal
from urllib.parse import unquote, urlsplit

from pydantic import BaseModel, ConfigDict, Field, field_validator

_ALLOWED_PROXY_SCHEMES = frozenset({"http", "https", "socks5", "socks5h"})
_MAX_PROXY_URL_LEN = 2048


def _has_control_char(text: str) -> bool:
    return any(
        ord(c) < 0x20 or ord(c) == 0x7F or 0x80 <= ord(c) <= 0x9F
        for c in text
    )


def _validate_proxy_url(value: str | None) -> str | None:
    if value is None:
        return None
    if _has_control_char(value):
        raise ValueError("proxy URL must not contain control characters")
    cleaned = value.strip()
    if not cleaned:
        return ""
    if len(cleaned) > _MAX_PROXY_URL_LEN:
        raise ValueError(f"proxy URL exceeds maximum length of {_MAX_PROXY_URL_LEN}")
    if any(c.isspace() for c in cleaned):
        raise ValueError("proxy URL must not contain whitespace")
    try:
        parsed = urlsplit(cleaned)
        port = parsed.port
    except ValueError as exc:
        raise ValueError(f"invalid proxy URL: {exc}") from exc

    scheme = (parsed.scheme or "").lower()
    host = parsed.hostname
    if scheme not in _ALLOWED_PROXY_SCHEMES or not host:
        raise ValueError(
            "proxy must be a valid URL with scheme http/https/socks5/socks5h and a host"
        )
    if port is not None and not (1 <= port <= 65535):
        raise ValueError("proxy port must be between 1 and 65535")

    authority = parsed.netloc.rpartition("@")[2]
    if authority.endswith(":"):
        raise ValueError("proxy URL has a trailing ':' without a port")
    if authority.startswith("["):
        end_bracket = authority.find("]")
        if end_bracket == -1:
            raise ValueError("invalid IPv6 bracketed host in proxy URL")
        suffix = authority[end_bracket + 1 :]
        if suffix and not (suffix.startswith(":") and suffix[1:].isdigit()):
            raise ValueError("invalid characters after IPv6 bracket in proxy URL")
        ipv6_text = host.split("%", 1)[0]
        try:
            ip_obj = ipaddress.ip_address(ipv6_text)
        except ValueError as exc:
            raise ValueError(f"invalid IPv6 address in proxy URL: {exc}") from exc
        if ip_obj.version != 6:
            raise ValueError("bracketed proxy host must be a valid IPv6 address")
    else:
        if "[" in authority or "]" in authority or ":" in host:
            raise ValueError("IPv6 proxy host must be enclosed in brackets [...]")
        if len(host) > 253 or any(ch in host for ch in (",", ";", "?", "#", "\\")):
            raise ValueError("invalid hostname in proxy URL")

    if parsed.path not in ("", "/") or parsed.query or parsed.fragment:
        raise ValueError("proxy URL must not include a path, query string, or fragment")
    if scheme in ("socks5", "socks5h") and (
        parsed.username is not None or parsed.password is not None
    ):
        user_b = unquote(parsed.username or "").encode("utf-8")
        pass_b = unquote(parsed.password or "").encode("utf-8")
        if len(user_b) > 255 or len(pass_b) > 255:
            raise ValueError("SOCKS5 username/password must not exceed 255 bytes")
    return cleaned


class _Lenient(BaseModel):
    model_config = ConfigDict(extra="allow")


class ChatMessage(_Lenient):
    role: str
    content: str | list[dict[str, Any]] | None = None
    name: str | None = None


class ChatCompletionRequest(_Lenient):
    model: str | None = None
    messages: list[ChatMessage] = Field(min_length=1)
    stream: bool = False
    stream_options: dict[str, Any] | None = None
    user: str | None = None
    # Accepted for compatibility; muse.ai does not expose sampling or tool controls.
    temperature: float | None = None
    top_p: float | None = None
    max_tokens: int | None = None
    tools: list[dict[str, Any]] | None = None
    tool_choice: Any = None


class ImageGenerationRequest(_Lenient):
    prompt: str = Field(min_length=1)
    model: str | None = None
    n: int = Field(default=1, ge=1, le=4)
    size: str | None = None
    response_format: Literal["url", "b64_json"] = "url"
    # Same values as OpenAI gpt-image-1. "transparent" cuts the subject out locally
    # and returns an RGBA PNG; "auto"/"opaque" leave the image as generated.
    background: Literal["auto", "opaque", "transparent"] | None = None
    image: str | list[str] | None = Field(
        default=None, description="Reference image(s) as data URL, http(s) URL or base64 (max 4)")
    # A sync call can outlive a proxy's timeout (Cloudflare cuts at ~100 s with 524), so
    # "async": true returns a task to poll at GET /v1/images/generations/{task_id}.
    async_: bool = Field(default=False, alias="async")
    user: str | None = None


class VideoCreateRequest(_Lenient):
    prompt: str = Field(min_length=1)
    model: str | None = None
    size: str | None = None
    duration: int | None = Field(default=None, ge=1, le=60)
    seconds: int | None = Field(default=None, ge=1, le=60)
    image: str | None = Field(default=None, description="First frame as data URL or http(s) URL")
    input_reference: str | None = None


class AccountCreate(BaseModel):
    label: str = ""
    # Three accepted formats:
    #   1. dict {"name": "value"}
    #   2. raw "Cookie" request header string "a=1; b=2"
    #   3. list of cookie-export entries (e.g. EditThisCookie):
    #      [{"name": ..., "value": ..., "expirationDate": ...}, ...]
    cookies: dict[str, str] | str | list[dict[str, Any]] = Field(default_factory=dict)
    cookie_expires: dict[str, int] = Field(default_factory=dict)
    enabled: bool = True
    proxy: str | None = None

    @field_validator("proxy")
    @classmethod
    def _check_proxy(cls, v: str | None) -> str | None:
        return _validate_proxy_url(v)


class AccountsImportRequest(BaseModel):
    """Bulk cookie import: every entry is validated like AccountCreate; entries that
    fail are reported individually without aborting the batch."""
    accounts: list[AccountCreate] = Field(min_length=1)


class AccountUpdate(BaseModel):
    label: str | None = None
    enabled: bool | None = None
    cookies: dict[str, str] | None = None
    cookie_expires: dict[str, int] | None = None
    proxy: str | None = None

    @field_validator("proxy")
    @classmethod
    def _check_proxy(cls, v: str | None) -> str | None:
        return _validate_proxy_url(v)


class KeyCreate(BaseModel):
    name: str = Field(min_length=1, max_length=100)
    note: str = Field(default="", max_length=500)


class KeyUpdate(BaseModel):
    name: str | None = Field(default=None, min_length=1, max_length=100)
    note: str | None = Field(default=None, max_length=500)
    revoked: bool | None = None
