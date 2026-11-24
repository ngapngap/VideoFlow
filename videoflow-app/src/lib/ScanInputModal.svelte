<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import { api, toAssetUrl, type UntrackedImageItem } from "./api";
  import { formatApiError, showToast } from "./store";
  import Icons from "./Icons.svelte";
  import Modal from "./Modal.svelte";

  export let open = false;
  export let items: UntrackedImageItem[] = [];
  export let running = false;
  export let cacheBust = 0;

  const dispatch = createEventDispatcher<{
    close: void;
    imported: { count: number };
  }>();

  let importingInput = false;
  let selectedImportNames = new Set<string>();
  let defaultImportPromptImg = "";
  let defaultImportPromptVid = "";
  let wasOpen = false;

  $: if (open && !wasOpen) {
    wasOpen = true;
    selectedImportNames = new Set(items.map((i) => i.file_name));
  } else if (!open && wasOpen) {
    wasOpen = false;
  }

  function formatBytes(bytes?: number | null): string {
    if (typeof bytes !== "number" || !Number.isFinite(bytes) || bytes <= 0) return "";
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
  }

  function toggleImportItem(fileName: string): void {
    const next = new Set(selectedImportNames);
    if (next.has(fileName)) {
      next.delete(fileName);
    } else {
      next.add(fileName);
    }
    selectedImportNames = next;
  }

  function toggleAllImportItems(): void {
    if (selectedImportNames.size === items.length) {
      selectedImportNames = new Set();
    } else {
      selectedImportNames = new Set(items.map((i) => i.file_name));
    }
  }

  async function executeBatchImport(): Promise<void> {
    if (selectedImportNames.size === 0 || importingInput || running) return;
    importingInput = true;
    try {
      const names = Array.from(selectedImportNames);
      const count = await api.import_input_images(
        names,
        defaultImportPromptImg.trim(),
        defaultImportPromptVid.trim()
      );
      dispatch("imported", { count });
      dispatch("close");
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      importingInput = false;
    }
  }
</script>

<Modal
  {open}
  title="Quét & Nhập ảnh mới từ thư mục input/"
  width="680px"
  disableClose={importingInput}
  on:close={() => dispatch("close")}
>
  {#if items.length === 0}
    <div class="scan-empty">
      <Icons name="check" size={28} />
      <div class="scan-empty-title">Tất cả ảnh trong <code>input/</code> đã có trong CSV</div>
      <p>Không phát hiện file ảnh (<code>.png</code>, <code>.jpg</code>, <code>.jpeg</code>, <code>.webp</code>) nào chưa được theo dõi.</p>
    </div>
  {:else}
    <div class="modal-stack">
      <div class="scan-head-row">
        <span>
          Phát hiện <strong>{items.length} ảnh mới</strong> trong <code>input/</code> chưa có trong <code>prompts.csv</code>:
        </span>
        <button type="button" class="btn-ghost tiny-btn" on:click={toggleAllImportItems}>
          {selectedImportNames.size === items.length ? "Bỏ chọn tất cả" : "Chọn tất cả"}
        </button>
      </div>

      <div class="scan-grid">
        {#each items as item (item.file_name)}
          {@const isPicked = selectedImportNames.has(item.file_name)}
          <label class="scan-item" class:picked={isPicked}>
            <input
              type="checkbox"
              checked={isPicked}
              on:change={() => toggleImportItem(item.file_name)}
            />
            <span class="scan-thumb">
              <img
                src={toAssetUrl(item.abs_path, cacheBust)}
                alt={item.file_name}
                loading="lazy"
              />
            </span>
            <span class="scan-meta">
              <span class="scan-name selectable" title={item.file_name}>{item.file_name}</span>
              <span class="scan-size">{formatBytes(item.size_bytes)}</span>
            </span>
          </label>
        {/each}
      </div>

      <div class="scan-prompts">
        <div>
          <label class="label" for="import-default-img-prompt">
            Prompt tạo ảnh mặc định (tùy chọn — áp dụng cho các ảnh nhập)
          </label>
          <textarea
            id="import-default-img-prompt"
            class="input prompt"
            rows="2"
            placeholder="Có thể để trống và sửa riêng từng dòng sau..."
            bind:value={defaultImportPromptImg}
          ></textarea>
        </div>
        <div>
          <label class="label" for="import-default-vid-prompt">
            Prompt tạo video mặc định (tùy chọn)
          </label>
          <textarea
            id="import-default-vid-prompt"
            class="input prompt"
            rows="2"
            placeholder="Nhập prompt chuyển động chung nếu muốn..."
            bind:value={defaultImportPromptVid}
          ></textarea>
        </div>
      </div>
    </div>
  {/if}

  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={importingInput}
      on:click={() => dispatch("close")}
    >
      Đóng
    </button>
    {#if items.length > 0}
      <button
        type="button"
        class="btn-primary"
        disabled={selectedImportNames.size === 0 || importingInput || running}
        on:click={executeBatchImport}
      >
        <Icons name="plus" size={15} />
        {importingInput
          ? "Đang nhập..."
          : `Nhập ${selectedImportNames.size} ảnh vào CSV`}
      </button>
    {/if}
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
  .tiny-btn {
    height: 26px;
    padding: 0 8px;
    font-size: 11px;
    gap: 4px;
  }
  .prompt {
    width: 100%;
    min-height: 70px;
    max-height: 180px;
    resize: vertical;
    line-height: 1.45;
  }
  .scan-empty {
    padding: 28px 16px;
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: center;
    gap: 8px;
    color: var(--c-green);
  }
  .scan-empty-title {
    font-size: 15px;
    font-weight: 700;
    color: var(--c-text);
  }
  .scan-empty p {
    font-size: var(--fs-label);
    color: var(--c-muted);
  }
  .scan-head-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-size: var(--fs-label);
  }
  .scan-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(195px, 1fr));
    gap: 8px;
    max-height: 240px;
    overflow-y: auto;
    padding: 2px;
  }
  .scan-item {
    border: 1px solid var(--c-border);
    border-radius: 8px;
    padding: 7px 9px;
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    background: var(--c-card);
  }
  .scan-item.picked {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .scan-thumb {
    width: 38px;
    height: 38px;
    border-radius: 6px;
    overflow: hidden;
    background: var(--c-surface-subtle);
    flex-shrink: 0;
  }
  .scan-thumb img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .scan-meta {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .scan-name {
    font-size: 12px;
    font-weight: 600;
    color: var(--c-text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .scan-size {
    font-size: 11px;
    color: var(--c-muted);
  }
  .scan-prompts {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px;
  }

  @media (max-width: 820px) {
    .scan-prompts {
      grid-template-columns: 1fr;
    }
  }
</style>
