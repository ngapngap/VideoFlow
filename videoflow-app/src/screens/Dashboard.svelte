<script lang="ts">
  // Màn hình Tổng quan (6.2, 6.2b, 6.8, 6.14): Onboarding 1-chạm, 3 thẻ nguồn, 4 thẻ số liệu, tiến độ đa luồng 1..3, Start/Stop/Chạy thử
  import { createEventDispatcher } from "svelte";
  import {
    project,
    running,
    currentRun,
    progressText,
    progressPercent,
    progressStep,
    activeRows,
    rowChangedTick,
    lastRunFinished,
    pendingRowsFilter,
    endpointStatus,
    confirmStartEnabled,
    beginProjectSwitch,
    resetProjectRunState,
    showToast,
    formatApiError,
  } from "../lib/store";
  import { api, type DryRunResult, type ProbeResult } from "../lib/api";
  import Icons from "../lib/Icons.svelte";
  import Modal from "../lib/Modal.svelte";

  const dispatch = createEventDispatcher<{
    navigate: "dash" | "rows" | "history" | "folder" | "settings";
  }>();

  let dryRun: DryRunResult | null = null;
  let dryRunLoading = false;
  let dryRunModalOpen = false;
  let confirmModalOpen = false;
  let dontAskAgain = false;
  let starting = false;
  let stopping = false;

  // Luồng chọn/khởi tạo dự án trực tiếp ngay từ màn hình Onboarding (6.7b)
  let onboardingPicking = false;
  let probeModalOpen = false;
  let probedPath = "";
  let probeInfo: ProbeResult | null = null;
  let probeMode: "giu_lai" | "ghi_de" = "giu_lai";
  let probeApplying = false;

  let totalRows = 0;
  let pendingCount = 0;
  let doneCount = 0;
  let errorRows = 0;
  let skippedCount = 0;
  let loadError: string | null = null;
  let refreshSeq = 0;

  $: if (!$running) {
    stopping = false;
  }

  $: activeEntries = Object.entries($activeRows ?? {});
  $: activeWorkerCount = Object.keys($activeRows ?? {}).length || ($running ? 1 : 0);

  $: structOk =
    !!$project &&
    !loadError &&
    $project.checklist.length >= 5 &&
    $project.checklist.every((i) => i.ok);

  $: missingChecklist = ($project?.checklist ?? [])
    .filter((i) => !i.ok)
    .map((i) => i.key)
    .join(", ");

  // Tự động làm mới khi dự án đang mở hoặc trạng thái dòng thay đổi
  $: {
    const projectPath = $project?.path ?? null;
    const tick = $rowChangedTick;
    void tick;
    if (projectPath) {
      void refresh();
    } else {
      dryRun = null;
      totalRows = 0;
      pendingCount = 0;
      doneCount = 0;
      errorRows = 0;
      skippedCount = 0;
      loadError = null;
    }
  }

  export async function refresh(): Promise<void> {
    if (!$project) return;
    const seq = ++refreshSeq;
    try {
      const [rowsRes, drRes] = await Promise.allSettled([
        api.get_rows(undefined, undefined, 1, 1),
        api.start_dry_run(),
      ]);
      if (seq !== refreshSeq) return;

      let errMsg: string | null = null;

      if (rowsRes.status === "fulfilled") {
        const sc = rowsRes.value.status_counts ?? {};
        totalRows = sc["Tất cả"] ?? rowsRes.value.total ?? $project.row_count ?? 0;
        doneCount = sc["Đã tạo video"] ?? 0;
        errorRows =
          (sc["Lỗi tạo image"] ?? 0) +
          (sc["Lỗi tạo video"] ?? 0) +
          (sc["Lỗi tạm thời"] ?? 0);
        skippedCount = sc["Bỏ qua"] ?? 0;
      } else {
        errMsg = formatApiError(rowsRes.reason);
      }

      if (drRes.status === "fulfilled") {
        dryRun = drRes.value;
        pendingCount = drRes.value.total;
      } else if (!errMsg) {
        errMsg = formatApiError(drRes.reason);
      }

      loadError = errMsg;
    } catch (e) {
      if (seq === refreshSeq) {
        loadError = formatApiError(e);
      }
    }
  }

  async function pickOnboardingFolder(): Promise<void> {
    if ($running || onboardingPicking) return;
    onboardingPicking = true;
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
      // Mục 6.7b: nếu đã đủ cấu trúc và prompts.csv hợp lệ thì mở thẳng không hỏi ghi đè
      if (probe.has_full_structure && (probe.prompts_valid ?? true)) {
        beginProjectSwitch();
        const info = await api.load_project(dir);
        resetProjectRunState();
        project.set(info);
        rowChangedTick.update((n) => n + 1);
        showToast("success", `Đã mở dự án "${info.name}" (${info.row_count} dòng).`);
        return;
      }
      probedPath = dir;
      probeInfo = probe;
      probeMode = "giu_lai";
      probeModalOpen = true;
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      onboardingPicking = false;
    }
  }

  function joinCsvPath(base: string): string {
    const sep = base.includes("\\") ? "\\" : "/";
    return `${base.replace(/[\\/]+$/, "")}${sep}prompts.csv`;
  }

  async function revealProbedCsv(): Promise<void> {
    if (!probedPath) return;
    try {
      await api.open_path_in_explorer(joinCsvPath(probedPath), true);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function confirmOnboardingCreate(): Promise<void> {
    if (!probedPath || !probeInfo || probeApplying) return;
    probeApplying = true;
    beginProjectSwitch();
    try {
      const info =
        probeInfo.has_full_structure && probeInfo.prompts_valid !== false && probeMode === "giu_lai"
          ? await api.load_project(probedPath)
          : await api.create_project(probedPath, probeMode);
      resetProjectRunState();
      project.set(info);
      rowChangedTick.update((n) => n + 1);
      probeModalOpen = false;
      showToast("success", `Đã sẵn sàng dự án "${info.name}" (${info.row_count} dòng).`);
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      probeApplying = false;
    }
  }

  async function revealPromptsCsv(): Promise<void> {
    if (!$project) return;
    const base = $project.path.replace(/[\\/]+$/, "");
    try {
      await api.open_path_in_explorer(`${base}/prompts.csv`, true);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function doDryRun(): Promise<void> {
    if (!$project || $running) return;
    dryRunLoading = true;
    try {
      dryRun = await api.start_dry_run();
      pendingCount = dryRun.total;
      loadError = null;
      dryRunModalOpen = true;
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      dryRunLoading = false;
    }
  }

  async function handleStartClick(): Promise<void> {
    if (!$project || $running || starting) return;
    starting = true;
    try {
      const freshDryRun = await api.start_dry_run();
      dryRun = freshDryRun;
      pendingCount = freshDryRun.total;
      if (freshDryRun.total === 0) {
        showToast("info", "Không có dòng nào đang chờ xử lý.");
        return;
      }
      if ($confirmStartEnabled || freshDryRun.is_csv_locked) {
        dontAskAgain = false;
        confirmModalOpen = true;
        return;
      }
      await executeStartRun();
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      starting = false;
    }
  }

  async function confirmAndStart(): Promise<void> {
    if (starting || $running) return;
    starting = true;
    try {
      if (dontAskAgain && $confirmStartEnabled) {
        try {
          const view = await api.get_settings();
          const updated = { ...view.settings, confirm_start: false };
          await api.save_settings(updated);
          confirmStartEnabled.set(false);
        } catch {
          // Không chặn luồng chạy nếu lưu tùy chọn phụ thất bại
        }
      }
      confirmModalOpen = false;
      dryRunModalOpen = false;
      await api.start_run("tat_ca");
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      starting = false;
    }
  }

  async function executeStartRun(): Promise<void> {
    if ($running || !$project) return;
    starting = true;
    try {
      dryRunModalOpen = false;
      await api.start_run("tat_ca");
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      starting = false;
    }
  }

  async function stopRun(): Promise<void> {
    if (!$running || stopping) return;
    stopping = true;
    try {
      await api.stop_run();
      showToast("info", "Đang dừng an toàn sau khi bước hiện tại hoàn tất...");
    } catch (e) {
      stopping = false;
      showToast("error", formatApiError(e));
    }
  }

  function openErrorRows(): void {
    pendingRowsFilter.set("Lỗi");
    dispatch("navigate", "rows");
  }

  function formatStepBadge(step: string | undefined | null): string {
    if (!step) return "Đang chuẩn bị";
    if (step === "B2") return "B2: Tạo ảnh";
    if (step === "B3") return "B3: Tạo video";
    if (step === "download") return "B3: Tải video";
    return step;
  }

  function formatDurationMs(ms: number | undefined | null): string {
    if (typeof ms !== "number" || !Number.isFinite(ms) || ms < 0) return "";
    const totalSec = Math.round(ms / 1000);
    const m = Math.floor(totalSec / 60);
    const s = totalSec % 60;
    return m > 0 ? `${m}m ${s}s` : `${s}s`;
  }

  function groupBadgeClass(s: string): string {
    if (s === "Đang chờ") return "badge-cho";
    if (s === "Đã tạo image") return "badge-image";
    if (s === "Đã tạo video") return "badge-video";
    if (s === "Bỏ qua") return "badge-bo-qua";
    if (s === "Lỗi tạo image") return "badge-loi-image";
    if (s === "Lỗi tạm thời") return "badge-loi-tam-thoi";
    if (s.startsWith("Lỗi")) return "badge-loi-video";
    return "badge-cho";
  }
</script>

<div class="dash-screen">
  {#if !$project}
    <!-- Trạng thái rỗng Onboarding 3 bước (mục 6.2 & 6.7b) -->
    <section class="card onboarding-card">
      <div class="onboarding-hero">
        <div class="hero-icon">
          <Icons name="sparkles" size={24} />
        </div>
        <div>
          <span class="hero-kicker">Bắt đầu nhanh · Local-only</span>
          <h2 class="hero-title">Chào mừng bạn đến với VideoFlow Desktop</h2>
          <p class="hero-desc">
            Toàn bộ ảnh tham chiếu, prompt và video đầu ra đều nằm trực tiếp trong thư mục dự án trên máy tính của bạn. Hoàn thành 3 bước dưới đây để chạy lượt đầu tiên:
          </p>
        </div>
      </div>

      <div class="onboarding-steps">
        <div class="step-card">
          <div class="step-num">1</div>
          <div class="step-title">Chọn thư mục dự án Local</div>
          <p class="step-desc">
            Chọn một thư mục trên máy. Ứng dụng tự tạo đủ cấu trúc chuẩn <code>input/</code>, <code>image_out/</code>, <code>video_out/</code> và file mẫu <code>prompts.csv</code>.
          </p>
        </div>

        <div class="step-card">
          <div class="step-top-row">
            <div class="step-num">2</div>
            {#if $endpointStatus.imageConfigured && $endpointStatus.videoConfigured}
              <span class="badge badge-video">
                <Icons name="check" size={12} /> Đã lưu khóa
              </span>
            {:else}
              <span class="badge badge-sua-prompt">Cần thiết lập</span>
            {/if}
          </div>
          <div class="step-title">Nhập API Key ảnh &amp; video</div>
          <p class="step-desc">
            Cấu hình endpoint và API key cho bước Tạo ảnh (B2) &amp; Tạo video (B3). Khóa được bảo vệ trong Windows Credential Manager.
          </p>
        </div>

        <div class="step-card">
          <div class="step-num">3</div>
          <div class="step-title">Kiểm tra &amp; Bấm START</div>
          <p class="step-desc">
            Đặt ảnh tham chiếu vào <code>input/</code>, điền prompt trong <code>prompts.csv</code>, bấm <strong>Chạy thử</strong> để đối chiếu rồi nhấn <strong>START</strong>.
          </p>
        </div>
      </div>

      <div class="onboarding-actions">
        <button
          type="button"
          class="btn-primary"
          disabled={onboardingPicking}
          on:click={pickOnboardingFolder}
        >
          <Icons name="folder-open" size={17} />
          {onboardingPicking ? "Đang chọn thư mục..." : "Chọn thư mục dự án..."}
        </button>
        <button
          type="button"
          class="btn-secondary"
          on:click={() => dispatch("navigate", "settings")}
        >
          <Icons name="settings" size={16} />
          Thiết lập API Key
        </button>
        <button
          type="button"
          class="btn-ghost"
          on:click={() => dispatch("navigate", "folder")}
        >
          Dự án mở gần đây
        </button>
      </div>
    </section>
  {:else}
    {#if dryRun?.is_csv_locked}
      <div class="warn-banner selectable" role="status">
        <div class="warn-banner-text">
          <Icons name="alert" size={18} />
          <span>
            <strong>prompts.csv đang mở trong ứng dụng khác (Excel)</strong> — hãy đóng file trước khi chạy để cập nhật tức thì.
          </span>
        </div>
        <button type="button" class="btn-secondary small-btn" on:click={refresh}>
          <Icons name="refresh" size={14} />
          Kiểm tra lại
        </button>
      </div>
    {/if}

    {#if loadError}
      <div class="err-banner selectable" role="alert">
        <div class="err-banner-main">
          <Icons name="alert" size={18} />
          <div>
            <div class="err-banner-title">Không nạp được dữ liệu dự án</div>
            <div class="err-banner-msg">{loadError}</div>
          </div>
        </div>
        <div class="err-banner-actions">
          <button type="button" class="btn-primary small-btn" on:click={refresh}>
            Thử lại
          </button>
          <button type="button" class="btn-secondary small-btn" on:click={revealPromptsCsv}>
            <Icons name="folder-open" size={13} />
            Mở prompts.csv
          </button>
          <button
            type="button"
            class="btn-secondary small-btn"
            on:click={() => dispatch("navigate", "folder")}
          >
            Kiểm tra thư mục
          </button>
        </div>
      </div>
    {/if}

    <!-- Hàng 3 thẻ nguồn (mục 6.2) -->
    <div class="source-row">
      <div class="card src-card">
        <div class="src-label">Dự án Local</div>
        <div class="src-value selectable" title={$project.path}>
          {$project.name}
        </div>
        <button
          type="button"
          class="btn-secondary small"
          disabled={$running}
          title={$running ? "Dừng lượt chạy trước khi đổi dự án" : "Đổi dự án khác"}
          on:click={() => dispatch("navigate", "folder")}
        >
          Đổi dự án
        </button>
      </div>

      <div class="card src-card">
        <div class="src-label">Cấu trúc thư mục</div>
        <div
          class="src-value status-line"
          title={loadError
            ? loadError
            : structOk
              ? "Đầy đủ input/, prompts.csv, image_out/, video_out/ và quyền ghi"
              : `Thiếu: ${missingChecklist}`}
        >
          <span class="status-dot" class:ok={structOk} class:bad={!structOk}></span>
          <span>
            {structOk
              ? "Đủ 5 mục kiểm tra"
              : loadError
                ? "Lỗi đọc prompts.csv"
                : "Thiếu thành phần"}
          </span>
        </div>
        <button
          type="button"
          class="btn-secondary small"
          on:click={() => dispatch("navigate", "folder")}
        >
          Xem kiểm tra
        </button>
      </div>

      <div class="card src-card">
        <div class="src-label">Nguồn dòng việc</div>
        <div
          class="src-value"
          title={`Tổng ${totalRows} dòng · ${pendingCount} dòng chờ xử lý${skippedCount > 0 ? ` · ${skippedCount} dòng bỏ qua` : ""}`}
        >
          prompts.csv · {totalRows} dòng
        </div>
        <button
          type="button"
          class="btn-secondary small"
          on:click={() => dispatch("navigate", "rows")}
        >
          Mở dòng việc
        </button>
      </div>
    </div>

    <!-- Hàng 4 thẻ số liệu -->
    <div class="metric-row">
      <div class="card metric">
        <div class="metric-top">
          <span class="metric-label">Dòng chờ xử lý</span>
          <span class="metric-icon teal"><Icons name="rows" size={16} /></span>
        </div>
        <div class="metric-num">{pendingCount}</div>
      </div>

      <div class="card metric">
        <div class="metric-top">
          <span class="metric-label">Đã xong</span>
          <span class="metric-icon green"><Icons name="check" size={16} /></span>
        </div>
        <div class="metric-num ok">{doneCount}</div>
      </div>

      <button
        type="button"
        class="card metric clickable"
        class:has-errors={errorRows > 0}
        on:click={openErrorRows}
        title={errorRows > 0 ? "Nhấn để mở bảng Dòng việc lọc sẵn tất cả dòng lỗi" : "Xem bảng Dòng việc"}
      >
        <div class="metric-top">
          <span class="metric-label">Dòng lỗi cần xem</span>
          <span class="metric-icon" class:danger={errorRows > 0}>
            <Icons name="alert" size={16} />
          </span>
        </div>
        <div class="metric-num" class:danger-num={errorRows > 0}>{errorRows}</div>
      </button>

      <div class="card metric" class:running-card={$running}>
        <div class="metric-top">
          <span class="metric-label">Đang chạy</span>
          <span class="metric-icon" class:teal={$running}>
            <Icons name="play" size={16} />
          </span>
        </div>
        <div class="metric-num" class:teal-num={$running}>{activeWorkerCount}</div>
      </div>
    </div>

    <!-- Khối tiến độ trực tiếp (mục 6.14 + hiển thị luồng song song 1..3) -->
    <div class="card progress-block" class:active-run={$running}>
      {#if $running}
        <div class="progress-head">
          <div class="progress-head-left">
            <span class="pulse-dot"></span>
            <span class="progress-title">
              Lượt chạy hiện tại{$currentRun?.run_id || $currentRun?.id ? ` #${$currentRun?.run_id ?? $currentRun?.id}` : ""}
            </span>
            {#if activeEntries.length <= 1 && $progressStep?.step}
              <span class="badge badge-image">{formatStepBadge($progressStep.step)}</span>
            {/if}
            {#if activeEntries.length <= 1 && $progressStep?.rowKey}
              <span class="row-pill selectable" title="Dòng đang xử lý">
                {$progressStep.rowKey}
              </span>
            {/if}
          </div>
          <span class="progress-pct">{Math.floor($progressPercent)}%</span>
        </div>

        <div class="progress-bar">
          <div style="width: {$progressPercent}%"></div>
        </div>

        {#if activeEntries.length > 1}
          <div class="concurrent-pills">
            {#each activeEntries as [rKey, info] (rKey)}
              <span class="worker-pill selectable" title={info.detail}>
                <span class="worker-step">{formatStepBadge(info.step)}</span>
                <strong>{rKey}</strong>
                {#if info.detail}
                  <span class="worker-detail">· {info.detail}</span>
                {/if}
              </span>
            {/each}
          </div>
        {:else}
          <div class="progress-detail selectable">
            {$progressText || "Đang nạp danh sách dòng việc..."}
          </div>
        {/if}
      {:else if $lastRunFinished}
        <div class="progress-head">
          <div class="progress-head-left">
            <span class="progress-title">
              Lượt vừa kết thúc{$lastRunFinished.run_id ? ` #${$lastRunFinished.run_id}` : ""}
            </span>
            <span
              class="badge"
              class:badge-video={$lastRunFinished.status === "hoan_tat" && ($lastRunFinished.error_count ?? 0) === 0}
              class:badge-sua-prompt={$lastRunFinished.status === "da_dung"}
              class:badge-loi-video={$lastRunFinished.status === "gian_doan" || (($lastRunFinished.error_count ?? 0) > 0 && $lastRunFinished.status !== "da_dung")}
            >
              {$lastRunFinished.status === "da_dung"
                ? "Đã dừng an toàn"
                : $lastRunFinished.status === "gian_doan"
                  ? "Gián đoạn"
                  : ($lastRunFinished.error_count ?? 0) > 0
                    ? "Hoàn tất (có lỗi)"
                    : "Hoàn tất"}
            </span>
          </div>
          <div class="progress-links">
            {#if ($lastRunFinished.error_count ?? 0) > 0}
              <button type="button" class="btn-ghost" on:click={openErrorRows}>
                Xem {$lastRunFinished.error_count} dòng lỗi
              </button>
            {/if}
            <button
              type="button"
              class="btn-ghost"
              on:click={() => dispatch("navigate", "history")}
            >
              Mở Lịch sử
            </button>
          </div>
        </div>
        <div class="progress-bar">
          <div style="width: 100%"></div>
        </div>
        <div class="progress-detail selectable">
          Đã xử lý xong: <strong>{$lastRunFinished.done_count ?? 0}</strong> dòng thành công ·
          <strong>{$lastRunFinished.error_count ?? 0}</strong> dòng lỗi
          {#if $lastRunFinished.duration_ms}
            · Thời lượng {formatDurationMs($lastRunFinished.duration_ms)}
          {/if}
        </div>
      {:else}
        <div class="idle-progress">
          <div class="progress-title muted">Chưa có lượt chạy nào trong phiên này</div>
          <div class="progress-detail">
            Bấm <strong>Chạy thử (không ghi)</strong> để rà soát danh sách hoặc nhấn <strong>START</strong> để bắt đầu xử lý các dòng đang chờ.
          </div>
        </div>
      {/if}
    </div>

    <!-- Cụm điều khiển chính Start / Stop / Chạy thử -->
    <div class="action-row">
      <button
        type="button"
        class="btn-start"
        disabled={$running || starting || !$project || pendingCount === 0 || !!loadError}
        title={!$project
          ? "Chưa chọn thư mục dự án"
          : loadError
            ? "Sửa lỗi nạp dữ liệu trước khi chạy"
            : pendingCount === 0
              ? "Không có dòng nào đang chờ xử lý"
              : `Bắt đầu chạy ${pendingCount} dòng`}
        on:click={handleStartClick}
      >
        <Icons name="play" size={18} />
        {#if $running}
          Đang chạy…
        {:else if pendingCount === 0}
          Không có dòng chờ
        {:else}
          START - Chạy {pendingCount} dòng
        {/if}
      </button>

      <button
        type="button"
        class="btn-danger stop"
        disabled={!$running || stopping}
        title={$running ? "Dừng an toàn sau khi bước đang dở hoàn tất" : "Hiện không có lượt chạy nào"}
        on:click={stopRun}
      >
        <Icons name="stop" size={16} />
        {stopping ? "Đang dừng..." : "STOP"}
      </button>

      <button
        type="button"
        class="btn-secondary tall"
        disabled={$running || dryRunLoading || !$project}
        on:click={doDryRun}
      >
        <Icons name="beaker" size={16} />
        {dryRunLoading ? "Đang quét..." : "Chạy thử (không ghi)"}
      </button>

      <div class="action-spacer"></div>

      <button
        type="button"
        class="btn-ghost"
        disabled={$running}
        on:click={refresh}
        title="Làm mới số liệu tổng quan"
      >
        <Icons name="refresh" size={15} />
        Làm mới
      </button>
    </div>
  {/if}
</div>

<!-- Modal khởi tạo / mở thư mục từ Onboarding (6.7b) -->
<Modal
  open={probeModalOpen}
  title={probeInfo?.is_empty_dir ? "Khởi tạo dự án mới" : "Xác nhận cấu trúc dự án"}
  width="500px"
  disableClose={probeApplying}
  on:close={() => (probeModalOpen = false)}
>
  {#if probeInfo}
    <div class="modal-stack">
      <div class="summary-kv">
        <span class="kv-label">Thư mục đã chọn:</span>
        <strong class="selectable">{probedPath}</strong>
      </div>

      {#if probeInfo.is_empty_dir}
        <p class="dryrun-summary">
          Thư mục đang trống. Ứng dụng sẽ tự động khởi tạo <code>input/</code>, <code>image_out/</code>, <code>video_out/</code>, <code>prompts.csv</code> mẫu và <code>state.db</code>.
        </p>
      {:else}
        {#if probeInfo.prompts_error}
          <div class="modal-csv-warn selectable">
            <Icons name="alert" size={17} />
            <div>
              <strong>Phát hiện vấn đề trong prompts.csv:</strong> {probeInfo.prompts_error}
              <div style="margin-top: 6px;">
                <button type="button" class="btn-secondary small-btn" on:click={revealProbedCsv}>
                  <Icons name="folder-open" size={13} />
                  Hiện prompts.csv trong Explorer
                </button>
              </div>
            </div>
          </div>
        {/if}
        <div class="mode-list">
          <label class="mode-option" class:selected={probeMode === "giu_lai"}>
            <input type="radio" name="dashProbeMode" value="giu_lai" bind:group={probeMode} />
            <div>
              <div class="mode-option-title">Giữ lại dữ liệu hiện có (Khuyên dùng)</div>
              <div class="mode-option-sub">
                Giữ nguyên dữ liệu và sản phẩm đã có; chỉ tạo bổ sung các thư mục/file còn thiếu.
              </div>
            </div>
          </label>
          <label class="mode-option" class:selected={probeMode === "ghi_de"}>
            <input type="radio" name="dashProbeMode" value="ghi_de" bind:group={probeMode} />
            <div>
              <div class="mode-option-title">Tạo mới từ đầu (sao lưu prompts.csv cũ)</div>
              <div class="mode-option-sub">
                Đổi tên <code>prompts.csv</code> cũ sang bản sao lưu, làm mới <code>state.db</code>; không xóa ảnh/video trên đĩa.
              </div>
            </div>
          </label>
        </div>
      {/if}
    </div>
  {/if}

  <div slot="footer" class="modal-footer-bar">
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
      class={probeMode === "ghi_de" && !probeInfo?.is_empty_dir ? "btn-danger" : "btn-primary"}
      disabled={probeApplying}
      on:click={confirmOnboardingCreate}
    >
      {probeApplying
        ? "Đang xử lý..."
        : probeInfo?.is_empty_dir
          ? "Khởi tạo dự án"
          : probeMode === "ghi_de"
            ? "Sao lưu & Tạo mới"
            : "Giữ lại & Mở dự án"}
    </button>
  </div>
</Modal>

<!-- Modal Xác nhận trước khi START (mục 6.8 + TC-44) -->
<Modal
  open={confirmModalOpen}
  title="Xác nhận trước khi chạy"
  width="540px"
  disableClose={starting}
  on:close={() => (confirmModalOpen = false)}
>
  <div class="modal-stack">
    <div class="summary-kv">
      <div>
        <span class="kv-label">Dự án Local:</span>
        <strong class="selectable">{$project?.name ?? ""}</strong>
      </div>
      <div>
        <span class="kv-label">Số dòng sẽ xử lý:</span>
        <strong>{dryRun?.total ?? pendingCount} dòng</strong>
      </div>
    </div>

    {#if dryRun && dryRun.groups.length > 0}
      <div class="group-box">
        <div class="group-box-title">Phân nhóm trạng thái sẽ chạy</div>
        {#each dryRun.groups as g}
          <div class="dryrun-row">
            <span class="badge {groupBadgeClass(g.status)}">
              {g.status} ({g.count})
            </span>
            <span class="dryrun-keys selectable">
              {g.row_keys.slice(0, 5).join(", ")}{g.row_keys.length > 5
                ? ` … +${g.row_keys.length - 5} dòng`
                : ""}
            </span>
          </div>
        {/each}
      </div>
    {/if}

    <div class="endpoint-box">
      <div class="group-box-title">Trạng thái khóa Endpoint</div>
      <div class="ep-pills">
        <span class="ep-pill" class:ok={$endpointStatus.imageConfigured} class:missing={!$endpointStatus.imageConfigured}>
          <Icons name={$endpointStatus.imageConfigured ? "check" : "alert"} size={13} />
          Endpoint ảnh: {$endpointStatus.imageConfigured ? "Đã cấu hình key" : "Chưa có API key"}
        </span>
        <span class="ep-pill" class:ok={$endpointStatus.videoConfigured} class:missing={!$endpointStatus.videoConfigured}>
          <Icons name={$endpointStatus.videoConfigured ? "check" : "alert"} size={13} />
          Endpoint video: {$endpointStatus.videoConfigured ? "Đã cấu hình key" : "Chưa có API key"}
        </span>
      </div>
      {#if !$endpointStatus.imageConfigured || !$endpointStatus.videoConfigured}
        <div class="ep-warn-note">
          Chưa lưu đủ API key — lượt chạy có thể dừng với lỗi xác thực.
          <button
            type="button"
            class="inline-link"
            on:click={() => {
              confirmModalOpen = false;
              dispatch("navigate", "settings");
            }}
          >
            Mở Cài đặt
          </button>
        </div>
      {/if}
    </div>

    {#if dryRun?.is_csv_locked}
      <div class="modal-csv-warn selectable">
        <Icons name="alert" size={18} />
        <div>
          <strong>prompts.csv đang bị khóa ghi</strong> (ví dụ đang mở trong Excel) — kết quả sẽ không ghi được ngay vào CSV, ứng dụng vẫn lưu vào <code>state.db</code> và đồng bộ bổ sung sau. Hãy đóng Excel nếu có thể.
        </div>
      </div>
    {/if}

    {#if $confirmStartEnabled}
      <label class="dont-ask-row" for="confirm-dont-ask-again">
        <input
          id="confirm-dont-ask-again"
          type="checkbox"
          bind:checked={dontAskAgain}
        />
        <span>Không hỏi lại ở các lần sau (có thể bật lại trong Cài đặt)</span>
      </label>
    {/if}
  </div>

  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={starting}
      on:click={() => (confirmModalOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={$running || starting}
      on:click={confirmAndStart}
    >
      <Icons name="play" size={15} />
      Xác nhận chạy
    </button>
  </div>
</Modal>

<!-- Modal Kết quả Chạy thử / Dry-run 560px (mục 6.2 + TC-41) -->
<Modal
  open={dryRunModalOpen}
  title="Kết quả chạy thử (không ghi dữ liệu)"
  width="560px"
  disableClose={starting}
  on:close={() => (dryRunModalOpen = false)}
>
  {#if dryRun}
    <div class="modal-stack">
      <p class="dryrun-summary">
        Đã quét dự án <strong>{$project?.name ?? ""}</strong>: có
        <strong>{dryRun.total} dòng</strong> đủ điều kiện xử lý ở lượt tiếp theo (chưa gọi API, không thay đổi file).
      </p>

      {#if dryRun.groups.length > 0}
        <div class="group-box">
          {#each dryRun.groups as g}
            <div class="dryrun-row">
              <span class="badge {groupBadgeClass(g.status)}">
                {g.status} ({g.count})
              </span>
              <span class="dryrun-keys selectable">
                {g.row_keys.slice(0, 6).join(", ")}{g.row_keys.length > 6
                  ? ` … +${g.row_keys.length - 6} dòng`
                  : ""}
              </span>
            </div>
          {/each}
        </div>
      {:else}
        <div class="empty-dryrun">
          Tất cả các dòng trong <code>prompts.csv</code> đã hoàn tất hoặc đang ở trạng thái bỏ qua.
        </div>
      {/if}

      {#if dryRun.is_csv_locked}
        <div class="modal-csv-warn selectable">
          <Icons name="alert" size={18} />
          <div>
            <strong>prompts.csv đang mở trong ứng dụng khác (Excel)</strong> — hãy đóng file trước khi chạy thật để cập nhật tức thì.
          </div>
        </div>
      {/if}
    </div>
  {/if}

  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={starting}
      on:click={() => (dryRunModalOpen = false)}
    >
      Đóng
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={$running || starting || (dryRun?.total ?? 0) === 0}
      on:click={executeStartRun}
    >
      <Icons name="play" size={15} />
      Chạy thật luôn
    </button>
  </div>
</Modal>

<style>
  .dash-screen {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  /* Onboarding Empty State */
  .onboarding-card {
    padding: 22px 24px;
    display: flex;
    flex-direction: column;
    gap: 18px;
    background: linear-gradient(180deg, #ffffff 0%, #f8fbfc 100%);
  }
  .onboarding-hero {
    display: flex;
    align-items: flex-start;
    gap: 16px;
  }
  .hero-icon {
    width: 46px;
    height: 46px;
    border-radius: 12px;
    background: var(--c-teal-light);
    color: var(--c-teal);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
  }
  .hero-kicker {
    display: inline-block;
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--c-teal);
    margin-bottom: 3px;
  }
  .hero-title {
    font-size: 18px;
    font-weight: 700;
    color: var(--c-text);
    margin-bottom: 4px;
  }
  .hero-desc {
    font-size: var(--fs-body);
    color: var(--c-muted);
    max-width: 720px;
  }
  .onboarding-steps {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 14px;
  }
  .step-card {
    background: var(--c-card);
    border: 1px solid var(--c-border);
    border-radius: 10px;
    padding: 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  .step-top-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .step-num {
    width: 26px;
    height: 26px;
    border-radius: 50%;
    background: var(--c-teal);
    color: #fff;
    font-size: 13px;
    font-weight: 700;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .step-title {
    font-size: 14px;
    font-weight: 700;
    color: var(--c-text);
  }
  .step-desc {
    font-size: var(--fs-label);
    color: var(--c-muted);
    line-height: 1.48;
  }
  .step-desc code {
    background: var(--c-surface-subtle);
    padding: 1px 5px;
    border-radius: 4px;
    font-size: 12px;
    color: var(--c-text);
  }
  .onboarding-actions {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }

  /* Cảnh báo khóa CSV & lỗi nạp */
  .warn-banner {
    background: var(--c-warn-bg);
    border: 1px solid #f4d39b;
    color: var(--c-warn-dark);
    border-radius: 10px;
    padding: 9px 14px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    font-size: var(--fs-label);
  }
  .warn-banner-text {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .err-banner {
    background: var(--c-danger-bg);
    border: 1px solid #efb9b4;
    color: var(--c-danger);
    border-radius: 10px;
    padding: 10px 14px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
  }
  .err-banner-main {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    flex: 1;
    min-width: 240px;
  }
  .err-banner-title {
    font-weight: 700;
    font-size: var(--fs-body);
  }
  .err-banner-msg {
    font-size: var(--fs-label);
    margin-top: 2px;
  }
  .err-banner-actions {
    display: flex;
    gap: 8px;
    flex-shrink: 0;
    flex-wrap: wrap;
  }
  .small-btn {
    height: 32px;
    padding: 0 12px;
    font-size: var(--fs-label);
    gap: 5px;
  }

  /* Thẻ nguồn (3 cột ngang ở >= 820px, gồm cả 1024x640) */
  .source-row {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 14px;
  }
  .src-card {
    min-height: 64px;
    padding: 10px 112px 10px 16px;
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: 2px;
    position: relative;
    min-width: 0;
  }
  .src-card .btn-secondary.small {
    height: 28px;
    padding: 0 10px;
    font-size: 12px;
    position: absolute;
    right: 12px;
    top: 50%;
    transform: translateY(-50%);
  }
  .src-card .btn-secondary.small:active:not(:disabled) {
    transform: translateY(calc(-50% + 1px));
  }
  .src-label {
    font-size: var(--fs-hint);
    color: var(--c-muted);
  }
  .src-value {
    font-weight: 600;
    font-size: var(--fs-body);
    color: var(--c-text);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .src-value.status-line {
    display: flex;
    align-items: center;
    gap: 7px;
  }
  .src-value.status-line > span:last-child {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .status-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
    background: var(--c-border-strong);
  }
  .status-dot.ok {
    background: var(--c-green);
  }
  .status-dot.bad {
    background: var(--c-danger);
  }

  /* Thẻ số liệu (4 cột ngang ở >= 820px) */
  .metric-row {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 14px;
  }
  .metric {
    min-height: 84px;
    padding: 14px 16px;
    display: flex;
    flex-direction: column;
    justify-content: space-between;
    align-items: stretch;
    text-align: left;
    gap: 6px;
  }
  button.metric.clickable {
    width: 100%;
    font-weight: normal;
    white-space: normal;
    cursor: pointer;
  }
  button.metric.clickable:hover {
    border-color: var(--c-teal);
    box-shadow: var(--shadow-hover);
  }
  button.metric.clickable.has-errors {
    border-color: #efb9b4;
    background: linear-gradient(180deg, #ffffff 0%, #fff8f7 100%);
  }
  button.metric.clickable.has-errors:hover {
    border-color: var(--c-danger);
  }
  .metric.running-card {
    border-color: #b7dde1;
    background: linear-gradient(180deg, #ffffff 0%, #f2fafb 100%);
  }
  .metric-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    width: 100%;
  }
  .metric-label {
    font-size: var(--fs-hint);
    font-weight: 600;
    color: var(--c-muted);
  }
  .metric-icon {
    width: 26px;
    height: 26px;
    border-radius: 7px;
    background: var(--c-surface-subtle);
    color: var(--c-muted);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
  }
  .metric-icon.teal {
    background: var(--c-teal-light);
    color: var(--c-teal);
  }
  .metric-icon.green {
    background: var(--c-green-bg);
    color: var(--c-green);
  }
  .metric-icon.danger {
    background: var(--c-danger-bg);
    color: var(--c-danger);
  }
  .metric-num {
    font-size: var(--fs-big-number);
    font-weight: 700;
    line-height: 1.1;
    color: var(--c-text);
    font-variant-numeric: tabular-nums;
  }
  .metric-num.ok {
    color: var(--c-green);
  }
  .metric-num.danger-num {
    color: var(--c-danger);
  }
  .metric-num.teal-num {
    color: var(--c-teal);
  }

  /* Khối tiến độ & pill song song */
  .progress-block {
    min-height: 70px;
    padding: 13px 16px;
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: 7px;
  }
  .progress-block.active-run {
    border-color: #b7dde1;
  }
  .progress-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
  }
  .progress-head-left {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .pulse-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--c-teal);
    box-shadow: 0 0 0 3px rgba(14, 124, 134, 0.2);
  }
  .progress-title {
    font-weight: 700;
    font-size: var(--fs-body);
    color: var(--c-text);
  }
  .progress-title.muted {
    color: var(--c-muted);
    font-weight: 600;
  }
  .row-pill {
    font-size: 12px;
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 6px;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    color: var(--c-text);
  }
  .concurrent-pills {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 2px;
  }
  .worker-pill {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    padding: 3px 9px;
    border-radius: 999px;
    background: var(--c-teal-subtle);
    border: 1px solid #b7dde1;
    color: var(--c-text);
    max-width: 100%;
  }
  .worker-step {
    font-size: 11px;
    font-weight: 700;
    color: var(--c-teal-dark);
  }
  .worker-detail {
    color: var(--c-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 220px;
  }
  .progress-pct {
    font-size: var(--fs-body);
    font-weight: 700;
    color: var(--c-teal);
  }
  .progress-links {
    display: flex;
    align-items: center;
    gap: 4px;
  }
  .progress-detail {
    font-size: var(--fs-label);
    color: var(--c-muted);
  }
  .idle-progress {
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  /* Hàng nút hành động chính */
  .action-row {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
  }
  .action-spacer {
    flex: 1;
  }
  .btn-danger.stop {
    height: var(--h-btn-start);
    min-width: 124px;
  }
  .btn-secondary.tall {
    height: var(--h-btn-start);
  }

  /* Nội dung Modal */
  .modal-stack {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .summary-kv {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
    background: var(--c-surface-subtle);
    padding: 10px 12px;
    border-radius: 8px;
    font-size: var(--fs-label);
    word-break: break-all;
  }
  .kv-label {
    color: var(--c-muted);
    margin-right: 4px;
  }
  .mode-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .mode-option {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 10px 12px;
    border: 1px solid var(--c-border);
    border-radius: 8px;
    cursor: pointer;
    background: #fff;
  }
  .mode-option.selected {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .mode-option input {
    margin-top: 3px;
  }
  .mode-option-title {
    font-weight: 600;
    font-size: var(--fs-label);
  }
  .mode-option-sub {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    margin-top: 2px;
  }
  .group-box,
  .endpoint-box {
    border: 1px solid var(--c-border);
    border-radius: 8px;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .group-box-title {
    font-size: var(--fs-hint);
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--c-muted);
  }
  .dryrun-summary {
    font-size: var(--fs-body);
    color: var(--c-text);
  }
  .dryrun-row {
    font-size: var(--fs-label);
    color: var(--c-muted);
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .dryrun-keys {
    color: var(--c-text);
    word-break: break-all;
  }
  .empty-dryrun {
    padding: 14px;
    border-radius: 8px;
    background: var(--c-surface-subtle);
    color: var(--c-muted);
    font-size: var(--fs-label);
    text-align: center;
  }
  .ep-pills {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .ep-pill {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    font-size: 12px;
    font-weight: 600;
    padding: 4px 10px;
    border-radius: 999px;
  }
  .ep-pill.ok {
    background: var(--c-green-bg);
    color: var(--c-green);
  }
  .ep-pill.missing {
    background: var(--c-warn-bg);
    color: var(--c-warn-dark);
  }
  .ep-warn-note {
    font-size: 12px;
    color: var(--c-warn-dark);
  }
  .inline-link {
    background: none;
    border: none;
    padding: 0 2px;
    font-size: 12px;
    font-weight: 700;
    color: var(--c-teal);
    text-decoration: underline;
    height: auto;
  }
  .modal-csv-warn {
    background: var(--c-warn-bg);
    border: 1px solid #f4d39b;
    color: var(--c-warn-dark);
    border-radius: 8px;
    padding: 10px 12px;
    display: flex;
    align-items: flex-start;
    gap: 10px;
    font-size: var(--fs-label);
  }
  .dont-ask-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--fs-label);
    color: var(--c-muted);
    cursor: pointer;
  }
  .modal-footer-bar {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    width: 100%;
  }

  @media (max-width: 820px) {
    .onboarding-steps {
      grid-template-columns: 1fr;
    }
    .source-row {
      grid-template-columns: 1fr;
    }
    .metric-row {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }
</style>
