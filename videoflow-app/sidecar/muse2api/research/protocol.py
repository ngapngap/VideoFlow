"""Strict, offline readers and safe reply selection for VF1/VFCTX stages.

DOM inputs are snapshots of one completed assistant turn. This module never
submits requests and never joins messages, turns, or code blocks together.
"""

from __future__ import annotations

import re
from collections.abc import Callable, Mapping
from typing import Any

from .vf1 import parse as read_vf1

SOURCE = "URL THUMB CAPTION COVERAGE LIKES VIEWS COUNTRY MARKET_EVIDENCE BUY_URL CTA IDEA".split()
CARD = "TITLE CONTENT VISUAL PRINT_SIDE PRINT_TEXT FORMULA".split()
CHECK = "VERDICT CODE EVIDENCE CHANGE".split()


def extract_context(text: str, op_id: str, stage: str) -> str:
    """Return the one closed envelope for this operation/stage."""
    begin, end = f"VFCTX BEGIN {op_id} {stage}", f"VFCTX END {op_id} {stage}"
    active: tuple[str, str] | None = None
    body: list[str] = []
    found: list[str] = []
    for no, line in enumerate(text.splitlines(), 1):
        tag = line.strip()
        if tag.startswith("VFCTX BEGIN "):
            if active is not None:
                raise ValueError(f"Nested context at line {no}")
            parts = tag.split(" ")
            if len(parts) != 4 or not parts[2] or not parts[3]:
                raise ValueError(f"Malformed context at line {no}")
            active = (parts[2], parts[3])
            if tag == begin:
                body = []
            continue
        if tag.startswith("VFCTX END "):
            parts = tag.split(" ")
            if len(parts) != 4 or not parts[2] or not parts[3]:
                raise ValueError(f"Malformed context at line {no}")
            if active is None or (parts[2], parts[3]) != active:
                raise ValueError(f"Unmatched or mismatched context at line {no}")
            if tag == end:
                found.append("\n".join(body))
            active = None
            continue
        if active == (op_id, stage):
            body.append(line)
    if active is not None or len(found) != 1:
        raise ValueError("Missing, duplicate, or incomplete context")
    return found[0]


def _entries(body: str, allowed: list[str]) -> list[tuple[str, str]]:
    out, key, buf = [], None, []
    for line in body.splitlines():
        tag = line.strip()
        if tag in allowed or re.fullmatch(r"RECORD [1-9][0-9]*", tag):
            if key is not None:
                out.append((key, "\n".join(buf).strip()))
            key, buf = tag, []
        elif key is not None:
            buf.append(line)
        elif tag:
            raise ValueError("Text before first field")
    if key is not None:
        out.append((key, "\n".join(buf).strip()))
    return out


def _fields(
    items: list[tuple[str, str]], names: list[str], context: str
) -> dict[str, str | None]:
    for index in range(max(len(items), len(names))):
        expected = names[index] if index < len(names) else "<end>"
        actual, value = items[index] if index < len(items) else ("<end>", "")
        if actual != expected:
            raise ValueError(
                f"{context} expected field {expected} at position {index + 1}, got {actual}"
            )
        if not value:
            raise ValueError(f"{context} field {expected} at position {index + 1} is blank")
    result = {}
    for k, v in items:
        lines = v.splitlines()
        result[k.lower()] = (
            None
            if v == "-"
            else "\n".join(line[2:] if line.startswith("\\ ") else line for line in lines)
        )
    return result


def _records(items: list[tuple[str, str]], names: list[str], ordinals: list[int], context: str):
    width = len(names) + 1
    if len(items) != width * len(ordinals):
        raise ValueError(f"{context} records: expected {len(ordinals)}, got {len(items) // width}")
    rows = []
    for pos, ordinal in enumerate(ordinals):
        start = pos * width
        if items[start] != (f"RECORD {ordinal}", ""):
            actual = items[start][0] if start < len(items) else "<end>"
            raise ValueError(
                f"{context} record {ordinal} header at entry {start + 1}: expected RECORD {ordinal}, got {actual}"
            )
        rows.append(
            dict(
                ordinal=ordinal,
                **_fields(items[start + 1 : start + width], names, f"{context} record {ordinal}"),
            )
        )
    return rows


