<script lang="ts">
  // Màn hình Cài đặt (6.6, 6.6b): 5 tab, 100% tùy chọn hoạt động thật, kiểm tra hợp lệ nghiêm ngặt, 0 A11y warnings
  import { createEventDispatcher, onDestroy, onMount } from "svelte";
  const dispatch = createEventDispatcher<{ navigate: string }>();
  import {
    confirmStartEnabled,
    endpointStatus,
    formatApiError,
    maxAttemptsStore,
    museAccounts,
    museSidecarStatus,
    project,
    refreshMuseState,
    resetProjectRunState,
    rowChangedTick,
    running,
    settingsDirty,
    showToast,
  } from "../lib/store";
  import {
    api,
    type MuseStatusView,
    type ProbeResult,
    type Settings,
    type SettingsView,
  } from "../lib/api";
  import { executeSettingsSave } from "../lib/settingsSave";
  import Icons from "../lib/Icons.svelte";
  import Modal from "../lib/Modal.svelte";

  type Tab = "project" | "image" | "video" | "run" | "general";
  let tab: Tab = "project";
  let loading = true;
  let saving = false;
  let recheckingProject = false;

  let s: Settings | null = null;
  let imageKeyPreview = "";
  let videoKeyPreview = "";
  let hasImageKey = false;
  let hasVideoKey = false;

  let imageKeyInput = "";
  let videoKeyInput = "";
  let pendingClearImageKey = false;
  let pendingClearVideoKey = false;

  let showImageKey = false;
  let showVideoKey = false;
  let imageKeyHideTimer: ReturnType<typeof setTimeout> | null = null;
  let videoKeyHideTimer: ReturnType<typeof setTimeout> | null = null;

  let dirty = false;
  let saveMsg = "";
  let saveMsgTimer: ReturnType<typeof setTimeout> | null = null;

  function clearSaveMsgTimer() {
    if (saveMsgTimer) {
      clearTimeout(saveMsgTimer);
      saveMsgTimer = null;
    }
  }

  // Trạng thái kiểm tra kết nối & danh sách model riêng biệt cho từng tab
  let testingImage = false;
  let testImage = "";
  let testImageOk: boolean | null = null;

  let testingVideo = false;
  let testVideo = "";
  let testVideoOk: boolean | null = null;

  let loadingImageModels = false;
  let imageModels: string[] = [];
  let imageModelMsg = "";

  let loadingVideoModels = false;
  let videoModels: string[] = [];
  let videoModelMsg = "";

  // ━━ VideoFlow Muse nội bộ: engine + pool cookie + hạn mức (tab Endpoint video) ━━
  let museLoading = false;
  let museErr = "";
  let museStarting = false;
  let museStopping = false;
  let museRestarting = false;
  $: museSidecarBusy = museStarting || museStopping || museRestarting;
  let destroyed = false;
  let museRefreshSeq = 0;

  async function museRefresh() {
    const seq = ++museRefreshSeq;
    museLoading = true;
    museErr = "";
    try {
      const res = await refreshMuseState({ force: true });
      if (destroyed || seq !== museRefreshSeq) return;
      if (res.error) {
        museErr = res.error;
      }
    } catch (e) {
      if (!destroyed && seq === museRefreshSeq) {
        museErr = formatApiError(e);
      }
    } finally {
      if (!destroyed && seq === museRefreshSeq) {
        museLoading = false;
      }
    }
  }

  async function museStart() {
    if (museSidecarBusy || $running || $museSidecarStatus?.sidecar.running) return;
    museStarting = true;
    museErr = "";
    try {
      await api.muse_start_sidecar();
      showToast("success", "Đã khởi động engine VideoFlow Muse.");
      await museRefresh();
    } catch (e) {
      museErr = formatApiError(e);
      showToast("error", museErr);
    } finally {
      museStarting = false;
    }
  }

  /** Dừng rồi khởi động lại engine để áp dụng Cài đặt mới (port/chromium/headless). */
  async function museRestart() {
    if (museSidecarBusy || $running || saving) return;
    if (dirty) {
      if (!formValid) {
        museErr = validationErrors[0] || "Cấu hình chưa hợp lệ.";
        showToast("error", museErr);
        return;
      }
      await save();
      if (dirty) return;
    }
    museRestarting = true;
    museErr = "";
    try {
      await api.muse_stop_sidecar();
      await api.muse_start_sidecar();
      showToast("success", "Đã áp dụng cài đặt mới cho engine VideoFlow Muse.");
    } catch (e) {
      museErr = formatApiError(e);
      showToast("error", `Áp dụng thất bại: ${museErr}`);
      // Engine có thể đang chết sau khi Dừng → thử khởi động lại để không để app ở
      // trạng thái "badge Đang chạy nhưng engine đã chết".
      try {
        await api.muse_start_sidecar();
        showToast("info", "Đã khởi động lại engine với cấu hình đang lưu.");
      } catch (retryErr) {
        museErr = `${museErr} — khởi động lại cũng lỗi: ${formatApiError(retryErr)}`;
      }
    } finally {
      // Luôn đồng bộ badge/nút theo trạng thái THẬT, kể cả khi lỗi.
      await museRefresh();
      museRestarting = false;
    }
  }

  async function museStop() {
    if (museSidecarBusy || $running) return;
    museStopping = true;
    try {
      await api.muse_stop_sidecar();
      showToast("info", "Đã dừng engine VideoFlow Muse.");
      await museRefresh();
    } catch (e) {
      museErr = formatApiError(e);
    } finally {
      museStopping = false;
    }
  }

  /** Các trường cấu hình engine đã đổi so với lúc spawn (dùng cho banner cảnh báo). */
  function museChangedFields(status: MuseStatusView | null, cfg: Settings | null): string[] {
    const st = status?.sidecar;
    if (!st?.running || !cfg) return [];
    const out: string[] = [];
    if (st.cfg_port != null && Number(cfg.muse_sidecar_port) !== st.cfg_port) {
      out.push(`Port ${st.cfg_port} → ${cfg.muse_sidecar_port}`);
    }
    const curChrome = (cfg.muse_chromium_path || "").trim();
    const spawnedChrome = (st.cfg_chromium || "").trim();
    // Chuẩn hoá để so sánh: bỏ tiền tố verbatim `\\?\`, quy `/` → `\`, bỏ hoa/thường.
    // Gõ đúng đường dẫn đang dùng (chỉ khác dấu) thì KHÔNG báo "đã đổi".
    const norm = (p: string) =>
      p
        .replace(/^\\\\\?\\/, "")
        .replace(/\//g, "\\")
        .replace(/\\+$/, "")
        .toLowerCase();
    const effective = norm(curChrome || status?.chromium_path || "");
    if (effective !== norm(spawnedChrome || status?.chromium_path || "")) {
      out.push(`Edge/Chrome → ${curChrome || "tự dò"}`);
    }
    if (st.cfg_headless != null && Boolean(cfg.muse_headless) !== st.cfg_headless) {
      out.push(cfg.muse_headless ? "bật headless" : "tắt headless");
    }
    return out;
  }

  $: museChanged = museChangedFields($museSidecarStatus, s);

  // Khi mở tab Video: luôn nạp status sidecar mới nhất từ store
  function onSelectTabVideo() {
    selectTab("video");
    void museRefresh();
  }

  // Modal xác nhận khi thư mục mới trống / thiếu thành phần / lỗi CSV
  let folderModalOpen = false;
  let probedDir = "";
  let probeInfo: ProbeResult | null = null;
  let folderMode: "giu_lai" | "ghi_de" = "giu_lai";
  let switchingFolder = false;

  const tabs: { key: Tab; label: string; icon: string }[] = [
    { key: "project", label: "Dự án Local", icon: "folder" },
    { key: "image", label: "Endpoint ảnh", icon: "image" },
    { key: "video", label: "Endpoint video", icon: "video" },
    { key: "run", label: "Chạy và thử lại", icon: "redo" },
    { key: "general", label: "Chung", icon: "settings" },
  ];

  $: settingsDirty.set(dirty);

  function hideAllKeyVisibility() {
    showImageKey = false;
    showVideoKey = false;
    if (imageKeyHideTimer) {
      clearTimeout(imageKeyHideTimer);
      imageKeyHideTimer = null;
    }
    if (videoKeyHideTimer) {
      clearTimeout(videoKeyHideTimer);
      videoKeyHideTimer = null;
    }
  }

  function toggleKeyVisibility(which: "anh" | "video") {
    if (which === "anh") {
      if (imageKeyHideTimer) {
        clearTimeout(imageKeyHideTimer);
        imageKeyHideTimer = null;
      }
      showImageKey = !showImageKey;
      if (showImageKey) {
        imageKeyHideTimer = setTimeout(() => {
          showImageKey = false;
          imageKeyHideTimer = null;
        }, 10000);
      }
    } else {
      if (videoKeyHideTimer) {
        clearTimeout(videoKeyHideTimer);
        videoKeyHideTimer = null;
      }
      showVideoKey = !showVideoKey;
      if (showVideoKey) {
        videoKeyHideTimer = setTimeout(() => {
          showVideoKey = false;
          videoKeyHideTimer = null;
        }, 10000);
      }
    }
  }

  function selectTab(nextTab: Tab) {
    if (nextTab !== tab) {
      hideAllKeyVisibility();
      tab = nextTab;
    }
  }

  onDestroy(() => {
    destroyed = true;
    ++museRefreshSeq;
    clearSaveMsgTimer();
    hideAllKeyVisibility();
    if (!dirty) settingsDirty.set(false);
  });

  function isValidIntInRange(val: unknown, min: number, max: number): boolean {
    if (val === null || val === undefined || (typeof val === "string" && val.trim() === "")) {
      return false;
    }
    const n = Number(val);
    return Number.isFinite(n) && Number.isInteger(n) && n >= min && n <= max;
  }

  function isValidHttpUrl(val: string | undefined | null): boolean {
    if (!val) return false;
    const t = val.trim();
    return /^https?:\/\/[^\s/$.?#].[^\s]*$/i.test(t);
  }

  function computeValidationErrors(cfg: Settings | null): string[] {
    if (!cfg) return [];
    const errs: string[] = [];
    if (cfg.image_source !== "internal") {
      if (!isValidHttpUrl(cfg.endpoint_image_base_url)) {
        errs.push("Base URL endpoint ảnh phải bắt đầu bằng http:// hoặc https://.");
      }
      if (!cfg.endpoint_image_model?.trim()) {
        errs.push("Model tạo ảnh không được để trống.");
      }
    }
    if (!isValidIntInRange(cfg.image_timeout_s, 5, 3600)) {
      errs.push("Timeout gọi ảnh phải là số nguyên từ 5 đến 3600 giây.");
    }
    if (cfg.video_source !== "internal") {
      if (!isValidHttpUrl(cfg.endpoint_video_base_url)) {
        errs.push("Base URL endpoint video phải bắt đầu bằng http:// hoặc https://.");
      }
      if (!cfg.endpoint_video_model?.trim()) {
        errs.push("Model tạo video không được để trống.");
      }
    }
    if (!isValidIntInRange(cfg.muse_sidecar_port, 1, 65535)) {
      errs.push("Port VideoFlow Muse phải là số nguyên từ 1 đến 65535.");
    }
    if (!cfg.video_size?.trim()) {
      errs.push("Kích thước khung hình video không được để trống.");
    }
    if (!isValidIntInRange(cfg.video_seconds, 1, 120)) {
      errs.push("Thời lượng video phải là số nguyên từ 1 đến 120 giây.");
    }
    if (!isValidIntInRange(cfg.poll_interval_s, 1, 600)) {
      errs.push("Chu kỳ poll trạng thái phải là số nguyên từ 1 đến 600 giây.");
    }
    if (!isValidIntInRange(cfg.poll_max_s, 10, 7200)) {
      errs.push("Thời gian poll tối đa phải là số nguyên từ 10 đến 7200 giây.");
    }
    if (!isValidIntInRange(cfg.video_create_timeout_s, 5, 3600)) {
      errs.push("Timeout tạo task video phải là số nguyên từ 5 đến 3600 giây.");
    }
    if (!isValidIntInRange(cfg.concurrency, 1, 3)) {
      errs.push("Số dòng chạy đồng thời phải từ 1 đến 3.");
    }
    if (!isValidIntInRange(cfg.max_attempts, 1, 10)) {
      errs.push("Số lần thử lại tối đa phải là số nguyên từ 1 đến 10.");
    }
    if (
      !Array.isArray(cfg.backoff_s) ||
      !isValidIntInRange(cfg.backoff_s[0], 1, 3600) ||
      !isValidIntInRange(cfg.backoff_s[1], 1, 3600) ||
      !isValidIntInRange(cfg.backoff_s[2], 1, 3600)
    ) {
      errs.push("Ba mốc giãn cách thử lại phải là số nguyên dương (1–3600 giây).");
    }
    if (!isValidIntInRange(cfg.schedule_hours, 1, 720)) {
      errs.push("Chu kỳ chạy theo lịch phải là số nguyên từ 1 đến 720 giờ.");
    }
    if (!isValidIntInRange(cfg.auto_cleanup_days, 0, 3650)) {
      errs.push("Số ngày tự dọn lịch sử phải là số nguyên từ 0 đến 3650 ngày.");
    }
    return errs;
  }

  $: validationErrors = computeValidationErrors(s);
  $: formValid = validationErrors.length === 0;

  function applySettingsView(view: SettingsView) {
    const cloned: Settings = JSON.parse(JSON.stringify(view.settings));
    if (!Array.isArray(cloned.backoff_s) || cloned.backoff_s.length < 3) {
      cloned.backoff_s = [30, 120, 300];
    }
    s = cloned;
    imageKeyPreview = view.image_key_preview ?? "";
    videoKeyPreview = view.video_key_preview ?? "";
    hasImageKey = Boolean(view.has_image_key);
    hasVideoKey = Boolean(view.has_video_key);
    pendingClearImageKey = false;
    pendingClearVideoKey = false;

    endpointStatus.set({
      imageConfigured: cloned.image_source === "internal" || hasImageKey,
      videoConfigured: cloned.video_source === "internal" || hasVideoKey,
    });
    confirmStartEnabled.set(Boolean(cloned.confirm_start));
    maxAttemptsStore.set(Math.max(1, Number(cloned.max_attempts) || 3));
  }

  async function loadSettings() {
    loading = true;
    try {
      const view = await api.get_settings();
      applySettingsView(view);
      imageKeyInput = "";
      videoKeyInput = "";
      hideAllKeyVisibility();
      dirty = false;
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      loading = false;
    }
  }

  onMount(loadSettings);

  function mark() {
    dirty = true;
    clearSaveMsgTimer();
    saveMsg = "";
  }

  function onImageKeyInput(e: Event) {
    imageKeyInput = (e.currentTarget as HTMLInputElement).value;
    if (imageKeyInput.trim()) pendingClearImageKey = false;
    mark();
  }

  function onVideoKeyInput(e: Event) {
    videoKeyInput = (e.currentTarget as HTMLInputElement).value;
    if (videoKeyInput.trim()) pendingClearVideoKey = false;
    mark();
  }

  // Đánh dấu xóa khóa nháp — chỉ xóa thật trong Keyring khi bấm "Lưu cài đặt" ("Hủy" khôi phục nguyên vẹn)
  function stageClearApiKey(which: "anh" | "video") {
    if ($running) return;
    if (which === "anh") {
      imageKeyInput = "";
      if (hasImageKey) {
        pendingClearImageKey = true;
        mark();
        showToast("info", "Đã đánh dấu xóa khóa API ảnh — bấm 'Lưu cài đặt' để áp dụng hoặc 'Hủy' để giữ lại.");
      }
    } else {
      videoKeyInput = "";
      if (hasVideoKey) {
        pendingClearVideoKey = true;
        mark();
        showToast("info", "Đã đánh dấu xóa khóa API video — bấm 'Lưu cài đặt' để áp dụng hoặc 'Hủy' để giữ lại.");
      }
    }
  }

  function onNotifyFinishChange() {
    mark();
    if (s?.notify_on_finish && typeof Notification !== "undefined") {
      if (Notification.permission === "default") {
        Notification.requestPermission().catch(() => {});
      }
    }
  }

  async function save() {
    if (!s || !formValid || $running || saving) return;
    saving = true;
    try {
      const payload: Settings = {
        ...s,
        endpoint_image_base_url: s.endpoint_image_base_url.trim(),
        endpoint_image_model: s.endpoint_image_model.trim(),
        endpoint_video_base_url: s.endpoint_video_base_url.trim(),
        endpoint_video_model: s.endpoint_video_model.trim(),
        video_size: s.video_size.trim(),
        video_seconds: Math.round(Number(s.video_seconds)),
        image_timeout_s: Math.round(Number(s.image_timeout_s)),
        video_create_timeout_s: Math.round(Number(s.video_create_timeout_s)),
        poll_interval_s: Math.round(Number(s.poll_interval_s)),
        poll_max_s: Math.round(Number(s.poll_max_s)),
        concurrency: Math.round(Number(s.concurrency)),
        max_attempts: Math.round(Number(s.max_attempts)),
        backoff_s: [
          Math.round(Number(s.backoff_s[0])),
          Math.round(Number(s.backoff_s[1])),
          Math.round(Number(s.backoff_s[2])),
        ],
        schedule_hours: Math.round(Number(s.schedule_hours)),
        auto_cleanup_days: Math.round(Number(s.auto_cleanup_days)),
        muse_chromium_path: (s.muse_chromium_path || "").trim(),
        muse_sidecar_port: Math.round(Number(s.muse_sidecar_port)),
      };

      const result = await executeSettingsSave({
        payload,
        imageKeyInput,
        videoKeyInput,
        pendingClearImageKey,
        pendingClearVideoKey,
        deps: {
          saveSettings: (cfg) => api.save_settings(cfg),
          setApiKey: (which, value) => api.set_api_key(which, value),
          getSettings: () => api.get_settings(),
        },
        onImageKeyApplied: () => {
          imageKeyInput = "";
          pendingClearImageKey = false;
        },
        onVideoKeyApplied: () => {
          videoKeyInput = "";
          pendingClearVideoKey = false;
        },
      });

      const shouldRefreshMuse =
        Boolean($museSidecarStatus?.sidecar.running) ||
        payload.video_source === "internal" ||
        payload.image_source === "internal";

      if (result.ok) {
        applySettingsView(result.view);
        hideAllKeyVisibility();
        dirty = false;
        // Cấu hình engine (port/Edge/headless) đổi → hỏi lại trạng thái để hiện cảnh báo
        // "cần khởi động lại" ngay, không bắt người dùng bấm Làm mới.
        if (shouldRefreshMuse) {
          void museRefresh();
        }
        saveMsg = "Đã lưu cài đặt thành công";
        showToast("success", "Đã lưu cấu hình hệ thống.");
        clearSaveMsgTimer();
        if (!destroyed) {
          saveMsgTimer = setTimeout(() => {
            saveMsgTimer = null;
            if (!dirty) saveMsg = "";
          }, 3000);
        }
      } else {
        if (result.settingsPersisted) {
          const keepPendingClearImage = pendingClearImageKey;
          const keepPendingClearVideo = pendingClearVideoKey;
          if (result.refreshedView) {
            applySettingsView(result.refreshedView);
          }
          pendingClearImageKey = keepPendingClearImage;
          pendingClearVideoKey = keepPendingClearVideo;
          if (shouldRefreshMuse) {
            void museRefresh();
          }
        }
        showToast("error", formatApiError(result.error));
      }
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      saving = false;
    }
  }

  async function cancel() {
    if ($running) return;
    clearSaveMsgTimer();
    await loadSettings();
    saveMsg = "";
    showToast("info", "Đã hoàn tác các thay đổi chưa lưu.");
  }

  // Chọn thư mục dự án an toàn: đủ cấu trúc + CSV hợp lệ -> mở thẳng; rỗng/thiếu/lỗi -> mở Modal
  async function pickProjectFolder() {
    if ($running) return;
    try {
      const dir = await api.pick_folder();
      if (!dir) return;
      const probe = await api.probe_folder(dir);
      const writeItem = probe.checklist.find((i) => i.key === "write");
      if (writeItem && !writeItem.ok) {
        showToast("error", writeItem.message || "Không có quyền ghi vào thư mục đã chọn.");
        return;
      }

      if (probe.has_full_structure && probe.prompts_valid !== false) {
        const info = await api.load_project(dir);
        resetProjectRunState();
        project.set(info);
        rowChangedTick.update((n) => n + 1);
        if (s) s.current_project = info.path;
        showToast("success", `Đã mở dự án "${info.name}" (${info.row_count} dòng).`);
        return;
      }

      probedDir = dir;
      probeInfo = probe;
      folderMode = "giu_lai";
      folderModalOpen = true;
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function confirmSwitchProjectFolder() {
    if (!probedDir || !probeInfo) return;
    switchingFolder = true;
    try {
      const info =
        probeInfo.has_full_structure &&
        probeInfo.prompts_valid !== false &&
        folderMode === "giu_lai"
          ? await api.load_project(probedDir)
          : await api.create_project(probedDir, folderMode);
      resetProjectRunState();
      project.set(info);
      rowChangedTick.update((n) => n + 1);
      if (s) {
        s.current_project = info.path;
      }
      folderModalOpen = false;
      showToast("success", `Đã chuyển thư mục dự án sang "${info.name}".`);
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      switchingFolder = false;
    }
  }

  async function recheckCurrentProject() {
    if (!$project || $running || recheckingProject) return;
    recheckingProject = true;
    try {
      const checklist = await api.check_structure();
      project.update((cur) => (cur ? { ...cur, checklist } : cur));
      rowChangedTick.update((n) => n + 1);
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      recheckingProject = false;
    }
  }

  function joinCsv(base: string): string {
    const sep = base.includes("\\") ? "\\" : "/";
    return `${base.replace(/[\\/]+$/, "")}${sep}prompts.csv`;
  }

  async function revealPromptsCsv(basePath: string) {
    if (!basePath) return;
    try {
      await api.open_path_in_explorer(joinCsv(basePath), true);
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  async function test(which: "anh" | "video") {
    if (!s) return;
    // Giữ nguyên chuỗi trim (KHÔNG đổi "" sang undefined để tránh test nhầm URL cũ trong cache!)
    const baseUrl =
      (which === "anh" ? s.endpoint_image_base_url : s.endpoint_video_base_url)?.trim() ?? "";
    const rawDraftKey = (which === "anh" ? imageKeyInput : videoKeyInput).trim();
    const pendingClear = which === "anh" ? pendingClearImageKey : pendingClearVideoKey;

    if (!baseUrl || !isValidHttpUrl(baseUrl)) {
      const msg = "Địa chỉ gốc (Base URL) trống hoặc không hợp lệ — hãy nhập URL http(s)://.";
      if (which === "anh") {
        testImageOk = false;
        testImage = msg;
      } else {
        testVideoOk = false;
        testVideo = msg;
      }
      return;
    }

    if (pendingClear && !rawDraftKey) {
      const msg = "Khóa API đang được đánh dấu xóa — hãy nhập khóa mới hoặc bấm Hủy để kiểm tra.";
      if (which === "anh") {
        testImageOk = false;
        testImage = msg;
      } else {
        testVideoOk = false;
        testVideo = msg;
      }
      return;
    }

    const draftKey = rawDraftKey || undefined;

    if (which === "anh") {
      testingImage = true;
      testImage = "";
      testImageOk = null;
    } else {
      testingVideo = true;
      testVideo = "";
      testVideoOk = null;
    }

    try {
      const [ok, ms, msg] = await api.test_connection(which, baseUrl, draftKey);
      const line = `${ok ? "Kết nối OK" : "Không đạt"} (${ms} ms) — ${msg}`;
      if (which === "anh") {
        testImageOk = ok;
        testImage = line;
      } else {
        testVideoOk = ok;
        testVideo = line;
      }
    } catch (e) {
      const errLine = formatApiError(e);
      if (which === "anh") {
        testImageOk = false;
        testImage = errLine;
      } else {
        testVideoOk = false;
        testVideo = errLine;
      }
    } finally {
      if (which === "anh") testingImage = false;
      else testingVideo = false;
    }
  }

  async function loadModels(which: "anh" | "video") {
    if (!s) return;
    const baseUrl =
      (which === "anh" ? s.endpoint_image_base_url : s.endpoint_video_base_url)?.trim() ?? "";
    const rawDraftKey = (which === "anh" ? imageKeyInput : videoKeyInput).trim();
    const pendingClear = which === "anh" ? pendingClearImageKey : pendingClearVideoKey;

    if (!baseUrl || !isValidHttpUrl(baseUrl)) {
      const msg = "Vui lòng nhập Địa chỉ gốc (Base URL) hợp lệ trước khi tải model.";
      if (which === "anh") imageModelMsg = msg;
      else videoModelMsg = msg;
      return;
    }

    if (pendingClear && !rawDraftKey) {
      const msg = "Khóa API đang được đánh dấu xóa — hãy nhập khóa mới để tải danh sách model.";
      if (which === "anh") imageModelMsg = msg;
      else videoModelMsg = msg;
      return;
    }

    const draftKey = rawDraftKey || undefined;

    if (which === "anh") {
      loadingImageModels = true;
      imageModelMsg = "";
    } else {
      loadingVideoModels = true;
      videoModelMsg = "";
    }

    try {
      const ids = await api.list_models(which, baseUrl, draftKey);
      if (which === "anh") {
        imageModels = ids;
        imageModelMsg =
          ids.length > 0
            ? `Đã tải ${ids.length} model từ endpoint ảnh — chọn bên dưới để áp dụng.`
            : "Endpoint ảnh không trả danh sách model nào.";
      } else {
        videoModels = ids;
        videoModelMsg =
          ids.length > 0
            ? `Đã tải ${ids.length} model từ endpoint video — chọn bên dưới để áp dụng.`
            : "Endpoint video không trả danh sách model nào.";
      }
    } catch (e) {
      const msg = formatApiError(e);
      if (which === "anh") imageModelMsg = msg;
      else videoModelMsg = msg;
    } finally {
      if (which === "anh") loadingImageModels = false;
      else loadingVideoModels = false;
    }
  }

  function onModelSelect(which: "anh" | "video", e: Event) {
    if (!s) return;
    const value = (e.currentTarget as HTMLSelectElement).value;
    if (!value) return;
    if (which === "anh") {
      s.endpoint_image_model = value;
      imageModels = [];
      imageModelMsg = `Đã chọn model ảnh: ${value}`;
    } else {
      s.endpoint_video_model = value;
      videoModels = [];
      videoModelMsg = `Đã chọn model video: ${value}`;
    }
    mark();
  }
</script>

<div class="settings-screen">
  {#if loading}
    <div class="loading card">Đang nạp cài đặt hệ thống...</div>
  {:else if s}
    {#if $running}
      <div class="running-lock-banner">
        <Icons name="alert" size={16} />
        <span>
          Lượt chạy đang thực hiện — toàn bộ trường cài đặt tạm khóa để đảm bảo tính nhất quán. Hãy
          dừng hoặc chờ lượt chạy kết thúc để chỉnh sửa.
        </span>
      </div>
    {/if}

    <div class="tabs" role="tablist" aria-label="Nhóm cài đặt">
      {#each tabs as t (t.key)}
        <button
          type="button"
          role="tab"
          aria-selected={tab === t.key}
          class="tab"
          class:active={tab === t.key}
          on:click={() => (t.key === "video" ? onSelectTabVideo() : selectTab(t.key))}
        >
          <Icons name={t.icon} size={15} />
          <span>{t.label}</span>
        </button>
      {/each}
    </div>

    <div class="form card">
      {#if tab === "project"}
        <label class="label" for="cfg-current-project">Thư mục dự án hiện tại</label>
        <div class="inline">
          <input
            id="cfg-current-project"
            class="input grow selectable"
            readonly
            disabled={$running}
            value={$project?.path ?? s.current_project ?? ""}
            placeholder="Chưa chọn thư mục dự án"
          />
          <button
            type="button"
            class="btn-secondary"
            disabled={$running}
            on:click={pickProjectFolder}
          >
            <Icons name="folder-open" size={15} />
            <span>Chọn thư mục...</span>
          </button>
        </div>
        <div class="hint">
          Dự án đủ cấu trúc và <code>prompts.csv</code> hợp lệ sẽ được mở trực tiếp (mục 6.7b); thư mục
          trống hoặc thiếu sẽ hiện hộp xác nhận an toàn.
        </div>

        <!-- Bảng kiểm tra 5 mục ngay tại Tab 1 (mục 6.6 Thẻ 1) -->
        <div class="checklist-panel">
          <div class="checklist-head">
            <span class="checklist-title">Trạng thái kiểm tra 5 mục cấu trúc</span>
            {#if $project}
              <button
                type="button"
                class="btn-ghost small-recheck"
                disabled={$running || recheckingProject}
                on:click={recheckCurrentProject}
              >
                <Icons name="refresh" size={13} />
                <span>{recheckingProject ? "Đang quét..." : "Kiểm tra lại"}</span>
              </button>
            {/if}
          </div>

          {#if !$project}
            <div class="checklist-empty">Chưa mở dự án nào để kiểm tra.</div>
          {:else}
            <div class="checklist-rows">
              {#each $project.checklist as item (item.key)}
                <div class="checklist-row" class:ok={item.ok} class:bad={!item.ok}>
                  <Icons name={item.ok ? "check" : "alert"} size={14} />
                  <span class="checklist-msg selectable">{item.message}</span>
                  {#if item.key === "prompts" && !item.ok}
                    <button
                      type="button"
                      class="btn-ghost small-recheck"
                      on:click={() => $project && revealPromptsCsv($project.path)}
                    >
                      <Icons name="external" size={12} />
                      <span>Hiện prompts.csv</span>
                    </button>
                  {/if}
                </div>
              {/each}
            </div>
          {/if}
        </div>

      {:else if tab === "image"}
        <div class="label" id="image-source-label">Nguồn sinh ảnh</div>
        <div class="source-radios" role="radiogroup" aria-labelledby="image-source-label">
          <label class="radio-card" class:active={s.image_source !== "internal"}>
            <input
              type="radio"
              name="image-source"
              value="gateway"
              checked={s.image_source !== "internal"}
              disabled={$running}
              on:change={() => {
                if (s) {
                  s.image_source = "gateway";
                  mark();
                }
              }}
            />
            <div>
              <strong>Gateway ngoài</strong>
              <span>Gọi endpoint OpenAI-compatible bên ngoài (mặc định hiện tại)</span>
            </div>
          </label>
          <label class="radio-card" class:active={s.image_source === "internal"}>
            <input
              type="radio"
              name="image-source"
              value="internal"
              checked={s.image_source === "internal"}
              disabled={$running}
              on:change={() => {
                if (s) {
                  s.image_source = "internal";
                  mark();
                  museRefresh();
                }
              }}
            />
            <div>
              <strong>VideoFlow Muse (nội bộ)</strong>
              <span>Tạo ảnh bằng model ảnh của muse.ai (muse-image), dùng chung engine/cookie với video</span>
            </div>
          </label>
        </div>

        {#if s.image_source === "internal"}
          <div class="hint" style="margin-bottom:8px">
            Ảnh được tạo trực tiếp bằng model <code>muse-image</code> của muse.ai qua engine
            VideoFlow Muse (chung engine với video). Tài khoản &amp; cookie quản lý ở mục
            <b>Tài khoản</b> trên thanh điều hướng; engine tự khởi động khi chạy.
          </div>
        {:else}
        <label class="label" for="cfg-img-base-url">Địa chỉ gốc (Base URL)</label>
        <input
          id="cfg-img-base-url"
          class="input full"
          class:invalid={!isValidHttpUrl(s.endpoint_image_base_url)}
          bind:value={s.endpoint_image_base_url}
          on:input={mark}
          disabled={$running}
          placeholder="https://gpt.photowrapped.com/v1"
        />

        <label class="label" for="cfg-img-api-key">
          API Key
          {#if pendingClearImageKey}
            <span class="pending-clear-tag">(sẽ xóa khi bấm Lưu cài đặt)</span>
          {:else if imageKeyPreview}
            (đã lưu: {imageKeyPreview})
          {:else}
            (chưa lưu trong Keyring)
          {/if}
        </label>
        <div class="inline">
          <div class="key-input-wrap grow">
            <input
              id="cfg-img-api-key"
              class="input full key-input"
              type={showImageKey ? "text" : "password"}
              placeholder={hasImageKey && !pendingClearImageKey
                ? "Nhập khóa mới để thay thế..."
                : "Nhập API key endpoint ảnh..."}
              value={imageKeyInput}
              on:input={onImageKeyInput}
              disabled={$running}
              autocomplete="off"
            />
            <button
              type="button"
              class="btn-ghost eye-btn"
              aria-label={showImageKey ? "Ẩn API key ảnh" : "Hiện API key ảnh (tự ẩn sau 10s)"}
              title={showImageKey ? "Ẩn khóa" : "Hiện khóa (tự ẩn sau 10 giây)"}
              disabled={$running}
              on:click={() => toggleKeyVisibility("anh")}
            >
              <Icons name={showImageKey ? "eye-off" : "eye"} size={15} />
            </button>
          </div>
          <button
            type="button"
            class="btn-secondary"
            disabled={$running || ((!hasImageKey || pendingClearImageKey) && !imageKeyInput)}
            on:click={() => stageClearApiKey("anh")}
          >
            <Icons name="trash" size={14} />
            <span>Xóa khóa</span>
          </button>
        </div>
        <div class="hint">
          Khóa API được bảo vệ riêng trong Windows Credential Manager; hiện mật khẩu tự ẩn sau 10 giây.
        </div>

        <label class="label" for="cfg-img-model">Model tạo ảnh</label>
        <div class="inline">
          <input
            id="cfg-img-model"
            class="input grow"
            class:invalid={!s.endpoint_image_model?.trim()}
            bind:value={s.endpoint_image_model}
            on:input={mark}
            disabled={$running}
            placeholder="gpt-image-2"
          />
          <button
            type="button"
            class="btn-secondary"
            disabled={$running || loadingImageModels}
            on:click={() => loadModels("anh")}
          >
            <Icons name="refresh" size={14} />
            <span>{loadingImageModels ? "Đang tải..." : "Tải danh sách model"}</span>
          </button>
        </div>

        {#if imageModels.length > 0}
          <label class="label" for="cfg-img-model-select">Chọn nhanh từ danh sách model phản hồi</label>
          <select
            id="cfg-img-model-select"
            class="input full"
            disabled={$running}
            on:change={(e) => onModelSelect("anh", e)}
          >
            <option value="">— Chọn model ảnh ({imageModels.length}) —</option>
            {#each imageModels as id (id)}
              <option value={id}>{id}</option>
            {/each}
          </select>
        {/if}

        {#if imageModelMsg}
          <div class="feedback-line selectable">{imageModelMsg}</div>
        {/if}
        {/if}

        <label class="label" for="cfg-img-timeout">Thời gian chờ tối đa mỗi lần gọi (giây)</label>
        <input
          id="cfg-img-timeout"
          class="input narrow"
          class:invalid={!isValidIntInRange(s.image_timeout_s, 5, 3600)}
          type="number"
          min="5"
          max="3600"
          bind:value={s.image_timeout_s}
          on:input={mark}
          disabled={$running}
        />

        {#if s.image_source !== "internal"}
        <div class="inline test-row">
          <button
            type="button"
            class="btn-secondary"
            disabled={testingImage}
            on:click={() => test("anh")}
          >
            <Icons name="link" size={15} />
            <span>{testingImage ? "Đang kiểm tra..." : "Kiểm tra kết nối"}</span>
          </button>
          {#if testImage}
            <span
              class="test-result selectable"
              class:ok={testImageOk === true}
              class:bad={testImageOk === false}
            >
              {testImage}
            </span>
          {/if}
        </div>
        <div class="hint">Kiểm tra trực tiếp Base URL và API key hiện có trên biểu mẫu.</div>
        {/if}

      {:else if tab === "video"}
        <div class="label" id="video-source-label">Nguồn sinh video</div>
        <div class="source-radios" role="radiogroup" aria-labelledby="video-source-label">
          <label class="radio-card" class:active={s.video_source !== "internal"}>
            <input
              type="radio"
              name="video-source"
              value="gateway"
              checked={s.video_source !== "internal"}
              disabled={$running}
              on:change={() => {
                if (s) {
                  s.video_source = "gateway";
                  mark();
                }
              }}
            />
            <div>
              <strong>Gateway ngoài</strong>
              <span>Gọi endpoint OpenAI-compatible bên ngoài (mặc định hiện tại)</span>
            </div>
          </label>
          <label class="radio-card" class:active={s.video_source === "internal"}>
            <input
              type="radio"
              name="video-source"
              value="internal"
              checked={s.video_source === "internal"}
              disabled={$running}
              on:change={() => {
                if (s) {
                  s.video_source = "internal";
                  mark();
                  museRefresh();
                }
              }}
            />
            <div>
              <strong>VideoFlow Muse (nội bộ)</strong>
              <span>Gateway chạy ngầm trên máy, sinh video trực tiếp từ muse.ai bằng cookie</span>
            </div>
          </label>
        </div>

        {#if s.video_source !== "internal"}
        <label class="label" for="cfg-vid-base-url">Địa chỉ gốc (Base URL)</label>
        <input
          id="cfg-vid-base-url"
          class="input full"
          class:invalid={!isValidHttpUrl(s.endpoint_video_base_url)}
          bind:value={s.endpoint_video_base_url}
          on:input={mark}
          disabled={$running}
          placeholder="https://muse.photowrapped.com/v1"
        />

        <label class="label" for="cfg-vid-api-key">
          API Key
          {#if pendingClearVideoKey}
            <span class="pending-clear-tag">(sẽ xóa khi bấm Lưu cài đặt)</span>
          {:else if videoKeyPreview}
            (đã lưu: {videoKeyPreview})
          {:else}
            (chưa lưu trong Keyring)
          {/if}
        </label>
        <div class="inline">
          <div class="key-input-wrap grow">
            <input
              id="cfg-vid-api-key"
              class="input full key-input"
              type={showVideoKey ? "text" : "password"}
              placeholder={hasVideoKey && !pendingClearVideoKey
                ? "Nhập khóa mới để thay thế..."
                : "Nhập API key endpoint video..."}
              value={videoKeyInput}
              on:input={onVideoKeyInput}
              disabled={$running}
              autocomplete="off"
            />
            <button
              type="button"
              class="btn-ghost eye-btn"
              aria-label={showVideoKey ? "Ẩn API key video" : "Hiện API key video (tự ẩn sau 10s)"}
              title={showVideoKey ? "Ẩn khóa" : "Hiện khóa (tự ẩn sau 10 giây)"}
              disabled={$running}
              on:click={() => toggleKeyVisibility("video")}
            >
              <Icons name={showVideoKey ? "eye-off" : "eye"} size={15} />
            </button>
          </div>
          <button
            type="button"
            class="btn-secondary"
            disabled={$running || ((!hasVideoKey || pendingClearVideoKey) && !videoKeyInput)}
            on:click={() => stageClearApiKey("video")}
          >
            <Icons name="trash" size={14} />
            <span>Xóa khóa</span>
          </button>
        </div>

        <label class="label" for="cfg-vid-model">Model tạo video</label>
        <div class="inline">
          <input
            id="cfg-vid-model"
            class="input grow"
            class:invalid={!s.endpoint_video_model?.trim()}
            bind:value={s.endpoint_video_model}
            on:input={mark}
            disabled={$running}
            placeholder="muse-video"
          />
          <button
            type="button"
            class="btn-secondary"
            disabled={$running || loadingVideoModels}
            on:click={() => loadModels("video")}
          >
            <Icons name="refresh" size={14} />
            <span>{loadingVideoModels ? "Đang tải..." : "Tải danh sách model"}</span>
          </button>
        </div>

        {#if videoModels.length > 0}
          <label class="label" for="cfg-vid-model-select">Chọn nhanh từ danh sách model phản hồi</label>
          <select
            id="cfg-vid-model-select"
            class="input full"
            disabled={$running}
            on:change={(e) => onModelSelect("video", e)}
          >
            <option value="">— Chọn model video ({videoModels.length}) —</option>
            {#each videoModels as id (id)}
              <option value={id}>{id}</option>
            {/each}
          </select>
        {/if}

        {#if videoModelMsg}
          <div class="feedback-line selectable">{videoModelMsg}</div>
        {/if}
        {/if}

        {#if s.video_source === "internal"}
        <div class="muse-panel">
          <div class="muse-head">
            <div class="muse-title">
              <Icons name="video" size={16} />
              <strong>VideoFlow Muse (nội bộ)</strong>
              {#if $museSidecarStatus?.sidecar.running}
                <span class="muse-badge ok">Đang chạy{$museSidecarStatus.sidecar.version ? ` · v${$museSidecarStatus.sidecar.version}` : ""}</span>
              {:else}
                <span class="muse-badge mute">Đã dừng</span>
              {/if}
            </div>
            <div class="muse-actions">
              <button
                type="button"
                class="btn-secondary"
                disabled={museSidecarBusy || $running || Boolean($museSidecarStatus?.sidecar.running)}
                on:click={museStart}
              >
                <Icons name="play" size={14} />
                <span>{museStarting ? "Đang khởi động..." : "Khởi động"}</span>
              </button>
              <button
                type="button"
                class="btn-secondary"
                disabled={museSidecarBusy || $running || !$museSidecarStatus?.sidecar.running}
                on:click={museStop}
              >
                <Icons name="stop" size={14} />
                <span>{museStopping ? "Đang dừng..." : "Dừng"}</span>
              </button>
              <button type="button" class="btn-secondary" disabled={museLoading} on:click={museRefresh}>
                <Icons name="refresh" size={14} />
                <span>{museLoading ? "Đang tải..." : "Làm mới"}</span>
              </button>
              {#if $museSidecarStatus?.sidecar.running && $museSidecarStatus.sidecar.restart_required}
                <button
                  type="button"
                  class="btn-primary"
                  disabled={museSidecarBusy || $running}
                  on:click={museRestart}
                  title="Cài đặt Port / Edge / headless đã đổi — áp dụng bằng cách khởi động lại engine"
                >
                  <Icons name="redo" size={14} />
                  <span>{museRestarting ? "Đang áp dụng..." : "Áp dụng & khởi động lại"}</span>
                </button>
              {/if}
            </div>
          </div>

          {#if $museSidecarStatus?.sidecar.running && $museSidecarStatus.sidecar.restart_required}
            <div class="muse-stale-config">
              <Icons name="alert" size={14} />
              <span>
                Engine đang chạy với cấu hình cũ:
                {#each (museChanged.length > 0 ? museChanged : ["cấu hình engine"]) as f, i (f)}
                  {i > 0 ? ", " : ""}<b>{f}</b>
                {/each}.
                Bấm <b>"Áp dụng &amp; khởi động lại"</b> để có hiệu lực.
              </span>
            </div>
          {/if}

          <div class="hint">
            Sidecar tự dò Microsoft Edge/Chrome trên máy và điều khiển headless. Cookie muse.ai lưu
            trong thư mục dữ liệu app và tự gia hạn phiên mỗi 10 phút.
            {#if $museSidecarStatus?.chromium_path}
              <br />Trình duyệt: <code>{$museSidecarStatus.chromium_path}</code>
            {/if}
          </div>

          {#if museErr}
            <div class="feedback-line bad selectable">{museErr}</div>
          {/if}

          <div class="muse-config">
            <div>
              <label class="label" for="muse-chromium">Đường dẫn Edge/Chrome (để trống = tự dò)</label>
              <input
                id="muse-chromium"
                class="input full"
                bind:value={s.muse_chromium_path}
                on:input={mark}
                disabled={$running}
                placeholder="C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
              />
            </div>
            <div class="muse-config-side">
              <label class="label" for="muse-port">Port</label>
              <input
                id="muse-port"
                class="input"
                type="number"
                min="1"
                max="65535"
                class:invalid={!isValidIntInRange(s.muse_sidecar_port, 1, 65535)}
                bind:value={s.muse_sidecar_port}
                on:input={mark}
                disabled={$running}
              />
              <label class="muse-checkbox">
                <input
                  type="checkbox"
                  bind:checked={s.muse_headless}
                  on:change={mark}
                  disabled={$running}
                />
                <span>Chạy ẩn trình duyệt (headless)</span>
              </label>
            </div>
          </div>

          <!-- UI-1: Thu gọn danh sách tài khoản thành 1 dòng tóm tắt kèm nút chuyển nhanh sang tab Tài khoản -->
          <div class="muse-accounts-summary-row">
            <div class="muse-summary-info">
              <Icons name="video-source" size={16} />
              <span>
                {$museAccounts.length} tài khoản · {$museAccounts.filter((a) => a.enabled).length} đang bật
              </span>
            </div>
            <button
              type="button"
              class="btn-secondary jump-accounts-btn"
              on:click={() => dispatch("navigate", "accounts")}
              title="Chuyển sang tab Tài khoản trên thanh điều hướng"
            >
              <Icons name="video-source" size={14} />
              <span>Quản lý tài khoản</span>
            </button>
          </div>
        </div>
        {/if}

        <div class="two-cols">
          <div>
            <label class="label" for="cfg-vid-size">Kích thước khung hình</label>
            <input
              id="cfg-vid-size"
              class="input full"
              class:invalid={!s.video_size?.trim()}
              bind:value={s.video_size}
              on:input={mark}
              disabled={$running}
              placeholder="720x1280"
            />
          </div>
          <div>
            <label class="label" for="cfg-vid-seconds">Thời lượng video (giây)</label>
            <input
              id="cfg-vid-seconds"
              class="input full"
              class:invalid={!isValidIntInRange(s.video_seconds, 1, 120)}
              type="number"
              min="1"
              max="120"
              bind:value={s.video_seconds}
              on:input={mark}
              disabled={$running}
            />
          </div>
        </div>

        <div class="two-cols">
          <div>
            <label class="label" for="cfg-poll-interval">Chu kỳ poll trạng thái (giây)</label>
            <input
              id="cfg-poll-interval"
              class="input full"
              class:invalid={!isValidIntInRange(s.poll_interval_s, 1, 600)}
              type="number"
              min="1"
              max="600"
              bind:value={s.poll_interval_s}
              on:input={mark}
              disabled={$running}
            />
          </div>
          <div>
            <label class="label" for="cfg-poll-max">Thời gian poll tối đa (giây - mặc định 600)</label>
            <input
              id="cfg-poll-max"
              class="input full"
              class:invalid={!isValidIntInRange(s.poll_max_s, 10, 7200)}
              type="number"
              min="10"
              max="7200"
              bind:value={s.poll_max_s}
              on:input={mark}
              disabled={$running}
            />
          </div>
        </div>
        <div class="hint">
          Theo Phụ lục v1.3.1: bộ đếm 600s tự làm mới khi <code>progress</code> tăng và giữ nguyên
          <code>task_id</code> khi hết giờ poll để không tốn thêm quota.
        </div>

        <label class="label" for="cfg-vid-create-timeout">Timeout lệnh tạo task video (giây)</label>
        <input
          id="cfg-vid-create-timeout"
          class="input narrow"
          class:invalid={!isValidIntInRange(s.video_create_timeout_s, 5, 3600)}
          type="number"
          min="5"
          max="3600"
          bind:value={s.video_create_timeout_s}
          on:input={mark}
          disabled={$running}
        />

        {#if s.video_source !== "internal"}
        <div class="inline test-row">
          <button
            type="button"
            class="btn-secondary"
            disabled={testingVideo}
            on:click={() => test("video")}
          >
            <Icons name="link" size={15} />
            <span>{testingVideo ? "Đang kiểm tra..." : "Kiểm tra kết nối"}</span>
          </button>
          {#if testVideo}
            <span
              class="test-result selectable"
              class:ok={testVideoOk === true}
              class:bad={testVideoOk === false}
            >
              {testVideo}
            </span>
          {/if}
        </div>
        {/if}

      {:else if tab === "run"}
        <div class="two-cols">
          <div>
            <label class="label" for="cfg-concurrency">Số dòng chạy đồng thời (1–3)</label>
            <input
              id="cfg-concurrency"
              class="input full"
              class:invalid={!isValidIntInRange(s.concurrency, 1, 3)}
              type="number"
              min="1"
              max="3"
              bind:value={s.concurrency}
              on:input={mark}
              disabled={$running}
            />
          </div>
          <div>
            <label class="label" for="cfg-max-attempts">Số lần thử lại tối đa mỗi dòng (1–10)</label>
            <input
              id="cfg-max-attempts"
              class="input full"
              class:invalid={!isValidIntInRange(s.max_attempts, 1, 10)}
              type="number"
              min="1"
              max="10"
              bind:value={s.max_attempts}
              on:input={mark}
              disabled={$running}
            />
          </div>
        </div>
        {#if Number(s.concurrency) > 1}
          <div class="warn-hint">
            Chế độ chạy song song {s.concurrency} dòng giúp rút ngắn thời gian tổng, nhưng hãy đảm bảo hạn mức
            rate-limit API của bạn cho phép nhiều phiên đồng thời.
          </div>
        {/if}

        <div class="field-group-title">
          Giãn cách thử lại lỗi tạm thời (giây) — Phụ lục v1.3.1: 30s / 120s / 300s
        </div>
        <div class="three-cols">
          <div>
            <label class="sub-label" for="cfg-backoff-1">Lần thử 1 (giây)</label>
            <input
              id="cfg-backoff-1"
              class="input full"
              class:invalid={!isValidIntInRange(s.backoff_s[0], 1, 3600)}
              type="number"
              min="1"
              max="3600"
              bind:value={s.backoff_s[0]}
              on:input={mark}
              disabled={$running}
            />
          </div>
          <div>
            <label class="sub-label" for="cfg-backoff-2">Lần thử 2 (giây)</label>
            <input
              id="cfg-backoff-2"
              class="input full"
              class:invalid={!isValidIntInRange(s.backoff_s[1], 1, 3600)}
              type="number"
              min="1"
              max="3600"
              bind:value={s.backoff_s[1]}
              on:input={mark}
              disabled={$running}
            />
          </div>
          <div>
            <label class="sub-label" for="cfg-backoff-3">Lần thử 3 (giây)</label>
            <input
              id="cfg-backoff-3"
              class="input full"
              class:invalid={!isValidIntInRange(s.backoff_s[2], 1, 3600)}
              type="number"
              min="1"
              max="3600"
              bind:value={s.backoff_s[2]}
              on:input={mark}
              disabled={$running}
            />
          </div>
        </div>
        <div class="hint">Lỗi HTTP 429 ưu tiên chờ theo header <code>Retry-After</code> nếu cổng API cung cấp.</div>

        <div class="section-divider"></div>

        <label class="check" for="cfg-schedule-enabled">
          <input
            id="cfg-schedule-enabled"
            type="checkbox"
            bind:checked={s.schedule_enabled}
            on:change={mark}
            disabled={$running}
          />
          <span>Tự động quét và chạy các dòng chờ theo lịch định kỳ</span>
        </label>

        <div class="two-cols" style="margin-top: 6px;">
          <div>
            <label class="label" for="cfg-schedule-hours">Chu kỳ chạy tự động (giờ)</label>
            <input
              id="cfg-schedule-hours"
              class="input full"
              class:invalid={!isValidIntInRange(s.schedule_hours, 1, 720)}
              type="number"
              min="1"
              max="720"
              bind:value={s.schedule_hours}
              on:input={mark}
              disabled={$running}
            />
          </div>
          <div>
            <label class="label" for="cfg-cleanup-days">Tự dọn nhật ký & lượt cũ sau (ngày, 0 = tắt)</label>
            <input
              id="cfg-cleanup-days"
              class="input full"
              class:invalid={!isValidIntInRange(s.auto_cleanup_days, 0, 3650)}
              type="number"
              min="0"
              max="3650"
              bind:value={s.auto_cleanup_days}
              on:input={mark}
              disabled={$running}
            />
          </div>
        </div>
        <div class="hint">
          Bộ lập lịch hoạt động khi ứng dụng đang mở hoặc thu nhỏ dưới System Tray. Tự dọn lịch sử luôn
          bảo toàn 100% file sản phẩm và trạng thái mới nhất của từng dòng.
        </div>

        <div class="section-divider"></div>

        <label class="check" for="cfg-auto-merge">
          <input
            id="cfg-auto-merge"
            type="checkbox"
            bind:checked={s.auto_merge}
            on:change={mark}
            disabled={$running}
          />
          <span>Tự động ghép video sau batch (Auto-merge FFmpeg)</span>
        </label>
        <div class="hint" style="margin-left: 24px;">
          Tự động ghép nối các clip video thành phẩm theo đúng thứ tự dòng CSV thành file tổng hợp tại <code>video_out/</code> sau khi hoàn tất batch.
        </div>

      {:else}
        <label class="check active-check" for="cfg-confirm-start">
          <input
            id="cfg-confirm-start"
            type="checkbox"
            bind:checked={s.confirm_start}
            on:change={mark}
            disabled={$running}
          />
          <span>Hỏi xác nhận tóm tắt dòng việc trước khi bấm START</span>
        </label>
        <div class="hint" style="margin-left: 24px;">
          Hiển thị hộp kiểm tra số lượng dòng sẽ chạy và cảnh báo nếu <code>prompts.csv</code> đang mở trong Excel.
        </div>

        <div class="section-divider"></div>

        <label class="check" for="cfg-notify-finish">
          <input
            id="cfg-notify-finish"
            type="checkbox"
            bind:checked={s.notify_on_finish}
            on:change={onNotifyFinishChange}
            disabled={$running}
          />
          <span>Thông báo màn hình &amp; nhấp nháy thanh tác vụ khi lượt chạy kết thúc</span>
        </label>

        <label class="check" for="cfg-min-tray">
          <input
            id="cfg-min-tray"
            type="checkbox"
            bind:checked={s.minimize_to_tray}
            on:change={mark}
            disabled={$running}
          />
          <span>Thu nhỏ xuống khay hệ thống (System Tray) khi bấm nút đóng cửa sổ</span>
        </label>

        <label class="check" for="cfg-start-win">
          <input
            id="cfg-start-win"
            type="checkbox"
            bind:checked={s.start_with_windows}
            on:change={mark}
            disabled={$running}
          />
          <span>Khởi động VideoFlow Desktop cùng Windows</span>
        </label>

        <div class="hint" style="margin-top: 12px;">
          Tệp cấu hình lưu an toàn tại <code>%APPDATA%/VideoFlowDesktop/settings.json</code> (không bao giờ chứa API key).
        </div>
      {/if}
    </div>

    {#if !formValid}
      <div class="validation-banner error-box">
        <Icons name="alert" size={15} />
        <span>{validationErrors[0]}</span>
      </div>
    {/if}

    <div class="savebar">
      <span
        class="dirty-text"
        class:visible={dirty || Boolean(saveMsg)}
        class:saved={!dirty && Boolean(saveMsg)}
        class:invalid={!formValid}
      >
        {#if !formValid}
          Vui lòng sửa trường không hợp lệ trước khi lưu
        {:else if saveMsg}
          {saveMsg}
        {:else}
          Có thay đổi chưa lưu
        {/if}
      </span>
      <button
        type="button"
        class="btn-secondary"
        disabled={!dirty || $running || saving}
        on:click={cancel}
      >
        Hủy
      </button>
      <button
        type="button"
        class="btn-primary"
        disabled={!dirty || !formValid || $running || saving}
        on:click={save}
      >
        <Icons name="check" size={15} />
        <span>{saving ? "Đang lưu..." : "Lưu cài đặt"}</span>
      </button>
    </div>
  {/if}
</div>

<!-- Modal xác nhận khi thư mục mới trống / thiếu thành phần / lỗi CSV -->
<Modal
  open={folderModalOpen}
  title={probeInfo?.is_empty_dir ? "Khởi tạo thư mục dự án mới" : "Xác nhận chuyển thư mục dự án"}
  width="480px"
  disableClose={switchingFolder}
  on:close={() => (folderModalOpen = false)}
>
  {#if probeInfo}
    <div class="folder-modal-body">
      <div class="probed-path selectable">{probedDir}</div>
      {#if probeInfo.prompts_error}
        <div class="probe-err-box error-box">
          <div>{probeInfo.prompts_error}</div>
          <button
            type="button"
            class="btn-secondary small-recheck"
            on:click={() => revealPromptsCsv(probedDir)}
          >
            <Icons name="external" size={13} />
            <span>Hiện prompts.csv trong Explorer</span>
          </button>
        </div>
      {/if}
      {#if probeInfo.is_empty_dir}
        <p class="folder-modal-desc">
          Thư mục này chưa có cấu trúc VideoFlow. Bạn có muốn khởi tạo <code>input/</code>,
          <code>image_out/</code>, <code>video_out/</code>, <code>prompts.csv</code> mẫu và
          <code>state.db</code> tại đây không?
        </p>
      {:else}
        <p class="folder-modal-desc">
          Thư mục đã có một phần dữ liệu ({probeInfo.row_count ?? 0} dòng hợp lệ). Chọn phương thức áp dụng:
        </p>
        <label class="radio-option" class:selected={folderMode === "giu_lai"}>
          <input type="radio" name="settingsFolderMode" value="giu_lai" bind:group={folderMode} />
          <div>
            <div class="radio-title">Giữ lại dữ liệu hiện có (Khuyên dùng)</div>
            <div class="radio-sub">Giữ nguyên toàn bộ CSV, lịch sử, ảnh/video và chỉ tạo bù thư mục thiếu.</div>
          </div>
        </label>
        <label class="radio-option danger-radio" class:selected={folderMode === "ghi_de"}>
          <input type="radio" name="settingsFolderMode" value="ghi_de" bind:group={folderMode} />
          <div>
            <div class="radio-title">Tạo mới từ đầu (sao lưu prompts.csv cũ)</div>
            <div class="radio-sub">Sao lưu CSV cũ sang file backup và làm mới cơ sở dữ liệu trạng thái.</div>
          </div>
        </label>
      {/if}
    </div>
  {/if}
  <div slot="footer">
    <button
      type="button"
      class="btn-secondary"
      disabled={switchingFolder}
      on:click={() => (folderModalOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class:btn-primary={Boolean(probeInfo?.is_empty_dir || folderMode !== "ghi_de")}
      class:btn-danger={Boolean(!probeInfo?.is_empty_dir && folderMode === "ghi_de")}
      disabled={switchingFolder}
      on:click={confirmSwitchProjectFolder}
    >
      {switchingFolder
        ? "Đang áp dụng..."
        : !probeInfo?.is_empty_dir && folderMode === "ghi_de"
          ? "Sao lưu & Tạo mới"
          : "Xác nhận"}
    </button>
  </div>
</Modal>

<style>
  .settings-screen {
    display: flex;
    flex-direction: column;
    width: 100%;
  }
  .loading {
    color: var(--c-muted);
  }
  .running-lock-banner {
    background: var(--c-warn-bg);
    color: var(--c-warn-dark);
    border: 1px solid #f4d39b;
    border-radius: 10px;
    padding: 10px 14px;
    margin-bottom: 14px;
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: var(--fs-label);
  }

  .tabs {
    display: flex;
    gap: 4px;
    margin-bottom: 16px;
    border-bottom: 1px solid var(--c-border);
    flex-wrap: wrap;
  }
  .tab {
    height: 40px;
    padding: 0 14px;
    background: transparent;
    color: var(--c-muted);
    border-radius: 8px 8px 0 0;
    border-bottom: 3px solid transparent;
    font-size: var(--fs-body);
    gap: 7px;
  }
  .tab:hover:not(.active) {
    color: var(--c-text);
    background: var(--c-surface-subtle);
  }
  .tab.active {
    color: var(--c-teal);
    border-bottom-color: var(--c-teal);
    font-weight: 600;
    background: var(--c-card);
  }

  .form {
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 20px;
  }
  .label {
    margin-top: 12px;
  }
  .label:first-child {
    margin-top: 0;
  }
  .pending-clear-tag {
    color: var(--c-danger);
    font-weight: 600;
    margin-left: 4px;
  }
  .field-group-title {
    font-size: var(--fs-label);
    font-weight: 600;
    color: var(--c-text);
    margin-top: 14px;
    margin-bottom: 4px;
  }
  .sub-label {
    display: block;
    font-size: var(--fs-hint);
    color: var(--c-muted);
    margin-bottom: 4px;
  }

  .input.full {
    width: 100%;
  }
  .input.narrow {
    width: 160px;
  }
  .inline {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .grow {
    flex: 1;
    min-width: 0;
  }
  .two-cols {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
    margin-top: 4px;
  }
  .three-cols {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 12px;
  }

  /* Bảng kiểm tra 5 mục ở Tab 1 */
  .checklist-panel {
    margin-top: 14px;
    border: 1px solid var(--c-border);
    border-radius: 10px;
    background: var(--c-surface-subtle);
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .checklist-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .checklist-title {
    font-size: var(--fs-label);
    font-weight: 700;
    color: var(--c-text);
  }
  .small-recheck {
    height: 26px;
    padding: 0 8px;
    font-size: 12px;
    gap: 4px;
  }
  .checklist-empty {
    font-size: var(--fs-hint);
    color: var(--c-muted);
  }
  .checklist-rows {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .checklist-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--fs-label);
    background: #fff;
    border: 1px solid var(--c-border);
    border-radius: 8px;
    padding: 7px 10px;
  }
  .checklist-row.ok {
    color: var(--c-green);
  }
  .checklist-row.bad {
    color: var(--c-danger);
    border-color: #edb8b3;
    background: #fffafa;
  }
  .checklist-msg {
    flex: 1;
    color: var(--c-text);
  }
  .checklist-row.bad .checklist-msg {
    color: var(--c-danger);
  }

  .key-input-wrap {
    position: relative;
    display: flex;
    align-items: center;
  }
  .key-input {
    padding-right: 38px;
  }
  .eye-btn {
    position: absolute;
    right: 4px;
    width: 28px;
    height: 28px;
    padding: 0;
    border-radius: 6px;
  }

  .hint {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    line-height: 1.45;
  }
  .warn-hint {
    font-size: var(--fs-hint);
    color: var(--c-warn-dark);
    background: var(--c-warn-bg);
    border: 1px solid #f4d39b;
    border-radius: 8px;
    padding: 7px 10px;
    margin-top: 4px;
  }
  .feedback-line {
    font-size: var(--fs-hint);
    color: var(--c-teal-dark);
    background: var(--c-teal-subtle);
    border: 1px solid rgba(14, 124, 134, 0.25);
    border-radius: 8px;
    padding: 7px 10px;
    margin-top: 4px;
  }
  .feedback-line.bad {
    color: var(--c-danger);
    background: var(--c-danger-bg);
    border-color: #edb8b3;
  }

  .check {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--fs-body);
    margin-top: 10px;
    cursor: pointer;
  }
  .active-check {
    font-weight: 600;
    margin-top: 2px;
  }
  .section-divider {
    height: 1px;
    background: var(--c-border);
    margin: 14px 0 6px;
  }

  .test-row {
    margin-top: 14px;
    flex-wrap: wrap;
  }
  .test-result {
    font-size: var(--fs-label);
    color: var(--c-muted);
    font-weight: 500;
  }
  .test-result.ok {
    color: var(--c-green);
  }
  .test-result.bad {
    color: var(--c-danger);
  }

  .validation-banner {
    margin-top: 12px;
    background: var(--c-danger-bg);
    color: var(--c-danger);
    border: 1px solid #edb8b3;
    border-radius: 10px;
    padding: 9px 14px;
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--fs-label);
    font-weight: 500;
  }

  /* Thanh Lưu thẳng hàng với thẻ .form, ẩn sạch .dirty-text khi chưa sửa */
  .savebar {
    position: sticky;
    bottom: 0;
    width: 100%;
    background: var(--c-card);
    border: 1px solid var(--c-border);
    box-shadow: 0 -4px 14px rgba(15, 23, 42, 0.05);
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    height: 60px;
    padding: 0 18px;
    margin-top: 14px;
    border-radius: var(--r-card);
  }
  .dirty-text {
    flex: 1;
    font-size: var(--fs-label);
    font-weight: 500;
    color: var(--c-warn-dark);
    opacity: 0;
    pointer-events: none;
    transition: opacity 0.15s ease;
  }
  .dirty-text.visible {
    opacity: 1;
    pointer-events: auto;
  }
  .dirty-text.saved {
    color: var(--c-green);
  }
  .dirty-text.invalid {
    opacity: 1;
    pointer-events: auto;
    color: var(--c-danger);
  }

  /* Modal chọn thư mục */
  .folder-modal-body {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .probed-path {
    padding: 8px 10px;
    border-radius: 8px;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    font-weight: 600;
    font-size: var(--fs-label);
    word-break: break-all;
  }
  .probe-err-box {
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
  .folder-modal-desc {
    font-size: var(--fs-label);
    color: var(--c-muted);
    line-height: 1.45;
  }
  .radio-option {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 10px 12px;
    border: 1px solid var(--c-border);
    border-radius: 8px;
    cursor: pointer;
  }
  .radio-option.selected {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .radio-option.danger-radio.selected {
    border-color: var(--c-danger);
    background: var(--c-danger-bg);
  }
  .radio-option input {
    margin-top: 3px;
  }
  .radio-title {
    font-weight: 600;
    font-size: var(--fs-label);
  }
  .radio-sub {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    margin-top: 2px;
  }

  /* ━━ Nguồn video: radio card ━━ */
  .source-radios {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px;
    margin-bottom: 6px;
  }
  .radio-card {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    padding: 12px;
    border: 1px solid var(--c-border);
    border-radius: 10px;
    background: var(--c-card);
    cursor: pointer;
    transition: border-color 0.15s, background 0.15s;
  }
  .radio-card:hover {
    border-color: var(--c-border-strong);
  }
  .radio-card.active {
    border-color: var(--c-teal);
    background: var(--c-teal-subtle);
  }
  .radio-card input {
    margin-top: 3px;
  }
  .radio-card strong {
    display: block;
    font-size: var(--fs-label);
  }
  .radio-card span {
    display: block;
    font-size: var(--fs-hint);
    color: var(--c-muted);
    margin-top: 2px;
  }

  /* ━━ muse2api nội bộ ━━ */
  .muse-panel {
    margin: 10px 0 16px;
    padding: 14px;
    border: 1px solid var(--c-border);
    border-radius: 12px;
    background: var(--c-surface-subtle);
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .muse-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    flex-wrap: wrap;
  }
  .muse-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--fs-body);
  }
  .muse-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .muse-badge {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-size: var(--fs-hint);
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 999px;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    color: var(--c-muted);
    white-space: nowrap;
  }
  .muse-badge.ok {
    background: var(--c-green-bg);
    border-color: color-mix(in srgb, var(--c-green) 35%, transparent);
    color: var(--c-green);
  }
  .muse-badge.mute {
    background: var(--c-surface-subtle);
    color: var(--c-muted);
  }
  .muse-accounts-summary-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 14px;
    background: var(--c-surface-subtle);
    border: 1px solid var(--c-border);
    border-radius: 8px;
    gap: 12px;
  }
  .muse-summary-info {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--c-text);
    font-weight: 600;
    font-size: 13px;
  }
  .jump-accounts-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 32px;
    padding: 0 12px;
    font-size: 12px;
  }
  .muse-stale-config {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    margin: 0 0 4px;
    padding: 8px 10px;
    border: 1px solid color-mix(in srgb, var(--c-warn) 45%, transparent);
    border-radius: 8px;
    background: var(--c-warn-bg);
    color: var(--c-warn-dark);
    font-size: var(--fs-hint);
    line-height: 1.45;
  }
  .muse-stale-config b {
    font-weight: 700;
  }
  .muse-config {
    display: grid;
    grid-template-columns: 1fr 170px;
    gap: 12px;
    align-items: end;
  }
  .muse-config-side {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .muse-config-side .input {
    width: 100%;
  }
  .muse-checkbox {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: var(--fs-hint);
    color: var(--c-muted);
  }
</style>
