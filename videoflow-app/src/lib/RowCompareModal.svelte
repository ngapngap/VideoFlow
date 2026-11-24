<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import { toAssetUrl, type FileVersionItem } from "./api";
  import Icons from "./Icons.svelte";
  import Modal from "./Modal.svelte";

  export let open = false;
  export let rowKey = "";
  export let imageVersions: FileVersionItem[] = [];
  export let videoVersions: FileVersionItem[] = [];
  export let initialKind: "image" | "video" = "image";
  export let rev = 0;
  export let running = false;
  export let switchingVersion = false;

  const dispatch = createEventDispatcher<{
    close: void;
    setCurrent: { kind: string; version: number };
  }>();

  let compareKind: "image" | "video" = "image";
  let compareLeftVer = 1;
  let compareRightVer = 2;
  let wasOpen = false;

  function initComparePair(list: FileVersionItem[]): void {
    if (list.length >= 2) {
      // Sắp xếp v cũ ở bên trái, v mới nhất ở bên phải
      const sortedAsc = [...list].sort((a, b) => a.version - b.version);
      compareLeftVer = sortedAsc[sortedAsc.length - 2].version;
      compareRightVer = sortedAsc[sortedAsc.length - 1].version;
    } else if (list.length === 1) {
      compareLeftVer = list[0].version;
      compareRightVer = list[0].version;
    }
  }

  $: if (open && !wasOpen) {
    wasOpen = true;
    compareKind = initialKind;
    initComparePair(compareKind === "video" ? videoVersions : imageVersions);
  } else if (!open && wasOpen) {
    wasOpen = false;
  }

  function switchCompareKind(nextKind: "image" | "video"): void {
    compareKind = nextKind;
    initComparePair(nextKind === "video" ? videoVersions : imageVersions);
  }

  $: compareCandidates = compareKind === "video" ? videoVersions : imageVersions;
  $: compareLeftItem =
    compareCandidates.find((f) => f.version === compareLeftVer) ??
    compareCandidates[compareCandidates.length - 1] ??
    null;
  $: compareRightItem =
    compareCandidates.find((f) => f.version === compareRightVer) ??
    compareCandidates[0] ??
    null;

  function formatBytes(bytes?: number | null): string {
    if (typeof bytes !== "number" || !Number.isFinite(bytes) || bytes <= 0) return "";
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
  }
</script>

<Modal
  {open}
  title={`So sánh phiên bản — ${rowKey}`}
  width="860px"
  disableClose={switchingVersion}
  on:close={() => dispatch("close")}
