"""Reference reader for the VF1 text envelope. No generative or JSON repair."""

import argparse
import json
from pathlib import Path

BRIEF = ("PRODUCT", "BUYER", "FORMULA")
ROW = ("TITLE", "CONFLICT", "REVEAL", "PAYOFF", "IMAGE", "VIDEO")


def parse(text, count):
    lines = text.splitlines()
    while lines and not lines[0].strip():
        lines.pop(0)
    while lines and not lines[-1].strip():
        lines.pop()
    if (
        lines
        and lines[0].strip() in ("```", "```text", "```plaintext")
        and lines[-1].strip() == "```"
    ):
        lines = lines[1:-1]
    markers = [("VF1 BEGIN", None)]
    markers += [("VF1 " + key, key) for key in BRIEF]
    for i in range(1, count + 1):
        markers.append((f"VF1 CASE {i}", None))
        markers.extend(("VF1 " + key, (i, key)) for key in ROW)
    markers.append(("VF1 END", None))
    cursor = 0
    field = None
    values = {}
    for line in lines:
        marker = line.strip()
        if marker.startswith("VF1 "):
            if cursor == len(markers) or marker != markers[cursor][0]:
                raise ValueError(f"Unexpected marker {marker!r} at step {cursor}")
            field = markers[cursor][1]
            cursor += 1
            if field is not None:
                values[field] = []
        elif field is not None:
            values[field].append(line)
        elif marker:
            raise ValueError(f"Unexpected text outside a field at step {cursor}")
    if cursor != len(markers):
        raise ValueError("Incomplete envelope")

    def value(key):
        result = "\n".join(values[key]).strip()
        if not result:
            raise ValueError(f"Empty field {key}")
        return result

    return {
        "protocol": "VF1",
        "brief": dict(zip(("product_lock", "buyer_reason", "story_formula"), map(value, BRIEF), strict=False)),
        "variants": [
            dict(
                ordinal=i,
                **{
                    name: value((i, key))
                    for key, name in zip(
                        ROW,
                        ("title", "conflict", "reveal", "payoff", "image_prompt", "video_prompt"), strict=False,
                    )
                },
            )
            for i in range(1, count + 1)
        ],
    }


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("raw", type=Path)
    ap.add_argument("count", type=int)
    ap.add_argument("output", type=Path)
    args = ap.parse_args()
    data = parse(args.raw.read_text(encoding="utf-8"), args.count)
    args.output.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
