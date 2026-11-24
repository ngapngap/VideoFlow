<script lang="ts">
  import { createEventDispatcher, onDestroy } from "svelte";
  import { formatApiError, showToast } from "./store";
  import Icons from "./Icons.svelte";
  import Modal from "./Modal.svelte";

  export let open = false;
  export let title = "";
  export let absPath: string | null = null;
  export let url = "";
  export let fileName = "";
  export let promptImage = "";
  export let promptVideo = "";
  export let initialDurationSec: number | null = null;
  export let initialResolution: string | null = null;
  export let accountUsed = "";

  const dispatch = createEventDispatcher<{
    close: void;
    reveal: string;
  }>();

  let videoPlayerEl: HTMLVideoElement | null = null;
  let durationSec: number | null = null;
  let width: number | null = null;
  let height: number | null = null;
  let copiedImagePrompt = false;
  let copiedVideoPrompt = false;
  let copyImgFeedbackTimer: ReturnType<typeof setTimeout> | null = null;
  let copyVidFeedbackTimer: ReturnType<typeof setTimeout> | null = null;
  let lastSessionKey = "";

  $: sessionKey = open ? `${absPath ?? ""}|${url}|${initialResolution ?? ""}|${initialDurationSec ?? ""}` : "";
  $: if (open && sessionKey !== lastSessionKey) {
    lastSessionKey = sessionKey;
    durationSec = initialDurationSec;
    width = null;
    height = null;
    if (initialResolution) {
      const parts = initialResolution.split("x");
      if (parts.length === 2) {
        width = parseInt(parts[0], 10) || null;
        height = parseInt(parts[1], 10) || null;
      }
    }
    copiedImagePrompt = false;
    copiedVideoPrompt = false;
  } else if (!open) {
    lastSessionKey = "";
    if (videoPlayerEl) {
      try {
        videoPlayerEl.pause();
      } catch {}
    }
  }

  onDestroy(() => {
    if (copyImgFeedbackTimer) clearTimeout(copyImgFeedbackTimer);
    if (copyVidFeedbackTimer) clearTimeout(copyVidFeedbackTimer);
    if (videoPlayerEl) {
      try {
        videoPlayerEl.pause();
      } catch {}
    }
  });

  function formatAspectRatio(w: number, h: number): string {
    if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0) return "";
    let a = Math.round(w);
    let b = Math.round(h);
    while (b !== 0) {
      const t = b;
      b = a % b;
      a = t;
    }
    const rw = Math.round(w) / a;
    const rh = Math.round(h) / a;
    if (rw <= 32 && rh <= 32) {
      return `${rw}:${rh}`;
    }
    return `${(w / h).toFixed(2)}:1`;
  }

  function formatSecs(sec?: number | null): string {
    if (typeof sec !== "number" || !Number.isFinite(sec) || sec < 0) return "—";
    const m = Math.floor(sec / 60);
    const s = Math.floor(sec % 60);
    const ms = Math.floor((sec % 1) * 10);
    if (m > 0) {
      return `${m}:${s.toString().padStart(2, "0")}s`;
    }
    return `${s}.${ms}s`;
  }

  function handleClose(): void {
    if (videoPlayerEl) {
      try {
        videoPlayerEl.pause();
      } catch {}
    }
    dispatch("close");
  }

  function handleModalVideoMetadata(e: Event): void {
    const el = e.currentTarget as HTMLVideoElement | null;
    if (!el) return;
    if (Number.isFinite(el.duration) && el.duration > 0) {
      durationSec = el.duration;
    }
    if (el.videoWidth > 0 && el.videoHeight > 0) {
      width = el.videoWidth;
      height = el.videoHeight;
    }
  }

  async function copyModalPrompt(kind: "image" | "video"): Promise<void> {
    const text = kind === "image" ? promptImage : promptVideo;
    if (!text || !text.trim()) {
      showToast("warn", `Prompt ${kind === "image" ? "tạo ảnh" : "tạo video"} đang trống.`);
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
      if (kind === "image") {
        copiedImagePrompt = true;
        if (copyImgFeedbackTimer) clearTimeout(copyImgFeedbackTimer);
        copyImgFeedbackTimer = setTimeout(() => {
          copiedImagePrompt = false;
          copyImgFeedbackTimer = null;
        }, 2000);
      } else {
        copiedVideoPrompt = true;
        if (copyVidFeedbackTimer) clearTimeout(copyVidFeedbackTimer);
        copyVidFeedbackTimer = setTimeout(() => {
          copiedVideoPrompt = false;
          copyVidFeedbackTimer = null;
        }, 2000);
      }
      showToast("success", `Đã sao chép prompt ${kind === "image" ? "tạo ảnh" : "tạo video"}.`);
    } catch (e) {
      showToast("error", `Không thể sao chép vào clipboard: ${formatApiError(e)}`);
    }
  }
</script>

<Modal
  {open}
  title={`Chi tiết video — ${title}`}
  width="880px"
  on:close={handleClose}