>
  <div class="modal-stack">
    {#if imageVersions.length >= 2 && videoVersions.length >= 2}
      <div class="compare-kind-tabs">
        <button
          type="button"
          class="chip"
          class:active={compareKind === "image"}
          on:click={() => switchCompareKind("image")}
        >
          <Icons name="image" size={14} />
          Ảnh sản phẩm ({imageVersions.length})
        </button>
        <button
          type="button"
          class="chip"
          class:active={compareKind === "video"}
          on:click={() => switchCompareKind("video")}
        >
          <Icons name="video" size={14} />
          Video sản phẩm ({videoVersions.length})
        </button>
      </div>
    {/if}

    <div class="compare-grid">
      <!-- Cột trái -->
      <div class="compare-pane">
        <div class="compare-pane-head">
          <label class="compare-lbl" for="compare-left-select">Phiên bản A:</label>
          <select id="compare-left-select" bind:value={compareLeftVer}>
            {#each compareCandidates as v (v.version)}
              <option value={v.version}>
                v{v.version} — {v.file_name}{v.is_current ? " (Bản chính)" : ""}
              </option>
            {/each}
          </select>
        </div>

        {#if compareLeftItem}
          <div class="compare-stage">
            {#if compareKind === "video"}
              {#key `left-${compareLeftItem.abs_path}-${rev}`}
                <video
                  controls
                  preload="metadata"
                  src={toAssetUrl(compareLeftItem.abs_path, rev)}
                >
                  <track kind="captions" />
                </video>
              {/key}
            {:else}
              <img
                src={toAssetUrl(compareLeftItem.abs_path, rev)}
                alt={`Phiên bản v${compareLeftItem.version}`}
              />
            {/if}
          </div>
          <div class="compare-pane-foot">
            <div class="compare-meta">
              <span class="ver-tag">v{compareLeftItem.version}</span>
              {#if compareLeftItem.is_current}
                <span class="current-pill">Đang là bản chính</span>
              {/if}
              {#if compareLeftItem.size_bytes}
                <span>{formatBytes(compareLeftItem.size_bytes)}</span>
              {/if}
            </div>
            <button
              type="button"
              class="btn-secondary small-btn"
              disabled={compareLeftItem.is_current || running || switchingVersion}
              on:click={() =>
                compareLeftItem &&
                dispatch("setCurrent", {
                  kind: compareLeftItem.kind,
                  version: compareLeftItem.version,
                })}
            >
              <Icons name="check" size={13} />
              {compareLeftItem.is_current ? "Đang dùng" : "Chọn bản này làm chính"}
            </button>
          </div>
        {/if}
      </div>

      <!-- Cột phải -->
      <div class="compare-pane">
        <div class="compare-pane-head">
          <label class="compare-lbl" for="compare-right-select">Phiên bản B:</label>
          <select id="compare-right-select" bind:value={compareRightVer}>
            {#each compareCandidates as v (v.version)}
              <option value={v.version}>
                v{v.version} — {v.file_name}{v.is_current ? " (Bản chính)" : ""}
              </option>
            {/each}
          </select>
        </div>

        {#if compareRightItem}
          <div class="compare-stage">
            {#if compareKind === "video"}
              {#key `right-${compareRightItem.abs_path}-${rev}`}
                <video
                  controls
                  preload="metadata"
                  src={toAssetUrl(compareRightItem.abs_path, rev)}
                >
                  <track kind="captions" />
                </video>
              {/key}
            {:else}
              <img
                src={toAssetUrl(compareRightItem.abs_path, rev)}
                alt={`Phiên bản v${compareRightItem.version}`}
              />
            {/if}
          </div>
          <div class="compare-pane-foot">
            <div class="compare-meta">
              <span class="ver-tag">v{compareRightItem.version}</span>
              {#if compareRightItem.is_current}
                <span class="current-pill">Đang là bản chính</span>
              {/if}
              {#if compareRightItem.size_bytes}
                <span>{formatBytes(compareRightItem.size_bytes)}</span>
              {/if}
            </div>
            <button
              type="button"
              class="btn-secondary small-btn"
              disabled={compareRightItem.is_current || running || switchingVersion}
              on:click={() =>
                compareRightItem &&
                dispatch("setCurrent", {
                  kind: compareRightItem.kind,
                  version: compareRightItem.version,
                })}
            >
              <Icons name="check" size={13} />
              {compareRightItem.is_current ? "Đang dùng" : "Chọn bản này làm chính"}
            </button>
          </div>
        {/if}
      </div>
    </div>
  </div>

  <div slot="footer" class="modal-footer-bar">
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
  .modal-stack {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .modal-footer-bar {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    width: 100%;
  }
  .small-btn {
    height: 32px;
    padding: 0 12px;
    font-size: 12px;
    gap: 5px;
  }
  .ver-tag {
    font-weight: 700;
    color: var(--c-teal);
    background: var(--c-teal-light);
    padding: 1px 6px;
    border-radius: 4px;
    font-size: 11px;
  }
  .current-pill {
    font-size: 10px;
    font-weight: 700;
    color: var(--c-green);
    background: var(--c-green-bg);
    padding: 1px 6px;
    border-radius: 999px;
  }
  .compare-kind-tabs {
    display: flex;
    gap: 8px;
  }
  .compare-grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 14px;
  }
  .compare-pane {
    border: 1px solid var(--c-border);
    border-radius: 10px;
    overflow: hidden;
    display: flex;
    flex-direction: column;
    background: var(--c-surface-subtle);
  }
  .compare-pane-head {
    padding: 8px 10px;
    background: var(--c-card);
    border-bottom: 1px solid var(--c-border);
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .compare-lbl {
    font-size: 12px;
    font-weight: 700;
    color: var(--c-muted);
    white-space: nowrap;
  }
  .compare-pane-head select {
    flex: 1;
    height: 32px;
    font-size: 12.5px;
    min-width: 0;
  }
  .compare-stage {
    height: 320px;
    background: #0d1520;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 8px;
    overflow: hidden;
  }
  .compare-stage img,
  .compare-stage video {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    display: block;
  }
  .compare-pane-foot {
    padding: 8px 10px;
    background: var(--c-card);
    border-top: 1px solid var(--c-border);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    flex-wrap: wrap;
  }
  .compare-meta {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--c-muted);
  }

  @media (max-width: 820px) {
    .compare-grid {
      grid-template-columns: 1fr;
    }
  }
</style>
