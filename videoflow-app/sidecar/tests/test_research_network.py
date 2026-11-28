import base64

import pytest

from muse2api.drivers.browser.research_network import ResearchHttpCapture


class FakeSession:
    async def send(self, method, params, timeout):
        assert method == "Network.getResponseBody"
        return {"body": base64.b64encode(b'{"choices":[]}').decode(), "base64Encoded": True}


@pytest.mark.asyncio
async def test_network_capture_requires_exact_muse_post_and_complete_response():
    capture = ResearchHttpCapture("op-1", "SOURCE")
    capture._request({"requestId": "bad-host", "request": {
        "url": "https://muse.ai.evil/api", "method": "POST", "postData": "VFCTX BEGIN op-1 SOURCE"}})
    capture._request({"requestId": "wrong-op", "request": {
        "url": "https://muse.ai/api", "method": "POST", "postData": "VFCTX BEGIN op-2 SOURCE"}})
    empty = await capture.snapshot(FakeSession())
    assert empty["http_available"] is False and empty["http_body"] is None

    capture._request({"requestId": "good", "request": {
        "url": "https://muse.ai/api/chat", "method": "POST",
        "postData": "VFCTX BEGIN op-1 SOURCE"}})
    capture._response({"requestId": "good", "response": {"status": 200}})
    incomplete = await capture.snapshot(FakeSession())
    assert incomplete["http_available"] is True and incomplete["http_complete"] is False
    capture._finished({"requestId": "good"})
    complete = await capture.snapshot(FakeSession())
    assert complete["http_status"] == 200 and complete["http_complete"] is True
    assert complete["http_body"] == '{"choices":[]}'


@pytest.mark.asyncio
async def test_ambiguous_matching_posts_are_not_selected():
    capture = ResearchHttpCapture("op-1", "CARD")
    for request_id in ("one", "two"):
        capture._request({"requestId": request_id, "request": {
            "url": "https://muse.ai/api", "method": "POST", "postData": "VFCTX BEGIN op-1 CARD"}})
    result = await capture.snapshot(FakeSession())
    assert result["http_ambiguous"] and not result["http_available"]
