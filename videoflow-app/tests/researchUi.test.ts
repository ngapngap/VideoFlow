import { describe, it } from "node:test";
import assert from "node:assert/strict";
import type { ResearchIdea, ResearchVariant } from "../src/lib/researchApi.js";
import {
  ideaReady,
  parseOptionalThreshold,
  safeHttpUrl,
  sourceDisplayValue,
  variantReadyForTransfer,
} from "../src/lib/researchUi.js";

describe("Reels research UI rules", () => {
  it("parses blank and non-negative integer filter thresholds", () => {
    assert.deepEqual(parseOptionalThreshold(""), { value: null, error: null });
    assert.deepEqual(parseOptionalThreshold(" 500 "), { value: 500, error: null });
    assert.match(parseOptionalThreshold("1.5").error ?? "", /số nguyên/);
    assert.match(parseOptionalThreshold("2147483648").error ?? "", /2.147.483.647/);
  });

  it("requires both ready status and committed content/image for a selectable idea", () => {
    const ready: ResearchIdea = {
      idea_id: "i1", source_id: "s1", revision: 1, status: "ready", asset: "image.png",
      title: "Idea", content: "Text", visual_brief: null, error: null,
    };
    assert.equal(ideaReady(ready), true);
    assert.equal(ideaReady({ ...ready, status: "running" }), false);
    assert.equal(ideaReady({ ...ready, asset: null }), false);
    assert.equal(ideaReady({ ...ready, content: " " }), false);
  });

  it("allows transfer only for READY variants with a passing CHECK", () => {
    const ready: ResearchVariant = {
      ordinal: 1, title: "T", conflict: "C", reveal: "R", payoff: "P",
      image_prompt: "B", video_prompt: "C", status: "ready",
      check_result: { verdict: "PASS", code: "OK" }, attempt: 1,
    };
    assert.equal(variantReadyForTransfer(ready), true);
    assert.equal(variantReadyForTransfer({ ...ready, check_result: { verdict: "FAIL" } }), false);
    assert.equal(variantReadyForTransfer({ ...ready, status: "failed" }), false);
  });

  it("only opens checked HTTP(S) source links and preserves missing values", () => {
    assert.equal(safeHttpUrl("https://example.org/reel"), "https://example.org/reel");
    assert.equal(safeHttpUrl("javascript:alert(1)"), null);
    assert.equal(sourceDisplayValue(null), "—");
    assert.equal(sourceDisplayValue("line 1\nline 2"), "line 1\nline 2");
  });
});
