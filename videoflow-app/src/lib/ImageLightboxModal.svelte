<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import Icons from "./Icons.svelte";
  import Modal from "./Modal.svelte";

  export let open = false;
  export let title = "";
  export let absPath = "";
  export let url = "";

  const dispatch = createEventDispatcher<{
    close: void;
    reveal: string;
  }>();

  let zoomScale = 1;
  let panX = 0;
  let panY = 0;
  let isPanning = false;
  let panStartX = 0;
  let panStartY = 0;
  let panOriginX = 0;
  let panOriginY = 0;
  let wasOpen = false;
  let lastUrl = "";

  $: if ((open && !wasOpen) || (open && url !== lastUrl)) {
    wasOpen = true;
    lastUrl = url;
    zoomScale = 1;
    panX = 0;
    panY = 0;
    isPanning = false;
  } else if (!open && wasOpen) {
    wasOpen = false;
    isPanning = false;
  }

  function clampZoom(next: number): number {
    return Math.min(4, Math.max(0.25, Math.round(next * 100) / 100));
  }

  function adjustZoom(delta: number): void {
    const next = clampZoom(zoomScale + delta);
    zoomScale = next;
    if (next <= 1) {
      panX = 0;
      panY = 0;
    }
  }

  function resetLightboxZoom(): void {
    zoomScale = 1;
    panX = 0;
    panY = 0;
  }

  function toggleDoubleClickZoom(): void {
    if (zoomScale === 1) {
      zoomScale = 2;
    } else {
      resetLightboxZoom();
    }
  }

  function onLightboxWheel(e: WheelEvent): void {
    e.preventDefault();
    const step = e.deltaY < 0 ? 0.25 : -0.25;
    adjustZoom(step);
  }

  function onLightboxPointerDown(e: PointerEvent): void {
    if (e.button !== 0) return;
    isPanning = true;
    panStartX = e.clientX;
    panStartY = e.clientY;
    panOriginX = panX;
    panOriginY = panY;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }

  function onLightboxPointerMove(e: PointerEvent): void {
    if (!isPanning) return;
    panX = panOriginX + (e.clientX - panStartX);
    panY = panOriginY + (e.clientY - panStartY);
  }

  function onLightboxPointerUp(e: PointerEvent): void {
    if (!isPanning) return;
    isPanning = false;
    try {
      (e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId);
    } catch {
      // bỏ qua nếu pointer đã nhả
    }
  }
</script>

<Modal
  {open}
  {title}
  width="880px"
  on:close={() => dispatch("close")}
>
  <div class="lightbox-toolbar">
    <div class="zoom-controls">
      <button
        type="button"
        class="btn-secondary zoom-btn"
        disabled={zoomScale <= 0.25}
        aria-label="Thu nhỏ ảnh"
        on:click={() => adjustZoom(-0.25)}
      >
        <Icons name="zoom-out" size={15} />
      </button>
      <span class="zoom-readout">{Math.round(zoomScale * 100)}%</span>
      <button
        type="button"
        class="btn-secondary zoom-btn"
        disabled={zoomScale >= 4}
        aria-label="Phóng to ảnh"
        on:click={() => adjustZoom(0.25)}
      >
        <Icons name="zoom-in" size={15} />
      </button>
      <button
        type="button"
        class="btn-ghost tiny-btn"
        on:click={resetLightboxZoom}
      >
        100% / Vừa khung
      </button>
    </div>
    <span class="zoom-hint">Lăn chuột để zoom (25%–400%) · Kéo chuột để di chuyển · Nhấp đúp đổi 100%/200%</span>
  </div>

  <!-- svelte-ignore a11y-no-noninteractive-element-interactions -->
  <div
    class="lightbox-viewport"
    class:grabbing={isPanning}
    role="application"
    aria-label="Khung phóng to và kéo xem ảnh"
    on:wheel={onLightboxWheel}
    on:dblclick={toggleDoubleClickZoom}
    on:pointerdown={onLightboxPointerDown}
    on:pointermove={onLightboxPointerMove}
    on:pointerup={onLightboxPointerUp}
    on:pointercancel={onLightboxPointerUp}
  >
    {#if url}
      <img
        class="lightbox-img"
        src={url}
        alt={title}
        draggable="false"
        style="transform: translate({panX}px, {panY}px) scale({zoomScale});"
      />
    {/if}
  </div>

  <div slot="footer" class="modal-footer-bar">
    {#if absPath}
      <button
        type="button"
        class="btn-secondary"
        on:click={() => dispatch("reveal", absPath)}
      >
        <Icons name="folder-open" size={15} />
        Hiện trong thư mục
      </button>
    {/if}
    <button
      type="button"
      class="btn-primary"
      on:click={() => dispatch("close")}
    >
      Đóng
    </button>
  </div>
</Modal>

<style>
  .modal-footer-bar {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    width: 100%;
  }
  .tiny-btn {
    height: 26px;
    padding: 0 8px;
    font-size: 11px;
    gap: 4px;
  }
  .lightbox-toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 8px;
    flex-wrap: wrap;
  }
  .zoom-controls {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .zoom-btn {
    width: 32px;
    height: 32px;
    padding: 0;
  }
  .zoom-readout {
    min-width: 52px;
    text-align: center;
    font-size: 13px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
    color: var(--c-text);
  }
  .zoom-hint {
    font-size: 11.5px;
    color: var(--c-muted);
  }
  .lightbox-viewport {
    height: 62vh;
    background: #0d1520;
    border-radius: 8px;
    display: flex;
    align-items: center;
    justify-content: center;
    overflow: hidden;
    cursor: grab;
    touch-action: none;
  }
  .lightbox-viewport.grabbing {
    cursor: grabbing;
  }
  .lightbox-img {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    display: block;
    transform-origin: center center;
    transition: transform 0.05s linear;
    user-select: none;
  }
</style>
