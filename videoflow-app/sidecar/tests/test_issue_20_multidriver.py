from __future__ import annotations

import asyncio
import contextlib
import re
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from httpx import ASGITransport, AsyncClient
from websockets.exceptions import ConnectionClosedOK
from websockets.frames import Close

from muse2api.accounts.model import Account
from muse2api.accounts.pool import AccountPool
from muse2api.accounts.store import JsonAccountStore
from muse2api.app import create_app
from muse2api.config import Settings
from muse2api.drivers.base import ChatRequest
from muse2api.drivers.browser.cdp import CDPError, CDPSession
from muse2api.drivers.browser.chromium import _build_wmi_filter
from muse2api.drivers.browser.driver import BrowserDriver
from muse2api.services.gateway import Gateway


@pytest.mark.asyncio
async def test_cdp_session_send_wraps_websocket_and_timeout_errors():
    class FakeWS:
        def __init__(self, fail_send: bool = False):
            self.fail_send = fail_send
            self.gate = asyncio.Event()

        def __aiter__(self):
            return self

        async def __anext__(self):
            await self.gate.wait()
            raise StopAsyncIteration

        async def send(self, _data: str) -> None:
            if self.fail_send:
                raise ConnectionClosedOK(Close(1000, "bye"), None)

        async def close(self) -> None:
            self.gate.set()

    # 1. WebSocketException on send -> CDPError & _pending cleaned
    s1 = CDPSession(FakeWS(fail_send=True))  # type: ignore[arg-type]
    with pytest.raises(CDPError, match="cdp send failed"):
        await s1.send("Page.enable", timeout=1.0)
    assert len(s1._pending) == 0
    await s1.close()

    # 2. TimeoutError waiting for response -> CDPError & _pending cleaned
    s2 = CDPSession(FakeWS(fail_send=False))  # type: ignore[arg-type]
    with pytest.raises(CDPError, match="cdp Page.enable failed"):
        await s2.send("Page.enable", timeout=0.05)
    assert len(s2._pending) == 0
    await s2.close()


def test_wmi_filter_boundary_acc_1_does_not_match_acc_10():
    p1 = Path(r"C:\Users\u\AppData\Roaming\VideoFlowDesktop\muse\profiles\acc_1")
    flt = _build_wmi_filter(p1, port=19211)
    assert str(p1) in flt
    assert "[regex]::Escape(" in flt
    pattern = r'--user-data-dir="?' + re.escape(str(p1)) + r'("|\s|$)'
    cmd_acc1 = r'msedge.exe --user-data-dir="C:\Users\u\AppData\Roaming\VideoFlowDesktop\muse\profiles\acc_1" --headless=new'
    cmd_acc10 = r'msedge.exe --user-data-dir="C:\Users\u\AppData\Roaming\VideoFlowDesktop\muse\profiles\acc_10" --headless=new'
    assert re.search(pattern, cmd_acc1, re.IGNORECASE) is not None
    assert re.search(pattern, cmd_acc10, re.IGNORECASE) is None


@pytest.mark.asyncio
async def test_delete_account_releases_sub_driver(tmp_path: Path):
    settings = Settings(data_dir=tmp_path / "data", api_key="test-key", driver="mock")
    app = create_app(settings)
    async with app.router.lifespan_context(app):
        svc = app.state.services
        svc.driver.release_account_driver = AsyncMock()
        acc = Account(id="acc_del_1", label="del", cookies={"hatch_sess": "s", "hatch_native_auth_device": "d"})
        await svc.pool.upsert(acc)

        transport = ASGITransport(app=app)
        async with AsyncClient(transport=transport, base_url="http://test") as client:
            r = await client.delete(
                "/admin/accounts/acc_del_1",
                headers={"Authorization": "Bearer test-key"},
            )
            assert r.status_code == 200
        svc.driver.release_account_driver.assert_awaited_once_with("acc_del_1")


@pytest.mark.asyncio
async def test_stream_disconnect_closes_inner_generator_and_releases_tab(tmp_path: Path):
    settings = Settings(data_dir=tmp_path / "data")
    pool = AccountPool(JsonAccountStore(tmp_path / "accounts.json"))
    acc = Account(id="acc_stream", cookies={"hatch_sess": "s", "hatch_native_auth_device": "d"})
    await pool.upsert(acc)

    inner_closed = False

    class FakeDriver(BrowserDriver):
        async def chat_stream(self, account: Account, req: ChatRequest):
            nonlocal inner_closed
            try:
                yield "hello"
                await asyncio.sleep(30)
                yield "world"
            finally:
                inner_closed = True

    drv = FakeDriver(settings)
    gw = Gateway(pool, drv)
    stream = gw.chat_stream(ChatRequest(prompt="hi", model="muse-chat"))
    async with contextlib.aclosing(stream):
        first = await anext(stream)
        assert first == "hello"
    assert inner_closed is True
