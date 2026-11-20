<script lang="ts">
  // Màn hình Thư mục Local (6.7, 6.7b): 5 mục kiểm tra cấu trúc, báo lỗi CSV, mở thẳng dự án đủ cấu trúc
  import {
    beginProjectSwitch,
    formatApiError,
    project,
    resetProjectRunState,
    rowChangedTick,
    running,
    showToast,
  } from "../lib/store";
  import { api, type ChecklistItem, type ProbeResult, type RecentProject } from "../lib/api";
  import Icons from "../lib/Icons.svelte";
  import Modal from "../lib/Modal.svelte";

  let recents: RecentProject[] = [];
  let probing = false;
  let repairing = false;
  let checking = false;

  // Trạng thái Modal xác nhận tạo/mở dự án khi thư mục trống / thiếu / lỗi CSV (6.7b)
  let modalOpen = false;
  let selectedDir = "";
  let probeResult: ProbeResult | null = null;
  let createMode: "giu_lai" | "ghi_de" = "giu_lai";
  let creating = false;

  interface FolderCardDef {
    key: string;
    label: string;
    desc: string;
    icon: string;
    subPath: string;
    revealFile: boolean;
    openLabel: string;
  }

  const folderCards: FolderCardDef[] = [
    {
      key: "input",
      label: "input/",
      desc: "Chứa ảnh tham chiếu đầu vào",
      icon: "image",
      subPath: "input",
      revealFile: false,
      openLabel: "Mở thư mục",
    },
    {
      key: "prompts",
      label: "prompts.csv",
      desc: "Bảng dòng việc UTF-8 BOM",
      icon: "file-text",
      subPath: "prompts.csv",
      revealFile: true,
      openLabel: "Hiện file",
    },
    {
      key: "image_out",
      label: "image_out/",
      desc: "Thư mục lưu ảnh đã tạo",
      icon: "sparkles",
      subPath: "image_out",
      revealFile: false,
      openLabel: "Mở thư mục",
    },
    {
      key: "video_out",
      label: "video_out/",
      desc: "Thư mục lưu video MP4",
      icon: "video",
      subPath: "video_out",
      revealFile: false,
      openLabel: "Mở thư mục",
    },
    {
      key: "write",
      label: "Quyền ghi đĩa",
      desc: "Kiểm tra đọc/ghi thư mục gốc",
      icon: "shield",
      subPath: "",
      revealFile: false,
      openLabel: "",
    },
  ];

  // Chỉ gọi list_recent_projects đúng 1 lần khi mount hoặc khi $project.path thực sự đổi
  let lastSyncedProjectPath: string | null | undefined = undefined;
  $: {
    const curPath = $project?.path ?? null;
    if (curPath !== lastSyncedProjectPath) {
      lastSyncedProjectPath = curPath;
      refresh();
    }
  }

  export async function refresh() {
    try {
      recents = await api.list_recent_projects();
    } catch (e) {
      console.warn("[LocalFolder] Không tải được danh sách gần đây:", e);
    }
  }

  function findChecklistItem(
    list: ChecklistItem[] | undefined,
    key: string
  ): ChecklistItem | undefined {
    return list?.find((c) => c.key === key);
  }

  async function switchProject(path: string) {
    if ($running) return;
    beginProjectSwitch();
    try {
      const info = await api.load_project(path);
      resetProjectRunState();
      lastSyncedProjectPath = info.path;
      project.set(info);
      rowChangedTick.update((n) => n + 1);
      await refresh();
      showToast("success", `Đã chuyển sang dự án "${info.name}" (${info.row_count} dòng).`);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function removeRecent(path: string) {
    try {
      await api.remove_recent_project(path);
      await refresh();
      showToast("info", "Đã xóa dự án khỏi danh sách gần đây.");
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  // Luồng 6.7b chuẩn: đủ 4 mục + prompts.csv hợp lệ -> mở thẳng; rỗng/thiếu/CSV lỗi -> mở Modal xác nhận
  async function openPickFolder() {
    if ($running || probing) return;
    probing = true;
    try {
      const dir = await api.pick_folder();
      if (!dir) return;
      beginProjectSwitch();
      const probe = await api.probe_folder(dir);
      const writeItem = probe.checklist.find((i) => i.key === "write");
      if (writeItem && !writeItem.ok) {
        showToast("error", writeItem.message || "Thư mục đã chọn không có quyền ghi.");
        return;
      }

      if (probe.has_full_structure && probe.prompts_valid !== false) {
        beginProjectSwitch();
        const info = await api.load_project(dir);
        resetProjectRunState();
        lastSyncedProjectPath = info.path;
        project.set(info);
        rowChangedTick.update((n) => n + 1);
        await refresh();
        showToast("success", `Đã mở dự án "${info.name}" (${info.row_count} dòng).`);
        return;
      }

      selectedDir = dir;
      probeResult = probe;
      createMode = "giu_lai";
      modalOpen = true;
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      probing = false;
    }
  }

  async function confirmProjectAction() {
    if (!selectedDir || !probeResult) return;
    creating = true;
    beginProjectSwitch();
    try {
      const info =
        probeResult.has_full_structure &&
        probeResult.prompts_valid !== false &&
        createMode === "giu_lai"
          ? await api.load_project(selectedDir)
          : await api.create_project(selectedDir, createMode);
      resetProjectRunState();
      lastSyncedProjectPath = info.path;
      project.set(info);
      rowChangedTick.update((n) => n + 1);
      modalOpen = false;
      await refresh();
      showToast(
        "success",
        createMode === "ghi_de"
          ? `Đã sao lưu CSV cũ và khởi tạo mới dự án "${info.name}".`
          : `Đã nạp dự án "${info.name}" (${info.row_count} dòng).`
      );
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      creating = false;
    }
  }

  async function repairMissingStructure() {
    if (!$project || $running || repairing) return;
    repairing = true;
    try {
      const info = await api.create_project($project.path, "giu_lai");
      lastSyncedProjectPath = info.path;
      project.set(info);
      rowChangedTick.update((n) => n + 1);
      showToast("success", "Đã kiểm tra và tạo lại đầy đủ các thành phần còn thiếu.");
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      repairing = false;
    }
  }

  async function recheckStructure() {
    if (!$project || $running || checking) return;
    checking = true;
    try {
      const checklist = await api.check_structure();
      project.update((cur) => (cur ? { ...cur, checklist } : cur));
      rowChangedTick.update((n) => n + 1);
      const allOk = checklist.every((i) => i.ok);
      showToast(
        allOk ? "success" : "warn",
        allOk
          ? "Đủ cả 5 mục cấu trúc dự án và prompts.csv hợp lệ."
          : "Phát hiện thành phần thiếu hoặc lỗi dữ liệu — xem chi tiết trên thẻ kiểm tra."
      );
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      checking = false;
    }
  }

  function joinTargetPath(base: string, subPath: string): string {
    if (!subPath) return base;
    const sep = base.includes("\\") ? "\\" : "/";
    const cleanBase = base.replace(/[\\/]+$/, "");
    return `${cleanBase}${sep}${subPath}`;
  }

  async function openInExplorer(card: FolderCardDef) {
    if (!$project) return;
    const target = joinTargetPath($project.path, card.subPath);
    try {
      await api.open_path_in_explorer(target, card.revealFile);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function revealCsvAt(dirPath: string) {
    if (!dirPath) return;
    try {
      await api.open_path_in_explorer(joinTargetPath(dirPath, "prompts.csv"), true);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function openProjectRoot() {
    if (!$project) return;
    try {
      await api.open_path_in_explorer($project.path, false);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  $: promptsItem = findChecklistItem($project?.checklist, "prompts");
  $: hasCsvParseError = Boolean(
    $project &&
      promptsItem &&
      !promptsItem.ok &&
      !promptsItem.message.toLowerCase().includes("thiếu file prompts.csv")
  );
  $: hasMissingItems = Boolean(
    $project && $project.checklist.some((c) => !c.ok && !(c.key === "prompts" && hasCsvParseError))
  );
</script>

<div class="folder-screen">
  <div class="project-head card">
    <div class="head-info">
      <div class="head-label">Thư mục dự án đang mở</div>
      {#if $project}
        <div class="head-title-row">
          <span class="head-name">{$project.name}</span>
          <span class="badge badge-image">{$project.row_count} dòng việc</span>
        </div>
        <div class="head-path selectable">{$project.path}</div>
      {:else}
        <div class="head-empty">Chưa mở dự án nào — hãy chọn một thư mục để bắt đầu</div>
      {/if}
    </div>

    <div class="head-actions">
      {#if $project}
        <button
          type="button"
          class="btn-secondary"
          on:click={openProjectRoot}
          title="Mở thư mục gốc trong Windows Explorer"
        >
          <Icons name="external" size={15} />
          <span>Mở trong Explorer</span>
        </button>
      {/if}
      <button
        type="button"
        class="btn-primary"
        on:click={openPickFolder}
        disabled={probing || $running}
      >
        <Icons name="folder-open" size={16} />
        <span>{probing ? "Đang kiểm tra..." : "Đổi / Tạo dự án mới"}</span>
      </button>
    </div>
  </div>

  <!-- 5 thẻ kiểm tra cấu trúc thư mục (6.6 & 6.7) -->
  <div class="section-header">
    <h2 class="section-title">Kiểm tra cấu trúc 5 thành phần</h2>
    <div class="section-actions">
      <button
        type="button"
        class="btn-ghost"
        disabled={!$project || $running || checking}
        on:click={recheckStructure}
      >
        <Icons name="refresh" size={14} />
        <span>{checking ? "Đang quét..." : "Kiểm tra lại"}</span>
      </button>
      <button
        type="button"
        class="btn-secondary"
        disabled={!$project || $running || repairing}
        on:click={repairMissingStructure}
      >
        <Icons name="plus" size={15} />
        <span>{repairing ? "Đang tạo lại..." : "Tạo lại phần thiếu"}</span>
      </button>
    </div>
  </div>

  {#if hasCsvParseError && $project && promptsItem}
    <div class="csv-error-banner error-box">
      <div class="csv-error-main">
        <Icons name="alert" size={18} />
        <div>
          <div class="csv-error-title">File <code>prompts.csv</code> có lỗi định dạng hoặc trùng khóa</div>
          <div class="csv-error-detail">{promptsItem.message}</div>
        </div>
      </div>
      <div class="csv-error-actions">
        <button
          type="button"
          class="btn-secondary"
          on:click={() => $project && revealCsvAt($project.path)}
        >
          <Icons name="external" size={14} />
          <span>Hiện prompts.csv trong Explorer</span>
        </button>
        <button
          type="button"
          class="btn-primary"
          disabled={$running || checking}
          on:click={recheckStructure}
        >
          <Icons name="refresh" size={14} />
          <span>Nạp lại sau khi sửa</span>
        </button>
      </div>
    </div>
  {/if}

  {#if hasMissingItems}
    <div class="missing-banner">
      <Icons name="alert" size={16} />
      <span>
        Một số thư mục hoặc tệp trong dự án đang bị thiếu. Bấm <strong>Tạo lại phần thiếu</strong> để
        ứng dụng tự động bổ sung mà không ghi đè dữ liệu cũ.
      </span>
    </div>
  {/if}

  <div class="cards-grid">
    {#each folderCards as f (f.key)}
      {@const item = findChecklistItem($project?.checklist, f.key)}
      {@const canOpenCard =
        Boolean($project) &&
        f.key !== "write" &&
        (Boolean(item?.ok) || (f.key === "prompts" && hasCsvParseError))}
      <div
        class="card folder-card"
        class:missing-card={Boolean($project && item && !item.ok)}
      >
        <div class="card-top">
          <span
            class="card-icon"
            class:ok={Boolean($project && item?.ok)}
            class:bad={Boolean($project && item && !item.ok)}
          >
            <Icons name={f.icon} size={18} />
          </span>
          {#if $project && f.key !== "write"}
            <button
              type="button"
              class="btn-ghost open-card-btn"
              disabled={!canOpenCard}
              title={f.revealFile ? "Hiện prompts.csv trong Explorer" : "Mở thư mục trong Explorer"}
              on:click={() => openInExplorer(f)}
            >
              <Icons name="external" size={13} />
              <span>{f.openLabel}</span>
            </button>
          {/if}
        </div>

        <div class="folder-name">{f.label}</div>
        <div class="folder-desc">{f.desc}</div>

        {#if !$project}
          <div class="folder-status neutral">— Chưa mở dự án</div>
        {:else if item?.ok}
          <div class="folder-status ok">
            <Icons name="check" size={14} />
            <span>Có sẵn / Hợp lệ</span>
          </div>
        {:else}
          <div class="folder-status bad selectable" title={item?.message ?? ""}>
            <Icons name="x" size={14} />
            <span class="bad-msg">{item?.message ?? "Thiếu thành phần"}</span>
          </div>
        {/if}
      </div>
    {/each}
  </div>

  <!-- Danh sách tối đa 5 dự án gần đây (Phụ lục v1.3.1 Q4) -->
  <div class="recents card">
    <div class="recents-head">
      <h2 class="recents-title">Dự án mở gần đây (tối đa 5)</h2>
      <span class="recents-sub">Theo Phụ lục v1.3.1 — chọn để chuyển nhanh</span>
    </div>

    {#if recents.length === 0}
      <div class="recents-empty">Chưa có lịch sử dự án gần đây.</div>
    {:else}
      <div class="recents-list">
        {#each recents as r (r.path)}
          <div class="recent-item" class:dimmed={!r.exists} class:active-item={r.is_current}>
            {#if r.exists}
              <button
                type="button"
                class="recent-main-btn"
                disabled={$running || r.is_current}
                on:click={() => switchProject(r.path)}
              >
                <Icons name={r.is_current ? "folder-open" : "folder"} size={16} />
                <div class="recent-texts">
                  <div class="recent-title-line">
                    <span class="recent-name">{r.name}</span>
                    {#if r.is_current}
                      <span class="badge badge-image">Đang mở</span>
                    {/if}
                  </div>
                  <div class="recent-path selectable">{r.path}</div>
                </div>
                <span class="recent-count">{r.row_count ?? 0} dòng</span>
              </button>
            {:else}
              <div class="recent-missing-body">
                <Icons name="alert" size={16} />
                <div class="recent-texts">
                  <div class="recent-title-line">
                    <span class="recent-name">{r.name}</span>
                    <span class="badge badge-loi-image">Không tìm thấy</span>
                  </div>
                  <div class="recent-path selectable">{r.path}</div>
                </div>
                <button
                  type="button"
                  class="btn-ghost danger-remove"
                  disabled={$running}
                  on:click={() => removeRecent(r.path)}
                >
                  <Icons name="trash" size={14} />
                  <span>Xóa</span>
                </button>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>

<!-- Modal kiểm tra & xác nhận khi thư mục trống / thiếu thành phần / lỗi CSV (mục 6.7b) -->
<Modal
  open={modalOpen}
  title={probeResult?.is_empty_dir ? "Khởi tạo dự án VideoFlow mới" : "Tùy chọn nạp / khởi tạo dự án"}
  width="500px"
  disableClose={creating}
  on:close={() => (modalOpen = false)}
>
  {#if probeResult}
    <div class="modal-probe">
      <div class="probe-dir-box">
        <span class="probe-dir-label">Đường dẫn đã chọn:</span>
        <div class="probe-dir-value selectable">{selectedDir}</div>
      </div>

      {#if probeResult.prompts_error}
        <div class="probe-csv-err error-box">
          <div>{probeResult.prompts_error}</div>
          <button
            type="button"
            class="btn-secondary small-explorer-btn"
            on:click={() => revealCsvAt(selectedDir)}
          >
            <Icons name="external" size={13} />
            <span>Hiện prompts.csv trong Explorer</span>
          </button>
        </div>
      {/if}

      <div class="probe-checklist">
        {#each probeResult.checklist as c (c.key)}
          <div class="probe-check-item" class:ok={c.ok} class:missing={!c.ok}>
            <Icons name={c.ok ? "check" : "x"} size={13} />
            <span>{c.message}</span>
          </div>
        {/each}
      </div>

      {#if probeResult.is_empty_dir}
        <div class="probe-note">
          Thư mục này chưa có dữ liệu dự án. Bấm <strong>Khởi tạo dự án mới</strong> để tạo tự động
          <code>input/</code>, <code>image_out/</code>, <code>video_out/</code>, file mẫu
          <code>prompts.csv</code> (UTF-8 BOM) và cơ sở dữ liệu <code>state.db</code>.
        </div>
      {:else}
        <div class="mode-group">
          <div class="mode-heading">
            Thư mục đã có một phần dữ liệu ({probeResult.row_count ?? 0} dòng hợp lệ trong
            <code>prompts.csv</code>). Bạn muốn thực hiện theo chế độ nào?
          </div>

          <label class="choice-box" class:active={createMode === "giu_lai"}>
            <input type="radio" name="folderCreateMode" value="giu_lai" bind:group={createMode} />
            <div>
              <div class="choice-title">Giữ lại dữ liệu hiện có (Khuyên dùng)</div>
              <div class="choice-desc">
                Giữ nguyên <code>prompts.csv</code>, lịch sử chạy <code>state.db</code> và toàn bộ ảnh/video
                đã xuất; chỉ tạo bù các thư mục còn thiếu.
              </div>
            </div>
          </label>

          <label class="choice-box danger-choice" class:active={createMode === "ghi_de"}>
            <input type="radio" name="folderCreateMode" value="ghi_de" bind:group={createMode} />
            <div>
              <div class="choice-title">Tạo mới từ đầu (sao lưu prompts.csv cũ)</div>
              <div class="choice-desc">
                Đổi tên <code>prompts.csv</code> hiện tại thành file sao lưu
                <code>prompts.backup_*.csv</code>, khởi tạo lại <code>state.db</code> mới và tạo file
                <code>prompts.csv</code> mẫu (không xóa file media trong các thư mục).
              </div>
            </div>
          </label>
        </div>
      {/if}
    </div>
  {/if}

  <div slot="footer">
    <button
      type="button"
      class="btn-secondary"
      disabled={creating}
      on:click={() => (modalOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class:btn-primary={Boolean(probeResult?.is_empty_dir || createMode !== "ghi_de")}
      class:btn-danger={Boolean(!probeResult?.is_empty_dir && createMode === "ghi_de")}
      disabled={creating}
      on:click={confirmProjectAction}
    >
      {#if creating}
        Đang thực hiện...
      {:else if probeResult?.is_empty_dir}
        Khởi tạo dự án mới
      {:else if createMode === "ghi_de"}
        Sao lưu & Tạo mới
      {:else}
        Mở & Giữ dữ liệu
      {/if}
    </button>
  </div>
</Modal>

<style>
  .folder-screen {
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-width: 1120px;
  }

  .project-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
    flex-wrap: wrap;
  }
  .head-info {
    min-width: 240px;
    flex: 1;
  }
  .head-label {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    margin-bottom: 4px;
  }
  .head-title-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 4px;
  }
  .head-name {
    font-size: 18px;
    font-weight: 700;
    color: var(--c-text);
  }
  .head-path {
    font-size: var(--fs-label);
    color: var(--c-muted);
    word-break: break-all;
  }
  .head-empty {
    font-size: var(--fs-body);
    font-weight: 600;
    color: var(--c-muted);
    margin-top: 2px;
  }
  .head-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .section-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: -4px;
  }
  .section-title {
    font-size: var(--fs-h-block);
    font-weight: 700;
  }
  .section-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .csv-error-banner {
    background: var(--c-danger-bg);
    color: var(--c-danger);
    border: 1px solid #edb8b3;
    border-radius: 10px;
    padding: 12px 14px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
  }
  .csv-error-main {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    flex: 1;
    min-width: 240px;
  }
  .csv-error-title {
    font-weight: 700;
    font-size: var(--fs-body);
    margin-bottom: 2px;
  }
  .csv-error-detail {
    font-size: var(--fs-label);
    line-height: 1.45;
  }
  .csv-error-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .missing-banner {
    background: var(--c-warn-bg);
    color: var(--c-warn-dark);
    border: 1px solid #f3d299;
    border-radius: 10px;
    padding: 10px 14px;
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: var(--fs-label);
  }

  .cards-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(195px, 1fr));
    gap: 14px;
  }
  .folder-card {
    display: flex;
    flex-direction: column;
    gap: 6px;
    transition: border-color 0.15s ease, box-shadow 0.15s ease;
  }
  .folder-card:hover {
    box-shadow: var(--shadow-hover);
  }
  .folder-card.missing-card {
    border-color: #edb8b3;
    background: #fffafa;
  }
  .card-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 4px;
  }
  .card-icon {
    width: 34px;
    height: 34px;
    border-radius: 8px;
    background: var(--c-surface-subtle);
    color: var(--c-muted);
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .card-icon.ok {
    background: var(--c-teal-light);
    color: var(--c-teal);
  }
  .card-icon.bad {
    background: var(--c-danger-bg);
    color: var(--c-danger);
  }
  .open-card-btn {
    height: 28px;
    padding: 0 8px;
    font-size: 12px;
    gap: 4px;
  }
  .folder-name {
    font-weight: 700;
    font-size: var(--fs-body);
  }
  .folder-desc {
    color: var(--c-muted);
    font-size: var(--fs-hint);
    margin-bottom: 6px;
    flex: 1;
  }
  .folder-status {
    font-size: var(--fs-hint);
    font-weight: 600;
    display: inline-flex;
    align-items: flex-start;
    gap: 5px;
    margin-top: auto;
  }
  .folder-status.neutral {
    color: var(--c-muted);
  }
  .folder-status.ok {
    color: var(--c-green);
    align-items: center;
  }
  .folder-status.bad {
    color: var(--c-danger);
  }
  .bad-msg {
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
    line-height: 1.35;
  }

  /* Dự án gần đây */
  .recents {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .recents-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
  }
  .recents-title {
    font-size: var(--fs-h-block);
    font-weight: 700;
  }
  .recents-sub {
    font-size: var(--fs-hint);
    color: var(--c-muted);
  }
  .recents-empty {
    color: var(--c-muted);
    font-size: var(--fs-label);
    padding: 8px 0;
  }
  .recents-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .recent-item {
    border: 1px solid var(--c-border);
    border-radius: 10px;
    overflow: hidden;
    background: #fff;
  }
  .recent-item.active-item {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .recent-item.dimmed {
    background: var(--c-surface-subtle);
  }
  .recent-main-btn {
    width: 100%;
    padding: 10px 14px;
    background: transparent;
    color: var(--c-text);
    display: flex;
    align-items: center;
    gap: 12px;
    text-align: left;
    border-radius: 0;
  }
  .recent-main-btn:disabled {
    opacity: 1;
    filter: none;
    cursor: default;
  }
  .recent-main-btn:hover:not(:disabled) {
    background: var(--c-teal-light);
  }
  .recent-missing-body {
    padding: 10px 14px;
    display: flex;
    align-items: center;
    gap: 12px;
    color: var(--c-muted);
  }
  .recent-texts {
    flex: 1;
    min-width: 0;
  }
  .recent-title-line {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .recent-name {
    font-weight: 600;
    font-size: var(--fs-body);
    color: var(--c-text);
  }
  .recent-path {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .recent-count {
    font-size: var(--fs-hint);
    font-weight: 600;
    color: var(--c-muted);
    background: #fff;
    border: 1px solid var(--c-border);
    padding: 2px 10px;
    border-radius: 999px;
  }
  .danger-remove {
    color: var(--c-danger);
    height: 30px;
    padding: 0 10px;
  }

  /* Modal probe */
  .modal-probe {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .probe-dir-box {
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    border-radius: 8px;
    padding: 8px 12px;
  }
  .probe-dir-label {
    font-size: var(--fs-hint);
    color: var(--c-muted);
  }
  .probe-dir-value {
    font-weight: 600;
    font-size: var(--fs-label);
    word-break: break-all;
    margin-top: 2px;
  }
  .probe-csv-err {
    background: var(--c-danger-bg);
    color: var(--c-danger);
    border: 1px solid #edb8b3;
    border-radius: 8px;
    padding: 10px 12px;
    font-size: var(--fs-label);
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .small-explorer-btn {
    align-self: flex-start;
    height: 28px;
    padding: 0 10px;
    font-size: var(--fs-hint);
  }
  .probe-checklist {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 6px 12px;
    padding: 8px 12px;
    border: 1px solid var(--c-border);
    border-radius: 8px;
    background: #fff;
  }
  .probe-check-item {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: var(--fs-hint);
  }
  .probe-check-item.ok {
    color: var(--c-green);
  }
  .probe-check-item.missing {
    color: var(--c-muted);
  }
  .probe-note {
    font-size: var(--fs-label);
    line-height: 1.5;
    color: var(--c-text);
    background: var(--c-teal-subtle);
    border: 1px solid rgba(14, 124, 134, 0.25);
    border-radius: 8px;
    padding: 10px 12px;
  }
  .mode-group {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .mode-heading {
    font-size: var(--fs-label);
    color: var(--c-muted);
    line-height: 1.45;
  }
  .choice-box {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 10px 12px;
    border: 1px solid var(--c-border);
    border-radius: 8px;
    cursor: pointer;
    background: #fff;
  }
  .choice-box.active {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .choice-box.danger-choice.active {
    border-color: var(--c-danger);
    background: var(--c-danger-bg);
  }
  .choice-box input {
    margin-top: 3px;
  }
  .choice-title {
    font-weight: 600;
    font-size: var(--fs-label);
  }
  .choice-desc {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    margin-top: 2px;
    line-height: 1.4;
  }
</style>
