<script lang="ts">
  // Màn hình Lịch sử (Wave 2 Full Option): lọc trạng thái/loại/ngày, Xuất CSV theo bộ lọc, Nhật ký + Xuất .log, Xem media trong app & phân trang chi tiết
  import { onMount } from "svelte";
  import {
    project,
    running,
    rowChangedTick,
    assetCacheBust,
    showToast,
    formatApiError,
  } from "../lib/store";
  import {
    api,
    toAssetUrl,
    type RunSummary,
    type RowSummary,
    type LogItem,
    type HistoryFilter,
  } from "../lib/api";
  import Icons from "../lib/Icons.svelte";
  import Modal from "../lib/Modal.svelte";

  const runStatusChips: { label: string; value: string | undefined }[] = [
    { label: "Tất cả", value: undefined },
    { label: "Hoàn tất", value: "hoan_tat" },
    { label: "Đã dừng", value: "da_dung" },
    { label: "Gián đoạn", value: "gian_doan" },
  ];

  const kindOptions: { label: string; value: string }[] = [
    { label: "Loại: Tất cả", value: "" },
    { label: "Chạy chung", value: "chung" },
    { label: "Chạy riêng", value: "rieng" },
    { label: "Theo lịch", value: "lich" },
    { label: "Làm lại", value: "lam_lai" },
  ];

  const detailRowStatuses = [
    "Tất cả",
    "Đang chờ",
    "Đã tạo image",
    "Đã tạo video",
    "Lỗi tạo image",
    "Lỗi tạo video",
    "Bỏ qua",
  ];

  const logLevels: { label: string; value: string | null }[] = [
    { label: "Tất cả", value: null },
    { label: "Info", value: "info" },
    { label: "Warn", value: "warn" },
    { label: "Error", value: "error" },
  ];

  const kindLabel: Record<string, string> = {
    chung: "Chạy chung",
    rieng: "Chạy riêng",
    lich: "Theo lịch",
    lam_lai: "Làm lại",
  };

  const statusLabel: Record<string, string> = {
    hoan_tat: "Hoàn tất",
    da_dung: "Đã dừng",
    gian_doan: "Gián đoạn",
    dang_chay: "Đang chạy",
  };

  const rawInternalStatuses = new Set(["chua_chay", "dang_chay", "xong", "loi"]);
  const LOG_FETCH_LIMIT = 250;

  let runs: RunSummary[] = [];
  let page = 1;
  let pageSize = 25;
  let pageCount = 1;
  let total = 0;

  // Bộ lọc bảng lượt chạy (Trạng thái + Loại + Từ ngày / Đến ngày)
  let runStatusFilter: string | undefined = undefined;
  let kindFilter = "";
  let dateFrom = "";
  let dateTo = "";

  let stats7d = { runs_count: 0, done_rows: 0, error_rows: 0 };
  let loading = false;
  let exportingCsv = false;

  // Chi tiết lượt chạy + sequence guard + phân trang dòng chi tiết
  let selectedRun: RunSummary | null = null;
  let runRows: RowSummary[] = [];
  let detailLoading = false;
  let runDetailSeq = 0;
  let detailChip = "Tất cả";
  let detailPage = 1;
  let detailPageSize = 25;

  // Modal xác nhận chạy lại dòng lỗi (luôn hiện + kiểm tra is_csv_locked theo TC-45)
  let rerunConfirmOpen = false;
  let rerunCheckingLock = false;
  let rerunCsvLocked = false;
  let rerunningErrors = false;

  // Modal Nhật ký kỹ thuật (760px) + sequence guard + Xuất file .log
  let logsModalOpen = false;
  let logsRunId: number | null = null;
  let logsLevel: string | null = null;
  let logs: LogItem[] = [];
  let logsLoading = false;
  let logsSeq = 0;
  let exportingLog = false;

  // Modal Xem trước Media ngay trong màn hình Lịch sử (mục 6.11)
  let mediaModalOpen = false;
  let mediaModalKind: "image" | "video" = "image";
  let mediaModalTitle = "";
  let mediaModalAbsPath = "";

  let mounted = false;
  let lastProjectPath: string | null = null;
  let lastTick = -1;
  let loadSeq = 0;

  $: hasActiveFilter = Boolean(runStatusFilter || kindFilter || dateFrom || dateTo);

  $: filteredDetailRows =
    detailChip === "Tất cả"
      ? runRows
      : runRows.filter((r) => r.status === detailChip);

  $: detailPageCount = Math.max(
    1,
    Math.ceil(filteredDetailRows.length / Math.max(1, detailPageSize))
  );

  $: safeDetailPage = Math.min(Math.max(1, detailPage), detailPageCount);

  $: pagedDetailRows = filteredDetailRows.slice(
    (safeDetailPage - 1) * detailPageSize,
    safeDetailPage * detailPageSize
  );

  $: errorDetailRows = runRows.filter((r) => r.status.startsWith("Lỗi"));

  function buildHistoryFilter(): HistoryFilter {
    return {
      status: runStatusFilter,
      kind: kindFilter || undefined,
      from: dateFrom || undefined,
      to: dateTo || undefined,
    };
  }

  onMount(async () => {
    mounted = true;
    lastProjectPath = $project?.path ?? null;
    lastTick = $rowChangedTick;
    if ($project) {
      await load();
    }
  });

  // Tự động nạp lại khi đổi dự án hoặc khi có sự kiện row_changed / run_finished
  $: if (mounted) {
    const curPath = $project?.path ?? null;
    const curTick = $rowChangedTick;
    if (curPath !== lastProjectPath) {
      lastProjectPath = curPath;
      lastTick = curTick;
      ++runDetailSeq;
      page = 1;
      selectedRun = null;
      runRows = [];
      detailLoading = false;
      detailChip = "Tất cả";
      detailPage = 1;
      if (curPath) {
        void load();
      } else {
        runs = [];
        total = 0;
        pageCount = 1;
        stats7d = { runs_count: 0, done_rows: 0, error_rows: 0 };
      }
    } else if (curTick !== lastTick) {
      lastTick = curTick;
      if (curPath) {
        void load({ silent: true });
        if (selectedRun) {
          void refreshSelectedRunSilently(selectedRun.id);
        }
      }
    }
  }

  export async function load(opts?: { silent?: boolean }): Promise<void> {
    if (!$project) {
      runs = [];
      total = 0;
      pageCount = 1;
      stats7d = { runs_count: 0, done_rows: 0, error_rows: 0 };
      return;
    }
    const seq = ++loadSeq;
    if (!opts?.silent) {
      loading = true;
    }
    try {
      const res = await api.get_history(
        page,
        pageSize,
        runStatusFilter,
        kindFilter || undefined,
        dateFrom || undefined,
        dateTo || undefined
      );
      if (seq !== loadSeq) return;
      runs = res.runs ?? [];
      total = res.total ?? 0;
      pageCount = Math.max(1, res.page_count || 1);
      if (res.stats_7d) {
        stats7d = res.stats_7d;
      }
    } catch (e) {
      if (seq === loadSeq && !opts?.silent) {
        showToast("error", formatApiError(e));
      }
    } finally {
      if (seq === loadSeq && !opts?.silent) {
        loading = false;
      }
    }
  }

  async function refreshSelectedRunSilently(runId: number): Promise<void> {
    const seq = ++runDetailSeq;
    try {
      const [r, rows] = await api.get_run_detail(runId);
      if (seq === runDetailSeq && selectedRun?.id === runId) {
        selectedRun = r;
        runRows = rows;
      }
    } catch {
      // Bỏ qua nếu lượt đã bị dọn tự động
    }
  }

  function selectStatusFilter(val: string | undefined): void {
    runStatusFilter = val;
    page = 1;
    void load();
  }

  function onFilterControlChange(): void {
    page = 1;
    void load();
  }

  function resetHistoryFilters(): void {
    runStatusFilter = undefined;
    kindFilter = "";
    dateFrom = "";
    dateTo = "";
    page = 1;
    void load();
  }

  function changePage(nextPage: number): void {
    if (nextPage < 1 || nextPage > pageCount || nextPage === page) return;
    page = nextPage;
    void load();
  }

  function onPageSizeChange(): void {
    page = 1;
    void load();
  }

  async function openRun(run: RunSummary): Promise<void> {
    if (selectedRun?.id === run.id) {
      ++runDetailSeq;
      detailLoading = false;
      selectedRun = null;
      runRows = [];
      return;
    }
    const seq = ++runDetailSeq;
    detailLoading = true;
    detailChip = "Tất cả";
    detailPage = 1;
    try {
      const [r, rows] = await api.get_run_detail(run.id);
      if (seq !== runDetailSeq) return;
      selectedRun = r;
      runRows = rows;
    } catch (e) {
      if (seq === runDetailSeq) {
        showToast("error", formatApiError(e));
      }
    } finally {
      if (seq === runDetailSeq) {
        detailLoading = false;
      }
    }
  }

  function selectDetailChip(s: string): void {
    detailChip = s;
    detailPage = 1;
  }

  // Luôn mở hộp xác nhận kèm kiểm tra khóa CSV trước khi chạy lại dòng lỗi (TC-45)
  async function handleRerunErrorsClick(): Promise<void> {
    if (!selectedRun || $running || selectedRun.error_count === 0 || rerunCheckingLock) {
      return;
    }
    rerunCheckingLock = true;
    rerunCsvLocked = false;
    try {
      const dr = await api.start_dry_run();
      rerunCsvLocked = Boolean(dr.is_csv_locked);
    } catch {
      rerunCsvLocked = false;
    } finally {
      rerunCheckingLock = false;
    }
    rerunConfirmOpen = true;
  }

  async function executeRerunErrors(): Promise<void> {
    if (!selectedRun || $running || rerunningErrors) return;
    rerunningErrors = true;
    try {
      const targetRunId = selectedRun.id;
      await api.rerun_error_rows(targetRunId);
      rerunConfirmOpen = false;
      showToast("info", `Đã bắt đầu chạy lại các dòng lỗi của lượt #${targetRunId}.`);
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      rerunningErrors = false;
    }
  }

  async function exportCsv(): Promise<void> {
    if (!$project || exportingCsv) return;
    exportingCsv = true;
    try {
      const savePath = await api.pick_save_csv();
      if (!savePath) return;
      await api.export_history_csv(savePath, buildHistoryFilter());
      showToast("success", "Đã xuất lịch sử ra file CSV thành công.");
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      exportingCsv = false;
    }
  }

  async function openLogsModal(runId: number | null = null): Promise<void> {
    logsRunId = runId;
    logsModalOpen = true;
    await fetchLogs();
  }

  function closeLogsModal(): void {
    ++logsSeq;
    logsLoading = false;
    logsModalOpen = false;
  }

  async function fetchLogs(): Promise<void> {
    if (!$project) {
      logs = [];
      return;
    }
    const seq = ++logsSeq;
    logsLoading = true;
    try {
      const items = await api.get_logs(logsRunId, logsLevel, LOG_FETCH_LIMIT);
      if (seq !== logsSeq) return;
      logs = items;
    } catch (e) {
      if (seq === logsSeq) {
        showToast("error", formatApiError(e));
      }
    } finally {
      if (seq === logsSeq) {
        logsLoading = false;
      }
    }
  }

  function selectLogLevel(lv: string | null): void {
    logsLevel = lv;
    void fetchLogs();
  }

  function clearLogRunScope(): void {
    logsRunId = null;
    void fetchLogs();
  }

  async function exportLogsFile(): Promise<void> {
    if (!$project || exportingLog) return;
    exportingLog = true;
    try {
      const savePath = await api.pick_save_log();
      if (!savePath) return;
      await api.export_logs_txt(savePath, logsRunId, logsLevel);
      showToast("success", "Đã xuất file nhật ký kỹ thuật (.log) thành công.");
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      exportingLog = false;
    }
  }

  async function copyLogsToClipboard(): Promise<void> {
    if (logs.length === 0) return;
    const text = logs
      .map((l) => {
        const ts = l.ts ? l.ts.replace("T", " ").slice(0, 19) : "";
        const runPart = l.run_id ? ` [run#${l.run_id}]` : "";
        const rowPart = l.row_key ? ` [${l.row_key}${l.step ? `:${l.step}` : ""}]` : "";
        const detailPart = l.detail ? ` (${l.detail})` : "";
        return `${ts} [${l.level.toUpperCase()}]${runPart}${rowPart} ${l.message}${detailPart}`;
      })
      .join("\n");
    try {
      await navigator.clipboard.writeText(text);
      showToast("success", `Đã sao chép ${logs.length} dòng nhật ký vào bộ nhớ tạm.`);
    } catch {
      showToast("warn", "Không sao chép tự động được — bạn có thể bôi đen trực tiếp khung nhật ký.");
    }
  }

  function cleanMediaFileName(val: string | undefined | null): string {
    if (!val) return "";
    const trimmed = val.trim();
    if (!trimmed || rawInternalStatuses.has(trimmed)) return "";
    return trimmed;
  }

  function buildOutputAbsPath(
    subDir: "image_out" | "video_out",
    fileName: string
  ): string {
    if (!$project || !fileName) return "";
    const base = $project.path.replace(/[\\/]+$/, "");
    return `${base}/${subDir}/${fileName}`;
  }

  function openMediaPreview(
    kind: "image" | "video",
    fileName: string,
    rowKey: string
  ): void {
    const subDir = kind === "video" ? "video_out" : "image_out";
    const abs = buildOutputAbsPath(subDir, fileName);
    if (!abs) return;
    mediaModalKind = kind;
    mediaModalTitle = `${kind === "video" ? "Video" : "Ảnh"} sản phẩm — ${rowKey} (${fileName})`;
    mediaModalAbsPath = abs;
    mediaModalOpen = true;
  }

  async function revealAbsPath(absPath: string): Promise<void> {
    if (!absPath) return;
    try {
      await api.open_path_in_explorer(absPath, true);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function revealOutputFile(
    subDir: "image_out" | "video_out",
    fileName: string
  ): Promise<void> {
    const target = buildOutputAbsPath(subDir, fileName);
    await revealAbsPath(target);
  }

  function formatStartedAt(iso: string): string {
    if (!iso) return "—";
    return iso.replace("T", " ").slice(0, 16);
  }

  function formatRunDuration(r: RunSummary): string {
    let ms: number | null =
      typeof r.duration_ms === "number" && Number.isFinite(r.duration_ms) && r.duration_ms >= 0
        ? r.duration_ms
        : null;

    if (ms === null && r.started_at && r.finished_at) {
      const t1 = Date.parse(r.started_at);
      const t2 = Date.parse(r.finished_at);
      if (Number.isFinite(t1) && Number.isFinite(t2) && t2 >= t1) {
        ms = t2 - t1;
      }
    }

    if (ms === null) {
      return r.status === "dang_chay" ? "Đang chạy…" : "—";
    }

    const totalSec = Math.max(0, Math.round(ms / 1000));
    const m = Math.floor(totalSec / 60);
    const s = totalSec % 60;
    return `${m}m ${s}s`;
  }

  function runStatusBadgeClass(status: string): string {
    if (status === "hoan_tat") return "badge-video";
    if (status === "da_dung") return "badge-sua-prompt";
    if (status === "gian_doan") return "badge-loi-video";
    if (status === "dang_chay") return "badge-image";
    return "badge-cho";
  }

  function rowStatusBadgeClass(s: string): string {
    if (s === "Đang chờ") return "badge-cho";
    if (s === "Đã tạo image") return "badge-image";
    if (s === "Đã tạo video") return "badge-video";
    if (s === "Lỗi tạo image") return "badge-loi-image";
    if (s === "Lỗi tạo video") return "badge-loi-video";
    if (s === "Lỗi tạm thời") return "badge-loi-tam-thoi";
    if (s === "Bỏ qua") return "badge-bo-qua";
    return "badge-cho";
  }
</script>

<div class="history-screen">
  <!-- Ghi chú phạm vi lịch sử theo Phụ lục v1.3.1 Quyết định 5 -->
  <div class="info-note selectable">
    <span class="info-icon"><Icons name="info" size={16} /></span>
    <span>
      Lịch sử và nhật ký lưu trong <code>state.db</code> của từng dự án: màn hình này luôn hiển thị dữ liệu của dự án đang mở{#if $project} (<strong>{$project.name}</strong>){/if}; đổi dự án là đổi lịch sử, sao chép thư mục dự án sang máy khác sẽ mang theo lịch sử của dự án đó.
    </span>
  </div>

  {#if !$project}
    <div class="card empty-project">
      <Icons name="history" size={32} />
      <div class="empty-project-title">Chưa mở dự án Local nào</div>
      <div class="empty-project-sub">
        Hãy mở một dự án ở màn hình <strong>Thư mục Local</strong> để xem lịch sử chạy và nhật ký kỹ thuật.
      </div>
    </div>
  {:else}
    <!-- 3 thẻ thống kê 7 ngày gần nhất -->
    <div class="stats-row">
      <div class="card stat-card">
        <div class="stat-top">
          <span class="stat-label">Lượt chạy 7 ngày</span>
          <span class="stat-icon teal"><Icons name="history" size={15} /></span>
        </div>
        <div class="stat-value">{stats7d.runs_count}</div>
      </div>

      <div class="card stat-card">
        <div class="stat-top">
          <span class="stat-label">Dòng hoàn tất (7 ngày)</span>
          <span class="stat-icon green"><Icons name="check" size={15} /></span>
        </div>
        <div class="stat-value ok">{stats7d.done_rows}</div>
      </div>

      <div class="card stat-card" class:has-err={stats7d.error_rows > 0}>
        <div class="stat-top">
          <span class="stat-label">Dòng lỗi (7 ngày)</span>
          <span class="stat-icon" class:danger={stats7d.error_rows > 0}>
            <Icons name="alert" size={15} />
          </span>
        </div>
        <div class="stat-value" class:danger-val={stats7d.error_rows > 0}>
          {stats7d.error_rows}
        </div>
      </div>
    </div>

    <!-- Thanh công cụ lọc Kết quả + Loại + Khoảng ngày + Nhật ký kỹ thuật + Xuất CSV -->
    <div class="toolbar">
      <div class="filter-chips">
        {#each runStatusChips as item}
          <button
            type="button"
            class="chip"
            class:active={runStatusFilter === item.value}
            on:click={() => selectStatusFilter(item.value)}
          >
            {item.label}
          </button>
        {/each}
      </div>

      <select
        id="history-kind-select"
        class="kind-select"
        aria-label="Lọc theo loại lượt chạy"
        bind:value={kindFilter}
        on:change={onFilterControlChange}
      >
        {#each kindOptions as k}
          <option value={k.value}>{k.label}</option>
        {/each}
      </select>

      <div class="date-range">
        <input
          id="history-date-from"
          class="input date-input"
          type="date"
          aria-label="Lọc từ ngày"
          title="Từ ngày"
          bind:value={dateFrom}
          on:change={onFilterControlChange}
        />
        <span class="date-sep">—</span>
        <input
          id="history-date-to"
          class="input date-input"
          type="date"
          aria-label="Lọc đến ngày"
          title="Đến ngày"
          bind:value={dateTo}
          on:change={onFilterControlChange}
        />
      </div>

      {#if hasActiveFilter}
        <button type="button" class="btn-ghost clear-btn" on:click={resetHistoryFilters}>
          <Icons name="x" size={13} />
          Xóa lọc
        </button>
      {/if}

      <div class="spacer"></div>

      <button
        type="button"
        class="btn-ghost"
        disabled={loading}
        on:click={() => load()}
        title="Làm mới danh sách lượt chạy"
      >
        <Icons name="refresh" size={15} />
        Nạp lại
      </button>

      <button
        type="button"
        class="btn-secondary"
        on:click={() => openLogsModal(null)}
      >
        <Icons name="terminal" size={15} />
        Nhật ký kỹ thuật
      </button>

      <button
        type="button"
        class="btn-secondary"
        disabled={exportingCsv || runs.length === 0}
        title="Xuất danh sách lượt chạy theo bộ lọc hiện tại ra file CSV"
        on:click={exportCsv}
      >
        <Icons name="download" size={15} />
        {exportingCsv ? "Đang xuất..." : "Xuất CSV"}
      </button>
    </div>

    <!-- Bảng danh sách các lượt chạy -->
    <div class="card table-card">
      <div class="table-scroll">
        <table>
          <thead>
            <tr>
              <th class="w-90">Lượt chạy</th>
              <th class="w-120">Loại</th>
              <th class="w-150">Bắt đầu</th>
              <th class="w-110">Thời lượng</th>
              <th class="w-80">Tổng</th>
              <th class="w-80">Xong</th>
              <th class="w-80">Lỗi</th>
              <th class="w-120">Kết quả</th>
            </tr>
          </thead>
          <tbody>
            {#each runs as r (r.id)}
              <tr
                class="run-row"
                class:selected={selectedRun?.id === r.id}
                on:click={() => openRun(r)}
                on:keydown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    void openRun(r);
                  }
                }}
                role="button"
                tabindex="0"
              >
                <td class="strong">
                  <span class="run-id-cell">
                    <Icons
                      name={selectedRun?.id === r.id ? "chevron-down" : "chevron-right"}
                      size={14}
                    />
                    #{r.id}
                  </span>
                </td>
                <td>
                  <span class="kind-pill">{kindLabel[r.kind] ?? r.kind}</span>
                </td>
                <td class="tabular">{formatStartedAt(r.started_at)}</td>
                <td class="tabular muted-cell">{formatRunDuration(r)}</td>
                <td class="tabular">{r.total_rows}</td>
                <td class="tabular" class:ok-text={r.done_count > 0}>{r.done_count}</td>
                <td
                  class="tabular"
                  class:err-text={r.error_count > 0}
                  class:muted-cell={r.error_count === 0}
                >
                  {r.error_count}
                </td>
                <td>
                  <span class="badge {runStatusBadgeClass(r.status)}">
                    {statusLabel[r.status] ?? r.status}
                  </span>
                </td>
              </tr>
            {:else}
              <tr>
                <td colspan="8" class="empty">
                  {loading
                    ? "Đang tải lịch sử các lượt chạy..."
                    : hasActiveFilter
                      ? "Không có lượt chạy nào khớp điều kiện lọc hiện tại."
                      : "Chưa có lượt chạy nào trong dự án này."}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>

      <div class="pagination">
        <label class="page-size-label" for="history-page-size">Mỗi trang:</label>
        <select id="history-page-size" bind:value={pageSize} on:change={onPageSizeChange}>
          <option value={10}>10</option>
          <option value={25}>25</option>
          <option value={50}>50</option>
          <option value={100}>100</option>
        </select>

        <div class="page-nav">
          <button
            type="button"
            class="btn-secondary"
            disabled={page <= 1 || loading}
            on:click={() => changePage(page - 1)}
          >
            <Icons name="chevron-left" size={14} />
            Trước
          </button>
          <span class="page-indicator">Trang {page} / {pageCount}</span>
          <button
            type="button"
            class="btn-secondary"
            disabled={page >= pageCount || loading}
            on:click={() => changePage(page + 1)}
          >
            Sau
            <Icons name="chevron-right" size={14} />
          </button>
        </div>

        <span class="spacer"></span>
        <span>
          {hasActiveFilter ? `Tổng ${total} lượt (đang lọc)` : `Tổng ${total} lượt`}
        </span>
      </div>
    </div>

    <!-- Chi tiết lượt chạy đang chọn -->
    {#if detailLoading && !selectedRun}
      <div class="card detail-loading">Đang tải chi tiết lượt chạy...</div>
    {:else if selectedRun}
      <div class="card detail-card">
        <div class="detail-head">
          <div class="detail-head-left">
            <div class="detail-title">Chi tiết lượt #{selectedRun.id}</div>
            <div class="detail-meta">
              <span class="kind-pill">{kindLabel[selectedRun.kind] ?? selectedRun.kind}</span>
              <span class="badge {runStatusBadgeClass(selectedRun.status)}">
                {statusLabel[selectedRun.status] ?? selectedRun.status}
              </span>
              <span>Thời lượng: <strong>{formatRunDuration(selectedRun)}</strong></span>
              <span>
                Xong: <strong>{selectedRun.done_count}</strong> · Lỗi: <strong>{selectedRun.error_count}</strong>
              </span>
            </div>
          </div>

          <div class="detail-actions">
            <button
              type="button"
              class="btn-secondary"
              on:click={() => selectedRun && openLogsModal(selectedRun.id)}
            >
              <Icons name="terminal" size={15} />
              Xem nhật ký lượt #{selectedRun.id}
            </button>

            <button
              type="button"
              class="btn-primary"
              disabled={$running || selectedRun.error_count === 0 || rerunningErrors || rerunCheckingLock}
              title={$running
                ? "Đang có lượt chạy — chờ xong hoặc bấm STOP"
                : selectedRun.error_count === 0
                  ? "Lượt này không có dòng lỗi nào cần chạy lại"
                  : `Chạy lại ${selectedRun.error_count} dòng lỗi của lượt #${selectedRun.id}`}
              on:click={handleRerunErrorsClick}
            >
              <Icons name="redo" size={15} />
              {rerunCheckingLock ? "Đang kiểm tra..." : "Chạy lại các dòng lỗi của lượt này"}
            </button>
          </div>
        </div>

        <div class="chips">
          {#each detailRowStatuses as s}
            {@const count =
              s === "Tất cả"
                ? runRows.length
                : runRows.filter((r) => r.status === s).length}
            {#if s === "Tất cả" || count > 0}
              <button
                type="button"
                class="chip"
                class:active={detailChip === s}
                on:click={() => selectDetailChip(s)}
              >
                <span>{s}</span>
                <span class="count">({count})</span>
              </button>
            {/if}
          {/each}
        </div>

        <div class="detail-table-scroll">
          <table>
            <thead>
              <tr>
                <th>Dòng</th>
                <th class="w-150">Trạng thái</th>
                <th>Ảnh sản phẩm</th>
                <th>Video sản phẩm</th>
                <th class="w-64">Thử</th>
              </tr>
            </thead>
            <tbody>
              {#each pagedDetailRows as r (r.row_key)}
                {@const imgName = cleanMediaFileName(r.image_file)}
                {@const vidName = cleanMediaFileName(r.video_file)}
                <tr>
                  <td class="strong selectable">{r.row_key}</td>
                  <td>
                    <span class="badge {rowStatusBadgeClass(r.status)}">
                      {r.status}
                    </span>
                  </td>
                  <td>
                    {#if imgName}
                      <div class="media-cell-group">
                        <button
                          type="button"
                          class="file-reveal-btn"
                          title={`Xem ảnh ${imgName} ngay trong ứng dụng`}
                          on:click={() => openMediaPreview("image", imgName, r.row_key)}
                        >
                          <Icons name="image" size={13} />
                          <span class="file-reveal-name">{imgName}</span>
                        </button>
                        <button
                          type="button"
                          class="btn-ghost explorer-icon-btn"
                          aria-label={`Hiện ${imgName} trong Explorer`}
                          title={`Hiện ${imgName} trong thư mục image_out/`}
                          on:click={() => revealOutputFile("image_out", imgName)}
                        >
                          <Icons name="folder-open" size={13} />
                        </button>
                      </div>
                    {:else}
                      <span class="muted-cell">—</span>
                    {/if}
                  </td>
                  <td>
                    {#if vidName}
                      <div class="media-cell-group">
                        <button
                          type="button"
                          class="file-reveal-btn video"
                          title={`Phát video ${vidName} ngay trong ứng dụng`}
                          on:click={() => openMediaPreview("video", vidName, r.row_key)}
                        >
                          <Icons name="video" size={13} />
                          <span class="file-reveal-name">{vidName}</span>
                        </button>
                        <button
                          type="button"
                          class="btn-ghost explorer-icon-btn"
                          aria-label={`Hiện ${vidName} trong Explorer`}
                          title={`Hiện ${vidName} trong thư mục video_out/`}
                          on:click={() => revealOutputFile("video_out", vidName)}
                        >
                          <Icons name="folder-open" size={13} />
                        </button>
                      </div>
                    {:else}
                      <span class="muted-cell">—</span>
                    {/if}
                  </td>
                  <td class="tabular">{r.attempts_count}</td>
                </tr>
                {#if r.last_error}
                  <tr class="err-row">
                    <td colspan="5" class="selectable">
                      <span class="err-row-inner">
                        <Icons name="alert" size={13} />
                        <span>{r.last_error}</span>
                      </span>
                    </td>
                  </tr>
                {/if}
              {:else}
                <tr>
                  <td colspan="5" class="empty">
                    Không có dòng nào khớp bộ lọc trạng thái trong lượt này.
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>

        {#if filteredDetailRows.length > 25 || detailPageCount > 1}
          <div class="pagination detail-pagination">
            <label class="page-size-label" for="detail-rows-page-size">Mỗi trang:</label>
            <select
              id="detail-rows-page-size"
              bind:value={detailPageSize}
              on:change={() => (detailPage = 1)}
            >
              <option value={10}>10</option>
              <option value={25}>25</option>
              <option value={50}>50</option>
              <option value={100}>100</option>
            </select>

            <div class="page-nav">
              <button
                type="button"
                class="btn-secondary"
                disabled={safeDetailPage <= 1}
                on:click={() => (detailPage = safeDetailPage - 1)}
              >
                <Icons name="chevron-left" size={14} />
                Trước
              </button>
              <span class="page-indicator">Trang {safeDetailPage} / {detailPageCount}</span>
              <button
                type="button"
                class="btn-secondary"
                disabled={safeDetailPage >= detailPageCount}
                on:click={() => (detailPage = safeDetailPage + 1)}
              >
                Sau
                <Icons name="chevron-right" size={14} />
              </button>
            </div>

            <span class="spacer"></span>
            <span>Tổng {filteredDetailRows.length} dòng</span>
          </div>
        {/if}
      </div>
    {/if}
  {/if}
</div>

<!-- Modal Xác nhận chạy lại các dòng lỗi của lượt (TC-45 + cảnh báo khóa CSV) -->
<Modal
  open={rerunConfirmOpen}
  title={`Chạy lại dòng lỗi — Lượt #${selectedRun?.id ?? ""}`}
  width="520px"
  disableClose={rerunningErrors}
  on:close={() => (rerunConfirmOpen = false)}
>
  <div class="modal-stack">
    <p class="modal-desc">
      Tạo một lượt chạy mới chỉ xử lý <strong>{selectedRun?.error_count ?? errorDetailRows.length} dòng bị lỗi</strong> của lượt <strong>#{selectedRun?.id ?? ""}</strong> (bộ đếm lỗi của các dòng này được mở khóa cho lượt chạy lại):
    </p>
    {#if errorDetailRows.length > 0}
      <div class="err-keys-box selectable">
        {errorDetailRows.slice(0, 15).map((r) => r.row_key).join(", ")}{errorDetailRows.length > 15
          ? ` … +${errorDetailRows.length - 15} dòng`
          : ""}
      </div>
    {/if}
    {#if rerunCsvLocked}
      <div class="csv-lock-warn selectable">
        <Icons name="alert" size={17} />
        <div>
          <strong>prompts.csv đang bị khóa ghi</strong> (ví dụ đang mở trong Excel) — kết quả sẽ chưa ghi ngay vào CSV mà được lưu trong <code>state.db</code> và đồng bộ sau.
        </div>
      </div>
    {/if}
  </div>

  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={rerunningErrors}
      on:click={() => (rerunConfirmOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={$running || rerunningErrors}
      on:click={executeRerunErrors}
    >
      <Icons name="redo" size={15} />
      {rerunningErrors ? "Đang khởi chạy..." : "Xác nhận chạy lại"}
    </button>
  </div>
</Modal>

<!-- Modal Xem trước Ảnh / Video sản phẩm ngay trong màn hình Lịch sử (mục 6.11) -->
<Modal
  open={mediaModalOpen}
  title={mediaModalTitle}
  width="760px"
  on:close={() => (mediaModalOpen = false)}
>
  <div class="history-media-stage">
    {#if mediaModalAbsPath}
      {#if mediaModalKind === "video"}
        {#key `${mediaModalAbsPath}-${$assetCacheBust}`}
          <video
            class="history-video-player"
            controls
            preload="metadata"
            src={toAssetUrl(mediaModalAbsPath, $assetCacheBust)}
          >
            <track kind="captions" />
          </video>
        {/key}
      {:else}
        <img
          class="history-image-preview"
          src={toAssetUrl(mediaModalAbsPath, $assetCacheBust)}
          alt={mediaModalTitle}
        />
      {/if}
    {/if}
  </div>

  <div slot="footer" class="modal-footer-bar">
    {#if mediaModalAbsPath}
      <button
        type="button"
        class="btn-secondary"
        on:click={() => revealAbsPath(mediaModalAbsPath)}
      >
        <Icons name="folder-open" size={15} />
        Hiện trong thư mục
      </button>
    {/if}
    <button
      type="button"
      class="btn-primary"
      on:click={() => (mediaModalOpen = false)}
    >
      Đóng
    </button>
  </div>
</Modal>

<!-- Modal Nhật ký kỹ thuật 760px + Xuất file .log (mục 6.5b) -->
<Modal
  open={logsModalOpen}
  title={logsRunId !== null
    ? `Nhật ký kỹ thuật — Lượt #${logsRunId}`
    : "Nhật ký kỹ thuật dự án"}
  width="760px"
  on:close={closeLogsModal}
>
  <div class="logs-modal-wrap">
    <div class="logs-toolbar">
      <div class="logs-levels">
        {#each logLevels as lv}
          <button
            type="button"
            class="chip small-chip"
            class:active={logsLevel === lv.value}
            on:click={() => selectLogLevel(lv.value)}
          >
            {lv.label}
          </button>
        {/each}
      </div>

      {#if logsRunId !== null}
        <button
          type="button"
          class="btn-ghost tiny-scope-btn"
          on:click={clearLogRunScope}
        >
          <Icons name="x" size={12} />
          Bỏ lọc lượt #{logsRunId}
        </button>
      {/if}

      <div class="spacer"></div>

      <button
        type="button"
        class="btn-ghost small-action"
        disabled={logsLoading}
        on:click={fetchLogs}
      >
        <Icons name="refresh" size={14} />
        Làm mới
      </button>

      <button
        type="button"
        class="btn-secondary small-action"
        disabled={logs.length === 0}
        on:click={copyLogsToClipboard}
      >
        Sao chép ({logs.length})
      </button>

      <button
        type="button"
        class="btn-secondary small-action"
        disabled={exportingLog || logs.length === 0}
        on:click={exportLogsFile}
      >
        <Icons name="download" size={14} />
        {exportingLog ? "Đang lưu..." : "Xuất file .log"}
      </button>
    </div>

    {#if logs.length >= LOG_FETCH_LIMIT}
      <div class="logs-cap-note">
        Đang hiển thị {LOG_FETCH_LIMIT} bản ghi mới nhất trên màn hình — bấm <strong>Xuất file .log</strong> để lưu đầy đủ toàn bộ nhật ký.
      </div>
    {/if}

    <div class="logs-box selectable">
      {#if logsLoading}
        <div class="logs-empty">Đang tải nhật ký kỹ thuật...</div>
      {:else if logs.length === 0}
        <div class="logs-empty">
          Chưa có bản ghi nhật ký nào khớp điều kiện lọc.
        </div>
      {:else}
        {#each logs as item (item.id)}
          <div class="log-line">
            <span class="log-ts">{item.ts ? item.ts.replace("T", " ").slice(0, 19) : "—"}</span>
            <span
              class="log-lv"
              class:info={item.level === "info"}
              class:warn={item.level === "warn"}
              class:error={item.level === "error"}
            >
              {item.level.toUpperCase()}
            </span>
            {#if item.run_id}
              <span class="log-tag">#{item.run_id}</span>
            {/if}
            {#if item.row_key}
              <span class="log-row">{item.row_key}{item.step ? ` · ${item.step}` : ""}</span>
            {:else if item.step}
              <span class="log-row">{item.step}</span>
            {/if}
            <span class="log-msg">
              {item.message}
              {#if item.detail}
                <span class="log-detail">[{item.detail}]</span>
              {/if}
            </span>
          </div>
        {/each}
      {/if}
    </div>
  </div>

  <div slot="footer" class="modal-footer-bar">
    <button type="button" class="btn-primary" on:click={closeLogsModal}>
      Đóng
    </button>
  </div>
</Modal>

<style>
  .history-screen {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  /* Ghi chú Phụ lục v1.3.1 Q5 */
  .info-note {
    background: var(--c-teal-subtle);
    border: 1px solid #cbe4e7;
    color: var(--c-text);
    border-radius: 10px;
    padding: 9px 14px;
    font-size: 12.5px;
    line-height: 1.45;
    display: flex;
    align-items: flex-start;
    gap: 10px;
  }
  .info-icon {
    color: var(--c-teal);
    margin-top: 1px;
    flex-shrink: 0;
  }
  .info-note code {
    background: #ffffff;
    border: 1px solid var(--c-border);
    padding: 0 5px;
    border-radius: 4px;
    font-size: 12px;
  }

  .empty-project {
    padding: 40px 24px;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    text-align: center;
    gap: 8px;
    color: var(--c-muted);
  }
  .empty-project-title {
    font-size: 16px;
    font-weight: 700;
    color: var(--c-text);
  }
  .empty-project-sub {
    font-size: var(--fs-label);
    max-width: 460px;
  }

  /* 3 thẻ thống kê 7 ngày */
  .stats-row {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 12px;
  }
  .stat-card {
    padding: 11px 16px;
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  .stat-card.has-err {
    border-color: #efb9b4;
    background: linear-gradient(180deg, #ffffff 0%, #fff8f7 100%);
  }
  .stat-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .stat-label {
    font-size: var(--fs-hint);
    font-weight: 600;
    color: var(--c-muted);
  }
  .stat-icon {
    width: 26px;
    height: 26px;
    border-radius: 7px;
    background: var(--c-surface-subtle);
    color: var(--c-muted);
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .stat-icon.teal {
    background: var(--c-teal-light);
    color: var(--c-teal);
  }
  .stat-icon.green {
    background: var(--c-green-bg);
    color: var(--c-green);
  }
  .stat-icon.danger {
    background: var(--c-danger-bg);
    color: var(--c-danger);
  }
  .stat-value {
    font-size: 24px;
    font-weight: 700;
    line-height: 1.15;
    color: var(--c-text);
    font-variant-numeric: tabular-nums;
  }
  .stat-value.ok {
    color: var(--c-green);
  }
  .stat-value.danger-val {
    color: var(--c-danger);
  }

  /* Thanh công cụ */
  .toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .filter-chips {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .kind-select {
    height: 32px;
    padding: 0 10px;
    font-size: 13px;
    width: 146px;
  }
  .date-range {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .date-input {
    height: 32px;
    padding: 0 8px;
    font-size: 12.5px;
    width: 136px;
  }
  .date-sep {
    color: var(--c-muted);
    font-size: 12px;
  }
  .clear-btn {
    height: 30px;
    padding: 0 10px;
    font-size: 12px;
  }
  .spacer {
    flex: 1;
  }

  /* Bảng lượt chạy */
  .table-card {
    padding: 0;
    overflow: hidden;
  }
  .table-scroll {
    overflow-x: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
  }
  thead th {
    height: var(--h-table-head);
    background: #eef3f6;
    border-bottom: 1px solid var(--c-border);
    font-size: var(--fs-label);
    color: var(--c-muted);
    text-align: left;
    padding: 0 12px;
    font-weight: 600;
    white-space: nowrap;
  }
  tbody td {
    height: var(--h-table-row);
    border-bottom: 1px solid #edf2f5;
    padding: 0 12px;
    font-size: var(--fs-body);
    vertical-align: middle;
  }
  .run-row {
    cursor: pointer;
    transition: background 0.1s ease;
  }
  .run-row:hover {
    background: #f5f9fb;
  }
  .run-row.selected {
    background: var(--c-teal-light);
  }
  .run-id-cell {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .kind-pill {
    display: inline-flex;
    align-items: center;
    height: 22px;
    padding: 0 8px;
    border-radius: 6px;
    font-size: 12px;
    font-weight: 600;
    background: var(--c-surface-subtle);
    color: var(--c-text);
    border: 1px solid var(--c-border);
  }
  .strong {
    font-weight: 600;
  }
  .tabular {
    font-variant-numeric: tabular-nums;
  }
  .muted-cell {
    color: var(--c-muted);
  }
  .ok-text {
    color: var(--c-green);
    font-weight: 600;
  }
  .err-text {
    color: var(--c-danger);
    font-weight: 700;
  }

  .w-90 {
    width: 90px;
  }
  .w-120 {
    width: 120px;
  }
  .w-150 {
    width: 150px;
  }
  .w-110 {
    width: 110px;
  }
  .w-80 {
    width: 80px;
  }
  .w-64 {
    width: 64px;
  }

  .empty {
    text-align: center;
    color: var(--c-muted);
    padding: 28px 16px;
  }

  .pagination {
    height: 46px;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 14px;
    border-top: 1px solid var(--c-border);
    font-size: var(--fs-label);
    color: var(--c-muted);
  }
  .detail-pagination {
    background: var(--c-surface-subtle);
  }
  .page-size-label {
    font-size: var(--fs-label);
    color: var(--c-muted);
  }
  .pagination select {
    height: 30px;
    width: 72px;
    padding: 0 8px;
  }
  .page-nav {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .pagination .btn-secondary {
    height: 30px;
    padding: 0 10px;
    font-size: 13px;
    gap: 4px;
  }
  .page-indicator {
    font-weight: 600;
    color: var(--c-text);
    padding: 0 4px;
  }

  /* Thẻ Chi tiết lượt chạy */
  .detail-loading {
    text-align: center;
    color: var(--c-muted);
    padding: 24px;
  }
  .detail-card {
    padding: 0;
    overflow: hidden;
  }
  .detail-head {
    padding: 14px 16px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
    border-bottom: 1px solid var(--c-border);
    background: var(--c-surface-subtle);
  }
  .detail-head-left {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .detail-title {
    font-weight: 700;
    font-size: var(--fs-h-block);
    color: var(--c-text);
  }
  .detail-meta {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    font-size: 12px;
    color: var(--c-muted);
  }
  .detail-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .chips {
    display: flex;
    gap: 8px;
    padding: 10px 16px;
    flex-wrap: wrap;
    border-bottom: 1px solid #edf2f5;
  }
  .detail-table-scroll {
    max-height: 360px;
    overflow: auto;
  }

  .media-cell-group {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    max-width: 100%;
  }
  .file-reveal-btn {
    height: 26px;
    padding: 0 8px;
    border-radius: 6px;
    font-size: 12px;
    font-weight: 500;
    background: var(--c-teal-subtle);
    color: var(--c-teal-dark);
    border: 1px solid #cbe4e7;
    gap: 5px;
    max-width: 210px;
  }
  .file-reveal-btn.video {
    background: var(--c-green-bg);
    color: var(--c-green);
    border-color: #bfe0cb;
  }
  .file-reveal-btn:hover {
    filter: brightness(0.95);
  }
  .file-reveal-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .explorer-icon-btn {
    width: 26px;
    height: 26px;
    padding: 0;
    border-radius: 6px;
    flex-shrink: 0;
  }

  .err-row td {
    height: auto;
    padding: 6px 12px 10px 24px;
    background: #fff9f8;
    color: var(--c-danger);
    font-size: 12px;
  }
  .err-row-inner {
    display: inline-flex;
    align-items: flex-start;
    gap: 6px;
  }

  /* Modal xác nhận chạy lại lỗi, Xem trước media & Nhật ký kỹ thuật */
  .modal-stack {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .modal-desc {
    font-size: var(--fs-body);
    color: var(--c-text);
    line-height: 1.5;
  }
  .err-keys-box {
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    border-radius: 8px;
    padding: 10px 12px;
    font-size: var(--fs-label);
    color: var(--c-text);
    word-break: break-all;
  }
  .csv-lock-warn {
    background: var(--c-warn-bg);
    border: 1px solid #f4d39b;
    color: var(--c-warn-dark);
    border-radius: 8px;
    padding: 10px 12px;
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

  .history-media-stage {
    background: #0d1520;
    border-radius: 8px;
    padding: 10px;
    max-height: 66vh;
    display: flex;
    align-items: center;
    justify-content: center;
    overflow: auto;
  }
  .history-image-preview {
    max-width: 100%;
    max-height: 62vh;
    object-fit: contain;
    display: block;
  }
  .history-video-player {
    width: 100%;
    max-height: 62vh;
    object-fit: contain;
    display: block;
  }

  .logs-modal-wrap {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .logs-toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .logs-levels {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .small-chip {
    height: 28px;
    padding: 0 10px;
    font-size: 12px;
  }
  .tiny-scope-btn {
    height: 28px;
    padding: 0 8px;
    font-size: 12px;
    background: var(--c-teal-light);
    color: var(--c-teal-dark);
  }
  .small-action {
    height: 30px;
    padding: 0 10px;
    font-size: 12px;
    gap: 5px;
  }
  .logs-cap-note {
    font-size: 12px;
    color: var(--c-warn-dark);
    background: var(--c-warn-bg);
    border: 1px solid #f4d39b;
    padding: 6px 10px;
    border-radius: 6px;
  }

  .logs-box {
    background: #0f172a;
    color: #e2e8f0;
    border-radius: 8px;
    padding: 10px 12px;
    min-height: 280px;
    max-height: 54vh;
    overflow: auto;
    font-family: Consolas, "Cascadia Mono", "Courier New", monospace;
    font-size: 12px;
    line-height: 1.55;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .logs-empty {
    color: #94a3b8;
    text-align: center;
    padding: 48px 16px;
    font-family: "Segoe UI", system-ui, sans-serif;
  }
  .log-line {
    display: flex;
    align-items: baseline;
    gap: 7px;
    flex-wrap: wrap;
    padding: 2px 0;
    border-bottom: 1px solid rgba(148, 163, 184, 0.12);
  }
  .log-ts {
    color: #94a3b8;
    flex-shrink: 0;
  }
  .log-lv {
    font-size: 10.5px;
    font-weight: 700;
    padding: 0 5px;
    border-radius: 4px;
    flex-shrink: 0;
  }
  .log-lv.info {
    background: rgba(56, 189, 248, 0.18);
    color: #7dd3fc;
  }
  .log-lv.warn {
    background: rgba(251, 191, 36, 0.2);
    color: #fcd34d;
  }
  .log-lv.error {
    background: rgba(248, 113, 113, 0.22);
    color: #fca5a5;
  }
  .log-tag {
    color: #5eead4;
    font-weight: 600;
  }
  .log-row {
    color: #cbd5e1;
    background: rgba(255, 255, 255, 0.08);
    padding: 0 5px;
    border-radius: 4px;
  }
  .log-msg {
    color: #f8fafc;
    word-break: break-word;
  }
  .log-detail {
    color: #94a3b8;
    margin-left: 4px;
  }

  @media (max-width: 820px) {
    .stats-row {
      grid-template-columns: 1fr;
    }
  }
</style>
