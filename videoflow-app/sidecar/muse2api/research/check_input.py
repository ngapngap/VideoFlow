"""Build a blind CHECK payload from frozen inputs only; no I/O or model calls."""

from collections.abc import Mapping, Sequence


def _required_text(value, label):
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{label} must be a non-empty string")
    return value


def check_input(card, batch, *, locked_ordinals=()):
    """Project a frozen CARD and full contiguous prompt batch into CHECK input.

    The returned object intentionally excludes all other card and variant data,
    including prose claims, titles, narrative beats, and previous verdicts.
    ``print_text`` alone may be ``None`` to represent no printed text.
    """
    if not isinstance(card, Mapping):
        raise ValueError("card must be a mapping")
    visual = _required_text(card.get("visual"), "card.visual")
    print_side = card.get("print_side")
    if not isinstance(print_side, str) or print_side not in {"front", "back", "both", "unknown"}:
        raise ValueError("card.print_side must be front, back, both, or unknown")
    if "print_text" not in card:
        raise ValueError("card.print_text must be present, including when null")
    print_text = card["print_text"]
    if print_text is not None and (not isinstance(print_text, str) or not print_text.strip()):
        raise ValueError("card.print_text must be a string or null")
    formula = _required_text(card.get("formula"), "card.formula")
    if not isinstance(batch, Sequence) or isinstance(batch, (str, bytes)) or not batch:
        raise ValueError("batch must be a non-empty sequence")

    candidates = []
    for position, item in enumerate(batch, 1):
        if not isinstance(item, Mapping):
            raise ValueError(f"candidate at position {position} must be a mapping")
        ordinal = item.get("ordinal")
        if isinstance(ordinal, bool) or not isinstance(ordinal, int) or ordinal != position:
            raise ValueError(
                f"candidate at position {position} must have contiguous ordinal {position}"
            )
        candidates.append(
            {
                "ordinal": ordinal,
                "image_prompt": _required_text(
                    item.get("image_prompt"), f"candidate {ordinal}.image_prompt"
                ),
                "video_prompt": _required_text(
                    item.get("video_prompt"), f"candidate {ordinal}.video_prompt"
                ),
            }
        )

    locked = list(locked_ordinals)
    if any(type(i) is not int or not 1 <= i <= len(batch) for i in locked) or locked != sorted(
        set(locked)
    ):
        raise ValueError("locked_ordinals must be unique, ordered original ordinals in this batch")
    payload = {
        "card": {
            "visual": visual,
            "print_side": print_side,
            "print_text": print_text,
            "formula": formula,
        },
        "candidates": candidates,
    }

    if locked:
        payload["locked_ordinals"] = locked
    return payload
