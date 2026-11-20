from __future__ import annotations

import asyncio
import base64
import mimetypes
import time
from collections.abc import Callable
from dataclasses import replace

from fastapi import APIRouter, Depends, Request
from pydantic import ValidationError
from starlette.datastructures import UploadFile

from ...core.media import _MAX_FETCH_BYTES, load_image_ref
from ...core.models import resolve_model
from ...drivers.base import ImageRequest, InputImage, MediaResult
from ...errors import InvalidRequest, NotFound
from ...services.container import Services
from ...services.tasks import Task
from ..deps import get_services, public_base, require_api_key
from ..schemas import ImageGenerationRequest

router = APIRouter(tags=["images"], dependencies=[Depends(require_api_key)])

# Reference images attached to one muse.ai message.
MAX_REFERENCE_IMAGES = 4

# Asking muse.ai for a "transparent background" makes it reply with text only, so
# ask for an easy-to-cut plain background and remove it locally instead.
_CUTOUT_HINT = "Isolated subject on a plain, uncluttered studio background."


async def _image_item(r: MediaResult, fmt: str, svc: Services, base: str) -> dict:
    item: dict = {"revised_prompt": r.revised_prompt}
    if fmt == "b64_json":
        item["b64_json"] = base64.b64encode(r.data).decode()
    else:
        name = await asyncio.to_thread(svc.media.save, r.data, r.mime, prefix="img")
        item["url"] = f"{base}/v1/media/{name}"
    return item


def _task_view(task: Task) -> dict:
    return {
        "id": task.id,
        "object": "image.task",
        "status": task.status.value,
        "progress": task.progress,
        "created_at": int(task.created_at),
        "model": task.request.get("model"),
        "result": task.result,
        "error": task.error,
    }


async def _generate(body: ImageGenerationRequest, refs: list[InputImage],
                    request: Request, svc: Services) -> dict:
    request.state.model = body.model or resolve_model(body.model, "image").id
    if len(refs) > MAX_REFERENCE_IMAGES:
        raise InvalidRequest(f"at most {MAX_REFERENCE_IMAGES} reference images are supported")
    if body.async_:
        return _submit(body, refs, request, svc)
    return await _run(body, refs, public_base(request), svc)


def _submit(body: ImageGenerationRequest, refs: list[InputImage], request: Request,
            svc: Services) -> dict:
    """Run the generation as a background task; results are stored as media URLs."""
    spec = resolve_model(body.model, "image")
    body = body.model_copy(update={"response_format": "url"})  # keep base64 out of tasks.json
    base = public_base(request)

    async def runner(progress) -> dict:
        return await _run(body, refs, base, svc, on_progress=progress)

    meta = {"model": spec.id, "prompt": body.prompt, "size": body.size, "n": body.n,
            "references": len(refs)}
    task = svc.tasks.submit("image", meta, runner)
    request.state.task_id = task.id
    return _task_view(task)


async def _run(
    body: ImageGenerationRequest,
    refs: list[InputImage],
    base: str,
    svc: Services,
    on_progress: Callable[[int], None] | None = None,
) -> dict:
    spec = resolve_model(body.model, "image")
    transparent = body.background == "transparent"
    prompt = f"{body.prompt.rstrip('. ')}. {_CUTOUT_HINT}" if transparent else body.prompt
    req = ImageRequest(
        prompt=prompt,
        model=spec.id,
        size=body.size,
        n=body.n,
        reference_images=refs,
        timeout=svc.settings.image_timeout,
        on_progress=on_progress,
    )
    results = await svc.gateway.generate_image(req)
    if transparent:
        # After the gateway call, so the account is not held during matting.
        results = [replace(r, data=await svc.matting.remove(r.data), mime="image/png")
                   for r in results]
    return {"created": int(time.time()),
            "data": [await _image_item(r, body.response_format, svc, base) for r in results]}


@router.post("/v1/images/generations")
async def generate_images(body: ImageGenerationRequest, request: Request,
                          svc: Services = Depends(get_services)) -> dict:
    refs = body.image if isinstance(body.image, list) else [body.image] if body.image else []
    if len(refs) > MAX_REFERENCE_IMAGES:
        raise InvalidRequest(f"at most {MAX_REFERENCE_IMAGES} reference images are supported")
    loaded = [InputImage(*(await load_image_ref(ref))) for ref in refs]
    return await _generate(body, loaded, request, svc)


@router.post("/v1/images/edits")
async def edit_images(request: Request, svc: Services = Depends(get_services)) -> dict:
    """OpenAI-style multipart edit: ``image`` / ``image[]`` files plus a prompt.

    muse.ai has no inpainting, so the images act as references for a new image
    and ``mask`` is rejected rather than silently ignored.
    """
    max_body = (MAX_REFERENCE_IMAGES + 1) * _MAX_FETCH_BYTES
    raw_cl = request.headers.get("content-length")
    if raw_cl is not None:
        try:
            cl_val = int(raw_cl.strip())
        except ValueError as exc:
            raise InvalidRequest("invalid Content-Length header") from exc
        if cl_val < 0:
            raise InvalidRequest("invalid Content-Length header")
        if cl_val > max_body:
            raise InvalidRequest("request body too large")

    form = await request.form(max_files=MAX_REFERENCE_IMAGES + 2)
    try:
        if form.get("mask") is not None:
            raise InvalidRequest("mask is not supported; muse.ai cannot inpaint a region")
        files = [f for key in ("image", "image[]") for f in form.getlist(key)
                 if isinstance(f, UploadFile)]
        if not files:
            raise InvalidRequest("at least one image file is required (field 'image' or 'image[]')")
        if len(files) > MAX_REFERENCE_IMAGES:
            raise InvalidRequest(f"at most {MAX_REFERENCE_IMAGES} reference images are supported")
        fields = {
            k: v for k, v in form.items() if isinstance(v, str) and k not in ("image", "image[]")
        }
        try:
            body = ImageGenerationRequest(**fields)
        except ValidationError as exc:
            raise InvalidRequest(exc.errors()[0].get("msg", "invalid request")) from exc
        refs = []
        for f in files:
            if f.size is not None and f.size > _MAX_FETCH_BYTES:
                raise InvalidRequest("uploaded image exceeds 20 MiB")
            mime = (f.content_type or "").split(";", 1)[0].strip().lower()
            if not mime.startswith("image/"):
                mime = mimetypes.guess_type(f.filename or "")[0] or "image/png"
            raw = await f.read(_MAX_FETCH_BYTES + 1)
            if len(raw) > _MAX_FETCH_BYTES:
                raise InvalidRequest("uploaded image exceeds 20 MiB")
            if not raw:
                raise InvalidRequest("uploaded image is empty")
            refs.append(InputImage(data=raw, mime=mime))
    finally:
        await form.close()
    return await _generate(body, refs, request, svc)


@router.get("/v1/images/generations/{task_id}")
async def get_image_task(task_id: str, request: Request,
                         svc: Services = Depends(get_services)) -> dict:
    task = svc.tasks.get(task_id)
    if task is None or task.kind != "image":
        raise NotFound(f"image task '{task_id}' not found")
    request.state.model = task.request.get("model")
    return _task_view(task)
