from muse2api.research.protocol import select_reply, validate_stage


def dom_turn(raw):
    return {
        "owner_id": "owner-1", "thread_id": "thread-1",
        "user_message_id": "user-1", "assistant_message_id": "assistant-1",
        "user_turn_id": "user-turn-1", "assistant_turn_id": "assistant-turn-1",
        "completed": True, "text_content": raw,
        "turn_messages": [
            {"message_id": "user-1", "turn_id": "user-turn-1", "role": "user",
             "terminal_status": None, "pre_nodes": []},
            {"message_id": "assistant-1", "turn_id": "assistant-turn-1", "role": "assistant",
             "terminal_status": "completed", "pre_nodes": [{"tag": "pre", "code_children": [
                 {"tag": "code", "class_name": "", "language": "", "text_content": raw}]}]},
        ],
    }


def test_complete_valid_http_has_precedence_over_dom():
    op = "op-1"
    raw = f"VFCTX BEGIN {op} SOURCE\nCOUNT\n0\nTOTAL\n-\nVFCTX END {op} SOURCE"
    def validator(text):
        return validate_stage(text, "SOURCE", op)
    result = select_reply(raw, None, {}, validator, stage="SOURCE", http_status=200, http_complete=True)
    assert result["via"] == "http" and result["value"] == []


def test_failed_or_incomplete_http_falls_back_to_exact_bound_dom():
    op = "op-2"
    raw = f"VFCTX BEGIN {op} SOURCE\nCOUNT\n0\nTOTAL\n-\nVFCTX END {op} SOURCE"
    binding = {"owner_id": "owner-1", "thread_id": "thread-1", "user_message_id": "user-1",
               "assistant_message_id": "assistant-1", "user_turn_id": "user-turn-1",
               "assistant_turn_id": "assistant-turn-1"}
    dom = dom_turn(raw)
    def validator(text):
        return validate_stage(text, "SOURCE", op)
    result = select_reply("partial HTTP", dom, binding, validator, stage="SOURCE",
                          http_status=200, http_complete=False)
    assert result["via"] == "dom_textContent" and result["raw"] == raw
    result = select_reply("HTTP error", dom, binding, validator, stage="SOURCE",
                          http_status=503, http_complete=True)
    assert result["via"] == "dom_textContent"