def read_source_page(body: str):
    items = _entries(body, SOURCE + ["COUNT", "TOTAL"])
    if len(items) < 2:
        raise ValueError("Missing count fields")
    head = _fields(items[:2], ["COUNT", "TOTAL"], "SOURCE header")
    count = head["count"]
    if count is None or not count.isascii() or not count.isdigit():
        raise ValueError("SOURCE header field COUNT at position 1 must be an ASCII integer")
    n = int(count)
    if n > 30:
        raise ValueError("SOURCE header field COUNT at position 1 exceeds 30")
    total = head["total"]
    if total is not None and (not total.isascii() or not total.isdigit() or int(total) < n):
        raise ValueError(
            "SOURCE header field TOTAL at position 2 must be an ASCII integer >= COUNT"
        )
    rows = _records(items[2:], SOURCE, list(range(1, n + 1)), "SOURCE")
    for row in rows:
        if row["coverage"] not in (None, "full", "excerpt", "unknown"):
            raise ValueError(
                f"SOURCE record {row['ordinal']} field COVERAGE at position 4 is invalid"
            )
        if row["country"] is not None and not re.fullmatch(r"[A-Z]{2}", row["country"]):
            raise ValueError(
                f"SOURCE record {row['ordinal']} field COUNTRY at position 7 must be a two-letter code"
            )
    return {"count": n, "reported_total": int(total) if total is not None else None, "items": rows}


def read_source(body: str):
    return read_source_page(body)["items"]


def read_card(body: str):
    card = _fields(_entries(body, CARD), CARD, "CARD")
    if card["print_side"] not in ("front", "back", "both", "unknown"):
        raise ValueError("CARD field PRINT_SIDE at position 4 is invalid")
    if any(not card[key] for key in ("title", "content", "visual", "formula")):
        key = next(key for key in ("title", "content", "visual", "formula") if not card[key])
        raise ValueError(
            f"CARD field {key.upper()} at position {CARD.index(key.upper()) + 1} is required"
        )
    return card


def read_check(body: str, ordinals: list[int]):
    rows = _records(_entries(body, CHECK), CHECK, ordinals, "CHECK")
    for row in rows:
        ok = row["verdict"] == "PASS" and row["code"] == "OK" and row["change"] is None
        bad = (
            row["verdict"] == "FAIL"
            and row["code"]
            in {"PRODUCT", "VISIBILITY", "CONTINUITY", "SALES", "DUPLICATE", "CONSTRAINT"}
            and bool(row["change"])
        )
        if not ((ok or bad) and row["evidence"]):
            raise ValueError(
                f"CHECK record {row['ordinal']} fields 1-4 (VERDICT/CODE/EVIDENCE/CHANGE) have an invalid combination"
            )
    return rows


def validate_stage(
    text: str,
    stage: str,
    op_id: str,
    *,
    count: int | None = None,
    ordinals: list[int] | None = None,
):
    if stage == "GENERATE" and "VFCTX BEGIN " not in text:
        if count is None:
            raise ValueError("GENERATE requires expected count")
        return read_vf1(text, count)
    body = extract_context(text, op_id, stage)
    if stage == "SOURCE":
        return read_source(body)
    if stage == "CARD":
        return read_card(body)
    if stage == "CHECK":
        if ordinals is None:
            raise ValueError("CHECK requires expected ordinals")
        return read_check(body, ordinals)
    if stage in ("GENERATE", "REPAIR"):
        if count is None:
            raise ValueError(f"{stage} requires expected count")
        return read_vf1(body, count)
    raise ValueError("Unknown stage")


def extract_http_payload(text: str, stage: str) -> str:
    """Extract one fenced plaintext payload; surrounding narrative is allowed.

    The legacy raw VF1 exception applies to GENERATE HTTP only. DOM never calls
    this function and requires VFCTX in the raw CODE.textContent.
    """
    lines = text.splitlines(keepends=True)
    fence_lines = [i for i, line in enumerate(lines) if line.strip().startswith("```")]
    if not fence_lines:
        plain = text.splitlines()
        if len(plain) >= 2:
            begin = re.fullmatch(r"VFCTX BEGIN ([^ ]+) ([A-Z]+)", plain[0].strip())
            end = re.fullmatch(r"VFCTX END ([^ ]+) ([A-Z]+)", plain[-1].strip())
            if begin and end and begin.groups() == end.groups() and begin.group(2) == stage:
                return text
        if stage == "GENERATE" and text.lstrip().startswith("VF1 BEGIN"):
            return text
        raise ValueError("HTTP answer has no plaintext code fence")
    if len(fence_lines) != 2:
        raise ValueError("HTTP answer must contain exactly one fence")
    first, last = fence_lines
    if lines[first].strip() != "```plaintext" or lines[last].strip() != "```" or first >= last:
        raise ValueError("HTTP answer must contain one plaintext fence")
    payload = "".join(lines[first + 1 : last])
    if payload.endswith("\r\n"):
        payload = payload[:-2]
    elif payload.endswith(("\n", "\r")):
        payload = payload[:-1]
    return payload


def _required_binding(binding: Mapping[str, str]):
    keys = (
        "owner_id",
        "thread_id",
        "user_message_id",
        "assistant_message_id",
        "user_turn_id",
        "assistant_turn_id",
    )
    if any(not isinstance(binding.get(key), str) or not binding[key].strip() for key in keys):
        raise ValueError("Incomplete operation binding")
    return keys


