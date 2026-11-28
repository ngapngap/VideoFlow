import pytest

from muse2api.services.research_prompts import build_prompt

CARD = {"visual": "navy tee", "print_side": "front", "print_text": "A\nB", "formula": "notice then respond"}
CASES = [{"ordinal": i, "image_prompt": f"opening {i}", "video_prompt": f"response {i}",
          "payoff": "UNVERIFIED-METADATA"} for i in range(1, 4)]


def test_generate_uses_only_approved_card_and_keeps_binary_separate():
    images = [{"mime": "image/png", "data_b64": "BINARY_CANARY"}]
    result = build_prompt("GENERATE", "op", {"card": CARD, "images": images, "n": 3,
                                            "api_key": "KEY_CANARY"})
    assert result["images"] == images
    assert "BINARY_CANARY" not in result["prompt"] and "KEY_CANARY" not in result["prompt"]
    assert result["prompt"].count("VF1 CASE ") == 3
    assert "VFCTX END op GENERATE" in result["prompt"]


def test_recheck_projects_only_actual_pairs_and_locked_ids():
    result = build_prompt("CHECK", "op", {"check_input": {
        "card": CARD, "candidates": CASES, "locked_ordinals": [1],
        "previous_verdict": "BAD_HINT"}})["prompt"]
    assert "UNVERIFIED-METADATA" not in result and "BAD_HINT" not in result
    assert '"locked_ordinals":[1]' in result
    assert all(f"RECORD {i}\nVERDICT\nCODE\nEVIDENCE\nCHANGE" in result for i in [1, 2, 3])


def test_repair_counts_local_cases_and_preserves_mapping():
    result = build_prompt("REPAIR", "op", {"card": CARD, "failed_ordinals": [2, 3],
                                          "locked_cases": CASES[:1], "failures": CASES[1:]})
    assert '"failed_ordinals":[2,3]' in result["prompt"]
    assert result["prompt"].count("VF1 CASE ") == 2
    with pytest.raises(ValueError):
        build_prompt("GENERATE", "op", {"card": CARD, "n": 1})