>
  <div class="video-modal-layout">
    <div class="video-player-pane">
      {#if url}
        <div class="video-frame-9-16">
          <video
            bind:this={videoPlayerEl}
            class="large-video-player"
            controls
            autoplay
            playsinline
            src={url}
            on:loadedmetadata={handleModalVideoMetadata}
          >
            <track kind="captions" />
          </video>
        </div>
      {:else}
        <div class="video-missing-box">
          <Icons name="alert" size={32} />
          <span>Không tìm thấy file video trên ổ đĩa</span>
        </div>
      {/if}
    </div>

    <div class="video-meta-pane">
      <div class="video-meta-card">
        <h3 class="meta-card-title">Thông số video</h3>
        <dl class="meta-grid">
          <div class="meta-grid-row">
            <dt class="meta-key">Tên file</dt>
            <dd class="meta-val file-val selectable" title={absPath ?? fileName}>
              {fileName || "—"}
            </dd>
          </div>
          <div class="meta-grid-row">
            <dt class="meta-key">Thời lượng</dt>
            <dd class="meta-val">
              {durationSec ? formatSecs(durationSec) : "—"}
            </dd>
          </div>
          <div class="meta-grid-row">
            <dt class="meta-key">Độ phân giải</dt>
            <dd class="meta-val">
              {#if width && height}
                {width}×{height} ({formatAspectRatio(width, height)})
              {:else}
                —
              {/if}
            </dd>
          </div>
          <div class="meta-grid-row">
            <dt class="meta-key">Tài khoản tạo</dt>
            <dd class="meta-val">
              {accountUsed || "Tài khoản Muse mặc định"}
            </dd>
          </div>
        </dl>
      </div>

      <div class="prompt-box-card">
        <div class="prompt-box-head">
          <span class="prompt-box-label">Prompt tạo ảnh</span>
          <button
            type="button"
            class="btn-ghost copy-prompt-btn"
            title="Sao chép prompt tạo ảnh"
            on:click={() => copyModalPrompt("image")}
          >
            {#if copiedImagePrompt}
              <Icons name="check" size={13} />
              <span class="copy-status-text">Đã sao chép</span>
            {:else}
              <Icons name="copy" size={13} />
              <span>Sao chép</span>
            {/if}
          </button>
        </div>
        <div class="prompt-box-body selectable">
          {promptImage || "(trống)"}
        </div>
      </div>

      <div class="prompt-box-card">
        <div class="prompt-box-head">
          <span class="prompt-box-label">Prompt tạo video</span>
          <button
            type="button"
            class="btn-ghost copy-prompt-btn"
            title="Sao chép prompt tạo video"
            on:click={() => copyModalPrompt("video")}
          >
            {#if copiedVideoPrompt}
              <Icons name="check" size={13} />
              <span class="copy-status-text">Đã sao chép</span>
            {:else}
              <Icons name="copy" size={13} />
              <span>Sao chép</span>
            {/if}
          </button>
        </div>
        <div class="prompt-box-body selectable">
          {promptVideo || "(trống)"}
        </div>
      </div>
    </div>
  </div>

  <div slot="footer" class="modal-footer-bar">
    {#if absPath}
      <button
        type="button"
        class="btn-secondary"
        on:click={() => absPath && dispatch("reveal", absPath)}
      >
        <Icons name="folder-open" size={15} />
        Hiện trong thư mục
      </button>
    {/if}
    <button
      type="button"
      class="btn-primary"
      on:click={handleClose}
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
  .video-modal-layout {
    display: grid;
    grid-template-columns: 310px 1fr;
    gap: 20px;
    align-items: start;
    min-height: 480px;
  }
  .video-player-pane {
    display: flex;
    justify-content: center;
    align-items: center;
    background: #090e17;
    border: 1px solid var(--c-border);
    border-radius: var(--r-card);
    padding: 10px;
  }
  .video-frame-9-16 {
    width: 100%;
    max-width: 280px;
    aspect-ratio: 9 / 16;
    background: #000;
    border-radius: 8px;
    overflow: hidden;
    display: flex;
    align-items: center;
    justify-content: center;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.4);
  }
  .large-video-player {
    width: 100%;
    height: 100%;
    object-fit: contain;
    background: #000;
  }
  .video-missing-box {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 10px;
    min-height: 260px;
    color: var(--c-muted);
    font-size: var(--fs-hint);
    text-align: center;
    padding: 20px;
  }
  .video-meta-pane {
    display: flex;
    flex-direction: column;
    gap: 14px;
    min-width: 0;
  }
  .video-meta-card {
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    border-radius: var(--r-card);
    padding: 14px 16px;
  }
  .meta-card-title {
    font-size: 13px;
    font-weight: 700;
    color: var(--c-text);
    margin: 0 0 10px 0;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .meta-grid {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: 10px 16px;
    margin: 0;
  }
  .meta-grid-row {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .meta-key {
    font-size: 11px;
    font-weight: 600;
    color: var(--c-muted);
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  .meta-val {
    font-size: 13px;
    font-weight: 500;
    color: var(--c-text);
    margin: 0;
    word-break: break-all;
  }
  .file-val {
    font-family: var(--font-mono, monospace);
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .prompt-box-card {
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    border-radius: var(--r-card);
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .prompt-box-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 12px;
    border-bottom: 1px solid var(--c-border);
    background: var(--c-card);
  }
  .prompt-box-label {
    font-size: 12px;
    font-weight: 700;
    color: var(--c-text);
  }
  .copy-prompt-btn {
    padding: 3px 8px;
    height: 24px;
    font-size: 11px;
    font-weight: 600;
    border-radius: 4px;
    gap: 4px;
  }
  .copy-status-text {
    color: var(--c-green);
    font-weight: 700;
  }
  .prompt-box-body {
    padding: 10px 12px;
    font-size: 12px;
    line-height: 1.5;
    color: var(--c-text);
    max-height: 115px;
    overflow-y: auto;
    white-space: pre-wrap;
    word-break: break-word;
    background: var(--c-card);
  }

  @media (max-width: 820px) {
    .video-modal-layout {
      grid-template-columns: 1fr;
    }
    .video-frame-9-16 {
      max-width: 240px;
    }
  }
</style>
