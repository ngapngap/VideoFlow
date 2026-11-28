"""Execute and publish a frozen prompt batch, including one bounded blind repair."""

import base64
import hashlib
import json
import time

from ..errors import InvalidRequest
from ..research.check_input import check_input
from .research_store import _json


async def run_batch(service, bid, stage, provider, stop):
    with service.store.db() as db:
        batch = dict(db.execute("SELECT * FROM batches WHERE batch_id=?", (bid,)).fetchone())
        revision = dict(
            db.execute(
                "SELECT * FROM idea_revisions WHERE idea_id=? AND revision=?",
                (batch["idea_id"], batch["revision"]),
            ).fetchone()
        )
        variants = [
            dict(v)
            for v in db.execute("SELECT * FROM variants WHERE batch_id=? ORDER BY ordinal", (bid,))
        ]
    asset = service._image_path(revision["asset_hash"], revision["asset_mime"]).read_bytes()
    if hashlib.sha256(asset).hexdigest() != revision["source_image_hash"]:
        raise InvalidRequest("input_invalid: reference image changed")
    card = {
        **json.loads(revision["visual_brief_json"]),
        "content": revision["content"],
        "title": revision["title"],
    }
    card.setdefault("formula", card["content"])
    images = (
        []
        if provider["source"] == "gateway"
        else [{"data_b64": base64.b64encode(asset).decode("ascii"), "mime": revision["asset_mime"]}]
    )
    common = {
        "prompt_language": batch["prompt_language"],
        "constraints": json.loads(batch["constraints_json"]),
        "images": images,
    }
    ids = [v["ordinal"] for v in variants]
    versions = [v["edit_version"] for v in variants]

    def assert_current(db):
        current = db.execute("SELECT status FROM batches WHERE batch_id=?", (bid,)).fetchone()
        actual = [
            r[0]
            for r in db.execute(
                "SELECT edit_version FROM variants WHERE batch_id=? ORDER BY ordinal", (bid,)
            )
        ]
        if stop.is_set() or current["status"] == "stopped" or actual != versions:
            raise InvalidRequest("batch stopped or inputs changed")

    def transition(status):
        with service.store.db() as db:
            db.execute("BEGIN IMMEDIATE")
            assert_current(db)
            db.execute(
                "UPDATE batches SET status=?,seq=seq+1,updated_at=? WHERE batch_id=?",
                (status, time.time(), bid),
            )
            db.commit()

    async def check(locked=(), attempt=1):
        transition("checking")
        projection = check_input(card, variants, locked_ordinals=locked)
        result, _ = await service._call_stage(
            bid,
            "CHECK",
            {**common, "check_input": projection},
            provider,
            ordinals=ids,
            attempt=attempt,
        )
        return result

    if stage == "GENERATE":
        transition("submitting")
        generated, _ = await service._call_stage(
            bid, stage, {**common, "card": card, "n": len(ids)}, provider, count=len(ids)
        )
        variants = generated["variants"]
        brief = generated["brief"]
    else:
        brief = json.loads(batch["raw_generate"] or "{}")
    if stage == "REPAIR":
        checks = [json.loads(v["check_result_json"]) for v in variants]
    else:
        checks = await check()
    failed = [v["ordinal"] for v in checks if v["verdict"] == "FAIL"]
    repaired = bool(
        failed and (stage == "REPAIR" or (stage == "GENERATE" and not batch["repair_count"]))
    )
    if repaired:
        transition("repairing")
        problems = [{**variants[i - 1], **checks[i - 1]} for i in failed]
        locked = [i for i in ids if i not in failed]
        payload = {
            **common,
            "card": card,
            "brief": brief,
            "failed_ordinals": failed,
            "failures": problems,
            "locked_cases": check_input(card, variants)["candidates"],
        }
        payload["locked_cases"] = [r for r in payload["locked_cases"] if r["ordinal"] in locked]
        replacement, _ = await service._call_stage(
            bid, "REPAIR", payload, provider, count=len(failed)
        )
        for original, patch in zip(failed, replacement["variants"], strict=True):
            variants[original - 1] = {**patch, "ordinal": original}
        checks = await check(locked, attempt=2)
    passed = sum(c["verdict"] == "PASS" for c in checks)
    status = "ready" if passed == len(ids) else "partial" if passed else "failed"
    with service.store.db() as db:
        db.execute("BEGIN IMMEDIATE")
        assert_current(db)
        for row, verdict in zip(variants, checks, strict=True):
            db.execute(
                """UPDATE variants SET title=?,conflict=?,reveal=?,payoff=?,image_prompt=?,video_prompt=?,
                       status=?,check_result_json=?,attempt=? WHERE batch_id=? AND ordinal=?""",
                (
                    *[
                        row[k]
                        for k in (
                            "title",
                            "conflict",
                            "reveal",
                            "payoff",
                            "image_prompt",
                            "video_prompt",
                        )
                    ],
                    verdict["verdict"].lower(),
                    _json(verdict),
                    2 if repaired and row["ordinal"] in failed else 1,
                    bid,
                    row["ordinal"],
                ),
            )
        db.execute(
            """UPDATE batches SET status=?,raw_generate=?,raw_check=?,repair_count=MAX(repair_count,?),
                   error_json=NULL,seq=seq+1,updated_at=? WHERE batch_id=?""",
            (status, _json(brief), _json(checks), int(repaired), time.time(), bid),
        )
        db.commit()
