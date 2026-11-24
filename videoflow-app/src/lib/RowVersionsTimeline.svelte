<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import type { FileVersionItem, TimelineItem } from "./api";
  import Icons from "./Icons.svelte";

  export let files: FileVersionItem[] = [];
  export let timeline: TimelineItem[] = [];
  export let canCompareVersions = false;
  export let previewFileOverride: FileVersionItem | null = null;
  export let running = false;
  export let switchingVersion = false;

  const dispatch = createEventDispatcher<{
    openCompare: void;
    setCurrent: { kind: string; version: number };
    previewVersion: FileVersionItem;
    reveal: string;
  }>();

  function formatBytes(bytes?: number | null): string {
    if (typeof bytes !== "number" || !Number.isFinite(bytes) || bytes <= 0) return "";
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
  }

  function formatTimelineTs(ts: string): string {
    if (!ts) return "—";
    return ts.replace("T", " ").slice(0, 19);
  }

  function formatDuration(ms?: number | null): string {
    if (typeof ms !== "number" || !Number.isFinite(ms) || ms < 0) return "";
    if (ms < 1000) return `${ms}ms`;
    const s = Math.round(ms / 1000);
    return s >= 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s}s`;
  }
</script>

<!-- (C) Lịch sử các phiên bản sản phẩm + Đặt bản chính + So sánh v1 vs v2 -->
<details class="drawer-section" open={files.length > 0}>
  <summary class="section-summary">
    <span>Phiên bản sản phẩm ({files.length})</span>
  </summary>
  {#if canCompareVersions}
    <div class="section-actions-header">
      <span class="section-actions-note">Đối chiếu trực quan các phiên bản</span>
      <button
        type="button"
        class="btn-secondary compare-trigger-btn"
        on:click={() => dispatch("openCompare")}
      >
        <Icons name="columns" size={13} />
        So sánh phiên bản
      </button>
    </div>
  {/if}
  {#if files.length > 0}
    <div class="version-list">
      {#each files as f (`${f.kind}-${f.version}-${f.file_name}`)}
        <div class="version-item" class:active-ver={previewFileOverride?.abs_path === f.abs_path}>
          <div class="version-main">
            <span class="ver-tag">v{f.version}</span>
            <span class="kind-tag" class:vid={f.kind === "video"}>
              {f.kind === "video" ? "Video" : "Ảnh"}
            </span>
            <span class="ver-name selectable" title={f.file_path}>{f.file_name}</span>
            {#if f.is_current}
              <span class="current-pill">Bản chính</span>
            {/if}
            {#if f.size_bytes}
              <span class="ver-size">{formatBytes(f.size_bytes)}</span>
            {/if}
          </div>
          <div class="version-btns">
            {#if !f.is_current}
              <button
                type="button"
                class="btn-ghost set-main-btn"
                disabled={running || switchingVersion}
                title="Đặt phiên bản này làm bản chính trong prompts.csv"
                on:click={() => dispatch("setCurrent", { kind: f.kind, version: f.version })}
              >
                Đặt chính
              </button>
            {/if}
            <button
              type="button"
              class="btn-ghost icon-mini"
              aria-label={`Xem phiên bản v${f.version} (${f.file_name}) trong khung xem trước`}
              title="Xem phiên bản này trong khung xem trước"
              on:click={() => dispatch("previewVersion", f)}
            >
              <Icons name="eye" size={13} />
            </button>
            <button
              type="button"
              class="btn-ghost icon-mini"
              aria-label={`Hiện ${f.file_name} trong thư mục Explorer`}
              title="Hiện file trong thư mục Explorer"
              on:click={() => dispatch("reveal", f.abs_path)}
            >
              <Icons name="external" size={13} />
            </button>
          </div>
        </div>
      {/each}
    </div>
  {:else}
    <div class="section-empty">Chưa có file sản phẩm nào được ghi nhận.</div>
  {/if}
</details>

<!-- Nhật ký các lần thử của dòng -->
<details class="drawer-section" open={timeline.length > 0}>
  <summary class="section-summary">
    <span>Lịch sử chạy dòng ({timeline.length})</span>
  </summary>
  {#if timeline.length > 0}
    <div class="timeline-list">
      {#each timeline as t, idx (idx)}
        <div class="timeline-item">
          <div class="tl-top">
            <div class="tl-badges">
              {#if t.run_id}
                <span class="tl-run">#{t.run_id}</span>
              {/if}
              <span class="tl-step">
                {t.step}{t.attempt_no ? ` · lần ${t.attempt_no}` : ""}
              </span>
              <span
                class="tl-res"
                class:ok={t.result === "ok"}
                class:warn={t.result === "loi_tam_thoi"}
                class:err={t.result !== "ok" && t.result !== "loi_tam_thoi"}
              >
                {t.result === "ok"
                  ? "Thành công"
                  : t.result === "loi_tam_thoi"
                    ? "Lỗi tạm thời"
                    : t.result === "loi_nghiep_vu"
                      ? "Lỗi nghiệp vụ"
                      : t.result}
              </span>
              {#if t.http_status}
                <span class="tl-dur">HTTP {t.http_status}</span>
              {/if}
              {#if t.duration_ms}
                <span class="tl-dur">{formatDuration(t.duration_ms)}</span>
              {/if}
            </div>
            <span class="tl-time">{formatTimelineTs(t.ts)}</span>
          </div>
          {#if t.error}
            <div class="tl-err selectable">{t.error}</div>
          {/if}
        </div>
      {/each}
    </div>
  {:else}
    <div class="section-empty">Dòng này chưa có lượt thực thi nào.</div>
  {/if}
</details>

<style>
  .drawer-section {
    border: 1px solid var(--c-border);
    border-radius: 10px;
    background: var(--c-card);
    overflow: hidden;
  }
  .section-summary {
    padding: 9px 12px;
    font-size: 12px;
    font-weight: 700;
    color: var(--c-text);
    background: var(--c-surface-subtle);
    cursor: pointer;
    list-style: none;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .section-summary::-webkit-details-marker {
    display: none;
  }
  .section-actions-header {
    padding: 6px 12px;
    border-top: 1px solid var(--c-border);
    background: var(--c-surface-subtle);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .section-actions-note {
    font-size: 11px;
    color: var(--c-muted);
  }
  .compare-trigger-btn {
    height: 24px;
    padding: 0 8px;
    font-size: 11px;
    gap: 4px;
  }
  .section-empty {
    padding: 12px;
    font-size: 12px;
    color: var(--c-muted);
    text-align: center;
  }

  .version-list,
  .timeline-list {
    display: flex;
    flex-direction: column;
  }
  .version-item {
    padding: 7px 10px;
    border-top: 1px solid var(--c-border);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-size: 12px;
  }
  .version-item.active-ver {
    background: var(--c-teal-subtle);
  }
  .version-main {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    flex-wrap: wrap;
  }
  .ver-tag {
    font-weight: 700;
    color: var(--c-teal);
    background: var(--c-teal-light);
    padding: 1px 6px;
    border-radius: 4px;
    font-size: 11px;
  }
  .kind-tag {
    font-size: 11px;
    font-weight: 600;
    color: var(--c-muted);
    background: var(--c-surface-subtle);
    padding: 1px 6px;
    border-radius: 4px;
  }
  .kind-tag.vid {
    color: var(--c-green);
    background: var(--c-green-bg);
  }
  .ver-name {
    color: var(--c-text);
    font-weight: 500;
    word-break: break-all;
  }
  .current-pill {
    font-size: 10px;
    font-weight: 700;
    color: var(--c-green);
    background: var(--c-green-bg);
    padding: 1px 6px;
    border-radius: 999px;
  }
  .ver-size {
    font-size: 11px;
    color: var(--c-muted);
  }
  .version-btns {
    display: flex;
    align-items: center;
    gap: 3px;
    flex-shrink: 0;
  }
  .set-main-btn {
    height: 24px;
    padding: 0 7px;
    font-size: 11px;
    font-weight: 600;
    color: var(--c-teal);
  }
  .icon-mini {
    width: 26px;
    height: 26px;
    padding: 0;
  }

  .timeline-item {
    padding: 8px 10px;
    border-top: 1px solid var(--c-border);
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12px;
  }
  .tl-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    flex-wrap: wrap;
  }
  .tl-badges {
    display: flex;
    align-items: center;
    gap: 5px;
    flex-wrap: wrap;
  }
  .tl-run {
    font-weight: 700;
    color: var(--c-text);
  }
  .tl-step {
    font-size: 11px;
    font-weight: 600;
    padding: 1px 6px;
    border-radius: 4px;
    background: var(--c-surface-subtle);
    color: var(--c-text);
  }
  .tl-res {
    font-size: 11px;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 4px;
  }
  .tl-res.ok {
    background: var(--c-green-bg);
    color: var(--c-green);
  }
  .tl-res.warn {
    background: var(--c-warn-bg);
    color: var(--c-warn-dark);
  }
  .tl-res.err {
    background: var(--c-danger-bg);
    color: var(--c-danger);
  }
  .tl-dur,
  .tl-time {
    font-size: 11px;
    color: var(--c-muted);
  }
  .tl-err {
    font-size: 11px;
    color: var(--c-danger);
    background: var(--c-danger-bg);
    padding: 4px 8px;
    border-radius: 4px;
    word-break: break-word;
  }
</style>
