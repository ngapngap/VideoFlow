<script lang="ts">
  // Màn hình Cài đặt (6.6, 6.6b): 5 tab, 100% tùy chọn hoạt động thật, kiểm tra hợp lệ nghiêm ngặt, 0 A11y warnings
  import { onDestroy, onMount } from "svelte";
  import {
    confirmStartEnabled,
    endpointStatus,
    formatApiError,
    maxAttemptsStore,
    project,
    resetProjectRunState,
    rowChangedTick,
    running,
    settingsDirty,
    showToast,
  } from "../lib/store";
  import { api, type ProbeResult, type Settings, type SettingsView } from "../lib/api";
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
  type MuseAccount = import("../lib/api").MuseAccount;
  type MuseQuotaSnapshot = import("../lib/api").MuseQuotaSnapshot;
  type MuseStatusView = import("../lib/api").MuseStatusView;

  let museStatus: import("../lib/api").MuseStatusView | null = null;
  let museAccounts: MuseAccount[] = [];
  let museLoading = false;
  let museActionBusy = false;
  let museErr = "";

  // Form thêm cookie: 3 ô = 3 cookie muse.ai (2 ô đầu bắt buộc, hatch_vml tùy chọn)
  let museNewLabel = "";
  let museCookieSess = ""; // hatch_sess (bắt buộc)
  let museCookieDevice = ""; // hatch_native_auth_device (bắt buộc)
  let museCookieVml = ""; // hatch_vml (tùy chọn)
  let museAllowDup = false;
  let museAddBusy = false;
  let museAddErr = "";
  let museDupPending = false; // lần bấm đầu trùng → hiện cảnh báo, lần 2 + allowDuplicate

  // Quota đang refresh từng account (loading trên nút)
  let museQuotaBusyId = "";
  let museQuotaErr: Record<string, string> = {};

  let museSidecarBusy = false;

  async function museRefresh() {
    museLoading = true;
    museErr = "";
    try {
      museStatus = await api.muse_sidecar_status();
      if (museStatus.sidecar.running) {
        try {
          const accs = await api.muse_list_accounts();
          museAccounts = accs.data ?? [];
        } catch {
          museAccounts = [];
        }
      } else {
        museAccounts = [];
      }
    } catch (e) {
      museErr = formatApiError(e);
    } finally {
      museLoading = false;
    }
  }

  async function museStart() {
    if (museSidecarBusy) return;
    museSidecarBusy = true;
    museErr = "";
    try {
      await api.muse_start_sidecar();
      showToast("success", "Đã khởi động engine VideoFlow Muse.");
      await museRefresh();
    } catch (e) {
      museErr = formatApiError(e);
      showToast("error", museErr);
    } finally {
      museSidecarBusy = false;
    }
  }

  async function museStop() {
    if (museSidecarBusy) return;
    museSidecarBusy = true;
    try {
      await api.muse_stop_sidecar();
      showToast("info", "Đã dừng engine VideoFlow Muse.");
      await museRefresh();
    } catch (e) {
      museErr = formatApiError(e);
    } finally {
      museSidecarBusy = false;
    }
  }

  /** Trạng thái hiển thị của account (theo accState của dashboard muse2api) */
  function museAccState(a: MuseAccount): { key: string; label: string; cls: string } {
    const now = Date.now() / 1000;
    const isQuota = /quota|credit|out of (usage|messages)|limit reached|exhaust/i.test(
      a.last_error || ""
    );
    if (!a.enabled) return { key: "disabled", label: "Tắt", cls: "mute" };
    if (a.status === "invalid") return { key: "invalid", label: "Lỗi phiên", cls: "err" };
    if (a.status === "cooling" && a.cooldown_until > now) {
      return isQuota
        ? { key: "quota", label: "Hết hạn mức", cls: "warn" }
        : { key: "cooling", label: "Tạm chờ", cls: "info" };
    }
    return { key: "active", label: "Hoạt động", cls: "ok" };
  }

  /** Sắp xếp: lỗi trước, rồi theo label — như renderAccounts của muse2api */
  function museSortedAccounts(list: MuseAccount[]): MuseAccount[] {
    const order: Record<string, number> = {
      quota: 0,
      invalid: 1,
      cooling: 2,
      active: 3,
      disabled: 4,
    };
    return [...list].sort(
      (a, b) =>
        (order[museAccState(a).key] ?? 9) - (order[museAccState(b).key] ?? 9) ||
        (a.label || a.id).localeCompare(b.label || b.id)
    );
  }

  $: museCounts = (() => {
    const c: Record<string, number> = {
      active: 0,
      quota: 0,
      cooling: 0,
      invalid: 0,
      disabled: 0,
    };
    for (const a of museAccounts) c[museAccState(a).key] += 1;
    return c;
  })();

  /** % đã dùng cho bar: ưu tiên weekly_used_pct, fallback tính từ dạng đếm */
  function museQuotaPct(q: MuseQuotaSnapshot | undefined): number | null {
    if (!q) return null;
    if (q.weekly_used_pct != null) return q.weekly_used_pct;
    if (q.messages_used != null && q.messages_limit) {
      return Math.round((q.messages_used / q.messages_limit) * 100);
    }
    return null;
  }

  function museQuotaBarClass(pct: number | null): string {
    if (pct == null) return "";
    if (pct >= 100) return "full";
    if (pct >= 80) return "high";
    return "";
  }

  function museTimeAgo(ts: number | undefined): string {
    if (!ts) return "";
    const diff = Date.now() / 1000 - ts;
    if (diff < 60) return "vừa xong";
    if (diff < 3600) return `${Math.floor(diff / 60)} phút trước`;
    if (diff < 86400) return `${Math.floor(diff / 3600)} giờ trước`;
    return `${Math.floor(diff / 86400)} ngày trước`;
  }

  function museCookieExpiry(a: MuseAccount): { txt: string; cls: string } | null {
    const exp = a.cookie_expires;
    if (!exp || Object.keys(exp).length === 0) return null;
    const ts =
      exp["hatch_vml"] != null
        ? exp["hatch_vml"]
        : Math.min(...Object.values(exp));
    const days = (ts - Date.now() / 1000) / 86400;
    if (days <= 0) return { txt: "Cookie hết hạn", cls: "err" };
    if (days < 1) return { txt: `Hết hạn trong ${Math.max(1, Math.floor(days * 24))}h`, cls: "err" };
    if (days < 3) return { txt: `Còn ${days.toFixed(1)} ngày`, cls: "warn" };
    return { txt: `Còn ${Math.round(days)} ngày`, cls: "info" };
  }

  // Svelte chỉ theo dõi dependency xuất hiện trực tiếp trong câu lệnh `$:` — liệt kê
  // trực tiếp 2 biến bắt buộc ở đây, không dựa vào hàm.
  $: museRequiredFilled =
    museCookieSess.trim().length > 0 && museCookieDevice.trim().length > 0;

  async function museAddCookie(allowDuplicate = false) {
    if (museAddBusy) return;
    museAddErr = "";
    const sess = museCookieSess.trim();
    const device = museCookieDevice.trim();
    const vml = museCookieVml.trim();
    if (!sess || !device) {
      museAddErr = "Thiếu cookie bắt buộc — cần cả hatch_sess và hatch_native_auth_device.";
      return;
    }
    const cookies: Record<string, string> = {
      hatch_sess: sess,
      hatch_native_auth_device: device,
    };
    if (vml) cookies.hatch_vml = vml;
    museAddBusy = true;
    try {
      await api.muse_add_cookie(museNewLabel.trim() || "acc", cookies, allowDuplicate);
      // Lưu thành công: tự xóa sạch cả 3 ô + nhãn + trạng thái cảnh báo
      museCookieSess = "";
      museCookieDevice = "";
      museCookieVml = "";
      museNewLabel = "";
      museAllowDup = false;
      museDupPending = false;
      museAddErr = "";
      showToast("success", "Đã thêm cookie vào pool.");
      await museRefresh();
    } catch (e) {
      const msg = formatApiError(e);
      if (msg.includes("hatch_sess already belongs")) {
        // Trùng session: lần đầu cảnh báo, lần 2 user phải tick cho phép
        if (!allowDuplicate && !museDupPending) {
          museDupPending = true;
          museAddErr =
            "Cookie trùng với một account đang có. Kiểm tra lại — nếu chắc chắn, tick 'Cho phép trùng session' và bấm Thêm lần nữa.";
        } else {
          museAddErr = msg;
        }
      } else {
        museAddErr = msg;
      }
    } finally {
      museAddBusy = false;
    }
  }

  async function museToggle(a: MuseAccount) {
    if (museActionBusy) return;
    museActionBusy = true;
    try {
      await api.muse_update_account(a.id, !a.enabled);
      await museRefresh();
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      museActionBusy = false;
    }
  }

  async function museDelete(a: MuseAccount) {
    if (museActionBusy) return;
    if (!window.confirm(`Xóa account "${a.label || a.id}" khỏi pool?`)) return;
    museActionBusy = true;
    try {
      await api.muse_delete_account(a.id);
      showToast("success", "Đã xóa account.");
      await museRefresh();
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      museActionBusy = false;
    }
  }

  async function museRenew(a: MuseAccount) {
    if (museActionBusy) return;
    museActionBusy = true;
    try {
      await api.muse_renew_account(a.id);
      showToast("success", "Đã gia hạn phiên (renew session).");
      await museRefresh();
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      museActionBusy = false;
    }
  }

  async function museRefreshQuota(a: MuseAccount, force: boolean) {
    if (museQuotaBusyId) return;
    museQuotaBusyId = a.id;
    delete museQuotaErr[a.id];
    museQuotaErr = { ...museQuotaErr };
    try {
      const res = await api.muse_account_quota(a.id, force);
      // Snapshot mới lưu vào meta của account trong danh sách hiện có
      const idx = museAccounts.findIndex((x) => x.id === a.id);
      if (idx >= 0) {
        museAccounts[idx] = { ...museAccounts[idx], meta: { ...museAccounts[idx].meta, quota: res.quota } };
        museAccounts = [...museAccounts];
      }
    } catch (e) {
      museQuotaErr = { ...museQuotaErr, [a.id]: formatApiError(e) };
    } finally {
      museQuotaBusyId = "";
    }
  }

  async function museDownloadExtension() {
    try {
      await api.muse_download_extension();
      showToast("success", "Đã tải extension import cookie — cài vào Chrome/Edge theo chế độ developer.");
    } catch (e) {
      showToast("error", formatApiError(e));
    }
  }

  // Khi mở tab Video lần đầu: nạp status sidecar (chưa spawn — chỉ đọc trạng thái)
  let museLoadedOnce = false;
  function onSelectTabVideo() {
    selectTab("video");
    if (!museLoadedOnce) {
      museLoadedOnce = true;
      museRefresh();
    }
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
      imageConfigured: hasImageKey,
      videoConfigured: hasVideoKey,
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
      if (imageKeyInput.trim()) {
        await api.set_api_key("anh", imageKeyInput.trim());
        imageKeyInput = "";
      } else if (pendingClearImageKey) {
        await api.set_api_key("anh", "");
      }

      if (videoKeyInput.trim()) {
        await api.set_api_key("video", videoKeyInput.trim());
        videoKeyInput = "";
      } else if (pendingClearVideoKey) {
        await api.set_api_key("video", "");
      }

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
      };

      await api.save_settings(payload);
      const view = await api.get_settings();
      applySettingsView(view);
      hideAllKeyVisibility();
      dirty = false;
      saveMsg = "Đã lưu cài đặt thành công";
      showToast("success", "Đã lưu cấu hình hệ thống.");
      setTimeout(() => {
        if (!dirty) saveMsg = "";
      }, 3000);
    } catch (e) {
      showToast("error", formatApiError(e));
    } finally {
      saving = false;
    }
  }

  async function cancel() {
    if ($running) return;
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
            VideoFlow Muse (chung engine với video). Cookie quản lý ở tab
            <b>Endpoint video</b> → panel "VideoFlow Muse (nội bộ)"; engine tự khởi động khi chạy.
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
        {/if}
        <div class="hint">Kiểm tra trực tiếp Base URL và API key hiện có trên biểu mẫu.</div>

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
              {#if museStatus?.sidecar.running}
                <span class="muse-badge ok">Đang chạy{museStatus.sidecar.version ? ` · v${museStatus.sidecar.version}` : ""}</span>
              {:else}
                <span class="muse-badge mute">Đã dừng</span>
              {/if}
            </div>
            <div class="muse-actions">
              <button
                type="button"
                class="btn-secondary"
                disabled={museSidecarBusy}
                on:click={museStart}
              >
                <Icons name="play" size={14} />
                <span>{museSidecarBusy ? "Đang khởi động..." : "Khởi động"}</span>
              </button>
              <button
                type="button"
                class="btn-secondary"
                disabled={museSidecarBusy || !museStatus?.sidecar.running}
                on:click={museStop}
              >
                <Icons name="stop" size={14} />
                <span>Dừng</span>
              </button>
              <button type="button" class="btn-secondary" disabled={museLoading} on:click={museRefresh}>
                <Icons name="refresh" size={14} />
                <span>{museLoading ? "Đang tải..." : "Làm mới"}</span>
              </button>
            </div>
          </div>

          <div class="hint">
            Sidecar tự dò Microsoft Edge/Chrome trên máy và điều khiển headless. Cookie muse.ai lưu
            trong thư mục dữ liệu app và tự gia hạn phiên mỗi 10 phút.
            {#if museStatus?.chromium_path}
              <br />Trình duyệt: <code>{museStatus.chromium_path}</code>
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

          {#if museStatus?.sidecar.running}
            <!-- Tổng quan pool -->
            <div class="muse-summary">
              {#if museAccounts.length === 0}
                <span class="muse-badge mute">Chưa có tài khoản — thêm cookie bên dưới</span>
              {:else}
                {#if museCounts.active}<span class="muse-badge ok">Hoạt động <b>{museCounts.active}</b></span>{/if}
                {#if museCounts.quota}<span class="muse-badge warn">Hết hạn mức <b>{museCounts.quota}</b></span>{/if}
                {#if museCounts.cooling}<span class="muse-badge info">Tạm chờ <b>{museCounts.cooling}</b></span>{/if}
                {#if museCounts.invalid}<span class="muse-badge err">Lỗi phiên <b>{museCounts.invalid}</b></span>{/if}
                {#if museCounts.disabled}<span class="muse-badge mute">Tắt <b>{museCounts.disabled}</b></span>{/if}
              {/if}
            </div>

            <!-- Card từng tài khoản: trạng thái + hạn mức + cookie expiry + thao tác -->
            {#each museSortedAccounts(museAccounts) as a (a.id)}
              {@const st = museAccState(a)}
              {@const q = a.meta?.quota}
              {@const pct = museQuotaPct(q)}
              {@const exp = museCookieExpiry(a)}
              <article class="muse-acc s-{st.key}">
                <div class="muse-acc-top">
                  <div class="grow">
                    <div class="muse-acc-name" title={a.label || a.id}>{a.label || a.id}</div>
                    <div class="muse-acc-id">{a.id}</div>
                  </div>
                  <span class="muse-badge {st.cls}">{st.label}</span>
                  <label class="muse-switch" title={a.enabled ? "Đang bật — bấm để tắt" : "Đang tắt — bấm để bật"}>
                    <input
                      type="checkbox"
                      checked={a.enabled}
                      disabled={museActionBusy || $running}
                      on:change={() => museToggle(a)}
                    />
                    <span></span>
                  </label>
                </div>

                <!-- Khối hạn mức: quota hàng tuần + quota bổ sung -->
                {#if q && (q.found || pct != null || q.plan)}
                  <div class="muse-quota">
                    <div class="qrail">
                      <b class={museQuotaBarClass(pct)} style="width:{pct != null ? Math.min(100, pct) : 0}%"></b>
                    </div>
                    <div class="qmeta">
                      <span>
                        {q.plan ? q.plan.replace(/\s*plan\s*$/i, "") : "—"}{pct != null ? ` · ${pct}% đã dùng` : ""}
                      </span>
                      <span>
                        reset {q.resets_at || "—"}{q.checked_at ? ` · ${museTimeAgo(q.checked_at)}` : ""}
                      </span>
                    </div>
                    {#if q.extra_left}
                      <div class="qextra">
                        Quota bổ sung còn: <b>{q.extra_left}</b>
                      </div>
                    {/if}
                    {#if q.error}
                      <div class="qerr selectable">{q.error}</div>
                    {/if}
                  </div>
                {:else}
                  <div class="muse-quota empty">Chưa có dữ liệu hạn mức — bấm "Đọc hạn mức".</div>
                {/if}

                {#if museQuotaErr[a.id]}
                  <div class="feedback-line bad selectable">{museQuotaErr[a.id]}</div>
                {/if}

                <div class="muse-acc-meta">
                  {#if exp}
                    <span class="muse-badge plain {exp.cls}">{exp.txt}</span>
                  {/if}
                  <span class="muse-stat">Thành công {a.success_count}</span>
                  {#if a.fail_count > 0}
                    <span class="muse-stat bad">Lỗi {a.fail_count}</span>
                  {/if}
                  {#if a.last_used_at > 0}
                    <span class="muse-stat">Dùng gần nhất {museTimeAgo(a.last_used_at)}</span>
                  {/if}
                </div>

                {#if a.last_error}
                  <div class="muse-last-error selectable" title="last_error">{a.last_error}</div>
                {/if}

                <div class="muse-acc-actions">
                  <button
                    type="button"
                    class="btn-secondary"
                    disabled={museQuotaBusyId === a.id || $running}
                    on:click={() => museRefreshQuota(a, true)}
                    title="Đọc lại hạn mức từ trang Settings → Usage của muse.ai (có thể bị từ chối khi account đang chạy)"
                  >
                    <Icons name="refresh" size={14} />
                    <span>{museQuotaBusyId === a.id ? "Đang đọc..." : "Đọc hạn mức"}</span>
                  </button>
                  <button
                    type="button"
                    class="btn-secondary"
                    disabled={museActionBusy || $running}
                    on:click={() => museRenew(a)}
                    title="Gia hạn phiên muse.ai (GET /api/session) và cập nhật cookie"
                  >
                    <Icons name="check" size={14} />
                    <span>Gia hạn phiên</span>
                  </button>
                  <button
                    type="button"
                    class="btn-secondary"
                    disabled={museActionBusy || $running}
                    on:click={() => museDelete(a)}
                  >
                    <Icons name="trash" size={14} />
                    <span>Xóa</span>
                  </button>
                </div>
              </article>
            {/each}
          {:else}
            <div class="muse-offline">
              Engine chưa chạy — bấm "Khởi động" ở trên (lần đầu 30–60 giây vì phải khởi động Edge), hoặc thêm cookie
              ngay bên dưới: engine sẽ tự khởi động khi bấm "Thêm cookie".
            </div>
          {/if}

          <!-- Thêm cookie: LUÔN hiển thị — không phụ thuộc trạng thái engine -->
          <div class="muse-add">
            <label class="label" for="muse-add-label">Tên tài khoản (nhãn)</label>
            <input
              id="muse-add-label"
              class="input full"
              bind:value={museNewLabel}
              disabled={museAddBusy || $running}
              placeholder="Ví dụ: tài khoản chính"
              autocomplete="off"
            />
            <div class="label" style="margin-top:2px">
              Cookie muse.ai — nhập 3 cookie lấy từ F12 → Application → Cookies
            </div>

            <label class="label" for="muse-cookie-sess">
              <code>hatch_sess</code> <span class="muse-req">* bắt buộc</span>
            </label>
            <input
              id="muse-cookie-sess"
              class="input full muse-cookie-input"
              bind:value={museCookieSess}
              disabled={museAddBusy || $running}
              placeholder="Dán giá trị cookie hatch_sess"
              autocomplete="off"
              spellcheck="false"
            />

            <label class="label" for="muse-cookie-device">
              <code>hatch_native_auth_device</code> <span class="muse-req">* bắt buộc</span>
            </label>
            <input
              id="muse-cookie-device"
              class="input full muse-cookie-input"
              bind:value={museCookieDevice}
              disabled={museAddBusy || $running}
              placeholder="Dán giá trị cookie hatch_native_auth_device"
              autocomplete="off"
              spellcheck="false"
            />

            <label class="label" for="muse-cookie-vml">
              <code>hatch_vml</code> <span class="muse-opt">(tùy chọn)</span>
            </label>
            <input
              id="muse-cookie-vml"
              class="input full muse-cookie-input"
              bind:value={museCookieVml}
              disabled={museAddBusy || $running}
              placeholder="Dán giá trị cookie hatch_vml nếu có"
              autocomplete="off"
              spellcheck="false"
            />

            <div class="hint">
              Đăng nhập muse.ai trên Edge/Chrome → F12 → tab <b>Application</b> → <b>Cookies</b> →
              <code>https://muse.ai</code> → copy giá trị từng cookie vào 3 ô trên
              (<code>hatch_vml</code> có thể không có — bỏ trống). Engine chưa chạy thì lần
              "Thêm cookie" đầu tiên sẽ tự khởi động engine (30–60 giây vì phải khởi động Edge).
            </div>
            <div class="inline muse-add-actions">
              <button
                type="button"
                class="btn-primary"
                disabled={museAddBusy || $running || !museRequiredFilled}
                on:click={() => museAddCookie(museAllowDup)}
              >
                <Icons name="check" size={14} />
                <span>{museAddBusy ? "Đang thêm..." : "Thêm cookie"}</span>
              </button>
              <button type="button" class="btn-secondary" disabled={$running} on:click={museDownloadExtension}>
                <Icons name="link" size={14} />
                <span>Tải extension import cookie</span>
              </button>
            </div>
            {#if museDupPending}
              <label class="muse-dup-allow">
                <input type="checkbox" bind:checked={museAllowDup} />
                <span>Cho phép trùng session (chỉ khi chắc chắn đây là cookie mới hợp lệ)</span>
              </label>
            {/if}
            {#if museAddErr}
              <div class="feedback-line bad selectable">{museAddErr}</div>
            {/if}
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
    max-width: 720px;
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
    max-width: 720px;
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

  /* Thanh Lưu thẳng hàng max-width 720px với thẻ .form, ẩn sạch .dirty-text khi chưa sửa */
  .savebar {
    position: sticky;
    bottom: 0;
    max-width: 720px;
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
  .muse-badge.warn {
    background: var(--c-warn-bg);
    border-color: color-mix(in srgb, var(--c-warn) 35%, transparent);
    color: var(--c-warn-dark);
  }
  .muse-badge.err {
    background: var(--c-danger-bg);
    border-color: color-mix(in srgb, var(--c-danger) 35%, transparent);
    color: var(--c-danger);
  }
  .muse-badge.info {
    background: var(--c-teal-light);
    border-color: color-mix(in srgb, var(--c-teal) 35%, transparent);
    color: var(--c-teal);
  }
  .muse-badge.mute {
    background: var(--c-surface-subtle);
    color: var(--c-muted);
  }
  .muse-badge.plain {
    font-weight: 500;
  }
  .muse-badge b {
    font-weight: 700;
  }
  .muse-summary {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .muse-acc {
    border: 1px solid var(--c-border);
    border-left: 4px solid var(--c-green);
    border-radius: 10px;
    background: var(--c-card);
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .muse-acc.s-quota {
    border-left-color: var(--c-warn);
    background: linear-gradient(180deg, var(--c-warn-bg), var(--c-card) 90px);
  }
  .muse-acc.s-invalid {
    border-left-color: var(--c-danger);
  }
  .muse-acc.s-cooling {
    border-left-color: var(--c-teal);
  }
  .muse-acc.s-disabled {
    border-left-color: var(--c-border-strong);
    opacity: 0.85;
  }
  .muse-acc-top {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .muse-acc-name {
    font-weight: 600;
    font-size: var(--fs-label);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .muse-acc-id {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    font-family: var(--font-mono, monospace);
  }
  .muse-switch {
    position: relative;
    display: inline-flex;
    align-items: center;
    cursor: pointer;
  }
  .muse-switch input {
    position: absolute;
    opacity: 0;
    width: 0;
    height: 0;
  }
  .muse-switch span {
    width: 34px;
    height: 20px;
    border-radius: 999px;
    background: var(--c-border-strong);
    position: relative;
    transition: background 0.15s;
  }
  .muse-switch span::after {
    content: "";
    position: absolute;
    top: 2px;
    left: 2px;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #fff;
    transition: transform 0.15s;
  }
  .muse-switch input:checked + span {
    background: var(--c-green);
  }
  .muse-switch input:checked + span::after {
    transform: translateX(14px);
  }
  .muse-switch input:disabled + span {
    opacity: 0.5;
    cursor: not-allowed;
  }
  /* Khối hạn mức (quota hàng tuần + quota bổ sung) */
  .muse-quota {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .muse-quota.empty {
    font-size: var(--fs-hint);
    color: var(--c-muted);
    font-style: italic;
  }
  .muse-quota .qrail {
    height: 7px;
    border-radius: 4px;
    background: var(--c-surface-subtle);
    overflow: hidden;
    border: 1px solid var(--c-border);
  }
  .muse-quota .qrail b {
    display: block;
    height: 100%;
    background: var(--c-teal);
    border-radius: 4px;
    transition: width 0.3s;
  }
  .muse-quota .qrail b.high {
    background: var(--c-warn);
  }
  .muse-quota .qrail b.full {
    background: var(--c-danger);
  }
  .muse-quota .qmeta {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    font-size: var(--fs-hint);
    color: var(--c-muted);
    overflow: hidden;
  }
  .muse-quota .qmeta span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .muse-quota .qextra {
    font-size: var(--fs-hint);
    color: var(--c-text);
  }
  .muse-quota .qextra b {
    color: var(--c-teal);
  }
  .muse-quota .qerr {
    font-size: var(--fs-hint);
    color: var(--c-warn-dark);
  }
  .muse-acc-meta {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    align-items: center;
    font-size: var(--fs-hint);
    color: var(--c-muted);
  }
  .muse-stat.bad {
    color: var(--c-danger);
  }
  .muse-last-error {
    font-size: var(--fs-hint);
    color: var(--c-danger);
    background: var(--c-danger-bg);
    border-radius: 6px;
    padding: 6px 8px;
    overflow-wrap: anywhere;
  }
  .muse-acc-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .muse-add {
    border-top: 1px dashed var(--c-border-strong);
    padding-top: 10px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .muse-textarea {
    resize: vertical;
    font-family: var(--font-mono, monospace);
    font-size: var(--fs-hint);
    min-height: 72px;
  }
  .muse-add-actions {
    margin-top: 4px;
  }
  .muse-dup-allow {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--fs-hint);
    color: var(--c-warn-dark);
    background: var(--c-warn-bg);
    border-radius: 6px;
    padding: 6px 8px;
  }
  .muse-cookie-input {
    font-family: var(--font-mono, monospace);
    font-size: var(--fs-hint);
  }
  .muse-req {
    color: var(--c-danger);
    font-weight: 600;
  }
  .muse-opt {
    color: var(--c-muted);
    font-weight: 400;
  }
  .muse-offline {
    font-size: var(--fs-label);
    color: var(--c-muted);
    padding: 10px 0;
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
