<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import type { DryRunResult } from "./api";
  import Icons from "./Icons.svelte";
  import Modal from "./Modal.svelte";

  export let unsavedPromptModalOpen = false;
  export let resetConfirmOpen = false;
  export let batchConfirmOpen = false;
  export let deleteConfirmOpen = false;

  export let detailRowKey = "";
  export let deleteTargetKey = "";
  export let projectName = "";
  export let selectedCount = 0;
  export let maxAttempts = 3;
  export let running = false;
  export let actionBusy = false;
  export let savingPrompts = false;
  export let batchStarting = false;
  export let endpointsConfigured = true;
  export let batchDryRunProbe: DryRunResult | null = null;

  const dispatch = createEventDispatcher<{
    cancelUnsaved: void;
    discardPrompt: void;
    savePrompt: void;
    closeReset: void;
    confirmReset: void;
    closeBatch: void;
    confirmBatch: void;
    closeDelete: void;
    confirmDelete: void;
  }>();
</script>

<!-- Modal Cảnh báo Prompt chưa lưu khi đóng hoặc đổi dòng trong Drawer -->
<Modal
  open={unsavedPromptModalOpen}
  title="Prompt đang sửa chưa lưu"
  width="440px"
  disableClose={savingPrompts}
  on:close={() => dispatch("cancelUnsaved")}
>
  <p class="confirm-text">
    Bạn đang sửa nháp prompt của dòng <strong class="selectable">{detailRowKey}</strong> nhưng chưa bấm <strong>Lưu prompt</strong>. Bạn muốn lưu lại trước khi rời ngăn chi tiết?
  </p>
  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-ghost"
      disabled={savingPrompts}
      on:click={() => dispatch("cancelUnsaved")}
    >
      Tiếp tục sửa
    </button>
    <button
      type="button"
      class="btn-secondary"
      disabled={savingPrompts}
      on:click={() => dispatch("discardPrompt")}
    >
      Bỏ thay đổi
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={savingPrompts || running}
      on:click={() => dispatch("savePrompt")}
    >
      <Icons name="check" size={14} />
      {savingPrompts ? "Đang lưu..." : "Lưu & Tiếp tục"}
    </button>
  </div>
</Modal>

<!-- Modal Xác nhận Đặt lại trạng thái về Đang chờ (mục 6.3c - 420px) -->
<Modal
  open={resetConfirmOpen}
  title="Đặt lại trạng thái về Đang chờ"
  width="420px"
  disableClose={actionBusy}
  on:close={() => dispatch("closeReset")}
>
  <p class="confirm-text">
    Đặt lại dòng <strong class="selectable">{detailRowKey}</strong> về trạng thái <strong>Đang chờ</strong> và xóa bộ đếm lỗi về <code>0 / {maxAttempts}</code>? Các file ảnh/video đã tạo trước đó vẫn được giữ nguyên.
  </p>
  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={actionBusy}
      on:click={() => dispatch("closeReset")}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={running || actionBusy}
      on:click={() => dispatch("confirmReset")}
    >
      <Icons name="refresh" size={14} />
      Xác nhận đặt lại
    </button>
  </div>
</Modal>

<!-- Modal Xác nhận Chạy các dòng đã chọn -->
<Modal
  open={batchConfirmOpen}
  title="Xác nhận chạy dòng đã chọn"
  width="480px"
  disableClose={batchStarting}
  on:close={() => dispatch("closeBatch")}
>
  <div class="modal-stack">
    <p class="confirm-text">
      Chuẩn bị chạy <strong>{selectedCount} dòng đã chọn</strong> trong dự án <strong>{projectName}</strong>.
    </p>
    {#if !endpointsConfigured}
      <div class="warn-inline-box">
        <Icons name="alert" size={16} />
        <span>Chưa cấu hình đủ API key ảnh/video — vui lòng kiểm tra mục Cài đặt nếu chưa nhập khóa.</span>
      </div>
    {/if}
    {#if batchDryRunProbe?.is_csv_locked}
      <div class="warn-inline-box selectable">
        <Icons name="alert" size={16} />
        <span>
          <strong>prompts.csv đang bị khóa ghi</strong> (mở trong Excel) — kết quả sẽ lưu vào <code>state.db</code> và đồng bộ sau khi đóng Excel.
        </span>
      </div>
    {/if}
    {#if batchDryRunProbe?.ffmpeg_missing_for_auto_merge || batchDryRunProbe?.ffmpeg_missing_for_noi_canh}
      <div class="warn-inline-box selectable">
        <Icons name="alert" size={16} />
        <span>
          <strong>Chưa tìm thấy FFmpeg:</strong>
          {batchDryRunProbe.ffmpeg_hint || "Cần cài FFmpeg để dùng Tự động ghép video / Nối cảnh."}
        </span>
      </div>
    {/if}
  </div>
  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={batchStarting}
      on:click={() => dispatch("closeBatch")}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={running || batchStarting}
      on:click={() => dispatch("confirmBatch")}
    >
      <Icons name="play" size={15} />
      Xác nhận chạy ({selectedCount})
    </button>
  </div>
</Modal>

<!-- Modal Xác nhận xóa dòng khỏi CSV (TC-49) -->
<Modal
  open={deleteConfirmOpen}
  title="Xác nhận xóa dòng khỏi CSV"
  width="440px"
  disableClose={actionBusy}
  on:close={() => dispatch("closeDelete")}
>
  <p class="confirm-text">
    Bạn có chắc chắn muốn xóa dòng <strong class="selectable">{deleteTargetKey}</strong> khỏi <code>prompts.csv</code>?
    Các file ảnh/video đã sinh ra trong thư mục đầu ra (nếu có) vẫn được giữ nguyên trên đĩa.
  </p>
  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={actionBusy}
      on:click={() => dispatch("closeDelete")}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-danger"
      disabled={running || actionBusy}
      on:click={() => dispatch("confirmDelete")}
    >
      <Icons name="trash" size={15} />
      Xác nhận xóa
    </button>
  </div>
</Modal>

<style>
  .modal-stack {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .confirm-text {
    font-size: var(--fs-body);
    color: var(--c-text);
    line-height: 1.5;
  }
  .warn-inline-box {
    background: var(--c-warn-bg);
    border: 1px solid var(--c-warn);
    color: var(--c-warn-dark);
    border-radius: 8px;
    padding: 9px 12px;
    display: flex;
    align-items: flex-start;
    gap: 8px;
    font-size: 12.5px;
  }
  .modal-footer-bar {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    width: 100%;
  }
</style>
