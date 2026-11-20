<script lang="ts">
  // Màn hình Dòng việc (Wave 2 Full Option): Table/Grid toggle, Quét & nhập ảnh input/, So sánh phiên bản v1/v2, Lightbox Zoom/Pan 25%-400%
  import { get } from "svelte/store";
  import { onDestroy, onMount } from "svelte";
  import {
    project,
    running,
    progressText,
    progressPercent,
    activeRows,
    selectedRows,
    rowChangedTick,
    pendingRowsFilter,
    endpointStatus,
    confirmStartEnabled,
    maxAttemptsStore,
    assetCacheBust,
    rowAssetRevs,
    rowsUiState,
    showToast,
    formatApiError,
  } from "../lib/store";
  import {
    api,
    toAssetUrl,
    type RowSummary,
    type RowDetail,
    type FileVersionItem,
    type UntrackedImageItem,
    type DryRunResult,
  } from "../lib/api";
  import Icons from "../lib/Icons.svelte";
  import Modal from "../lib/Modal.svelte";
  import {
    createSeqGuard,
    isRowLockedForBatch,
    pruneSelectedKeys,
    runWithLoadingGuard,
  } from "../lib/asyncGuard";

  const filterChips: { key: string; label: string }[] = [
    { key: "Tất cả", label: "Tất cả" },
    { key: "Đang chờ", label: "Đang chờ" },
    { key: "Đã tạo image", label: "Đã tạo image" },
    { key: "Đã tạo video", label: "Đã tạo video" },
    { key: "Lỗi", label: "Tất cả lỗi" },
    { key: "Lỗi tạo image", label: "Lỗi tạo image" },
    { key: "Lỗi tạo video", label: "Lỗi tạo video" },
    { key: "Lỗi tạm thời", label: "Lỗi tạm thời" },
    { key: "Bỏ qua", label: "Bỏ qua" },
  ];

  type PreviewTab = "ref" | "generated" | "video";
  type ViewMode = "table" | "grid";

  const initialUi = get(rowsUiState);

  let rows: RowSummary[] = [];
  let total = 0;
  let page = initialUi.page || 1;
  let pageSize = initialUi.pageSize || 25;
  let pageCount = 1;
  let chip = initialUi.chip || "Tất cả";
  let search = initialUi.search || "";
  let viewMode: ViewMode = initialUi.viewMode || "table";

  let loading = false;
  let loadError: string | null = null;
  let counts: Record<string, number> = {};
  let selected = new Set<string>(get(selectedRows));
  let brokenThumbs: Record<string, boolean> = {};
  let stoppingRun = false;

  // Ngăn Chi tiết 440px + bảo vệ race condition & prompt chưa lưu
  let detail: RowDetail | null = null;
  let detailLoading = false;
  let detailReqSeq = 0;
  let previewTab: PreviewTab = "ref";
  let previewFileOverride: FileVersionItem | null = null;

  let origPromptImage = "";
  let origPromptVideo = "";
  let draftPromptImage = "";
  let draftPromptVideo = "";
  let savingPrompts = false;
  let actionBusy = false;

  // Hộp xác nhận đóng/chuyển Drawer khi prompt đang sửa dở
  let unsavedPromptModalOpen = false;
  let pendingDrawerAction:
    | { type: "close" }
    | { type: "open"; rowKey: string; preferredTab?: PreviewTab }
    | null = null;

  // Hộp xác nhận Xóa dòng (từ bảng/grid hoặc từ Drawer) & Đặt lại trạng thái & Chạy hàng loạt
  let deleteConfirmOpen = false;
  let deleteTargetKey = "";
  let resetConfirmOpen = false;
  let batchConfirmOpen = false;
  let batchDryRunProbe: DryRunResult | null = null;
  let batchStarting = false;

  // (A) Quét & nhập hàng loạt ảnh từ input/
  let scanModalOpen = false;
  let scanningInput = false;
  let importingInput = false;
  let untrackedItems: UntrackedImageItem[] = [];
  let selectedImportNames = new Set<string>();
  let defaultImportPromptImg = "";
  let defaultImportPromptVid = "";

  // (C) So sánh phiên bản v1 vs v2 cạnh nhau
  let compareModalOpen = false;
  let compareKind: "image" | "video" = "image";
  let compareLeftVer = 1;
  let compareRightVer = 2;
  let switchingVersion = false;

  // (D) Lightbox xem ảnh phóng lớn 25%–400% + kéo di chuyển (pan)
  let lightboxOpen = false;
  let lightboxAbsPath = "";
  let lightboxTitle = "";
  let zoomScale = 1;
  let panX = 0;
  let panY = 0;
  let isPanning = false;
  let panStartX = 0;
  let panStartY = 0;
  let panOriginX = 0;
  let panOriginY = 0;

  let searchTimer: ReturnType<typeof setTimeout> | null = null;
  const loadGuard = createSeqGuard();
  let mounted = false;
  let hasLoadedRows = false;
  let lastPrunedMaxAttempts = get(maxAttemptsStore);
  let lastProjectPath: string | null = null;
  let lastTick = -1;

  $: if (mounted && hasLoadedRows && $maxAttemptsStore !== lastPrunedMaxAttempts) {
    pruneSelection($maxAttemptsStore);
  }

  $: if (!$running) {
    stoppingRun = false;
  }

  $: imgPromptEdited = !!detail && draftPromptImage !== origPromptImage;
  $: vidPromptEdited = !!detail && draftPromptVideo !== origPromptVideo;
  $: promptsDirty = imgPromptEdited || vidPromptEdited;

  $: selectableRows = rows.filter((r) => !isRowLockedForBatch(r, $maxAttemptsStore));
  $: allChecked =
    selectableRows.length > 0 &&
    selectableRows.every((r) => selected.has(r.row_key));
  $: someChecked =
    selectableRows.some((r) => selected.has(r.row_key)) && !allChecked;

  $: totalAllRows = counts["Tất cả"] ?? total;

  // Danh sách phiên bản theo loại trong Drawer
  $: imageVersions = (detail?.files ?? []).filter(
    (f) => f.kind === "anh" || f.kind === "image"
  );
  $: videoVersions = (detail?.files ?? []).filter((f) => f.kind === "video");
  $: canCompareVersions = imageVersions.length >= 2 || videoVersions.length >= 2;

  $: compareCandidates = compareKind === "video" ? videoVersions : imageVersions;
  $: compareLeftItem =
    compareCandidates.find((f) => f.version === compareLeftVer) ??
    compareCandidates[compareCandidates.length - 1] ??
    null;
  $: compareRightItem =
    compareCandidates.find((f) => f.version === compareRightVer) ??
    compareCandidates[0] ??
    null;

  // Đường dẫn media đang hiển thị trong ngăn Chi tiết
  $: activeImageVersion =
    previewFileOverride &&
    (previewFileOverride.kind === "anh" || previewFileOverride.kind === "image")
      ? previewFileOverride
      : null;
  $: activeVideoVersion =
    previewFileOverride && previewFileOverride.kind === "video"
      ? previewFileOverride
      : null;

  $: previewImageAbsPath =
    activeImageVersion?.abs_path ?? detail?.image_abs_path ?? null;
  $: previewImageName =
    activeImageVersion?.file_name ?? detail?.row.image_file ?? "";

  $: previewVideoAbsPath =
    activeVideoVersion?.abs_path ?? detail?.video_abs_path ?? null;
  $: previewVideoName =
    activeVideoVersion?.file_name ?? detail?.row.video_file ?? "";

  $: lightboxUrl = lightboxAbsPath
    ? toAssetUrl(
        lightboxAbsPath,
        rowRev(detail?.row.row_key, $rowAssetRevs, $assetCacheBust)
      )
    : "";

  // (E) Modal xem video lớn trong màn Dòng việc (UI-3)
  const videoModalGuard = createSeqGuard();
  let destroyed = false;
  let videoModalOpen = false;
  let videoModalDetail: RowDetail | null = null;
  let videoModalAbsPath: string | null = null;
  let videoModalFileName = "";
  let videoModalTitle = "";
  let videoModalPromptImage = "";
  let videoModalPromptVideo = "";
  let videoModalDurationSec: number | null = null;
  let videoModalWidth: number | null = null;
  let videoModalHeight: number | null = null;
  let videoModalAccount = "";
  let videoPlayerEl: HTMLVideoElement | null = null;
  let copiedImagePrompt = false;
  let copiedVideoPrompt = false;
  let copyImgFeedbackTimer: ReturnType<typeof setTimeout> | null = null;
  let copyVidFeedbackTimer: ReturnType<typeof setTimeout> | null = null;

  function rowRev(
    rowKey: string | null | undefined,
    revs: Record<string, number>,
    baseBust: number
  ): number {
    return Math.max((rowKey && revs[rowKey]) || 0, baseBust);
  }

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

  $: videoModalUrl = videoModalAbsPath
    ? toAssetUrl(
        videoModalAbsPath,
        rowRev(videoModalDetail?.row.row_key, $rowAssetRevs, $assetCacheBust)
      )
    : "";

  $: anyModalOpen =
    deleteConfirmOpen ||
    resetConfirmOpen ||
    unsavedPromptModalOpen ||
    batchConfirmOpen ||
    scanModalOpen ||
    compareModalOpen ||
    lightboxOpen ||
    videoModalOpen;

  function syncUiState(): void {
    rowsUiState.set({
      page,
      pageSize,
      chip,
      search,
      viewMode,
    });
  }

  function updateSelection(next: Set<string>): void {
    selected = next;
    selectedRows.set(new Set(next));
  }

  function pruneSelection(maxAtt: number): void {
    lastPrunedMaxAttempts = maxAtt;
    const pruned = pruneSelectedKeys(selected, rows, maxAtt);
    if (pruned !== selected) {
      updateSelection(pruned);
    }
  }

  function isRowOrphan(r: RowSummary, brokenMap: Record<string, boolean>): boolean {
    return r.ref_exists === false || Boolean(brokenMap[`ref:${r.row_key}`]);
  }

  function chipCount(key: string, c: Record<string, number>): number {
    if (key === "Lỗi") {
      return (
        (c["Lỗi tạo image"] ?? 0) +
        (c["Lỗi tạo video"] ?? 0) +
        (c["Lỗi tạm thời"] ?? 0)
      );
    }
    return c[key] ?? 0;
  }

  onMount(async () => {
    mounted = true;
    lastProjectPath = $project?.path ?? null;
    lastTick = $rowChangedTick;
    await consumePendingFilterAndLoad();
  });

  onDestroy(() => {
    destroyed = true;
    videoModalGuard.invalidate();
    ++detailReqSeq;
    loadGuard.invalidate();
    if (searchTimer) clearTimeout(searchTimer);
    if (copyImgFeedbackTimer) clearTimeout(copyImgFeedbackTimer);
    if (copyVidFeedbackTimer) clearTimeout(copyVidFeedbackTimer);
    if (videoPlayerEl) {
      try {
        videoPlayerEl.pause();
      } catch {}
    }
  });

  // Đọc bộ lọc chuyển tiếp từ Dashboard (ví dụ bấm thẻ "Dòng lỗi cần xem" -> chip = "Lỗi")
  $: if (mounted && $pendingRowsFilter) {
    void consumePendingFilterAndLoad();
  }

  // Tự động nạp lại khi đổi dự án hoặc khi có sự kiện row_changed / external_files_changed
  $: if (mounted) {
    const curPath = $project?.path ?? null;
    const curTick = $rowChangedTick;
    if (curPath !== lastProjectPath) {
      lastProjectPath = curPath;
      lastTick = curTick;
      hasLoadedRows = false;
      const resetUi = get(rowsUiState);
      page = resetUi.page || 1;
      pageSize = resetUi.pageSize || pageSize;
      chip = resetUi.chip || "Tất cả";
      search = resetUi.search || "";
      viewMode = resetUi.viewMode || viewMode;
      updateSelection(new Set());
      forceCloseDrawer();
      closeVideoModal();
      videoModalDetail = null;
      videoModalAbsPath = null;
      brokenThumbs = {};
      if (curPath) {
        void load();
      } else {
        loadGuard.invalidate();
        hasLoadedRows = false;
        loading = false;
        rows = [];
        total = 0;
        pageCount = 1;
        counts = {};
        loadError = null;
      }
    } else if (curTick !== lastTick) {
      lastTick = curTick;
      brokenThumbs = {};
      if (curPath) {
        void load({ silent: true });
        if (detail && !detailLoading) {
          void refreshOpenDetailSilently(detail.row.row_key);
        }
      }
    }
  }

  async function consumePendingFilterAndLoad(): Promise<void> {
    const pending = $pendingRowsFilter;
    if (pending) {
      pendingRowsFilter.set(null);
      page = 1;
      updateSelection(new Set());
      if (filterChips.some((c) => c.key === pending)) {
        chip = pending;
      } else {
        chip = "Tất cả";
      }
      syncUiState();
    }
    if ($project) {
      await load();
    }
  }

  export async function load(opts?: { silent?: boolean }): Promise<void> {
    if (!$project) {
      loadGuard.invalidate();
      hasLoadedRows = false;
      loading = false;
      rows = [];
      total = 0;
      pageCount = 1;
      counts = {};
      return;
    }
    const silent = Boolean(opts?.silent);
    if (!silent) {
      brokenThumbs = {};
      assetCacheBust.set(Date.now());
    }
    await runWithLoadingGuard(
      loadGuard,
      silent,
      (nextLoading) => {
        loading = nextLoading;
      },
      async (seq) => {
        const statusArg = chip === "Tất cả" ? undefined : chip;
        const queryArg = search.trim() ? search.trim() : undefined;
        const res = await api.get_rows(statusArg, queryArg, page, pageSize);
        if (!loadGuard.isCurrent(seq)) return;

        const nextPageCount = Math.max(1, res.page_count || 1);
        if (res.rows.length === 0 && page > nextPageCount) {
          page = nextPageCount;
          syncUiState();
          const retryRes = await api.get_rows(statusArg, queryArg, page, pageSize);
          if (!loadGuard.isCurrent(seq)) return;
          rows = retryRes.rows;
          total = retryRes.total;
          pageCount = Math.max(1, retryRes.page_count || 1);
          counts = retryRes.status_counts ?? counts;
        } else {
          rows = res.rows;
          total = res.total;
          pageCount = nextPageCount;
          counts = res.status_counts ?? counts;
        }
        hasLoadedRows = true;
        pruneSelection($maxAttemptsStore);
        loadError = null;
      },
      (e) => {
        loadError = formatApiError(e);
        if (!silent) {
          showToast("error", loadError);
        }
      }
    );
  }

  async function refreshOpenDetailSilently(rowKey: string): Promise<void> {
    const seq = ++detailReqSeq;
    try {
      const fresh = await api.get_row_detail(rowKey);
      if (seq !== detailReqSeq || !detail || detail.row.row_key !== rowKey) return;
      const keepDirty = promptsDirty;
      detail = fresh;
      if (!keepDirty) {
        syncPromptsFromDetail(fresh);
      }
    } catch {
      // Bỏ qua nếu dòng vừa bị xóa/đổi tên
    }
  }

  function syncPromptsFromDetail(d: RowDetail): void {
    origPromptImage = d.prompt_image ?? "";
    origPromptVideo = d.prompt_video ?? "";
    draftPromptImage = origPromptImage;
    draftPromptVideo = origPromptVideo;
  }

  function chooseInitialPreviewTab(d: RowDetail, preferred?: PreviewTab): PreviewTab {
    if (preferred) return preferred;
    if (d.video_abs_path) return "video";
    if (d.image_abs_path) return "generated";
    return "ref";
  }

  function selectChip(s: string): void {
    chip = chip === s && s !== "Tất cả" ? "Tất cả" : s;
    page = 1;
    updateSelection(new Set());
    syncUiState();
    void load();
  }

  function setViewMode(mode: ViewMode): void {
    if (viewMode === mode) return;
    viewMode = mode;
    syncUiState();
  }

  function onSearchInput(): void {
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      page = 1;
      updateSelection(new Set());
      syncUiState();
      void load();
    }, 300);
  }

  function clearSearch(): void {
    if (searchTimer) clearTimeout(searchTimer);
    search = "";
    page = 1;
    updateSelection(new Set());
    syncUiState();
    void load();
  }

  function clearAllFilters(): void {
    if (searchTimer) clearTimeout(searchTimer);
    chip = "Tất cả";
    search = "";
    page = 1;
    updateSelection(new Set());
    syncUiState();
    void load();
  }

  function changePage(nextPage: number): void {
    if (nextPage < 1 || nextPage > pageCount || nextPage === page) return;
    page = nextPage;
    updateSelection(new Set());
    syncUiState();
    void load();
  }

  function onPageSizeChange(): void {
    page = 1;
    updateSelection(new Set());
    syncUiState();
    void load();
  }

  function toggle(row: RowSummary): void {
    if (isRowLockedForBatch(row, $maxAttemptsStore)) return;
    const next = new Set(selected);
    if (next.has(row.row_key)) {
      next.delete(row.row_key);
    } else {
      next.add(row.row_key);
    }
    updateSelection(next);
  }

  function toggleAll(e: Event): void {
    const el = e.currentTarget as HTMLInputElement;
    if (el.checked) {
      updateSelection(new Set(selectableRows.map((r) => r.row_key)));
    } else {
      updateSelection(new Set());
    }
  }

  // Mở / đóng Drawer có bảo vệ sequence & kiểm tra prompt chưa lưu
  function requestOpenDetail(rowKey: string, preferredTab?: PreviewTab): void {
    if (detail && detail.row.row_key === rowKey) {
      if (preferredTab) previewTab = preferredTab;
      return;
    }
    if (detail && promptsDirty && detail.row.row_key !== rowKey) {
      pendingDrawerAction = { type: "open", rowKey, preferredTab };
      unsavedPromptModalOpen = true;
      return;
    }
    void loadDetailImmediate(rowKey, preferredTab);
  }

  async function loadDetailImmediate(
    rowKey: string,
    preferredTab?: PreviewTab
  ): Promise<void> {
    const seq = ++detailReqSeq;
    detailLoading = true;
    previewFileOverride = null;
    try {
      const d = await api.get_row_detail(rowKey);
      if (seq !== detailReqSeq) return;
      detail = d;
      syncPromptsFromDetail(d);
      previewTab = chooseInitialPreviewTab(d, preferredTab);
    } catch (e) {
      if (seq === detailReqSeq) {
        showToast("error", formatApiError(e));
      }
    } finally {
      if (seq === detailReqSeq) {
        detailLoading = false;
      }
    }
  }

  function requestCloseDrawer(): void {
    if (detail && promptsDirty) {
      pendingDrawerAction = { type: "close" };
      unsavedPromptModalOpen = true;
      return;
    }
    forceCloseDrawer();
  }

  function forceCloseDrawer(): void {
    ++detailReqSeq;
    detailLoading = false;
    detail = null;
    previewFileOverride = null;
    unsavedPromptModalOpen = false;
    pendingDrawerAction = null;
  }

  function discardPromptAndProceed(): void {
    const act = pendingDrawerAction;
    unsavedPromptModalOpen = false;
    pendingDrawerAction = null;
    if (detail) {
      syncPromptsFromDetail(detail);
    }
    if (!act || act.type === "close") {
      forceCloseDrawer();
    } else {
      void loadDetailImmediate(act.rowKey, act.preferredTab);
    }
  }

  async function savePromptAndProceed(): Promise<void> {
    const ok = await ensurePromptsSavedIfDirty();
    if (!ok) return;
    showToast("success", "Đã lưu prompt");
    await load({ silent: true });
    const act = pendingDrawerAction;
    unsavedPromptModalOpen = false;
    pendingDrawerAction = null;
    if (!act || act.type === "close") {
      forceCloseDrawer();
    } else {
      void loadDetailImmediate(act.rowKey, act.preferredTab);
    }
  }

  function onWindowKeydown(e: KeyboardEvent): void {
    if (e.key === "Escape" && (detail || detailLoading) && !anyModalOpen) {
      e.preventDefault();
      requestCloseDrawer();
    }
  }

  async function stopCurrentRun(): Promise<void> {
    if (!$running || stoppingRun) return;
    stoppingRun = true;
    try {
      await api.stop_run();
      showToast("info", "Đang dừng an toàn sau khi bước hiện tại hoàn tất...");
    } catch (e) {
      stoppingRun = false;
      showToast("error", formatApiError(e));
    }
  }

  async function handleRunSelectedClick(): Promise<void> {
    if (selected.size === 0 || $running || batchStarting) return;
    batchStarting = true;
    try {
      const dr = await api.start_dry_run();
      batchDryRunProbe = dr;
      if ($confirmStartEnabled || dr.is_csv_locked) {
        batchConfirmOpen = true;
        return;
      }
      await executeRunSelected();
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      batchStarting = false;
    }
  }

  async function executeRunSelected(): Promise<void> {
    if (selected.size === 0 || $running) return;
    const keys = Array.from(selected);
    batchStarting = true;
    try {
      batchConfirmOpen = false;
      await api.start_run("da_chon", keys);
      updateSelection(new Set());
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      batchStarting = false;
    }
  }

  async function runOne(rowKey: string): Promise<void> {
    if ($running) return;
    try {
      await api.run_row(rowKey);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function ensurePromptsSavedIfDirty(): Promise<boolean> {
    if (!detail || !promptsDirty) return true;
    const oldKey = detail.row.row_key;
    savingPrompts = true;
    try {
      const updated = await api.save_row_prompts(
        oldKey,
        draftPromptImage,
        draftPromptVideo
      );
      detail = updated;
      syncPromptsFromDetail(updated);
      if (updated.row.row_key !== oldKey && selected.has(oldKey)) {
        const next = new Set(selected);
        next.delete(oldKey);
        next.add(updated.row.row_key);
        updateSelection(next);
      }
      return true;
    } catch (e) {
      showToast("error", formatApiError(e));
      return false;
    } finally {
      savingPrompts = false;
    }
  }

  async function savePrompts(): Promise<void> {
    if (!detail || $running || !promptsDirty || savingPrompts) return;
    const oldKey = detail.row.row_key;
    savingPrompts = true;
    try {
      const updated = await api.save_row_prompts(
        oldKey,
        draftPromptImage,
        draftPromptVideo
      );
      detail = updated;
      syncPromptsFromDetail(updated);
      if (updated.row.row_key !== oldKey && selected.has(oldKey)) {
        const next = new Set(selected);
        next.delete(oldKey);
        next.add(updated.row.row_key);
        updateSelection(next);
      }
      showToast("success", "Đã lưu prompt");
      await load({ silent: true });
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      savingPrompts = false;
    }
  }

  async function runOneFromDrawer(): Promise<void> {
    if (!detail || $running || actionBusy) return;
    actionBusy = true;
    try {
      const ok = await ensurePromptsSavedIfDirty();
      if (!ok) return;
      await api.run_row(detail.row.row_key);
      showToast("info", `Đã bắt đầu chạy dòng ${detail.row.image_name}`);
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      actionBusy = false;
    }
  }

  async function rerunStep(step: "anh" | "video"): Promise<void> {
    if (!detail || $running || actionBusy) return;
    if (step === "video" && (!detail.row.image_file || imgPromptEdited)) return;
    actionBusy = true;
    try {
      const ok = await ensurePromptsSavedIfDirty();
      if (!ok) return;
      await api.rerun_row_step(detail.row.row_key, step);
      showToast(
        "info",
        step === "anh"
          ? `Đang chạy lại bước Ảnh + Video cho ${detail.row.image_name}`
          : `Đang chạy lại riêng bước Video cho ${detail.row.image_name}`
      );
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      actionBusy = false;
    }
  }

  async function confirmResetStatus(): Promise<void> {
    if (!detail || $running || actionBusy) return;
    actionBusy = true;
    const wasDirty = promptsDirty;
    try {
      const updated = await api.reset_row_status(detail.row.row_key);
      resetConfirmOpen = false;
      detail = updated;
      if (wasDirty) {
        origPromptImage = updated.prompt_image;
        origPromptVideo = updated.prompt_video;
      } else {
        syncPromptsFromDetail(updated);
      }
      showToast(
        "success",
        `Đã đặt lại trạng thái về Đang chờ (0/${$maxAttemptsStore} lần thử).`
      );
      await load({ silent: true });
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      actionBusy = false;
    }
  }

  async function skipRow(): Promise<void> {
    if (!detail || $running || actionBusy) return;
    actionBusy = true;
    try {
      const updated = await api.skip_row(detail.row.row_key);
      detail = updated;
      syncPromptsFromDetail(updated);
      const next = new Set(selected);
      next.delete(updated.row.row_key);
      updateSelection(next);
      showToast("info", `Đã đánh dấu bỏ qua dòng ${updated.row.image_name}.`);
      await load({ silent: true });
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      actionBusy = false;
    }
  }

  // Hỗ trợ Chọn lại ảnh & Xóa dòng trực tiếp từ hàng bảng/grid HOẶC từ Drawer
  async function remapRowByKey(targetKey: string): Promise<void> {
    if ($running || actionBusy) return;
    actionBusy = true;
    const wasDirty = detail?.row.row_key === targetKey && promptsDirty;
    try {
      const picked = await api.pick_image_in_input();
      if (!picked) return;
      const updated = await api.remap_reference_image(targetKey, picked);
      brokenThumbs = {};
      assetCacheBust.set(Date.now());
      if (detail && detail.row.row_key === targetKey) {
        detail = updated;
        if (wasDirty) {
          origPromptImage = updated.prompt_image;
          origPromptVideo = updated.prompt_video;
        } else {
          syncPromptsFromDetail(updated);
        }
        previewFileOverride = null;
        previewTab = "ref";
      }
      if (selected.has(targetKey)) {
        const next = new Set(selected);
        next.delete(targetKey);
        if (!isRowLockedForBatch(updated.row, $maxAttemptsStore)) {
          next.add(updated.row.row_key);
        }
        updateSelection(next);
      }
      showToast("success", `Đã cập nhật ảnh tham chiếu thành "${picked}".`);
      await load({ silent: true });
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      actionBusy = false;
    }
  }

  function openDeleteModalForKey(targetKey: string): void {
    if ($running || actionBusy) return;
    deleteTargetKey = targetKey;
    deleteConfirmOpen = true;
  }

  async function confirmDeleteRow(): Promise<void> {
    if (!deleteTargetKey || $running || actionBusy) return;
    actionBusy = true;
    const keyToDelete = deleteTargetKey;
    try {
      await api.delete_row(keyToDelete);
      deleteConfirmOpen = false;
      deleteTargetKey = "";
      if (detail && detail.row.row_key === keyToDelete) {
        forceCloseDrawer();
      }
      if (videoModalDetail?.row.row_key === keyToDelete) {
        closeVideoModal();
      }
      const next = new Set(selected);
      next.delete(keyToDelete);
      updateSelection(next);
      project.update((cur) =>
        cur ? { ...cur, row_count: Math.max(0, cur.row_count - 1) } : cur
      );
      showToast("success", `Đã xóa dòng "${keyToDelete}" khỏi prompts.csv.`);
      await load();
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      actionBusy = false;
    }
  }

  // (A) Quét & nhập hàng loạt ảnh từ thư mục input/
  async function openScanInputModal(): Promise<void> {
    if (!$project || $running || scanningInput) return;
    scanningInput = true;
    try {
      const items = await api.scan_untracked_input_images();
      untrackedItems = items;
      selectedImportNames = new Set(items.map((i) => i.file_name));
      scanModalOpen = true;
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      scanningInput = false;
    }
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
    if (selectedImportNames.size === untrackedItems.length) {
      selectedImportNames = new Set();
    } else {
      selectedImportNames = new Set(untrackedItems.map((i) => i.file_name));
    }
  }

  async function executeBatchImport(): Promise<void> {
    if (selectedImportNames.size === 0 || importingInput || $running) return;
    importingInput = true;
    try {
      const names = Array.from(selectedImportNames);
      const count = await api.import_input_images(
        names,
        defaultImportPromptImg.trim(),
        defaultImportPromptVid.trim()
      );
      scanModalOpen = false;
      brokenThumbs = {};
      assetCacheBust.set(Date.now());
      project.update((cur) =>
        cur ? { ...cur, row_count: cur.row_count + count } : cur
      );
      rowChangedTick.update((n) => n + 1);
      showToast("success", `Đã nhập ${count} ảnh mới từ input/ vào prompts.csv.`);
      await load();
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      importingInput = false;
    }
  }

  // (C) Đặt phiên bản làm bản chính & Mở cửa sổ so sánh v1 vs v2
  async function setAsCurrentVersion(
    kindRaw: string,
    version: number
  ): Promise<void> {
    if (!detail || $running || switchingVersion) return;
    switchingVersion = true;
    const normalizedKind: "image" | "video" =
      kindRaw === "video" ? "video" : "image";
    try {
      const updated = await api.set_current_file_version(
        detail.row.row_key,
        normalizedKind,
        version
      );
      detail = updated;
      previewFileOverride = null;
      assetCacheBust.set(Date.now());
      showToast(
        "success",
        `Đã chọn phiên bản v${version} (${normalizedKind === "video" ? "Video" : "Ảnh"}) làm bản chính.`
      );
      await load({ silent: true });
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      switchingVersion = false;
    }
  }

  function openCompareModal(initialKind?: "image" | "video"): void {
    const preferVideo =
      initialKind === "video"
        ? videoVersions.length >= 2
        : imageVersions.length < 2 && videoVersions.length >= 2;
    compareKind = preferVideo ? "video" : "image";
    initComparePair(compareKind === "video" ? videoVersions : imageVersions);
    compareModalOpen = true;
  }

  function switchCompareKind(nextKind: "image" | "video"): void {
    compareKind = nextKind;
    initComparePair(nextKind === "video" ? videoVersions : imageVersions);
  }

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

  async function revealInExplorer(absPath: string | null | undefined): Promise<void> {
    if (!absPath) return;
    try {
      await api.open_path_in_explorer(absPath, true);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  function previewVersionFile(f: FileVersionItem): void {
    previewFileOverride = f.is_current ? null : f;
    if (f.kind === "video") {
      previewTab = "video";
    } else if (f.kind === "anh" || f.kind === "image") {
      previewTab = "generated";
    } else {
      previewTab = "ref";
    }
  }

  // (D) Lightbox Zoom 25%-400% & Drag-to-Pan
  function openImageLightbox(
    absPath: string | null | undefined,
    title: string
  ): void {
    if (!absPath) return;
    lightboxAbsPath = absPath;
    lightboxTitle = title;
    zoomScale = 1;
    panX = 0;
    panY = 0;
    isPanning = false;
    lightboxOpen = true;
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

  // (E) Modal xem video lớn trong màn Dòng việc (UI-3)
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

  function openVideoModal(d: RowDetail | null | undefined): void {
    if (!d) return;
    videoModalGuard.invalidate();
    videoModalDetail = d;
    const sameDetail = detail?.row.row_key === d.row.row_key;
    videoModalAbsPath =
      (sameDetail ? activeVideoVersion?.abs_path : null) ?? d.video_abs_path ?? null;
    videoModalFileName =
      (sameDetail ? activeVideoVersion?.file_name : null) ?? d.row.video_file ?? "";
    videoModalTitle = d.row.image_name || d.row.row_key;
    videoModalPromptImage = d.prompt_image ?? "";
    videoModalPromptVideo = d.prompt_video ?? "";
    videoModalDurationSec = d.video_duration ?? null;
    videoModalWidth = null;
    videoModalHeight = null;
    if (d.video_resolution) {
      const parts = d.video_resolution.split("x");
      if (parts.length === 2) {
        videoModalWidth = parseInt(parts[0], 10) || null;
        videoModalHeight = parseInt(parts[1], 10) || null;
      }
    }
    videoModalAccount = d.account_used || "";
    copiedImagePrompt = false;
    copiedVideoPrompt = false;
    videoModalOpen = true;
  }

  async function openVideoModalByKey(rowKey: string): Promise<void> {
    if (detail && detail.row.row_key === rowKey && (detail.video_abs_path || activeVideoVersion)) {
      openVideoModal(detail);
      return;
    }
    const seq = videoModalGuard.next();
    const expectedProjectPath = $project?.path ?? null;
    try {
      const d = await api.get_row_detail(rowKey);
      if (
        destroyed ||
        !videoModalGuard.isCurrent(seq) ||
        ($project?.path ?? null) !== expectedProjectPath
      ) {
        return;
      }
      openVideoModal(d);
    } catch (e) {
      if (
        !destroyed &&
        videoModalGuard.isCurrent(seq) &&
        ($project?.path ?? null) === expectedProjectPath
      ) {
        showToast("error", formatApiError(e));
      }
    }
  }

  function closeVideoModal(): void {
    videoModalGuard.invalidate();
    if (videoPlayerEl) {
      try {
        videoPlayerEl.pause();
      } catch {}
    }
    videoModalOpen = false;
  }

  function handleModalVideoMetadata(e: Event): void {
    const el = e.currentTarget as HTMLVideoElement | null;
    if (!el) return;
    if (Number.isFinite(el.duration) && el.duration > 0) {
      videoModalDurationSec = el.duration;
    }
    if (el.videoWidth > 0 && el.videoHeight > 0) {
      videoModalWidth = el.videoWidth;
      videoModalHeight = el.videoHeight;
    }
  }

  async function copyModalPrompt(kind: "image" | "video"): Promise<void> {
    const text = kind === "image" ? videoModalPromptImage : videoModalPromptVideo;
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

  function projectFileUrl(
    subDir: "input" | "image_out" | "video_out",
    fileName: string,
    cacheKey: number
  ): string {
    if (!$project || !fileName) return "";
    const base = $project.path.replace(/[\\/]+$/, "");
    return toAssetUrl(`${base}/${subDir}/${fileName}`, cacheKey);
  }

  function markThumbBroken(key: string): void {
    brokenThumbs = { ...brokenThumbs, [key]: true };
  }

  function formatActiveLabel(info: { step: string; detail: string }): string {
    const stepPrefix =
      info.step === "B2"
        ? "Đang tạo image"
        : info.step === "B3"
          ? "Đang tạo video"
          : info.step === "download"
            ? "Đang tải video"
            : "Đang xử lý";
    return info.detail ? `${stepPrefix} · ${info.detail}` : stepPrefix;
  }

  function badgeClass(s: string): string {
    if (s === "Đang chờ") return "badge-cho";
    if (s === "Đã tạo image" || s === "Đang tạo image" || s === "Đang tạo video") {
      return "badge-image";
    }
    if (s === "Đã tạo video") return "badge-video";
    if (s === "Lỗi tạo image") return "badge-loi-image";
    if (s === "Lỗi tạo video") return "badge-loi-video";
    if (s === "Lỗi tạm thời") return "badge-loi-tam-thoi";
    if (s === "Bỏ qua") return "badge-bo-qua";
    return "badge-cho";
  }

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

<svelte:window on:keydown={onWindowKeydown} />

<div class="rows-screen">
  <div class="top-fixed">
    {#if $running}
      <div class="live-strip">
        <div class="live-strip-top">
          <span class="live-label">
            <span class="live-dot"></span>
            <span>{$progressText || "Đang xử lý lượt chạy..."}</span>
          </span>
          <div class="live-strip-right">
            <span class="live-note">Các nút Start riêng tạm khóa khi đang chạy</span>
            <button
              type="button"
              class="btn-danger live-stop-btn"
              disabled={stoppingRun}
              on:click={stopCurrentRun}
            >
              <Icons name="stop" size={13} />
              {stoppingRun ? "Đang dừng..." : "STOP"}
            </button>
          </div>
        </div>
        <div class="progress-bar compact-bar">
          <div style="width: {$progressPercent}%"></div>
        </div>
      </div>
    {/if}

    <div class="toolbar">
      <div class="search-wrap">
        <span class="search-icon"><Icons name="search" size={15} /></span>
        <input
          id="rows-search-input"
          class="input search"
          type="text"
          aria-label="Tìm theo tên ảnh tham chiếu"
          placeholder="Tìm theo tên ảnh tham chiếu..."
          bind:value={search}
          on:input={onSearchInput}
        />
        {#if search}
          <button
            type="button"
            class="clear-search"
            aria-label="Xóa từ khóa tìm kiếm"
            on:click={clearSearch}
          >
            <Icons name="x" size={13} />
          </button>
        {/if}
      </div>

      <button
        type="button"
        class="btn-secondary"
        on:click={() => load()}
        disabled={loading || $running || !$project}
      >
        <Icons name="refresh" size={15} />
        {loading ? "Đang nạp..." : "Nạp lại"}
      </button>

      <button
        type="button"
        class="btn-secondary"
        disabled={$running || !$project || scanningInput}
        title="Quét thư mục input/ để thêm nhanh các file ảnh chưa có trong prompts.csv"
        on:click={openScanInputModal}
      >
        <Icons name="folder-plus" size={15} />
        {scanningInput ? "Đang quét..." : "Quét ảnh input/"}
      </button>

      <!-- Segmented toggle chuyển đổi Bảng / Lưới -->
      <div class="view-toggle" role="group" aria-label="Chế độ hiển thị">
        <button
          type="button"
          class="view-btn"
          class:active={viewMode === "table"}
          title="Chế độ bảng chi tiết"
          on:click={() => setViewMode("table")}
        >
          <Icons name="list" size={15} />
          <span>Bảng</span>
        </button>
        <button
          type="button"
          class="view-btn"
          class:active={viewMode === "grid"}
          title="Chế độ lưới thẻ media"
          on:click={() => setViewMode("grid")}
        >
          <Icons name="grid" size={15} />
          <span>Lưới</span>
        </button>
      </div>

      <div class="spacer"></div>

      <button
        type="button"
        class="btn-primary"
        disabled={selected.size === 0 || $running || batchStarting || !$project}
        title={$running
          ? "Đang có lượt chạy — chờ xong hoặc bấm STOP"
          : selected.size === 0
            ? "Tích chọn ít nhất 1 dòng khả dụng để chạy"
            : `Chạy ${selected.size} dòng đã chọn`}
        on:click={handleRunSelectedClick}
      >
        <Icons name="play" size={15} />
        {selected.size > 0
          ? `Chạy ${selected.size} dòng đã chọn`
          : "Chạy dòng đã chọn"}
      </button>
    </div>

    <div class="chips">
      {#each filterChips as item}
        <button
          type="button"
          class="chip"
          class:active={chip === item.key}
          on:click={() => selectChip(item.key)}
        >
          <span>{item.label}</span>
          <span class="count">({chipCount(item.key, counts)})</span>
        </button>
      {/each}

      {#if chip !== "Tất cả" || search.trim() !== ""}
        <button type="button" class="btn-ghost reset-filter" on:click={clearAllFilters}>
          <Icons name="x" size={13} />
          Xóa lọc
        </button>
      {/if}
    </div>
  </div>

  {#if !$project}
    <div class="card empty-project">
      <Icons name="folder-open" size={32} />
      <div class="empty-project-title">Chưa mở dự án Local nào</div>
      <div class="empty-project-sub">
        Hãy chọn hoặc tạo thư mục dự án ở màn hình <strong>Thư mục Local</strong> để xem danh sách dòng việc.
      </div>
    </div>
  {:else}
    {#if loadError}
      <div class="load-err-card selectable" role="alert">
        <div class="load-err-text">
          <Icons name="alert" size={18} />
          <span>{loadError}</span>
        </div>
        <button type="button" class="btn-primary small-btn" on:click={() => load()}>
          Thử lại
        </button>
      </div>
    {/if}

    <div class="table-wrap card">
      {#if viewMode === "table"}
        <div class="table-scroll">
          <table>
            <thead>
              <tr>
                <th class="w-check">
                  <input
                    type="checkbox"
                    aria-label="Chọn tất cả dòng khả dụng trên trang hiện tại"
                    checked={allChecked}
                    indeterminate={someChecked}
                    disabled={selectableRows.length === 0 || $running}
                    on:change={toggleAll}
                  />
                </th>
                <th class="col-ref">Ảnh tham chiếu</th>
                <th class="w-76">Ảnh</th>
                <th class="w-84">Video</th>
                <th class="w-180">Trạng thái</th>
                <th class="w-68">Thử</th>
                <th class="w-96">Chi tiết</th>
                <th class="w-96">Thao tác</th>
              </tr>
            </thead>
            <tbody>
              {#each rows as r (r.row_key)}
                {@const isLockedRow = isRowLockedForBatch(r, $maxAttemptsStore)}
                {@const activeInfo = $activeRows[r.row_key]}
                {@const refThumbKey = `ref:${r.row_key}`}
                {@const genThumbKey = `gen:${r.row_key}:${r.image_file}`}
                {@const orphanRow = isRowOrphan(r, brokenThumbs)}
                <tr
                  class:running-row={Boolean(activeInfo)}
                  class:selected-row={detail?.row.row_key === r.row_key}
                >
                  <td>
                    <input
                      type="checkbox"
                      aria-label={`Chọn dòng ${r.image_name}`}
                      checked={selected.has(r.row_key)}
                      disabled={isLockedRow || $running}
                      title={r.status === "Đã tạo video"
                        ? "Dòng đã hoàn tất — mở Chi tiết để tạo bản mới (_vN)"
                        : r.status === "Bỏ qua"
                          ? "Dòng đang bỏ qua — mở Chi tiết để đặt lại trạng thái"
                          : r.attempts_count >= $maxAttemptsStore && !r.prompt_modified
                            ? `Đã chạm giới hạn ${$maxAttemptsStore}/${$maxAttemptsStore} lần thử — mở Chi tiết để đặt lại hoặc sửa prompt`
                            : `Chọn ${r.image_name}`}
                      on:change={() => toggle(r)}
                    />
                  </td>

                  <td class="col-ref">
                    <div class="ref-cell-wrap">
                      <button
                        type="button"
                        class="key-btn"
                        title="Nhấn để mở ngăn Chi tiết dòng"
                        on:click={() => requestOpenDetail(r.row_key)}
                      >
                        <span class="ref-mini">
                          {#if !orphanRow}
                            <img
                              src={projectFileUrl("input", r.image_name, rowRev(r.row_key, $rowAssetRevs, $assetCacheBust))}
                              alt=""
                              loading="lazy"
                              on:error={() => markThumbBroken(refThumbKey)}
                            />
                          {:else}
                            <span class="mini-missing" title="Thiếu ảnh tham chiếu trong input/">
                              <Icons name="alert" size={14} />
                            </span>
                          {/if}
                        </span>

                        <span class="key-meta">
                          <span class="key-name">{r.image_name}</span>
                          <span class="key-sub-badges">
                            {#if r.prompt_modified}
                              <span class="badge badge-sua-prompt mini-badge">Đã sửa prompt</span>
                            {/if}
                            {#if r.noi_canh}
                              <span class="badge badge-noi-canh mini-badge" title="Nối cảnh liên tục từ frame cuối video trước (P2-1)">Nối cảnh</span>
                            {/if}
                            {#if orphanRow}
                              <span class="orphan-tag">Thiếu ảnh gốc</span>
                            {:else if r.last_error}
                              <span class="err-inline" title={r.last_error}>
                                <Icons name="alert" size={12} />
                                Có ghi chú lỗi
                              </span>
                            {/if}
                          </span>
                        </span>
                      </button>

                      {#if orphanRow}
                        <div class="inline-orphan-btns">
                          <button
                            type="button"
                            class="btn-secondary orphan-inline-btn"
                            disabled={$running || actionBusy}
                            title="Chọn lại file ảnh trong input/ cho dòng này"
                            on:click={() => remapRowByKey(r.row_key)}
                          >
                            Chọn lại ảnh
                          </button>
                          <button
                            type="button"
                            class="btn-ghost orphan-inline-btn danger-text"
                            disabled={$running || actionBusy}
                            title="Xóa dòng này khỏi prompts.csv"
                            on:click={() => openDeleteModalForKey(r.row_key)}
                          >
                            Xóa
                          </button>
                        </div>
                      {/if}
                    </div>
                  </td>

                  <td>
                    {#if r.image_file}
                      <button
                        type="button"
                        class="thumb-btn"
                        title={`Ảnh đã tạo: ${r.image_file} (nhấn để xem)`}
                        on:click={() => requestOpenDetail(r.row_key, "generated")}
                      >
                        {#if !brokenThumbs[genThumbKey]}
                          <img
                            class="thumb-img"
                            src={projectFileUrl("image_out", r.image_file, rowRev(r.row_key, $rowAssetRevs, $assetCacheBust))}
                            alt={`Ảnh tạo ${r.image_file}`}
                            loading="lazy"
                            on:error={() => markThumbBroken(genThumbKey)}
                          />
                        {:else}
                          <span class="media-badge">
                            <Icons name="image" size={15} />
                          </span>
                        {/if}
                      </button>
                    {:else}
                      <span class="dash-muted">—</span>
                    {/if}
                  </td>

                  <td>
                    {#if r.video_file}
                      <button
                        type="button"
                        class="video-pill"
                        title={`Xem video lớn: ${r.video_file}`}
                        on:click={() => openVideoModalByKey(r.row_key)}
                      >
                        <Icons name="video" size={14} />
                        <span>MP4</span>
                      </button>
                    {:else}
                      <span class="dash-muted">—</span>
                    {/if}
                  </td>

                  <td>
                    {#if activeInfo}
                      <span class="badge badge-image live-row-badge" title={activeInfo.detail}>
                        <span class="spin-dot"></span>
                        <span class="live-badge-text">{formatActiveLabel(activeInfo)}</span>
                      </span>
                    {:else}
                      <span class="badge {badgeClass(r.status)}">
                        {r.status}
                      </span>
                    {/if}
                  </td>

                  <td>
                    <span
                      class="attempts-cell"
                      class:exhausted={r.attempts_count >= $maxAttemptsStore}
                    >
                      {r.attempts_count} / {$maxAttemptsStore}
                    </span>
                  </td>

                  <td>
                    <button
                      type="button"
                      class="btn-ghost row-detail-btn"
                      on:click={() => requestOpenDetail(r.row_key)}
                    >
                      <Icons name="eye" size={14} />
                      Chi tiết
                    </button>
                  </td>

                  <td>
                    <button
                      type="button"
                      class="btn-secondary run-one"
                      disabled={$running || isLockedRow}
                      title={$running
                        ? "Đang có lượt chạy — chờ chạy xong hoặc bấm STOP"
                        : r.status === "Đã tạo video"
                          ? "Dòng đã xong — mở Chi tiết để tạo phiên bản mới (_vN)"
                          : r.status === "Bỏ qua"
                            ? "Dòng đang bỏ qua — mở Chi tiết để đặt lại trạng thái"
                            : r.attempts_count >= $maxAttemptsStore && !r.prompt_modified
                              ? `Đã chạm ${$maxAttemptsStore}/${$maxAttemptsStore} lần thử — mở Chi tiết để đặt lại hoặc sửa prompt`
                              : `Chạy riêng dòng ${r.image_name}`}
                      on:click={() => runOne(r.row_key)}
                    >
                      <Icons name="play" size={12} />
                      Start
                    </button>
                  </td>
                </tr>
              {:else}
                <tr>
                  <td colspan="8" class="empty">
                    {#if loading}
                      Đang tải danh sách dòng việc...
                    {:else if chip !== "Tất cả" || search.trim() !== ""}
                      Không có dòng nào khớp bộ lọc hiện tại.
                      <button type="button" class="btn-ghost" on:click={clearAllFilters}>
                        Xóa lọc
                      </button>
                    {:else}
                      Chưa có dòng việc nào trong <code>prompts.csv</code> — bấm <strong>Quét ảnh input/</strong> để thêm ảnh nhanh.
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {:else}
        <!-- (B) Chế độ Lưới thẻ Media (Gallery Grid View) -->
        <div class="grid-scroll">
          {#if rows.length === 0}
            <div class="empty">
              {#if loading}
                Đang tải danh sách dòng việc...
              {:else if chip !== "Tất cả" || search.trim() !== ""}
                Không có dòng nào khớp bộ lọc hiện tại.
                <button type="button" class="btn-ghost" on:click={clearAllFilters}>
                  Xóa lọc
                </button>
              {:else}
                Chưa có dòng việc nào trong <code>prompts.csv</code> — bấm <strong>Quét ảnh input/</strong> để thêm ảnh nhanh.
              {/if}
            </div>
          {:else}
            <div class="gallery-grid">
              {#each rows as r (r.row_key)}
                {@const isLockedRow = isRowLockedForBatch(r, $maxAttemptsStore)}
                {@const activeInfo = $activeRows[r.row_key]}
                {@const refThumbKey = `ref:${r.row_key}`}
                {@const genThumbKey = `gen:${r.row_key}:${r.image_file}`}
                {@const orphanRow = isRowOrphan(r, brokenThumbs)}
                <div
                  class="media-grid-card"
                  class:running-card={Boolean(activeInfo)}
                  class:selected-card={detail?.row.row_key === r.row_key}
                >
                  <div class="grid-media-box">
                    <input
                      class="grid-check"
                      type="checkbox"
                      aria-label={`Chọn dòng ${r.image_name}`}
                      checked={selected.has(r.row_key)}
                      disabled={isLockedRow || $running}
                      on:change={() => toggle(r)}
                    />

                    <button
                      type="button"
                      class="grid-preview-trigger"
                      on:click={() =>
                        requestOpenDetail(
                          r.row_key,
                          r.video_file ? "video" : r.image_file ? "generated" : "ref"
                        )}
                    >
                      {#if r.image_file && !brokenThumbs[genThumbKey]}
                        <img
                          src={projectFileUrl("image_out", r.image_file, rowRev(r.row_key, $rowAssetRevs, $assetCacheBust))}
                          alt={`Ảnh sản phẩm ${r.image_file}`}
                          loading="lazy"
                          on:error={() => markThumbBroken(genThumbKey)}
                        />
                      {:else if !orphanRow}
                        <img
                          src={projectFileUrl("input", r.image_name, rowRev(r.row_key, $rowAssetRevs, $assetCacheBust))}
                          alt={`Ảnh gốc ${r.image_name}`}
                          loading="lazy"
                          on:error={() => markThumbBroken(refThumbKey)}
                        />
                      {:else}
                        <div class="grid-missing">
                          <Icons name="alert" size={24} />
                          <span>Thiếu ảnh input/</span>
                        </div>
                      {/if}
                    </button>

                    {#if r.video_file}
                      <button
                        type="button"
                        class="grid-vid-badge"
                        title={`Xem video lớn: ${r.video_file}`}
                        on:click|stopPropagation={() => openVideoModalByKey(r.row_key)}
                      >
                        <Icons name="video" size={12} />
                        MP4
                      </button>
                    {:else if r.image_file}
                      <span class="grid-img-badge">Ảnh tạo</span>
                    {/if}
                  </div>

                  <div class="grid-card-body">
                    <div class="grid-title-row">
                      <button
                        type="button"
                        class="grid-title-btn"
                        title={r.row_key}
                        on:click={() => requestOpenDetail(r.row_key)}
                      >
                        {r.image_name}
                      </button>
                      <span
                        class="attempts-cell"
                        class:exhausted={r.attempts_count >= $maxAttemptsStore}
                      >
                        {r.attempts_count}/{$maxAttemptsStore}
                      </span>
                    </div>

                    <div class="grid-badges-row">
                      {#if activeInfo}
                        <span class="badge badge-image live-row-badge">
                          <span class="spin-dot"></span>
                          <span class="live-badge-text">{formatActiveLabel(activeInfo)}</span>
                        </span>
                      {:else}
                        <span class="badge {badgeClass(r.status)}">{r.status}</span>
                      {/if}
                      {#if r.prompt_modified}
                        <span class="badge badge-sua-prompt mini-badge">Đã sửa</span>
                      {/if}
                      {#if r.noi_canh}
                        <span class="badge badge-noi-canh mini-badge" title="Nối cảnh liên tục từ frame cuối video trước (P2-1)">Nối cảnh</span>
                      {/if}
                    </div>

                    {#if r.last_error}
                      <div class="err-inline" title={r.last_error}>
                        <Icons name="alert" size={12} />
                        <span class="grid-err-text">{r.last_error}</span>
                      </div>
                    {/if}

                    {#if orphanRow}
                      <div class="grid-orphan-row">
                        <button
                          type="button"
                          class="btn-secondary orphan-inline-btn"
                          disabled={$running || actionBusy}
                          on:click={() => remapRowByKey(r.row_key)}
                        >
                          Chọn lại ảnh
                        </button>
                        <button
                          type="button"
                          class="btn-ghost orphan-inline-btn danger-text"
                          disabled={$running || actionBusy}
                          on:click={() => openDeleteModalForKey(r.row_key)}
                        >
                          Xóa
                        </button>
                      </div>
                    {/if}

                    <div class="grid-card-actions">
                      <button
                        type="button"
                        class="btn-ghost grid-act-btn"
                        on:click={() => requestOpenDetail(r.row_key)}
                      >
                        <Icons name="eye" size={13} />
                        Chi tiết
                      </button>
                      <button
                        type="button"
                        class="btn-secondary grid-act-btn"
                        disabled={$running || isLockedRow}
                        on:click={() => runOne(r.row_key)}
                      >
                        <Icons name="play" size={12} />
                        Start
                      </button>
                    </div>
                  </div>
                </div>
              {/each}
            </div>
          {/if}
        </div>
      {/if}

      <div class="pagination">
        <label class="page-size-label" for="rows-page-size">Mỗi trang:</label>
        <select id="rows-page-size" bind:value={pageSize} on:change={onPageSizeChange}>
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

        <span class="total-summary">
          {#if chip === "Tất cả" && !search.trim()}
            Tổng {total} dòng
          {:else}
            Tổng {total} dòng (đang lọc từ {totalAllRows} dòng)
          {/if}
        </span>
      </div>
    </div>
  {/if}
</div>

<!-- Ngăn trượt Chi tiết dòng 440px (chỉ phủ vùng nội dung dưới Titlebar, phải Sidebar) -->
{#if detail || detailLoading}
  <div class="drawer-backdrop" on:click={requestCloseDrawer} role="presentation"></div>
  <aside class="drawer" aria-label="Chi tiết dòng việc">
    <div class="drawer-head">
      <div class="drawer-head-info">
        <div class="drawer-title selectable">{detail?.row?.row_key ?? "Đang tải..."}</div>
        {#if detail}
          <div class="drawer-badges">
            {#if $activeRows[detail.row.row_key]}
              <span class="badge badge-image live-row-badge">
                <span class="spin-dot"></span>
                <span>{formatActiveLabel($activeRows[detail.row.row_key])}</span>
              </span>
            {:else}
              <span class="badge {badgeClass(detail.row.status)}">{detail.row.status}</span>
            {/if}
            {#if detail.row.prompt_modified}
              <span class="badge badge-sua-prompt">Đã sửa prompt</span>
            {/if}
            {#if detail.row.noi_canh}
              <span class="badge badge-noi-canh" title="Nối cảnh liên tục từ frame cuối video trước (P2-1)">Nối cảnh</span>
            {/if}
            <span class="meta-chip">
              Dòng #{detail.row.row_no} · Thử {detail.row.attempts_count}/{$maxAttemptsStore}
            </span>
          </div>
        {/if}
      </div>
      <button
        type="button"
        class="btn-ghost close"
        aria-label="Đóng ngăn chi tiết"
        on:click={requestCloseDrawer}
      >
        <Icons name="x" size={17} />
      </button>
    </div>

    {#if detailLoading && !detail}
      <div class="drawer-loading">Đang nạp chi tiết dòng việc...</div>
    {:else if detail}
      <div class="drawer-body">
        {#if !detail.ref_exists}
          <!-- Khôi phục dòng mồ côi thiếu ảnh tham chiếu (TC-48, TC-49) -->
          <div class="orphan-card selectable" role="alert">
            <div class="orphan-title">
              <Icons name="alert" size={16} />
              <span>Thiếu ảnh tham chiếu trong thư mục <code>input/</code></span>
            </div>
            <p class="orphan-desc">
              Không tìm thấy file <strong>{detail.row.image_name}</strong>. Chọn lại ảnh mới để ánh xạ giữ nguyên lịch sử sản phẩm, hoặc xóa dòng này khỏi CSV.
            </p>
            <div class="orphan-actions">
              <button
                type="button"
                class="btn-primary small-btn"
                disabled={$running || actionBusy}
                on:click={() => detail && remapRowByKey(detail.row.row_key)}
              >
                <Icons name="image" size={14} />
                Chọn lại ảnh tham chiếu...
              </button>
              <button
                type="button"
                class="btn-secondary small-btn danger-outline"
                disabled={$running || actionBusy}
                on:click={() => detail && openDeleteModalForKey(detail.row.row_key)}
              >
                <Icons name="trash" size={14} />
                Xóa dòng khỏi CSV
              </button>
            </div>
          </div>
        {/if}

        {#if detail.row.last_error}
          <div class="error-box selectable">
            <div class="error-box-head">
              <Icons name="alert" size={14} />
              <span>Thông điệp lỗi gần nhất</span>
            </div>
            <div>{detail.row.last_error}</div>
          </div>
        {/if}

        <!-- Trình xem Media tích hợp theo 3 tab (Tham chiếu / Ảnh tạo / Video) -->
        <div class="media-card">
          <div class="media-tabs" role="tablist">
            <button
              type="button"
              role="tab"
              aria-selected={previewTab === "ref"}
              class="media-tab"
              class:active={previewTab === "ref"}
              on:click={() => (previewTab = "ref")}
            >
              <Icons name="image" size={14} />
              Tham chiếu
              {#if !detail.ref_exists}
                <span class="tab-dot warn"></span>
              {/if}
            </button>

            <button
              type="button"
              role="tab"
              aria-selected={previewTab === "generated"}
              class="media-tab"
              class:active={previewTab === "generated"}
              on:click={() => (previewTab = "generated")}
            >
              <Icons name="sparkles" size={14} />
              Ảnh tạo
              {#if previewImageAbsPath}
                <span class="tab-dot ok"></span>
              {/if}
            </button>

            <button
              type="button"
              role="tab"
              aria-selected={previewTab === "video"}
              class="media-tab"
              class:active={previewTab === "video"}
              on:click={() => (previewTab = "video")}
            >
              <Icons name="video" size={14} />
              Video
              {#if previewVideoAbsPath}
                <span class="tab-dot ok"></span>
              {/if}
            </button>
          </div>

          {#if previewFileOverride}
            <div class="version-preview-banner">
              <span>
                Đang xem <strong>v{previewFileOverride.version}</strong> ({previewFileOverride.file_name})
              </span>
              <div class="banner-inline-btns">
                {#if !previewFileOverride.is_current}
                  <button
                    type="button"
                    class="inline-reset"
                    disabled={$running || switchingVersion}
                    on:click={() =>
                      previewFileOverride &&
                      setAsCurrentVersion(previewFileOverride.kind, previewFileOverride.version)}
                  >
                    Đặt làm bản chính
                  </button>
                {/if}
                <button
                  type="button"
                  class="inline-reset"
                  on:click={() => (previewFileOverride = null)}
                >
                  Về bản hiện tại
                </button>
              </div>
            </div>
          {/if}

          <div class="media-stage">
            {#if previewTab === "ref"}
              {#if detail.ref_exists && detail.ref_abs_path}
                <button
                  type="button"
                  class="stage-img-btn"
                  title="Nhấn để phóng lớn ảnh tham chiếu (hỗ trợ zoom 25%-400%)"
                  on:click={() =>
                    openImageLightbox(detail?.ref_abs_path, `Ảnh tham chiếu: ${detail?.row.image_name}`)}
                >
                  <img
                    class="stage-img"
                    src={toAssetUrl(detail.ref_abs_path, rowRev(detail.row.row_key, $rowAssetRevs, $assetCacheBust))}
                    alt={`Ảnh tham chiếu ${detail.row.image_name}`}
                  />
                </button>
                <div class="media-bar">
                  <span class="media-filename selectable" title={detail.ref_abs_path}>
                    input/{detail.row.image_name}
                  </span>
                  <div class="media-bar-btns">
                    <button
                      type="button"
                      class="btn-ghost tiny-btn"
                      on:click={() =>
                        openImageLightbox(detail?.ref_abs_path, `Ảnh tham chiếu: ${detail?.row.image_name}`)}
                    >
                      <Icons name="zoom-in" size={13} />
                      Phóng lớn
                    </button>
                    <button
                      type="button"
                      class="btn-ghost tiny-btn"
                      on:click={() => revealInExplorer(detail?.ref_abs_path)}
                    >
                      <Icons name="folder-open" size={13} />
                      Hiện file
                    </button>
                  </div>
                </div>
              {:else}
                <div class="stage-empty">
                  <Icons name="alert" size={26} />
                  <span>File ảnh tham chiếu không tồn tại trong <code>input/</code></span>
                </div>
              {/if}
            {:else if previewTab === "generated"}
              {#if previewImageAbsPath}
                <button
                  type="button"
                  class="stage-img-btn"
                  title="Nhấn để phóng lớn ảnh sản phẩm (hỗ trợ zoom 25%-400%)"
                  on:click={() =>
                    openImageLightbox(previewImageAbsPath, `Ảnh sản phẩm: ${previewImageName}`)}
                >
                  <img
                    class="stage-img"
                    src={toAssetUrl(previewImageAbsPath, rowRev(detail.row.row_key, $rowAssetRevs, $assetCacheBust))}
                    alt={`Ảnh đã tạo ${previewImageName}`}
                  />
                </button>
                <div class="media-bar">
                  <span class="media-filename selectable" title={previewImageAbsPath}>
                    {previewImageName}
                  </span>
                  <div class="media-bar-btns">
                    {#if imageVersions.length >= 2}
                      <button
                        type="button"
                        class="btn-ghost tiny-btn"
                        on:click={() => openCompareModal("image")}
                      >
                        <Icons name="columns" size={13} />
                        So sánh
                      </button>
                    {/if}
                    <button
                      type="button"
                      class="btn-ghost tiny-btn"
                      on:click={() =>
                        openImageLightbox(previewImageAbsPath, `Ảnh sản phẩm: ${previewImageName}`)}
                    >
                      <Icons name="zoom-in" size={13} />
                      Phóng lớn
                    </button>
                    <button
                      type="button"
                      class="btn-ghost tiny-btn"
                      on:click={() => revealInExplorer(previewImageAbsPath)}
                    >
                      <Icons name="folder-open" size={13} />
                      Hiện file
                    </button>
                  </div>
                </div>
              {:else}
                <div class="stage-empty">
                  <Icons name="image" size={26} />
                  <span>Chưa có ảnh sản phẩm cho dòng này</span>
                </div>
              {/if}
            {:else}
              {#if previewVideoAbsPath}
                {@const drawerVidRev = rowRev(detail.row.row_key, $rowAssetRevs, $assetCacheBust)}
                {#key `${previewVideoAbsPath}-${drawerVidRev}`}
                  <video
                    class="stage-video"
                    controls
                    preload="metadata"
                    src={toAssetUrl(previewVideoAbsPath, drawerVidRev)}
                    title="Nhấn đúp để phóng lớn video kèm prompt & metadata"
                    on:dblclick={() => openVideoModal(detail)}
                  >
                    <track kind="captions" />
                  </video>
                {/key}
                <div class="media-bar">
                  <span class="media-filename selectable" title={previewVideoAbsPath}>
                    {previewVideoName}
                  </span>
                  <div class="media-bar-btns">
                    {#if videoVersions.length >= 2}
                      <button
                        type="button"
                        class="btn-ghost tiny-btn"
                        on:click={() => openCompareModal("video")}
                      >
                        <Icons name="columns" size={13} />
                        So sánh
                      </button>
                    {/if}
                    <button
                      type="button"
                      class="btn-ghost tiny-btn"
                      title="Phóng lớn video kèm prompt & metadata (UI-3)"
                      on:click={() => openVideoModal(detail)}
                    >
                      <Icons name="zoom-in" size={13} />
                      Phóng lớn
                    </button>
                    <button
                      type="button"
                      class="btn-ghost tiny-btn"
                      on:click={() => revealInExplorer(previewVideoAbsPath)}
                    >
                      <Icons name="folder-open" size={13} />
                      Hiện file
                    </button>
                  </div>
                </div>
              {:else}
                <div class="stage-empty">
                  <Icons name="video" size={26} />
                  <span>Chưa có video sản phẩm cho dòng này</span>
                </div>
              {/if}
            {/if}
          </div>
        </div>

        <!-- Chỉnh sửa & lưu Prompt trực tiếp -->
        <div class="prompt-group">
          <div class="label-row">
            <label class="label" for="drawer-prompt-image">
              Prompt tạo ảnh (cột B)
            </label>
            <div class="label-meta">
              {#if imgPromptEdited}
                <span class="badge badge-sua-prompt mini-badge">Đã sửa</span>
              {/if}
              <span class="char-count">{draftPromptImage.length} ký tự</span>
            </div>
          </div>
          <textarea
            id="drawer-prompt-image"
            class="input prompt"
            rows="3"
            disabled={$running}
            placeholder="Nhập prompt tạo ảnh..."
            bind:value={draftPromptImage}
          ></textarea>

          <div class="label-row">
            <label class="label" for="drawer-prompt-video">
              Prompt tạo video (cột C)
            </label>
            <div class="label-meta">
              {#if vidPromptEdited}
                <span class="badge badge-sua-prompt mini-badge">Đã sửa</span>
              {/if}
              <span class="char-count">{draftPromptVideo.length} ký tự</span>
            </div>
          </div>
          <textarea
            id="drawer-prompt-video"
            class="input prompt"
            rows="3"
            disabled={$running}
            placeholder="Nhập prompt chuyển động video..."
            bind:value={draftPromptVideo}
          ></textarea>

          <div class="prompt-save-row">
            <span class="prompt-hint">
              {#if imgPromptEdited}
                Đã sửa prompt ảnh — cần chạy lại từ B2+B3 khi tạo phiên bản mới.
              {:else if promptsDirty}
                Prompt video có thay đổi chưa lưu — bấm Lưu prompt hoặc chạy lại ngay.
              {:else}
                Mọi chỉnh sửa sẽ cập nhật trực tiếp vào prompts.csv.
              {/if}
            </span>
            <button
              type="button"
              class="btn-secondary small-btn"
              disabled={$running || !promptsDirty || savingPrompts}
              on:click={savePrompts}
            >
              <Icons name="check" size={14} />
              {savingPrompts ? "Đang lưu..." : "Lưu prompt"}
            </button>
          </div>
        </div>

        <!-- Mục 6.10: Chưa hài lòng / Làm lại phiên bản mới (_vN) cho dòng Đã tạo video -->
        {#if detail.row.status === "Đã tạo video"}
          <div class="redo-panel">
            <div class="redo-panel-title">
              <Icons name="redo" size={15} />
              <span>Chưa hài lòng? Sửa prompt rồi tạo phiên bản mới (<code>_vN</code>)</span>
            </div>
            <p class="redo-panel-desc">
              {#if imgPromptEdited}
                Bạn đã sửa <strong>Prompt tạo ảnh</strong> — nút <em>Tạo lại riêng Video</em> được khóa để tránh lệch ảnh gốc; hãy chạy lại từ <strong>Ảnh + Video</strong>.
              {:else if vidPromptEdited}
                Bạn chỉ sửa prompt video — có thể <strong>Tạo lại riêng Video</strong> trên ảnh hiện có mà không tốn lượt tạo ảnh.
              {:else}
                Phiên bản cũ vẫn được giữ nguyên trên đĩa và so sánh được ngay trong danh sách phiên bản.
              {/if}
            </p>
            <div class="redo-btns">
              <button
                type="button"
                class="btn-primary small-btn"
                disabled={$running || actionBusy}
                on:click={() => rerunStep("anh")}
              >
                <Icons name="sparkles" size={14} />
                Tạo lại từ Ảnh + Video
              </button>
              <button
                type="button"
                class="btn-secondary small-btn"
                disabled={$running || actionBusy || !detail.row.image_file || imgPromptEdited}
                title={imgPromptEdited
                  ? "Prompt tạo ảnh đã sửa — bắt buộc chạy lại từ bước Ảnh + Video (B2+B3)"
                  : "Tạo video phiên bản mới (_vN) từ ảnh sản phẩm hiện có"}
                on:click={() => rerunStep("video")}
              >
                <Icons name="video" size={14} />
                Tạo lại riêng Video
              </button>
            </div>
          </div>
        {/if}

        <!-- (C) Lịch sử các phiên bản sản phẩm + Đặt bản chính + So sánh v1 vs v2 -->
        <details class="drawer-section" open={detail.files.length > 0}>
          <summary class="section-summary">
            <span>Phiên bản sản phẩm ({detail.files.length})</span>
          </summary>
          {#if canCompareVersions}
            <div class="section-actions-header">
              <span class="section-actions-note">Đối chiếu trực quan các phiên bản</span>
              <button
                type="button"
                class="btn-secondary compare-trigger-btn"
                on:click={() => openCompareModal()}
              >
                <Icons name="columns" size={13} />
                So sánh phiên bản
              </button>
            </div>
          {/if}
          {#if detail.files.length > 0}
            <div class="version-list">
              {#each detail.files as f (`${f.kind}-${f.version}-${f.file_name}`)}
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
                        disabled={$running || switchingVersion}
                        title="Đặt phiên bản này làm bản chính trong prompts.csv"
                        on:click={() => setAsCurrentVersion(f.kind, f.version)}
                      >
                        Đặt chính
                      </button>
                    {/if}
                    <button
                      type="button"
                      class="btn-ghost icon-mini"
                      aria-label={`Xem phiên bản v${f.version} (${f.file_name}) trong khung xem trước`}
                      title="Xem phiên bản này trong khung xem trước"
                      on:click={() => previewVersionFile(f)}
                    >
                      <Icons name="eye" size={13} />
                    </button>
                    <button
                      type="button"
                      class="btn-ghost icon-mini"
                      aria-label={`Hiện ${f.file_name} trong thư mục Explorer`}
                      title="Hiện file trong thư mục Explorer"
                      on:click={() => revealInExplorer(f.abs_path)}
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

        <!-- Nhật ký các lần thử của dòng (detail.timeline) -->
        <details class="drawer-section" open={detail.timeline.length > 0}>
          <summary class="section-summary">
            <span>Lịch sử chạy dòng ({detail.timeline.length})</span>
          </summary>
          {#if detail.timeline.length > 0}
            <div class="timeline-list">
              {#each detail.timeline as t, idx (idx)}
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

        {#if detail.ref_exists}
          <div class="extra-row-tools">
            <button
              type="button"
              class="btn-ghost tiny-btn"
              disabled={$running || actionBusy}
              on:click={() => detail && remapRowByKey(detail.row.row_key)}
            >
              <Icons name="link" size={13} />
              Chọn lại ảnh tham chiếu...
            </button>
            <button
              type="button"
              class="btn-ghost tiny-btn danger-text"
              disabled={$running || actionBusy}
              on:click={() => detail && openDeleteModalForKey(detail.row.row_key)}
            >
              <Icons name="trash" size={13} />
              Xóa dòng khỏi CSV
            </button>
          </div>
        {/if}
      </div>

      <!-- Chân ngăn cố định (mục 6.3c & 6.10) -->
      <div class="drawer-foot">
        {#if detail.row.status !== "Đã tạo video"}
          <button
            type="button"
            class="btn-primary full"
            disabled={$running || actionBusy || detail.row.status === "Bỏ qua"}
            title={detail.row.status === "Bỏ qua"
              ? "Đặt lại trạng thái về Đang chờ trước khi chạy"
              : "Chạy riêng dòng này từ bước tiếp theo"}
            on:click={runOneFromDrawer}
          >
            <Icons name="play" size={15} />
            Start dòng này
          </button>

          <div class="two">
            <button
              type="button"
              class="btn-secondary"
              disabled={$running || actionBusy}
              title="Chạy lại từ bước tạo ảnh (B2) rồi tiếp tục tạo video (B3)"
              on:click={() => rerunStep("anh")}
            >
              <Icons name="redo" size={14} />
              Chạy lại bước ảnh (B2+B3)
            </button>
            <button
              type="button"
              class="btn-secondary"
              disabled={$running || actionBusy || !detail.row.image_file || imgPromptEdited}
              title={imgPromptEdited
                ? "Bạn đã sửa Prompt tạo ảnh — cần chạy lại bước ảnh (B2+B3)"
                : !detail.row.image_file
                  ? "Cần có ảnh sản phẩm trước khi chạy riêng bước video"
                  : "Chạy lại riêng bước tạo video (B3) từ ảnh hiện có"}
              on:click={() => rerunStep("video")}
            >
              <Icons name="video" size={14} />
              Chạy lại bước video (B3)
            </button>
          </div>
        {/if}

        <div class="two">
          <button
            type="button"
            class="btn-secondary"
            disabled={$running || actionBusy}
            on:click={() => (resetConfirmOpen = true)}
          >
            <Icons name="refresh" size={14} />
            Đặt lại về Đang chờ
          </button>
          <button
            type="button"
            class="btn-ghost danger-text"
            disabled={$running || actionBusy || detail.row.status === "Bỏ qua"}
            on:click={skipRow}
          >
            Bỏ qua dòng này
          </button>
        </div>
      </div>
    {/if}
  </aside>
{/if}

<!-- Modal Cảnh báo Prompt chưa lưu khi đóng hoặc đổi dòng trong Drawer -->
<Modal
  open={unsavedPromptModalOpen}
  title="Prompt đang sửa chưa lưu"
  width="440px"
  disableClose={savingPrompts}
  on:close={() => {
    unsavedPromptModalOpen = false;
    pendingDrawerAction = null;
  }}
>
  <p class="confirm-text">
    Bạn đang sửa nháp prompt của dòng <strong class="selectable">{detail?.row.row_key ?? ""}</strong> nhưng chưa bấm <strong>Lưu prompt</strong>. Bạn muốn lưu lại trước khi rời ngăn chi tiết?
  </p>
  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-ghost"
      disabled={savingPrompts}
      on:click={() => {
        unsavedPromptModalOpen = false;
        pendingDrawerAction = null;
      }}
    >
      Tiếp tục sửa
    </button>
    <button
      type="button"
      class="btn-secondary"
      disabled={savingPrompts}
      on:click={discardPromptAndProceed}
    >
      Bỏ thay đổi
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={savingPrompts || $running}
      on:click={savePromptAndProceed}
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
  on:close={() => (resetConfirmOpen = false)}
>
  <p class="confirm-text">
    Đặt lại dòng <strong class="selectable">{detail?.row.row_key ?? ""}</strong> về trạng thái <strong>Đang chờ</strong> và xóa bộ đếm lỗi về <code>0 / {$maxAttemptsStore}</code>? Các file ảnh/video đã tạo trước đó vẫn được giữ nguyên.
  </p>
  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={actionBusy}
      on:click={() => (resetConfirmOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={$running || actionBusy}
      on:click={confirmResetStatus}
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
  on:close={() => (batchConfirmOpen = false)}
>
  <div class="modal-stack">
    <p class="confirm-text">
      Chuẩn bị chạy <strong>{selected.size} dòng đã chọn</strong> trong dự án <strong>{$project?.name ?? ""}</strong>.
    </p>
    {#if !$endpointStatus.imageConfigured || !$endpointStatus.videoConfigured}
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
  </div>
  <div slot="footer" class="modal-footer-bar">
    <button
      type="button"
      class="btn-secondary"
      disabled={batchStarting}
      on:click={() => (batchConfirmOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={$running || batchStarting}
      on:click={executeRunSelected}
    >
      <Icons name="play" size={15} />
      Xác nhận chạy ({selected.size})
    </button>
  </div>
</Modal>

<!-- Modal Xác nhận xóa dòng khỏi CSV (TC-49) -->
<Modal
  open={deleteConfirmOpen}
  title="Xác nhận xóa dòng khỏi CSV"
  width="440px"
  disableClose={actionBusy}
  on:close={() => (deleteConfirmOpen = false)}
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
      on:click={() => (deleteConfirmOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-danger"
      disabled={$running || actionBusy}
      on:click={confirmDeleteRow}
    >
      <Icons name="trash" size={15} />
      Xác nhận xóa
    </button>
  </div>
</Modal>

<!-- (A) Modal Quét & Nhập hàng loạt ảnh từ input/ -->
<Modal
  open={scanModalOpen}
  title="Quét & Nhập ảnh mới từ thư mục input/"
  width="680px"
  disableClose={importingInput}
  on:close={() => (scanModalOpen = false)}
>
  {#if untrackedItems.length === 0}
    <div class="scan-empty">
      <Icons name="check" size={28} />
      <div class="scan-empty-title">Tất cả ảnh trong <code>input/</code> đã có trong CSV</div>
      <p>Không phát hiện file ảnh (<code>.png</code>, <code>.jpg</code>, <code>.jpeg</code>, <code>.webp</code>) nào chưa được theo dõi.</p>
    </div>
  {:else}
    <div class="modal-stack">
      <div class="scan-head-row">
        <span>
          Phát hiện <strong>{untrackedItems.length} ảnh mới</strong> trong <code>input/</code> chưa có trong <code>prompts.csv</code>:
        </span>
        <button type="button" class="btn-ghost tiny-btn" on:click={toggleAllImportItems}>
          {selectedImportNames.size === untrackedItems.length ? "Bỏ chọn tất cả" : "Chọn tất cả"}
        </button>
      </div>

      <div class="scan-grid">
        {#each untrackedItems as item (item.file_name)}
          {@const isPicked = selectedImportNames.has(item.file_name)}
          <label class="scan-item" class:picked={isPicked}>
            <input
              type="checkbox"
              checked={isPicked}
              on:change={() => toggleImportItem(item.file_name)}
            />
            <span class="scan-thumb">
              <img
                src={toAssetUrl(item.abs_path, $assetCacheBust)}
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
      on:click={() => (scanModalOpen = false)}
    >
      Đóng
    </button>
    {#if untrackedItems.length > 0}
      <button
        type="button"
        class="btn-primary"
        disabled={selectedImportNames.size === 0 || importingInput || $running}
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

<!-- (C) Modal So sánh phiên bản v1 vs v2 cạnh nhau (mục 6.11, TC-08 - 860px) -->
<Modal
  open={compareModalOpen}
  title={`So sánh phiên bản — ${detail?.row.row_key ?? ""}`}
  width="860px"
  disableClose={switchingVersion}
  on:close={() => (compareModalOpen = false)}
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
          {@const compareRev = rowRev(detail?.row.row_key, $rowAssetRevs, $assetCacheBust)}
          <div class="compare-stage">
            {#if compareKind === "video"}
              {#key `left-${compareLeftItem.abs_path}-${compareRev}`}
                <video
                  controls
                  preload="metadata"
                  src={toAssetUrl(compareLeftItem.abs_path, compareRev)}
                >
                  <track kind="captions" />
                </video>
              {/key}
            {:else}
              <img
                src={toAssetUrl(compareLeftItem.abs_path, compareRev)}
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
              disabled={compareLeftItem.is_current || $running || switchingVersion}
              on:click={() =>
                compareLeftItem &&
                setAsCurrentVersion(compareLeftItem.kind, compareLeftItem.version)}
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
          {@const compareRev = rowRev(detail?.row.row_key, $rowAssetRevs, $assetCacheBust)}
          <div class="compare-stage">
            {#if compareKind === "video"}
              {#key `right-${compareRightItem.abs_path}-${compareRev}`}
                <video
                  controls
                  preload="metadata"
                  src={toAssetUrl(compareRightItem.abs_path, compareRev)}
                >
                  <track kind="captions" />
                </video>
              {/key}
            {:else}
              <img
                src={toAssetUrl(compareRightItem.abs_path, compareRev)}
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
              disabled={compareRightItem.is_current || $running || switchingVersion}
              on:click={() =>
                compareRightItem &&
                setAsCurrentVersion(compareRightItem.kind, compareRightItem.version)}
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
      on:click={() => (compareModalOpen = false)}
    >
      Đóng
    </button>
  </div>
</Modal>

<!-- (D) Cửa sổ xem ảnh phóng lớn Zoom 25%–400% & Drag-to-Pan (mục 6.11, TC-17) -->
<Modal
  open={lightboxOpen}
  title={lightboxTitle}
  width="880px"
  on:close={() => (lightboxOpen = false)}
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
    {#if lightboxUrl}
      <img
        class="lightbox-img"
        src={lightboxUrl}
        alt={lightboxTitle}
        draggable="false"
        style="transform: translate({panX}px, {panY}px) scale({zoomScale});"
      />
    {/if}
  </div>

  <div slot="footer" class="modal-footer-bar">
    {#if lightboxAbsPath}
      <button
        type="button"
        class="btn-secondary"
        on:click={() => revealInExplorer(lightboxAbsPath)}
      >
        <Icons name="folder-open" size={15} />
        Hiện trong thư mục
      </button>
    {/if}
    <button
      type="button"
      class="btn-primary"
      on:click={() => (lightboxOpen = false)}
    >
      Đóng
    </button>
  </div>
</Modal>

<!-- (E) Modal xem video lớn trong màn Dòng việc kèm prompt và metadata (UI-3) -->
<Modal
  open={videoModalOpen}
  title={`Chi tiết video — ${videoModalTitle}`}
  width="880px"
  on:close={closeVideoModal}
>
  <div class="video-modal-layout">
    <div class="video-player-pane">
      {#if videoModalUrl}
        <div class="video-frame-9-16">
          <video
            bind:this={videoPlayerEl}
            class="large-video-player"
            controls
            autoplay
            playsinline
            src={videoModalUrl}
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
      <!-- Thẻ thông số video (Metadata) -->
      <div class="video-meta-card">
        <h3 class="meta-card-title">Thông số video</h3>
        <dl class="meta-grid">
          <div class="meta-grid-row">
            <dt class="meta-key">Tên file</dt>
            <dd class="meta-val file-val selectable" title={videoModalAbsPath ?? videoModalFileName}>
              {videoModalFileName || "—"}
            </dd>
          </div>
          <div class="meta-grid-row">
            <dt class="meta-key">Thời lượng</dt>
            <dd class="meta-val">
              {videoModalDurationSec ? formatSecs(videoModalDurationSec) : "—"}
            </dd>
          </div>
          <div class="meta-grid-row">
            <dt class="meta-key">Độ phân giải</dt>
            <dd class="meta-val">
              {#if videoModalWidth && videoModalHeight}
                {videoModalWidth}×{videoModalHeight} ({formatAspectRatio(videoModalWidth, videoModalHeight)})
              {:else}
                —
              {/if}
            </dd>
          </div>
          <div class="meta-grid-row">
            <dt class="meta-key">Tài khoản tạo</dt>
            <dd class="meta-val">
              {videoModalAccount || "Tài khoản Muse mặc định"}
            </dd>
          </div>
        </dl>
      </div>

      <!-- Ô hiển thị nguyên văn "Prompt tạo ảnh" kèm nút copy -->
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
          {videoModalPromptImage || "(trống)"}
        </div>
      </div>

      <!-- Ô hiển thị nguyên văn "Prompt tạo video" kèm nút copy -->
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
          {videoModalPromptVideo || "(trống)"}
        </div>
      </div>
    </div>
  </div>

  <div slot="footer" class="modal-footer-bar">
    {#if videoModalAbsPath}
      <button
        type="button"
        class="btn-secondary"
        on:click={() => revealInExplorer(videoModalAbsPath)}
      >
        <Icons name="folder-open" size={15} />
        Hiện trong thư mục
      </button>
    {/if}
    <button
      type="button"
      class="btn-primary"
      on:click={closeVideoModal}
    >
      Đóng
    </button>
  </div>
</Modal>

<style>
  .rows-screen {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    gap: 10px;
  }

  .top-fixed {
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .live-strip {
    background: var(--c-teal-subtle);
    border: 1px solid #b7dde1;
    border-radius: 10px;
    padding: 7px 12px;
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  .live-strip-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    font-size: 12px;
    flex-wrap: wrap;
  }
  .live-label {
    font-weight: 600;
    color: var(--c-teal-dark);
    display: inline-flex;
    align-items: center;
    gap: 7px;
    min-width: 0;
  }
  .live-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--c-teal);
    flex-shrink: 0;
  }
  .live-strip-right {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .live-note {
    color: var(--c-muted);
  }
  .live-stop-btn {
    height: 28px;
    padding: 0 10px;
    font-size: 12px;
    gap: 5px;
  }
  .compact-bar {
    height: 6px;
  }

  .toolbar {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .search-wrap {
    position: relative;
    width: 248px;
    display: flex;
    align-items: center;
  }
  .search-icon {
    position: absolute;
    left: 10px;
    color: var(--c-muted);
    pointer-events: none;
    display: inline-flex;
  }
  .search {
    width: 100%;
    padding-left: 32px;
    padding-right: 30px;
  }
  .clear-search {
    position: absolute;
    right: 6px;
    width: 22px;
    height: 22px;
    padding: 0;
    border-radius: 50%;
    background: transparent;
    color: var(--c-muted);
  }
  .clear-search:hover {
    background: var(--c-surface-subtle);
    color: var(--c-text);
  }

  .view-toggle {
    display: inline-flex;
    align-items: center;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    border-radius: 8px;
    padding: 2px;
    height: 36px;
  }
  .view-btn {
    height: 30px;
    padding: 0 10px;
    border-radius: 6px;
    font-size: 12.5px;
    font-weight: 600;
    background: transparent;
    color: var(--c-muted);
    gap: 5px;
  }
  .view-btn.active {
    background: var(--c-card);
    color: var(--c-teal);
    box-shadow: 0 1px 2px rgba(15, 23, 42, 0.08);
  }

  .spacer {
    flex: 1;
  }

  .chips {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
  }
  .reset-filter {
    height: 30px;
    padding: 0 10px;
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

  .load-err-card {
    background: var(--c-danger-bg);
    border: 1px solid #efb9b4;
    color: var(--c-danger);
    border-radius: 10px;
    padding: 9px 14px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    font-size: var(--fs-label);
    flex-shrink: 0;
  }
  .load-err-text {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  /* Vùng dữ liệu chính co giãn linh hoạt ở cả 1024x640 (min-height: 0) */
  .table-wrap {
    flex: 1;
    min-height: 0;
    padding: 0;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  .table-scroll,
  .grid-scroll {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
  table {
    width: 100%;
    border-collapse: separate;
    border-spacing: 0;
  }
  thead th {
    position: sticky;
    top: 0;
    z-index: 2;
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
    height: 48px;
    border-bottom: 1px solid #edf2f5;
    padding: 4px 12px;
    font-size: var(--fs-body);
    vertical-align: middle;
  }
  tbody tr {
    transition: background 0.1s ease;
  }
  tbody tr:hover {
    background: #f5f9fb;
  }
  tbody tr.running-row {
    background: var(--c-teal-light);
  }
  tbody tr.selected-row {
    background: var(--c-teal-subtle);
  }

  .w-check {
    width: 40px;
  }
  .col-ref {
    min-width: 210px;
  }
  .w-76 {
    width: 76px;
  }
  .w-84 {
    width: 84px;
  }
  .w-180 {
    width: 180px;
  }
  .w-68 {
    width: 68px;
  }
  .w-96 {
    width: 96px;
  }

  .ref-cell-wrap {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .key-btn {
    flex: 1;
    min-width: 0;
    padding: 3px 0;
    background: transparent;
    color: var(--c-text);
    justify-content: flex-start;
    text-align: left;
    gap: 10px;
    font-weight: 600;
  }
  .key-btn:hover .key-name {
    color: var(--c-teal);
  }
  .ref-mini {
    width: 36px;
    height: 36px;
    border-radius: 7px;
    border: 1px solid var(--c-border);
    background: var(--c-surface-subtle);
    overflow: hidden;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
  }
  .ref-mini img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .mini-missing {
    color: var(--c-warn-dark);
    display: inline-flex;
  }
  .key-meta {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .key-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .key-sub-badges {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .mini-badge {
    height: 18px;
    padding: 0 6px;
    font-size: 11px;
  }
  .orphan-tag {
    font-size: 11px;
    font-weight: 700;
    color: var(--c-warn-dark);
  }
  .err-inline {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    font-size: 11px;
    font-weight: 600;
    color: var(--c-danger);
  }
  .inline-orphan-btns {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-shrink: 0;
  }
  .orphan-inline-btn {
    height: 26px;
    padding: 0 8px;
    font-size: 11.5px;
  }

  /* Thumbnail 40x40px trong cột Ảnh */
  .thumb-btn {
    width: 40px;
    height: 40px;
    padding: 0;
    border-radius: 7px;
    border: 1px solid var(--c-border);
    background: var(--c-surface-subtle);
    overflow: hidden;
  }
  .thumb-btn:hover {
    border-color: var(--c-teal);
    box-shadow: 0 2px 6px rgba(14, 124, 134, 0.2);
  }
  .thumb-img {
    width: 40px;
    height: 40px;
    object-fit: cover;
    display: block;
  }
  .media-badge {
    color: var(--c-teal);
    display: inline-flex;
  }

  .video-pill {
    height: 28px;
    padding: 0 9px;
    border-radius: 6px;
    font-size: 12px;
    font-weight: 600;
    background: var(--c-green-bg);
    color: var(--c-green);
    border: 1px solid #bfe0cb;
    gap: 4px;
  }
  .video-pill:hover {
    filter: brightness(0.95);
  }

  .live-row-badge {
    max-width: 210px;
    gap: 6px;
  }
  .live-badge-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .spin-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--c-teal);
    flex-shrink: 0;
    animation: row-pulse 1s infinite ease-in-out;
  }
  @keyframes row-pulse {
    0%,
    100% {
      opacity: 0.45;
      transform: scale(0.85);
    }
    50% {
      opacity: 1;
      transform: scale(1.2);
    }
  }

  .dash-muted {
    color: var(--c-muted);
  }
  .attempts-cell {
    font-size: var(--fs-label);
    color: var(--c-muted);
    font-variant-numeric: tabular-nums;
  }
  .attempts-cell.exhausted {
    color: var(--c-danger);
    font-weight: 700;
  }

  .row-detail-btn {
    padding: 0 10px;
    gap: 5px;
  }
  .run-one {
    height: 32px;
    padding: 0 10px;
    font-size: 13px;
    gap: 5px;
  }

  .empty {
    text-align: center;
    color: var(--c-muted);
    padding: 32px 16px;
  }

  /* (B) Chế độ Lưới thẻ Media (Gallery Grid) */
  .grid-scroll {
    padding: 14px;
  }
  .gallery-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(212px, 1fr));
    gap: 14px;
  }
  .media-grid-card {
    border: 1px solid var(--c-border);
    border-radius: 10px;
    background: var(--c-card);
    overflow: hidden;
    display: flex;
    flex-direction: column;
    transition: border-color 0.14s ease, box-shadow 0.14s ease;
  }
  .media-grid-card:hover {
    border-color: var(--c-teal);
    box-shadow: var(--shadow-hover);
  }
  .media-grid-card.running-card {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .media-grid-card.selected-card {
    border-color: var(--c-teal-dark);
  }
  .grid-media-box {
    position: relative;
    height: 150px;
    background: #101824;
    overflow: hidden;
  }
  .grid-check {
    position: absolute;
    top: 8px;
    left: 8px;
    z-index: 2;
    width: 16px;
    height: 16px;
    accent-color: var(--c-teal);
  }
  .grid-preview-trigger {
    width: 100%;
    height: 100%;
    padding: 0;
    border-radius: 0;
    background: transparent;
  }
  .grid-preview-trigger img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .grid-missing {
    width: 100%;
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
    color: #fbbf24;
    font-size: 12px;
  }
  .grid-vid-badge {
    position: absolute;
    right: 8px;
    bottom: 8px;
    height: 24px;
    padding: 0 8px;
    border-radius: 6px;
    font-size: 11px;
    font-weight: 700;
    background: rgba(16, 185, 129, 0.92);
    color: #fff;
    gap: 4px;
  }
  .grid-img-badge {
    position: absolute;
    right: 8px;
    bottom: 8px;
    padding: 2px 7px;
    border-radius: 6px;
    font-size: 11px;
    font-weight: 600;
    background: rgba(15, 23, 42, 0.72);
    color: #e2e8f0;
  }
  .grid-card-body {
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    flex: 1;
  }
  .grid-title-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
  }
  .grid-title-btn {
    padding: 0;
    height: auto;
    background: transparent;
    color: var(--c-text);
    font-size: 13px;
    font-weight: 700;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
    justify-content: flex-start;
  }
  .grid-title-btn:hover {
    color: var(--c-teal);
  }
  .grid-badges-row {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .grid-orphan-row {
    display: flex;
    gap: 6px;
  }
  .grid-card-actions {
    margin-top: auto;
    display: flex;
    gap: 6px;
    padding-top: 2px;
  }
  .grid-act-btn {
    flex: 1;
    height: 30px;
    padding: 0 8px;
    font-size: 12px;
    gap: 4px;
  }

  /* Thanh phân trang cố định đáy bảng */
  .pagination {
    height: 46px;
    flex-shrink: 0;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 14px;
    border-top: 1px solid var(--c-border);
    background: var(--c-card);
    font-size: var(--fs-label);
    color: var(--c-muted);
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
  .total-summary {
    font-weight: 500;
  }

  /* Ngăn trượt Chi tiết 440px: chỉ phủ vùng nội dung (dưới Titlebar / Banner, phải Sidebar) */
  .drawer-backdrop {
    position: fixed;
    top: var(--h-top-offset, var(--h-titlebar));
    left: var(--w-sidebar-current, var(--w-sidebar));
    right: 0;
    bottom: var(--h-statusbar);
    background: rgba(15, 23, 42, 0.34);
    backdrop-filter: blur(1px);
    z-index: 40;
  }
  .drawer {
    position: fixed;
    right: 0;
    top: var(--h-top-offset, var(--h-titlebar));
    bottom: var(--h-statusbar);
    width: min(var(--w-detail-drawer), calc(100vw - var(--w-sidebar-current, var(--w-sidebar))));
    background: var(--c-card);
    border-left: 1px solid var(--c-border);
    z-index: 41;
    box-shadow: -8px 0 28px rgba(15, 23, 42, 0.16);
    display: flex;
    flex-direction: column;
    animation: slide-in 0.18s ease-out;
  }
  @media (max-width: 1100px) {
    .drawer-backdrop {
      left: var(--w-sidebar-collapsed);
    }
    .drawer {
      width: min(var(--w-detail-drawer), calc(100vw - var(--w-sidebar-collapsed)));
    }
  }
  @keyframes slide-in {
    from {
      transform: translateX(32px);
      opacity: 0;
    }
    to {
      transform: none;
      opacity: 1;
    }
  }

  .drawer-head {
    padding: 12px 16px;
    border-bottom: 1px solid var(--c-border);
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 10px;
  }
  .drawer-head-info {
    min-width: 0;
    flex: 1;
  }
  .drawer-title {
    font-size: 15px;
    font-weight: 700;
    color: var(--c-text);
    word-break: break-all;
    margin-bottom: 5px;
  }
  .drawer-badges {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .meta-chip {
    font-size: 11px;
    color: var(--c-muted);
    background: var(--c-surface-subtle);
    padding: 2px 8px;
    border-radius: 999px;
  }
  .close {
    width: 32px;
    height: 32px;
    padding: 0;
    flex-shrink: 0;
  }
  .drawer-loading {
    padding: 32px 16px;
    text-align: center;
    color: var(--c-muted);
  }

  .drawer-body {
    padding: 14px 16px;
    overflow-y: auto;
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 13px;
  }

  /* Khôi phục dòng thiếu ảnh tham chiếu */
  .orphan-card {
    background: var(--c-warn-bg);
    border: 1px solid #f4d39b;
    border-radius: 10px;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .orphan-title {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    font-weight: 700;
    color: var(--c-warn-dark);
  }
  .orphan-desc {
    font-size: 12px;
    color: var(--c-text);
    line-height: 1.45;
  }
  .orphan-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .small-btn {
    height: 32px;
    padding: 0 12px;
    font-size: 12px;
    gap: 5px;
  }
  .danger-outline {
    color: var(--c-danger);
    border-color: #efb9b4;
  }
  .danger-outline:hover:not(:disabled) {
    border-color: var(--c-danger);
    color: var(--c-danger);
    background: var(--c-danger-bg);
  }

  .error-box {
    background: var(--c-danger-bg);
    border: 1px solid #efb9b4;
    color: var(--c-danger);
    border-radius: 8px;
    padding: 10px 12px;
    font-size: 12px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .error-box-head {
    display: flex;
    align-items: center;
    gap: 6px;
    font-weight: 700;
  }

  /* Khối xem trước Media tích hợp */
  .media-card {
    border: 1px solid var(--c-border);
    border-radius: 10px;
    overflow: hidden;
    background: var(--c-surface-subtle);
  }
  .media-tabs {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    background: #e9eff3;
    padding: 3px;
    gap: 3px;
  }
  .media-tab {
    height: 30px;
    padding: 0 8px;
    border-radius: 7px;
    font-size: 12px;
    font-weight: 600;
    background: transparent;
    color: var(--c-muted);
    gap: 5px;
  }
  .media-tab.active {
    background: var(--c-card);
    color: var(--c-teal);
    box-shadow: 0 1px 2px rgba(15, 23, 42, 0.08);
  }
  .tab-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
  }
  .tab-dot.ok {
    background: var(--c-green);
  }
  .tab-dot.warn {
    background: var(--c-warn);
  }

  .version-preview-banner {
    background: var(--c-warn-bg);
    color: var(--c-warn-dark);
    font-size: 11px;
    padding: 5px 10px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    border-bottom: 1px solid #f4d39b;
  }
  .banner-inline-btns {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .inline-reset {
    height: auto;
    padding: 0;
    font-size: 11px;
    font-weight: 700;
    background: transparent;
    color: var(--c-teal);
    text-decoration: underline;
  }

  .media-stage {
    display: flex;
    flex-direction: column;
  }
  .stage-img-btn {
    width: 100%;
    padding: 8px;
    background: #101824;
    border-radius: 0;
    max-height: 248px;
    overflow: hidden;
  }
  .stage-img {
    max-width: 100%;
    max-height: 232px;
    object-fit: contain;
    border-radius: 4px;
    display: block;
  }
  .stage-video {
    width: 100%;
    max-height: 268px;
    background: #0d1520;
    object-fit: contain;
    display: block;
  }
  .media-bar {
    padding: 6px 10px;
    background: var(--c-card);
    border-top: 1px solid var(--c-border);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .media-filename {
    font-size: 11px;
    color: var(--c-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .media-bar-btns {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-shrink: 0;
  }
  .tiny-btn {
    height: 26px;
    padding: 0 8px;
    font-size: 11px;
    gap: 4px;
  }
  .stage-empty {
    min-height: 120px;
    padding: 18px;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: var(--c-muted);
    font-size: 12px;
    text-align: center;
  }

  /* Chỉnh sửa Prompt */
  .prompt-group {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .label-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-top: 2px;
  }
  .label-row .label {
    margin-bottom: 0;
  }
  .label-meta {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .char-count {
    font-size: 11px;
    color: var(--c-muted);
  }
  .prompt {
    width: 100%;
    min-height: 70px;
    max-height: 180px;
    resize: vertical;
    line-height: 1.45;
  }
  .prompt-save-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-top: 2px;
  }
  .prompt-hint {
    font-size: 11px;
    color: var(--c-muted);
    line-height: 1.35;
  }

  /* Khối Làm lại phiên bản mới (_vN) */
  .redo-panel {
    background: linear-gradient(180deg, #f4fbfb 0%, #ebf6f7 100%);
    border: 1px solid #b7dde1;
    border-radius: 10px;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .redo-panel-title {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    font-weight: 700;
    color: var(--c-teal-dark);
  }
  .redo-panel-desc {
    font-size: 12px;
    color: var(--c-muted);
    line-height: 1.45;
  }
  .redo-btns {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .redo-btns > button {
    flex: 1;
  }

  /* Danh sách phiên bản & Timeline */
  .drawer-section {
    border: 1px solid var(--c-border);
    border-radius: 8px;
    background: var(--c-card);
    overflow: hidden;
  }
  .section-summary {
    padding: 8px 12px;
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
    border-top: 1px solid #eef2f5;
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
    border-top: 1px solid #eef2f5;
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
    border-top: 1px solid #eef2f5;
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
    background: #fdf3f2;
    padding: 4px 8px;
    border-radius: 4px;
    word-break: break-word;
  }

  .extra-row-tools {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding-top: 2px;
  }

  /* Chân ngăn cố định */
  .drawer-foot {
    padding: 11px 16px;
    border-top: 1px solid var(--c-border);
    background: var(--c-surface-subtle);
    display: flex;
    flex-direction: column;
    gap: 8px;
    flex-shrink: 0;
  }
  .full {
    width: 100%;
  }
  .two {
    display: flex;
    gap: 8px;
  }
  .two > button {
    flex: 1;
    padding: 0 10px;
    font-size: 12px;
  }
  .danger-text {
    color: var(--c-danger);
  }
  .danger-text:hover:not(:disabled) {
    background: var(--c-danger-bg);
    color: var(--c-danger);
  }

  /* Modal chung, Quét input/, So sánh v1/v2 & Lightbox Zoom/Pan */
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
    border: 1px solid #f4d39b;
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
    background: #fff;
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

  /* (E) Modal xem video lớn (UI-3) */
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
  .grid-err-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
    .scan-prompts,
    .compare-grid {
      grid-template-columns: 1fr;
    }
    .video-modal-layout {
      grid-template-columns: 1fr;
    }
    .video-frame-9-16 {
      max-width: 240px;
    }
  }
</style>
