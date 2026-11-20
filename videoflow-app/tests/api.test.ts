import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { toAssetUrl } from "../src/lib/api.js";

function withMockConvertFileSrc(
  impl: (filePath: string, protocol?: string) => string,
  fn: (calls: string[]) => void
): void {
  const g = globalThis as { window?: unknown };
  const hadWindow = Object.prototype.hasOwnProperty.call(g, "window");
  const prevWindow = g.window;
  const calls: string[] = [];
  g.window = {
    __TAURI_INTERNALS__: {
      convertFileSrc: (filePath: string, protocol?: string) => {
        calls.push(filePath);
        return impl(filePath, protocol);
      },
    },
  };
  try {
    fn(calls);
  } finally {
    if (hadWindow) {
      g.window = prevWindow;
    } else {
      delete g.window;
    }
  }
}

describe("toAssetUrl", () => {
  it("returns empty string for null, undefined, empty, or whitespace-only paths", () => {
    withMockConvertFileSrc(
      (p) => `https://asset.localhost/${p}`,
      (calls) => {
        assert.equal(toAssetUrl(null), "");
        assert.equal(toAssetUrl(undefined), "");
        assert.equal(toAssetUrl(""), "");
        assert.equal(toAssetUrl("   \t\n "), "");
        assert.equal(calls.length, 0);
      }
    );
  });

  it("normalizes Windows extended-length drive and UNC prefixes before calling convertFileSrc", () => {
    withMockConvertFileSrc(
      (p) => `https://asset.localhost/${encodeURIComponent(p)}`,
      (calls) => {
        const driveUrl = toAssetUrl("  \\\\?\\E:\\VideoFlow\\output\\shot.mp4  ");
        assert.equal(
          driveUrl,
          `https://asset.localhost/${encodeURIComponent("E:\\VideoFlow\\output\\shot.mp4")}`
        );

        const uncUpper = toAssetUrl("\\\\?\\UNC\\nas01\\media\\render.png");
        assert.equal(
          uncUpper,
          `https://asset.localhost/${encodeURIComponent("\\\\nas01\\media\\render.png")}`
        );

        const uncLower = toAssetUrl("\\\\?\\unc\\nas01\\media\\render2.png");
        assert.equal(
          uncLower,
          `https://asset.localhost/${encodeURIComponent("\\\\nas01\\media\\render2.png")}`
        );

        const standard = toAssetUrl("C:\\Users\\demo\\img.png");
        assert.equal(
          standard,
          `https://asset.localhost/${encodeURIComponent("C:\\Users\\demo\\img.png")}`
        );

        assert.deepEqual(calls, [
          "E:\\VideoFlow\\output\\shot.mp4",
          "\\\\nas01\\media\\render.png",
          "\\\\nas01\\media\\render2.png",
          "C:\\Users\\demo\\img.png",
        ]);
      }
    );
  });

  it("appends encoded cache-busting query parameters safely with ? or &", () => {
    withMockConvertFileSrc(
      (p) => (p.includes("withquery") ? `https://asset.localhost/${p}?token=1` : `https://asset.localhost/${p}`),
      () => {
        assert.equal(toAssetUrl("C:\\a.png", 12345), "https://asset.localhost/C:\\a.png?v=12345");
        assert.equal(toAssetUrl("C:\\a.png", 0), "https://asset.localhost/C:\\a.png?v=0");
        assert.equal(
          toAssetUrl("C:\\withquery.png", "rev 2&3"),
          "https://asset.localhost/C:\\withquery.png?token=1&v=rev%202%263"
        );
        assert.equal(toAssetUrl("C:\\a.png", ""), "https://asset.localhost/C:\\a.png");
        assert.equal(toAssetUrl("C:\\a.png", null), "https://asset.localhost/C:\\a.png");
        assert.equal(toAssetUrl("C:\\a.png", undefined), "https://asset.localhost/C:\\a.png");
      }
    );
  });
});
