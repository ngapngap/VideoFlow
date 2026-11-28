import asyncio
import base64
import json
from io import BytesIO
from types import SimpleNamespace

import pytest
from PIL import Image

from muse2api.services.research_service import ResearchService


class WireReplay:
    def __init__(self):
        self.calls = []
        self.responses = {}

    async def submit_muse(self, op_id, stage, data):
        self.calls.append(stage)
        if stage == "SOURCE":
            body = "COUNT\n1\nTOTAL\n9\nRECORD 1\nURL\nhttps://www.facebook.com/reel/42/\nTHUMB\nhttps://example.org/test.png\nCAPTION\nOrder here https://example.org/tee\nCOVERAGE\nfull\nLIKES\n501\nVIEWS\n30001\nCOUNTRY\nUS\nMARKET_EVIDENCE\nUS delivery\nBUY_URL\nhttps://example.org/tee\nCTA\nOrder here\nIDEA\nA printed tee"
        elif stage == "CARD":
            assert len(data["images"]) == 1
            body = "TITLE\nTee\nCONTENT\nApproved concept\nVISUAL\nBlue tee\nPRINT_SIDE\nfront\nPRINT_TEXT\nTEST\nFORMULA\nNotice then respond"
        elif stage in {"GENERATE", "REPAIR"}:
            body = "VF1 BEGIN\nVF1 PRODUCT\nBlue tee\nVF1 BUYER\nWearer\nVF1 FORMULA\nNotice then respond"
            for i in range(1, 3 if stage == "GENERATE" else 2):
                body += f"\nVF1 CASE {i}"
                for label in ["TITLE", "CONFLICT", "REVEAL", "PAYOFF", "IMAGE", "VIDEO"]:
                    body += f"\nVF1 {label}\n{stage} {i} {label}"
            body += "\nVF1 END"
        else:
            assert "VF1 PAYOFF" not in data["prompt"]
            first = self.calls.count("CHECK") == 1
            body = "RECORD 1\nVERDICT\nPASS\nCODE\nOK\nEVIDENCE\nFrame 1\nCHANGE\n-\nRECORD 2\n"
            body += (
                "VERDICT\nFAIL\nCODE\nVISIBILITY\nEVIDENCE\nBlocked\nCHANGE\nClear print"
                if first
                else "VERDICT\nPASS\nCODE\nOK\nEVIDENCE\nVisible print\nCHANGE\n-"
            )
        content = f"VFCTX BEGIN {op_id} {stage}\n{body}\nVFCTX END {op_id} {stage}"
        self.responses[op_id] = {
            "status": "completed",
            "http_status": 200,
            "http_complete": True,
            "http_body": json.dumps({"choices": [{"message": {"content": content}}]}),
        }

    async def get_muse(self, op_id):
        return self.responses[op_id]


@pytest.mark.asyncio
async def test_research_from_search_through_repair_and_transfer(tmp_path, monkeypatch):
    replay = WireReplay()
    service = ResearchService(tmp_path, SimpleNamespace(research_transport=replay))
    pic = BytesIO()
    Image.new("RGB", (4, 4)).save(pic, format="PNG")

    async def image(*args):
        return pic.getvalue(), "image/png"

    monkeypatch.setattr(service, "_decode_asset", image)
    sid = service.search_start(
        {"request_id": "search", "topic": "printed shirts", "filters": {"country": "US"}}
    )["session_id"]
    await asyncio.gather(*service._jobs.values())
    session = service.search_get(sid)
    assert session["status"] == "completed", session
    rows = service.search_sources({"session_id": sid, "bucket": "qualified"})
    assert (
        rows["items"][0]["source_id"] == "42"
        and rows["total"] == 1
        and session["reported_total"] == 9
    )
    service.set_sources({"session_id": sid, "source_ids": [{"source_id": "42"}]})
    idea = service.prepare_ideas(
        {"session_id": sid, "request_id": "prepare", "source_ids": ["42"]}
    )["ideas"][0]
    await asyncio.gather(*service._jobs.values())
    idea = service.search_get(sid)["ideas"][0]
    assert idea["status"] == "ready", idea
    select = {"session_id": sid, "idea_id": idea["idea_id"], "revision": idea["revision"]}
    service.select_idea(select)
    bid = service.develop_start(
        {**select, "request_id": "start", "n": 2, "_provider": {"source": "internal"}}
    )["batch_id"]
    await asyncio.gather(*service._jobs.values())
    batch = service.batch_get({"batch_id": bid})["batch"]
    assert batch["status"] == "ready", batch
    assert replay.calls == ["SOURCE", "CARD", "GENERATE", "CHECK", "REPAIR", "CHECK"]
    transfer = {"batch_id": bid, "ordinals": [1, 2], "request_id": "transfer", "project": "demo"}
    payload = service.transfer_prepare(transfer)
    assert base64.b64decode(payload["image_b64"]) == pic.getvalue()
    assert [r["video_prompt"] for r in payload["rows"]] == ["GENERATE 1 VIDEO", "REPAIR 1 VIDEO"]
    receipt = service.transfer_commit({**transfer, "added": 2, "existing": 0})
    assert service.transfer_prepare(transfer) == receipt
