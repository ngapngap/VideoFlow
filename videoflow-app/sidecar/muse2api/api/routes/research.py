"""Admin-only internal research API used by the VideoFlow Rust bridge."""
from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Depends, Request

from ...errors import InvalidRequest, NotFound
from ..deps import get_services, require_admin_key

router = APIRouter(prefix="/internal/research", tags=["internal-research"],
                   dependencies=[Depends(require_admin_key)])


@router.post("/{action}")
async def research_action(action: str, body: dict[str, Any], request: Request):
    """Single authenticated action endpoint; never expose this route to a webview."""
    service = get_services(request).research
    if service is None:
        raise NotFound("research service is unavailable")
    actions = {
        "search_start": lambda: service.search_start(body),
        "search_get": lambda: service.search_get(str(body.get("session_id", ""))),
        "search_list": lambda: service.search_list(body.get("cursor"), body.get("limit", 20)),
        "search_sources": lambda: service.search_sources(body),
        "set_sources": lambda: service.set_sources(body),
        "prepare_ideas": lambda: service.prepare_ideas(body),
        "idea_get": lambda: service.idea_get(body),
        "idea_update": lambda: service.idea_update(body),
        "select_idea": lambda: service.select_idea(body),
        "develop_start": lambda: service.develop_start(body),
        "batch_get": lambda: service.batch_get(body),
        "batch_resume": lambda: service.batch_resume(body),
        "batch_stop": lambda: service.batch_stop(body),
        "search_stop": lambda: service.search_stop(body),
        "search_resume": lambda: service.search_resume(body),
        "variant_edit": lambda: service.variant_edit(body),
        "develop_repair": lambda: service.develop_repair(body),
        "transfer_prepare": lambda: service.transfer_prepare(body),
        "transfer_commit": lambda: service.transfer_commit(body),
        "prompt_test": lambda: service.prompt_test(body),
    }
    if action not in actions:
        raise InvalidRequest("unsupported research action")
    result = actions[action]()
    if hasattr(result, "__await__"):
        result = await result
    return {"ok": True, "data": result}
