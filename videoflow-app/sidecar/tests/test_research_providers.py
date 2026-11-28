
import pytest

from muse2api.services import providers


class FakeResponse:
    def __init__(self, status_code, text):
        self.status_code = status_code
        self.text = text

    async def aiter_bytes(self):
        yield self.text.encode()


class FakeAsyncClient:
    response = None
    calls = []

    def __init__(self, **kwargs):
        self.kwargs = kwargs

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        return False

    class Stream:
        def __init__(self, response): self.response = response
        async def __aenter__(self): return self.response
        async def __aexit__(self, *args): return False

    def stream(self, method, url, *, headers, json):
        assert method == "POST"
        self.calls.append((url, headers, json, self.kwargs))
        return self.Stream(self.response)


@pytest.mark.asyncio
async def test_gateway_text_returns_raw_receipt_without_key_echo(monkeypatch):
    FakeAsyncClient.calls = []
    FakeAsyncClient.response = FakeResponse(200, '{"choices":[{"message":{"content":"VFCTX raw"}}]}')
    monkeypatch.setattr(providers.httpx, "AsyncClient", FakeAsyncClient)
    key = "private-test-key"
    result = await providers.run_gateway_text(
        {"base_url": "https://gateway.example/v1/", "model": "test-model", "api_key": key},
        "prompt", 30)
    assert result == {"http_status": 200, "raw_http": FakeAsyncClient.response.text,
                      "content": "VFCTX raw", "complete": True, "error": None}
    url, headers, body, kwargs = FakeAsyncClient.calls[0]
    assert url == "https://gateway.example/v1/chat/completions"
    assert headers["Authorization"] == f"Bearer {key}"
    assert body["stream"] is False and body["model"] == "test-model"
    assert key not in repr(result) and kwargs["trust_env"] is False


@pytest.mark.asyncio
async def test_gateway_error_keeps_raw_body_and_rejects_embedded_credentials(monkeypatch):
    FakeAsyncClient.response = FakeResponse(401, '{"error":"bad key"}')
    monkeypatch.setattr(providers.httpx, "AsyncClient", FakeAsyncClient)
    result = await providers.run_gateway_text(
        {"base_url": "https://gateway.example/v1", "model": "test", "api_key": "secret"}, "p")
    assert result["http_status"] == 401 and result["raw_http"] == '{"error":"bad key"}'
    assert result["complete"] is False and result["content"] is None
    with pytest.raises(ValueError, match="without embedded credentials"):
        await providers.run_gateway_text(
            {"base_url": "https://user:pass@gateway.example/v1", "model": "m", "api_key": "k"}, "p")


@pytest.mark.asyncio
async def test_gateway_response_body_has_a_hard_size_limit(monkeypatch):
    FakeAsyncClient.response = FakeResponse(200, "x" * (2 * 1024 * 1024 + 1))
    monkeypatch.setattr(providers.httpx, "AsyncClient", FakeAsyncClient)
    result = await providers.run_gateway_text(
        {"base_url": "https://gateway.example/v1", "model": "test", "api_key": "secret"}, "p")
    assert result["complete"] is False
    assert result["error"]["code"] == "gateway_response_too_large"
    assert len(result["raw_http"]) == 2 * 1024 * 1024 + 1


def test_gateway_url_appends_chat_completions_once():
    assert providers._completion_url("https://gateway.example/custom") == "https://gateway.example/custom/chat/completions"
    assert providers._completion_url("https://gateway.example/custom/chat/completions") == "https://gateway.example/custom/chat/completions"