def adapt_dom_code_node(dom_turn: Mapping[str, Any], binding: Mapping[str, str]) -> str:
    """Select one raw plaintext code node from a terminal, adjacent bound turn.

    Adapter expects an ordered `turn_messages` slice starting at the bound user
    message and continuing through the bound completed assistant message. Each
    entry has role, message_id, turn_id, terminal_status and `pre_nodes`; each
    PRE has one CODE child in `code_children`. It returns that node's raw
    textContent unchanged. Assistant status bubbles may share one turn_id.
    """
    keys = _required_binding(binding)
    for key in keys:
        actual = dom_turn.get(key)
        if not isinstance(actual, str) or not actual.strip() or actual != binding[key]:
            raise ValueError("DOM turn binding mismatch")
    messages = dom_turn.get("turn_messages")
    if not isinstance(messages, list):
        raise ValueError("Missing ordered turn message evidence")
    user_at = next(
        (
            i
            for i, m in enumerate(messages)
            if isinstance(m, Mapping) and m.get("message_id") == binding["user_message_id"]
        ),
        None,
    )
    assistant_at = next(
        (
            i
            for i, m in enumerate(messages)
            if isinstance(m, Mapping) and m.get("message_id") == binding["assistant_message_id"]
        ),
        None,
    )
    if user_at is None or assistant_at is None or assistant_at <= user_at:
        raise ValueError("Bound user/assistant messages are absent or unordered")
    user = messages[user_at]
    if user.get("role") != "user" or user.get("turn_id") != binding["user_turn_id"]:
        raise ValueError("Bound user message does not match its turn")
    assistant_group = messages[user_at + 1 : assistant_at + 1]
    if not assistant_group or any(
        not isinstance(m, Mapping)
        or m.get("role") != "assistant"
        or m.get("turn_id") != binding["assistant_turn_id"]
        for m in assistant_group
    ):
        raise ValueError("Assistant turn is not contiguous after the bound user")
    if any(
        isinstance(m, Mapping)
        and m.get("role") == "assistant"
        and m.get("turn_id") == binding["assistant_turn_id"]
        for m in messages[assistant_at + 1 :]
    ):
        raise ValueError("Bound assistant message is not final in its turn")
    message_ids = [m.get("message_id") for m in [user, *assistant_group]]
    if any(not isinstance(i, str) or not i for i in message_ids) or len(set(message_ids)) != len(
        message_ids
    ):
        raise ValueError("Missing or duplicate message ID")
    if any(m.get("pre_nodes") for m in assistant_group[:-1]):
        raise ValueError("Code block outside final assistant message")
    candidate = assistant_group[-1]
    if candidate.get("terminal_status") != "completed":
        raise ValueError("Assistant turn is not terminal")
    pres = []
    for msg in assistant_group:
        for pre in msg.get("pre_nodes", []):
            pres.append(pre)
    if len(pres) != 1 or not isinstance(pres[0], Mapping) or pres[0].get("tag") != "pre":
        raise ValueError("Expected exactly one PRE code node in the assistant turn")
    codes = pres[0].get("code_children")
    if not isinstance(codes, list) or len(codes) != 1 or not isinstance(codes[0], Mapping):
        raise ValueError("Expected one CODE child in PRE")
    code = codes[0]
    class_name = code.get("class_name", "")
    language = code.get("language", "")
    if (
        code.get("tag") != "code"
        or class_name not in ("", "language-plaintext")
        or language not in ("", "plaintext")
    ):
        raise ValueError("Unsupported code node language")
    if not isinstance(code.get("text_content"), str):
        raise ValueError("Missing raw textContent")
    return code["text_content"]


def select_reply(
    http_text: str | None,
    dom_turn: Mapping[str, Any] | None,
    binding: Mapping[str, str],
    validator: Callable[[str], Any],
    *,
    stage: str,
    http_status: int | None = None,
    http_complete: bool = False,
):
    """Use a complete 2xx HTTP receipt first, then an exact bound DOM turn.

    The DOM adapter requires a unique raw plaintext code node and explicit
    terminal/turn-adjacency proof. No network side effect occurs here.
    """
    if (
        http_text is not None
        and http_complete
        and http_status is not None
        and 200 <= http_status < 300
    ):
        try:
            payload = extract_http_payload(http_text, stage)
            return {
                "via": "http",
                "raw": http_text,
                "payload": payload,
                "value": validator(payload),
            }
        except (ValueError, TypeError, KeyError):
            pass
    if not dom_turn:
        raise ValueError("No valid completed reply")
    raw = adapt_dom_code_node(dom_turn, binding)
    if "VFCTX BEGIN " not in raw or "VFCTX END " not in raw:
        raise ValueError("DOM fallback requires a VFCTX envelope")
    value = validator(raw)
    return {"via": "dom_textContent", "raw": raw, "value": value}
