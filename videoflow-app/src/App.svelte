<script context="module" lang="ts">
  export type Screen = "dash" | "rows" | "history" | "folder" | "settings";
</script>

<script lang="ts">
  // Khung chung 6.1: header 56px, sidebar 232px/64px, statusbar 28px, Toast, Switcher & Close-Guard
  import { onDestroy, onMount } from "svelte";
  import {
    activeRows,
    closeConfirmOpen,
    confirmStartEnabled,
    endpointStatus,
    formatApiError,
    lastRunFinished,
    maxAttemptsStore,
    progressPercent,
    progressText,
    project,
    resetProjectRunState,
    rowChangedTick,
    running,
    settingsDirty,
    showToast,
  } from "./lib/store";
  import { listen } from "@tauri-apps/api/event";
  import {
    api,
    type ProbeResult,
    type RecentProject,
    type UpdateInfo,
    type UpdateProgress,
  } from "./lib/api";
  import Icons from "./lib/Icons.svelte";
  import Modal from "./lib/Modal.svelte";
  import Toast from "./lib/Toast.svelte";
  import Dashboard from "./screens/Dashboard.svelte";
  import Rows from "./screens/Rows.svelte";
  import History from "./screens/History.svelte";
  import LocalFolder from "./screens/LocalFolder.svelte";
  import Settings from "./screens/Settings.svelte";

  let screen: Screen = "dash";
  let sidebarCollapsed = false;
  let stoppingHeader = false;
  let quittingApp = false;

  const navItems: { key: Screen; label: string; icon: string }[] = [
    { key: "dash", label: "Tổng quan", icon: "dashboard" },
    { key: "rows", label: "Dòng việc", icon: "rows" },
    { key: "history", label: "Lịch sử", icon: "history" },
    { key: "folder", label: "Thư mục Local", icon: "folder" },
  ];

  const screenTitles: Record<Screen, string> = {
    dash: "Tổng quan vận hành",
    rows: "Quản lý dòng việc",
    history: "Lịch sử & Nhật ký",
    folder: "Thư mục dự án Local",
    settings: "Cài đặt hệ thống",
  };

  const checklistNames: Record<string, string> = {
    input: "input/",
    prompts: "prompts.csv",
    image_out: "image_out/",
    video_out: "video_out/",
    write: "quyền ghi",
  };

  $: structureOk =
    Boolean($project) &&
    ($project?.checklist?.length ?? 0) >= 5 &&
    ($project?.checklist ?? []).every((i) => i.ok);

  $: missingParts = $project
    ? $project.checklist
        .filter((i) => !i.ok)
        .map((i) => checklistNames[i.key] ?? i.key)
        .join(", ")
    : "";

  $: activeCount = Object.keys($activeRows).length || ($running ? 1 : 0);

  $: settingsNeedsAttention =
    !$endpointStatus.imageConfigured ||
    !$endpointStatus.videoConfigured ||
    Boolean($project && !structureOk);

  // Giữ tóm tắt lượt chạy trên Statusbar trong 10 giây sau khi kết thúc (mục 6.14, TC-37)
  let postRunSummary = "";
  let postRunTimer: ReturnType<typeof setTimeout> | null = null;
  let seenFinishedRunId: number | null = null;

  $: if ($lastRunFinished && $lastRunFinished.run_id !== seenFinishedRunId) {
    seenFinishedRunId = $lastRunFinished.run_id;
    const stLabel =
      $lastRunFinished.status === "da_dung"
        ? "Đã dừng"
        : $lastRunFinished.status === "gian_doan"
          ? "Gián đoạn"
          : "Hoàn tất";
    postRunSummary = `${stLabel} lượt #${$lastRunFinished.run_id}: ${$lastRunFinished.done_count ?? 0} xong, ${$lastRunFinished.error_count ?? 0} lỗi`;
    if (postRunTimer) clearTimeout(postRunTimer);
    postRunTimer = setTimeout(() => {
      postRunSummary = "";
    }, 10000);
  }

  $: if (!$lastRunFinished) {
    seenFinishedRunId = null;
    postRunSummary = "";
    if (postRunTimer) {
      clearTimeout(postRunTimer);
      postRunTimer = null;
    }
  }

  onDestroy(() => {
    if (postRunTimer) clearTimeout(postRunTimer);
  });

  // Chốt chặn chuyển màn hình khi Cài đặt có thay đổi chưa lưu
  let unsavedModalOpen = false;
  let pendingScreen: Screen | null = null;

  function requestNavigate(target: Screen) {
    if (target === screen) return;
    if (screen === "settings" && $settingsDirty) {
      pendingScreen = target;
      unsavedModalOpen = true;
      return;
    }
    screen = target;
  }

  function confirmDiscardSettings() {
    settingsDirty.set(false);
    unsavedModalOpen = false;
    if (pendingScreen) {
      screen = pendingScreen;
      pendingScreen = null;
    }
  }

  function handleDashboardNavigate(e: CustomEvent<string>) {
    const next = e.detail as Screen;
    if (next) requestNavigate(next);
  }

  async function handleHeaderStop() {
    if (!$running || stoppingHeader) return;
    stoppingHeader = true;
    try {
      await api.stop_run();
      showToast("info", "Đã gửi lệnh dừng an toàn — chờ bước hiện tại hoàn tất.");
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      stoppingHeader = false;
    }
  }

  async function handleHideToTray() {
    try {
      await api.hide_to_tray();
      closeConfirmOpen.set(false);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function handleStopAndQuit() {
    quittingApp = true;
    try {
      await api.force_quit_app(true);
    } catch (e) {
      quittingApp = false;
      showToast("error", formatApiError(e));
    }
  }

  // Modal Đổi dự án nhanh trên thanh tiêu đề (Phụ lục v1.3.1 Q4 - 420px)
  let switcherOpen = false;
  let switcherLoading = false;
  let switcherRecents: RecentProject[] = [];

  // Hộp xác nhận khi thư mục mới rỗng, thiếu thành phần hoặc lỗi prompts.csv (6.7b)
  let probeModalOpen = false;
  let probedPath = "";
  let probeInfo: ProbeResult | null = null;
  let probeMode: "giu_lai" | "ghi_de" = "giu_lai";
  let probeApplying = false;

  async function openSwitcher() {
    if ($running) return;
    switcherOpen = true;
    switcherLoading = true;
    try {
      switcherRecents = await api.list_recent_projects();
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      switcherLoading = false;
    }
  }

  async function switchRecentProject(path: string) {
    if ($running) return;
    try {
      const info = await api.load_project(path);
      resetProjectRunState();
      project.set(info);
      rowChangedTick.update((n) => n + 1);
      switcherOpen = false;
      showToast("success", `Đã mở dự án "${info.name}" (${info.row_count} dòng).`);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function removeMissingRecent(path: string) {
    try {
      await api.remove_recent_project(path);
      switcherRecents = await api.list_recent_projects();
      showToast("info", "Đã xóa dự án không còn tồn tại khỏi danh sách.");
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  function joinCsvPath(base: string): string {
    const sep = base.includes("\\") ? "\\" : "/";
    return `${base.replace(/[\\/]+$/, "")}${sep}prompts.csv`;
  }

  async function revealProbedCsv() {
    if (!probedPath) return;
    try {
      await api.open_path_in_explorer(joinCsvPath(probedPath), true);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function pickOtherFolderFromSwitcher() {
    try {
      const dir = await api.pick_folder();
      if (!dir) return;
      const probe = await api.probe_folder(dir);
      const writeItem = probe.checklist.find((i) => i.key === "write");
      if (writeItem && !writeItem.ok) {
        showToast("error", writeItem.message || "Không có quyền ghi vào thư mục đã chọn.");
        return;
      }

      // Mục 6.7b: nếu thư mục đã đủ cấu trúc và prompts.csv hợp lệ -> mở thẳng luôn!
      if (probe.has_full_structure && probe.prompts_valid !== false) {
        const info = await api.load_project(dir);
        resetProjectRunState();
        project.set(info);
        rowChangedTick.update((n) => n + 1);
        switcherOpen = false;
        showToast("success", `Đã mở dự án "${info.name}" (${info.row_count} dòng).`);
        return;
      }

      switcherOpen = false;
      probedPath = dir;
      probeInfo = probe;
      probeMode = "giu_lai";
      probeModalOpen = true;
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function confirmProbeCreate() {
    if (!probedPath || !probeInfo) return;
    probeApplying = true;
    try {
      const info =
        probeInfo.has_full_structure && probeInfo.prompts_valid !== false && probeMode === "giu_lai"
          ? await api.load_project(probedPath)
          : await api.create_project(probedPath, probeMode);
      resetProjectRunState();
      project.set(info);
      rowChangedTick.update((n) => n + 1);
      probeModalOpen = false;
      showToast("success", `Đã mở dự án "${info.name}" (${info.row_count} dòng).`);
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      probeApplying = false;
    }
  }

  async function bootstrapApp() {
    try {
      const view = await api.get_settings();
      endpointStatus.set({
        imageConfigured: Boolean(view.has_image_key),
        videoConfigured: Boolean(view.has_video_key),
      });
      confirmStartEnabled.set(view.settings?.confirm_start ?? true);
      maxAttemptsStore.set(Math.max(1, Number(view.settings?.max_attempts) || 3));
    } catch (e) {
      console.warn("[VideoFlow] Không nạp được cài đặt ban đầu:", e);
    }

    try {
      const isRun = await api.is_running();
      running.set(Boolean(isRun));
    } catch {
      // bỏ qua nếu chưa sẵn sàng
    }

    try {
      const recents = await api.list_recent_projects();
      const firstExisting = recents.find((r) => r.exists);
      if (firstExisting) {
        const info = await api.load_project(firstExisting.path);
        resetProjectRunState();
        project.set(info);
        rowChangedTick.update((n) => n + 1);
      }
    } catch (e) {
      console.warn("[VideoFlow] Không tự mở được dự án gần đây:", e);
    }
  }

  // ━━ Cập nhật ứng dụng (auto-update qua GitHub Releases) ━━
  let appVersion = "1.4.0";
  let updateInfo: UpdateInfo | null = null;
  let updateChecking = false;
  let updateInstalling = false;
  let updatePercent: number | null = null;
  let updateError = "";
  let updateDismissed = false;
  let unlistenUpdateProgress: (() => void) | null = null;

  async function checkForUpdates(silent = true) {
    if (updateChecking || updateInstalling) return;
    updateChecking = true;
    updateError = "";
    try {
      appVersion = await api.app_version();
      const info = await api.check_app_update();
      updateInfo = info;
      // Lỗi mạng/không có endpoint: chỉ hiện khi người dùng chủ động bấm kiểm tra.
      if (!info.available && info.notes) {
        if (!silent) updateError = info.notes;
      }
    } catch (e) {
      if (!silent) updateError = formatApiError(e);
    } finally {
      updateChecking = false;
    }
  }

  async function installUpdate() {
    if (updateInstalling) return;
    updateInstalling = true;
    updateError = "";
    updatePercent = 0;
    try {
      await api.install_app_update();
      // Thành công thì app tự restart; không chạy tới đây.
    } catch (e) {
      updateError = formatApiError(e);
      updateInstalling = false;
      updatePercent = null;
    }
  }

  onMount(() => {
    bootstrapApp();
    checkForUpdates(true);
    listen<UpdateProgress>("update_progress", (ev) => {
      updatePercent = ev.payload.percent;
    })
      .then((un) => {
        unlistenUpdateProgress = un;
      })
      .catch(() => {});
  });

  onDestroy(() => {
    if (unlistenUpdateProgress) unlistenUpdateProgress();
  });
</script>

<div class="layout">
  <aside class="sidebar" class:collapsed={sidebarCollapsed}>
    <div class="sidebar-top">
      <div class="logo" title={`VideoFlow Desktop v${appVersion}`}>
        <span class="logo-mark">
          <Icons name="sparkles" size={16} />
        </span>
        <span class="logo-text">VideoFlow</span>
      </div>
      <button
        type="button"
        class="btn-ghost collapse-btn"
        aria-label={sidebarCollapsed ? "Mở rộng thanh điều hướng" : "Thu gọn thanh điều hướng"}
        title={sidebarCollapsed ? "Mở rộng menu" : "Thu gọn menu"}
        on:click={() => (sidebarCollapsed = !sidebarCollapsed)}
      >
        <Icons name={sidebarCollapsed ? "chevron-right" : "chevron-left"} size={16} />
      </button>
    </div>

    {#if $project}
      <button
        type="button"
        class="sidebar-project"
        title={`Đang mở: ${$project.path}\nNhấn để đổi dự án`}
        disabled={$running}
        on:click={openSwitcher}
      >
        <Icons name="folder-open" size={14} />
        <span class="sidebar-project-text">{$project.name}</span>
      </button>
    {:else}
      <button
        type="button"
        class="sidebar-project empty"
        title="Nhấn để chọn thư mục dự án"
        disabled={$running}
        on:click={openSwitcher}
      >
        <Icons name="folder" size={14} />
        <span class="sidebar-project-text">Chưa chọn dự án</span>
      </button>
    {/if}

    <nav aria-label="Điều hướng chính">
      {#each navItems as item}
        <button
          type="button"
          class="nav-item"
          class:active={screen === item.key}
          title={item.label}
          on:click={() => requestNavigate(item.key)}
        >
          <Icons name={item.icon} size={18} />
          <span class="nav-label">{item.label}</span>
        </button>
      {/each}
    </nav>

    <div class="sidebar-bottom">
      <button
        type="button"
        class="nav-item settings-btn"
        class:active={screen === "settings"}
        title={$settingsDirty
          ? "Cài đặt (có thay đổi chưa lưu)"
          : settingsNeedsAttention
            ? "Cài đặt (cần kiểm tra API key / cấu trúc)"
            : "Cài đặt"}
        on:click={() => requestNavigate("settings")}
      >
        <Icons name="settings" size={18} />
        <span class="nav-label">Cài đặt</span>
        {#if $settingsDirty}
          <span class="status-badge-dot warn" title="Có thay đổi chưa lưu"></span>
        {:else if settingsNeedsAttention}
          <span class="status-badge-dot alert" title="Thiếu API key hoặc cấu trúc dự án"></span>
        {/if}
      </button>
    </div>
  </aside>

  <div class="main-shell">
    <header class="titlebar">
      <div class="titlebar-left">
        <h1 class="titlebar-heading">{screenTitles[screen]}</h1>
      </div>

      <div class="titlebar-right">
        {#if $running}
          <div class="running-pill">
            <button
              type="button"
              class="running-pill-info"
              title="Nhấn để xem tiến độ chi tiết tại Tổng quan"
              on:click={() => requestNavigate("dash")}
            >
              <span class="pulse-dot"></span>
              <span>Đang chạy {activeCount} dòng · {Math.floor($progressPercent)}%</span>
            </button>
            <button
              type="button"
              class="running-pill-stop"
              disabled={stoppingHeader}
              title="Dừng an toàn sau bước đang thực hiện"
              on:click={handleHeaderStop}
            >
              <Icons name="stop" size={12} />
              <span>{stoppingHeader ? "Đang dừng..." : "STOP"}</span>
            </button>
          </div>
        {/if}

        <button
          type="button"
          class="project-switcher-btn"
          disabled={$running}
          title={$project ? `${$project.path} — Nhấn để đổi dự án` : "Chọn thư mục dự án Local"}
          on:click={openSwitcher}
        >
          <Icons name={$project ? "folder-open" : "folder"} size={15} />
          <span class="switcher-name">{$project ? $project.name : "Chọn dự án..."}</span>
          {#if $project}
            <span class="switcher-count">{$project.row_count} dòng</span>
          {/if}
          <Icons name="chevron-down" size={14} />
        </button>

        <button
          type="button"
          class="btn-ghost header-gear"
          class:active={screen === "settings"}
          aria-label="Mở màn hình Cài đặt"
          title="Cài đặt"
          on:click={() => requestNavigate("settings")}
        >
          <Icons name="settings" size={18} />
        </button>
      </div>
    </header>

    <main class="content">
      {#if screen === "dash"}
        <Dashboard on:navigate={handleDashboardNavigate} />
      {:else if screen === "rows"}
        <Rows />
      {:else if screen === "history"}
        <History />
      {:else if screen === "folder"}
        <LocalFolder />
      {:else}
        <Settings />
      {/if}
    </main>
  </div>
</div>

<footer class="statusbar">
  <span class="status-section struct-section">
    <span
      class="dot"
      class:ok={structureOk}
      class:bad={Boolean($project) && !structureOk}
    ></span>
    {#if structureOk}
      <span>Cấu trúc đủ (5/5)</span>
    {:else if $project}
      <span class="missing-parts-text" title={`Thiếu/lỗi: ${missingParts}`}>
        Kiểm tra: {missingParts}
      </span>
    {:else}
      <span>Chưa chọn dự án</span>
    {/if}
  </span>

  <span class="status-divider"></span>

  <span class="status-section">
    <span>Ảnh:</span>
    <span
      class="dot"
      class:ok={$endpointStatus.imageConfigured}
      class:warn={!$endpointStatus.imageConfigured}
    ></span>
    <span>{$endpointStatus.imageConfigured ? "có key" : "chưa có key"}</span>
    <span class="sep">·</span>
    <span>Video:</span>
    <span
      class="dot"
      class:ok={$endpointStatus.videoConfigured}
      class:warn={!$endpointStatus.videoConfigured}
    ></span>
    <span>{$endpointStatus.videoConfigured ? "có key" : "chưa có key"}</span>
  </span>

  <span class="status-spacer"></span>

  <span class="status-section progress-status">
    {#if $running}
      <div class="mini-progress" aria-hidden="true">
        <div style="width: {$progressPercent}%"></div>
      </div>
      <span class="truncate-status">{$progressText || "Đang xử lý..."}</span>
    {:else if postRunSummary}
      <span class="dot ok"></span>
      <span class="truncate-status">{postRunSummary}</span>
    {:else}
      <span>Sẵn sàng</span>
    {/if}
    <span class="sep">·</span>
    <button
      type="button"
      class="version-tag version-btn"
      title={updateChecking ? "Đang kiểm tra cập nhật..." : "Kiểm tra bản cập nhật"}
      disabled={updateChecking}
      on:click={() => checkForUpdates(false)}
    >
      v{appVersion}{updateInfo?.available ? " · có bản mới" : ""}
    </button>
  </span>
</footer>

<Toast />

{#if updateInfo?.available && !updateDismissed}
  <div class="update-banner" role="status" aria-live="polite">
    <div class="update-icon">
      <Icons name="refresh" size={16} />
    </div>
    <div class="update-body">
      <div class="update-title">
        Có bản cập nhật <b>v{updateInfo.version}</b>
        <span class="update-current">(đang dùng v{updateInfo.current_version})</span>
      </div>
      {#if updateInfo.notes}
        <div class="update-notes">{updateInfo.notes}</div>
      {/if}
      {#if updateInstalling}
        <div class="update-progress">
          <div class="update-rail">
            <b style="width:{updatePercent ?? 0}%"></b>
          </div>
          <span>{updatePercent != null ? `${updatePercent}%` : "Đang tải..."}</span>
        </div>
      {/if}
      {#if updateError}
        <div class="update-error">{updateError}</div>
      {/if}
    </div>
    <div class="update-actions">
      <button
        type="button"
        class="btn-secondary update-btn"
        disabled={updateInstalling}
        on:click={() => (updateDismissed = true)}
      >
        Để sau
      </button>
      <button
        type="button"
        class="btn-primary update-btn"
        disabled={updateInstalling || $running}
        title={$running ? "Dừng pipeline trước khi cập nhật" : "Tải và cài, sau đó app tự khởi động lại"}
        on:click={installUpdate}
      >
        <Icons name="refresh" size={14} />
        <span>{updateInstalling ? "Đang cập nhật..." : "Cập nhật & khởi động lại"}</span>
      </button>
    </div>
  </div>
{/if}

<!-- Modal Đổi dự án nhanh 420px (Phụ lục v1.3.1 Q4) -->
<Modal open={switcherOpen} title="Đổi dự án gần đây" width="420px" on:close={() => (switcherOpen = false)}>
  {#if switcherLoading}
    <div class="modal-muted">Đang tải danh sách dự án...</div>
  {:else if switcherRecents.length === 0}
    <div class="empty-recents">
      <Icons name="folder" size={24} />
      <p>Chưa có dự án nào được mở gần đây.</p>
    </div>
  {:else}
    <div class="switcher-list">
      {#each switcherRecents as r (r.path)}
        <div class="switcher-item" class:missing={!r.exists} class:current={r.is_current}>
          {#if r.exists}
            <button
              type="button"
              class="switcher-select"
              on:click={() => switchRecentProject(r.path)}
            >
              <div class="switcher-item-top">
                <span class="switcher-item-name">{r.name}</span>
                {#if r.is_current}
                  <span class="badge badge-image">Đang mở</span>
                {/if}
                <span class="switcher-item-rows">{r.row_count ?? 0} dòng</span>
              </div>
              <div class="switcher-item-path selectable">{r.path}</div>
            </button>
          {:else}
            <div class="switcher-missing-info">
              <div class="switcher-item-top">
                <span class="switcher-item-name">{r.name}</span>
                <span class="badge badge-loi-image">Không tìm thấy</span>
              </div>
              <div class="switcher-item-path selectable">{r.path}</div>
            </div>
            <button
              type="button"
              class="btn-ghost remove-recent-btn"
              title="Xóa khỏi danh sách gần đây"
              on:click={() => removeMissingRecent(r.path)}
            >
              <Icons name="trash" size={14} />
              <span>Xóa khỏi danh sách</span>
            </button>
          {/if}
        </div>
      {/each}
    </div>
  {/if}

  <div slot="footer" class="switcher-footer">
    <button type="button" class="btn-secondary" on:click={() => (switcherOpen = false)}>
      Đóng
    </button>
    <button type="button" class="btn-primary" on:click={pickOtherFolderFromSwitcher}>
      <Icons name="folder-open" size={16} />
      <span>Chọn thư mục khác...</span>
    </button>
  </div>
</Modal>

<!-- Modal xác nhận khi thư mục mới rỗng, thiếu thành phần hoặc prompts.csv lỗi (6.7b) -->
<Modal
  open={probeModalOpen}
  title={probeInfo?.is_empty_dir ? "Khởi tạo dự án mới" : "Xác nhận bổ sung / làm mới dự án"}
  width="480px"
  disableClose={probeApplying}
  on:close={() => (probeModalOpen = false)}
>
  {#if probeInfo}
    <div class="probe-box">
      <div class="probe-path-label">Thư mục đã chọn:</div>
      <div class="probe-path selectable">{probedPath}</div>

      {#if probeInfo.prompts_error}
        <div class="error-box probe-csv-err">
          <div>{probeInfo.prompts_error}</div>
          <button type="button" class="btn-secondary csv-open-btn" on:click={revealProbedCsv}>
            <Icons name="external" size={14} />
            <span>Hiện prompts.csv trong Explorer</span>
          </button>
        </div>
      {/if}

      {#if probeInfo.is_empty_dir}
        <p class="probe-desc">
          Thư mục này chưa có cấu trúc VideoFlow. Ứng dụng sẽ khởi tạo <code>input/</code>,
          <code>image_out/</code>, <code>video_out/</code>, file mẫu <code>prompts.csv</code> và
          <code>state.db</code>.
        </p>
      {:else}
        <p class="probe-desc">
          Thư mục có dữ liệu một phần hoặc cần khắc phục cấu trúc ({probeInfo.row_count ?? 0} dòng hợp lệ).
          Chọn cách xử lý:
        </p>
        <div class="mode-options">
          <label class="mode-card" class:selected={probeMode === "giu_lai"}>
            <input type="radio" name="appProbeMode" value="giu_lai" bind:group={probeMode} />
            <div>
              <div class="mode-title">Giữ lại dữ liệu hiện có (Khuyên dùng)</div>
              <div class="mode-sub">
                Giữ nguyên <code>prompts.csv</code>, lịch sử và sản phẩm cũ; chỉ tạo bù thư mục thiếu.
              </div>
            </div>
          </label>
          <label class="mode-card danger-card" class:selected={probeMode === "ghi_de"}>
            <input type="radio" name="appProbeMode" value="ghi_de" bind:group={probeMode} />
            <div>
              <div class="mode-title">Tạo mới từ đầu (sao lưu prompts.csv cũ)</div>
              <div class="mode-sub">
                Đổi tên <code>prompts.csv</code> cũ sang bản sao lưu, làm mới <code>state.db</code> và tạo
                CSV mẫu.
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
      disabled={probeApplying}
      on:click={() => (probeModalOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class:btn-primary={Boolean(probeInfo?.is_empty_dir || probeMode !== "ghi_de")}
      class:btn-danger={Boolean(!probeInfo?.is_empty_dir && probeMode === "ghi_de")}
      disabled={probeApplying}
      on:click={confirmProbeCreate}
    >
      {#if probeApplying}
        Đang xử lý...
      {:else if probeInfo?.is_empty_dir}
        Khởi tạo dự án
      {:else if probeMode === "ghi_de"}
        Sao lưu & Tạo mới
      {:else}
        Mở & Bổ sung phần thiếu
      {/if}
    </button>
  </div>
</Modal>

<!-- Modal cảnh báo thay đổi chưa lưu ở màn hình Cài đặt -->
<Modal
  open={unsavedModalOpen}
  title="Có thay đổi chưa lưu"
  width="400px"
  on:close={() => (unsavedModalOpen = false)}
>
  <p class="unsaved-msg">
    Bạn đang có các thay đổi chưa lưu trong mục <strong>Cài đặt</strong>. Nếu rời màn hình ngay bây
    giờ, các chỉnh sửa nháp sẽ bị hủy bỏ.
  </p>
  <div slot="footer">
    <button type="button" class="btn-secondary" on:click={() => (unsavedModalOpen = false)}>
      Ở lại để lưu
    </button>
    <button type="button" class="btn-danger" on:click={confirmDiscardSettings}>
      Bỏ thay đổi
    </button>
  </div>
</Modal>

<!-- Modal bảo vệ khi đóng cửa sổ lúc lượt chạy đang diễn ra -->
<Modal
  open={$closeConfirmOpen}
  title="Lượt chạy đang thực hiện"
  width="460px"
  disableClose={quittingApp}
  on:close={() => closeConfirmOpen.set(false)}
>
  <p class="unsaved-msg">
    VideoFlow đang xử lý dòng việc. Bạn có thể thu nhỏ ứng dụng xuống khay hệ thống (System Tray) để
    tiếp tục chạy nền, hoặc dừng an toàn rồi thoát.
  </p>
  <div slot="footer">
    <button
      type="button"
      class="btn-secondary"
      disabled={quittingApp}
      on:click={() => closeConfirmOpen.set(false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={quittingApp}
      on:click={handleHideToTray}
    >
      Thu nhỏ xuống khay
    </button>
    <button
      type="button"
      class="btn-danger"
      disabled={quittingApp}
      on:click={handleStopAndQuit}
    >
      {quittingApp ? "Đang dừng..." : "Dừng & Thoát"}
    </button>
  </div>
</Modal>

<style>
  .layout {
    display: flex;
    height: calc(100% - var(--h-statusbar));
    width: 100%;
    overflow: hidden;
  }

  /* Sidebar 232px -> 64px responsive */
  .sidebar {
    width: var(--w-sidebar);
    flex-shrink: 0;
    background: var(--c-card);
    border-right: 1px solid var(--c-border);
    padding: 12px 10px;
    display: flex;
    flex-direction: column;
    transition: width 0.18s ease, padding 0.18s ease;
  }
  .sidebar.collapsed {
    width: var(--w-sidebar-collapsed);
    padding: 10px 8px;
  }

  .sidebar-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    min-height: 36px;
    padding: 0 4px;
    margin-bottom: 8px;
  }
  .logo {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
  }
  .logo-mark {
    width: 30px;
    height: 30px;
    border-radius: 8px;
    background: linear-gradient(135deg, var(--c-teal) 0%, var(--c-teal-dark) 100%);
    color: #fff;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    box-shadow: 0 2px 6px rgba(14, 124, 134, 0.25);
  }
  .logo-text {
    font-weight: 700;
    font-size: 16px;
    letter-spacing: -0.01em;
    color: var(--c-text);
  }
  .collapse-btn {
    width: 28px;
    height: 28px;
    padding: 0;
    border-radius: 6px;
    flex-shrink: 0;
  }

  .sidebar-project {
    height: 34px;
    padding: 0 10px;
    margin-bottom: 12px;
    border-radius: 8px;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    color: var(--c-text);
    font-size: var(--fs-hint);
    font-weight: 600;
    display: flex;
    align-items: center;
    justify-content: flex-start;
    gap: 8px;
    width: 100%;
    overflow: hidden;
  }
  .sidebar-project:hover:not(:disabled) {
    border-color: var(--c-teal);
    color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .sidebar-project.empty {
    color: var(--c-muted);
    font-weight: 500;
  }
  .sidebar-project-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 4px;
    flex: 1;
  }
  .nav-item {
    height: var(--h-nav-item);
    padding: 0 12px;
    font-size: var(--fs-body);
    font-weight: 500;
    text-align: left;
    background: transparent;
    color: var(--c-text);
    border-radius: var(--r-btn);
    gap: 10px;
    justify-content: flex-start;
    width: 100%;
    position: relative;
  }
  .nav-item:hover:not(:disabled):not(.active) {
    background: var(--c-teal-light);
    color: var(--c-teal-dark);
  }
  .nav-item.active {
    background: var(--c-teal);
    color: #fff;
    font-weight: 600;
    box-shadow: 0 2px 6px rgba(14, 124, 134, 0.22);
  }
  .nav-label {
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .sidebar-bottom {
    margin-top: auto;
    padding-top: 10px;
    border-top: 1px solid var(--c-border);
  }
  .status-badge-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    margin-left: auto;
    flex-shrink: 0;
  }
  .status-badge-dot.warn {
    background: var(--c-warn);
  }
  .status-badge-dot.alert {
    background: var(--c-danger);
  }

  /* Trạng thái thu gọn sidebar (giữ nguyên logo-mark & căn giữa bánh răng không lệch) */
  .sidebar.collapsed .logo-text,
  .sidebar.collapsed .sidebar-project-text,
  .sidebar.collapsed .nav-label {
    display: none;
  }
  .sidebar.collapsed .sidebar-top {
    flex-direction: column;
    justify-content: center;
    gap: 6px;
    padding: 0;
  }
  .sidebar.collapsed .sidebar-project,
  .sidebar.collapsed .nav-item {
    justify-content: center;
    padding: 0;
  }
  .sidebar.collapsed .status-badge-dot {
    position: absolute;
    top: 7px;
    right: 9px;
    margin-left: 0;
    border: 1.5px solid var(--c-card);
  }

  @media (max-width: 1100px) {
    .sidebar {
      width: var(--w-sidebar-collapsed);
      padding: 10px 8px;
    }
    .sidebar .logo-text,
    .sidebar .sidebar-project-text,
    .sidebar .nav-label,
    .sidebar .collapse-btn {
      display: none;
    }
    .sidebar .sidebar-top {
      justify-content: center;
      padding: 0;
    }
    .sidebar .sidebar-project,
    .sidebar .nav-item {
      justify-content: center;
      padding: 0;
    }
    .sidebar .status-badge-dot {
      position: absolute;
      top: 7px;
      right: 9px;
      margin-left: 0;
      border: 1.5px solid var(--c-card);
    }
  }

  /* Cột nội dung chính + Header 56px */
  .main-shell {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    height: 100%;
  }
  .titlebar {
    height: var(--h-titlebar);
    flex-shrink: 0;
    background: var(--c-card);
    border-bottom: 1px solid var(--c-border);
    padding: 0 24px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
  }
  .titlebar-left {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
  }
  .titlebar-heading {
    font-size: var(--fs-h-screen);
    font-weight: 700;
    color: var(--c-text);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .titlebar-right {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-shrink: 0;
  }

  .running-pill {
    height: 34px;
    padding: 2px 3px 2px 10px;
    border-radius: 999px;
    background: var(--c-teal-light);
    border: 1px solid rgba(14, 124, 134, 0.32);
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }
  .running-pill-info {
    height: 28px;
    padding: 0 4px;
    background: transparent;
    color: var(--c-teal-dark);
    font-size: var(--fs-hint);
    font-weight: 600;
    gap: 7px;
  }
  .running-pill-stop {
    height: 26px;
    padding: 0 10px;
    border-radius: 999px;
    background: var(--c-danger);
    color: #fff;
    font-size: 11px;
    font-weight: 700;
    gap: 4px;
  }
  .running-pill-stop:hover:not(:disabled) {
    filter: brightness(0.92);
  }
  .pulse-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--c-teal);
    animation: pulse 1.3s infinite ease-in-out;
  }
  @keyframes pulse {
    0%,
    100% {
      transform: scale(0.9);
      opacity: 0.7;
    }
    50% {
      transform: scale(1.2);
      opacity: 1;
    }
  }

  .project-switcher-btn {
    height: 34px;
    padding: 0 12px;
    border-radius: 999px;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    color: var(--c-text);
    font-size: var(--fs-label);
    font-weight: 600;
    gap: 7px;
    max-width: 280px;
  }
  .project-switcher-btn:hover:not(:disabled) {
    border-color: var(--c-teal);
    color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .switcher-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 140px;
  }
  .switcher-count {
    font-size: 11px;
    font-weight: 600;
    padding: 1px 7px;
    border-radius: 999px;
    background: #fff;
    color: var(--c-muted);
    border: 1px solid var(--c-border);
  }

  .header-gear {
    width: 34px;
    height: 34px;
    padding: 0;
    border-radius: 8px;
  }
  .header-gear.active {
    background: var(--c-teal-light);
    color: var(--c-teal);
  }

  .content {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 16px 24px 20px;
  }

  /* Status bar 28px chống tràn ở 1024px + thanh tiến độ thu gọn 160x6px */
  .statusbar {
    height: var(--h-statusbar);
    background: var(--c-card);
    border-top: 1px solid var(--c-border);
    display: flex;
    align-items: center;
    padding: 0 14px;
    gap: 10px;
    font-size: var(--fs-hint);
    color: var(--c-muted);
    overflow: hidden;
  }
  .status-section {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .struct-section {
    min-width: 0;
    max-width: 250px;
    flex-shrink: 1;
  }
  .missing-parts-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .status-divider {
    width: 1px;
    height: 12px;
    background: var(--c-border);
    flex-shrink: 0;
  }
  .status-spacer {
    flex: 1;
    min-width: 8px;
  }
  .progress-status {
    min-width: 0;
    flex-shrink: 1;
    max-width: 52%;
    overflow: hidden;
  }
  .mini-progress {
    width: 160px;
    height: 6px;
    border-radius: 999px;
    background: #e4ebee;
    overflow: hidden;
    flex-shrink: 0;
  }
  .mini-progress > div {
    height: 100%;
    border-radius: 999px;
    background: var(--c-teal);
    transition: width 0.25s ease;
  }
  .truncate-status {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .version-tag {
    flex-shrink: 0;
  }
  .sep {
    opacity: 0.6;
    margin: 0 2px;
    flex-shrink: 0;
  }
  .dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--c-border-strong);
    flex-shrink: 0;
  }
  .dot.ok {
    background: var(--c-green);
  }
  .dot.warn {
    background: var(--c-warn);
  }
  .dot.bad {
    background: var(--c-danger);
  }

  /* Modal Switcher & Probe */
  .modal-muted {
    color: var(--c-muted);
    padding: 12px 0;
    text-align: center;
  }
  .empty-recents {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 20px 8px;
    color: var(--c-muted);
    text-align: center;
  }
  .switcher-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .switcher-item {
    border: 1px solid var(--c-border);
    border-radius: 10px;
    background: #fff;
    overflow: hidden;
  }
  .switcher-item.current {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .switcher-item.missing {
    background: var(--c-surface-subtle);
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .switcher-select {
    width: 100%;
    padding: 10px 12px;
    background: transparent;
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 4px;
    text-align: left;
    border-radius: 0;
  }
  .switcher-select:hover {
    background: var(--c-teal-light);
  }
  .switcher-item-top {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
  }
  .switcher-item-name {
    font-weight: 600;
    color: var(--c-text);
    font-size: var(--fs-body);
  }
  .switcher-item-rows {
    margin-left: auto;
    font-size: var(--fs-hint);
    color: var(--c-muted);
    font-weight: 500;
  }
  .switcher-item-path {
    font-size: 11px;
    color: var(--c-muted);
    word-break: break-all;
  }
  .remove-recent-btn {
    align-self: flex-end;
    color: var(--c-danger);
    height: 28px;
    padding: 0 10px;
  }
  .switcher-footer {
    display: flex;
    justify-content: space-between;
    width: 100%;
    gap: 8px;
  }

  .probe-box {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .probe-path-label {
    font-size: var(--fs-hint);
    color: var(--c-muted);
  }
  .probe-path {
    padding: 8px 10px;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    border-radius: 8px;
    font-weight: 600;
    font-size: var(--fs-label);
    word-break: break-all;
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
  .csv-open-btn {
    align-self: flex-start;
    height: 30px;
    padding: 0 10px;
    font-size: var(--fs-hint);
  }
  .probe-desc {
    font-size: var(--fs-label);
    color: var(--c-muted);
    line-height: 1.5;
  }
  .mode-options {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .mode-card {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 10px 12px;
    border: 1px solid var(--c-border);
    border-radius: 8px;
    cursor: pointer;
    background: #fff;
  }
  .mode-card.selected {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .mode-card.danger-card.selected {
    border-color: var(--c-danger);
    background: var(--c-danger-bg);
  }
  .mode-card input {
    margin-top: 3px;
  }
  .mode-title {
    font-weight: 600;
    font-size: var(--fs-label);
  }
  .mode-sub {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    margin-top: 2px;
  }
  .unsaved-msg {
    line-height: 1.5;
    color: var(--c-text);
  }
  /* ━━ Banner cập nhật ứng dụng ━━ */
  .version-btn {
    background: none;
    border: none;
    padding: 0 4px;
    color: inherit;
    font: inherit;
    cursor: pointer;
    border-radius: 4px;
  }
  .version-btn:hover:not(:disabled) {
    background: var(--c-surface-subtle);
    color: var(--c-teal);
  }
  .version-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .update-banner {
    position: fixed;
    right: 16px;
    bottom: 40px;
    z-index: 900;
    width: min(560px, calc(100vw - 32px));
    display: flex;
    gap: 12px;
    align-items: flex-start;
    padding: 14px;
    border: 1px solid var(--c-teal);
    border-radius: 12px;
    background: var(--c-card);
    box-shadow: 0 8px 28px rgba(15, 40, 50, 0.18);
  }
  .update-icon {
    flex: 0 0 auto;
    width: 30px;
    height: 30px;
    display: grid;
    place-items: center;
    border-radius: 50%;
    background: var(--c-teal-light);
    color: var(--c-teal);
  }
  .update-body {
    flex: 1 1 auto;
    min-width: 0;
  }
  .update-title {
    font-size: var(--fs-label);
    font-weight: 600;
  }
  .update-current {
    font-weight: 400;
    color: var(--c-muted);
  }
  .update-notes {
    margin-top: 4px;
    font-size: var(--fs-hint);
    color: var(--c-muted);
    max-height: 72px;
    overflow: auto;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .update-progress {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 8px;
    font-size: var(--fs-hint);
    color: var(--c-muted);
  }
  .update-rail {
    flex: 1 1 auto;
    height: 6px;
    border-radius: 3px;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    overflow: hidden;
  }
  .update-rail b {
    display: block;
    height: 100%;
    background: var(--c-teal);
    transition: width 0.25s;
  }
  .update-error {
    margin-top: 6px;
    font-size: var(--fs-hint);
    color: var(--c-danger);
    overflow-wrap: anywhere;
  }
  .update-actions {
    flex: 0 0 auto;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .update-btn {
    white-space: nowrap;
  }
</style>