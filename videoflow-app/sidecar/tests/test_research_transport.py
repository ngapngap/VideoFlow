import asyncio
from contextlib import asynccontextmanager
from types import SimpleNamespace

import pytest

from muse2api.services.transport import MuseResearchTransport


class Pool:
    @asynccontextmanager
    async def lease(self, *, hint=None):
        yield SimpleNamespace(account=SimpleNamespace(id="owner-test"))


class Driver:
    def __init__(self, *, wait_for_cancel=False):
        self.calls = 0
        self.started = asyncio.Event()
        self.wait_for_cancel = wait_for_cancel

    async def research_reply(self, account, **kwargs):
        self.calls += 1
        self.started.set()
        if self.wait_for_cancel:
            await kwargs["cancel"].wait()
            return {"status": "stopped", "http_status": None, "http_body": None,
                    "raw_ui_code_textcontent": None, "dom_turn": None}
        return {"status": "completed", "http_status": None, "http_body": None,
                "raw_ui_code_textcontent": "raw-code", "dom_turn": {"owner_id": account.id}}


def prompt(op, stage="SOURCE"):
    return f"ask\nVFCTX BEGIN {op} {stage}\nfields\nVFCTX END {op} {stage}"


@pytest.mark.asyncio
async def test_submit_is_idempotent_and_snapshot_excludes_transient_provider():
    driver = Driver()
    transport = MuseResearchTransport(None, Pool(), driver)
    request = {"prompt": prompt("op-1"), "timeout_s": 30,
               "_provider": {"api_key": "do-not-persist"}}
    first = await transport.submit_muse("op-1", "SOURCE", request)
    replay = await transport.submit_muse("op-1", "SOURCE", request)
    assert first["operation_id"] == replay["operation_id"] == "op-1"
    assert "do-not-persist" not in repr(first)
    await driver.started.wait()
    for _ in range(20):
        result = await transport.get_muse("op-1")
        if result["status"] == "completed":
            break
        await asyncio.sleep(0)
    assert result["raw_ui_code_textcontent"] == "raw-code"
    assert driver.calls == 1
    with pytest.raises(ValueError, match="payload conflict"):
        await transport.submit_muse("op-1", "SOURCE", {"prompt": prompt("op-1") + " changed"})
    await transport.shutdown()


@pytest.mark.asyncio
async def test_stop_before_submit_is_a_tombstone_and_active_stop_is_idempotent():
    driver = Driver(wait_for_cancel=True)
    transport = MuseResearchTransport(None, Pool(), driver)
    stopped = await transport.stop_muse("op-stopped-first")
    assert stopped["status"] == "stopped" and stopped["stop_requested"] is True
    replay = await transport.submit_muse("op-stopped-first", "SOURCE",
                                         {"prompt": prompt("op-stopped-first")})
    assert replay["status"] == "stopped" and driver.calls == 0

    await transport.submit_muse("op-running", "SOURCE", {"prompt": prompt("op-running")})
    await driver.started.wait()
    first_stop = await transport.stop_muse("op-running")
    second_stop = await transport.stop_muse("op-running")
    assert first_stop["stop_requested"] and second_stop["stop_requested"]
    for _ in range(20):
        result = await transport.get_muse("op-running")
        if result["status"] == "stopped":
            break
        await asyncio.sleep(0)
    assert result["status"] == "stopped" and driver.calls == 1
    await transport.shutdown()
