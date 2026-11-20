<script lang="ts">
  import { createEventDispatcher } from "svelte";
  import { api, type MuseAccount, type MuseQuotaSnapshot, type ProxyItem } from "./api";
  import { isAccountActionBlocked, shouldSyncSelectedProxy } from "./asyncGuard";
  import Icons from "./Icons.svelte";
  import Modal from "./Modal.svelte";
  import { formatApiError, showToast } from "./store";

  export let account: MuseAccount;
  export let proxies: ProxyItem[] = [];
  export let allAccounts: MuseAccount[] = [];
  export let running: boolean = false;

  const dispatch = createEventDispatcher<{
    refresh: { id: string };
    deleted: { id: string };
    updated: { id: string };
    openLogin: { id: string };
    openView: { id: string };
  }>();

  let actionBusy = false;
  let quotaBusy = false;
  let quotaErr = "";
  let prevPropProxyId = account.proxy_id || "";
  let selectedProxyId = prevPropProxyId;
  let proxySaving = false;
  let deleteConfirmOpen = false;

  $: {
    const nextPropProxyId = account.proxy_id || "";
    if (shouldSyncSelectedProxy(prevPropProxyId, nextPropProxyId, proxySaving)) {
      prevPropProxyId = nextPropProxyId;
      selectedProxyId = nextPropProxyId;
    }
  }

  $: actionBlocked = isAccountActionBlocked({
    actionBusy,
    proxySaving,
    running,
    accountBusy: Boolean(account.busy),
  });
  $: proxyOrQuotaBlocked = isAccountActionBlocked({
    actionBusy,
    proxySaving,
    quotaBusy,
    running,
    accountBusy: Boolean(account.busy),
  });

  function formatProxyOptionLabel(p: ProxyItem): string {
    const dot =
      p.last_status === "alive" ? "🟢" : p.last_status === "dead" ? "🔴" : "⚪";
    const ipPart = p.exit_ip ? p.exit_ip : `${p.host}:${p.port}`;
    const latPart =
      p.latency_ms != null
        ? `${p.latency_ms}ms`
        : p.last_status === "dead"
          ? "Lỗi"
          : "Chưa test";
    return `${dot} ${p.name} (${p.kind.toUpperCase()} · ${ipPart} · ${latPart})`;
  }

  $: currentProxyObj = proxies.find((p) => p.id === selectedProxyId) || null;

  $: antiLinkConflicts = (() => {
    if (!selectedProxyId) return [] as string[];
    const curIp = (currentProxyObj?.exit_ip || account.proxy_exit_ip || "").trim();
    const names: string[] = [];
    for (const other of allAccounts) {
      if (other.id === account.id) continue;
      const sameProxy = Boolean(other.proxy_id && other.proxy_id === selectedProxyId);
      const otherIp = (other.proxy_exit_ip || "").trim();
      const sameIp = Boolean(curIp && otherIp && curIp === otherIp);
      if (sameProxy || sameIp) {
        names.push(other.label || other.id);
      }
    }
    if (names.length === 0 && currentProxyObj) {
      for (const assignedId of currentProxyObj.assigned_account_ids) {
        if (assignedId !== account.id) {
          const found = allAccounts.find((a) => a.id === assignedId);
          names.push(found?.label || assignedId);
        }
      }
    }
    return Array.from(new Set(names));
  })();

  function hasPositiveExtra(extra: string | null | undefined): boolean {
    if (!extra || !extra.trim()) return false;
    return /[1-9]/.test(extra);
  }

  function isResetDatePassed(resetsAt: string | null | undefined): boolean {
    if (!resetsAt || !resetsAt.trim()) return false;
    const raw = resetsAt.trim();
    const parsed = Date.parse(raw);
    if (!Number.isNaN(parsed)) {
      return parsed <= Date.now();
    }
    return false;
  }

  function isQuotaExhausted(q: MuseQuotaSnapshot | undefined): boolean {
    const usedPct = museQuotaPct(q);
    if (usedPct == null || usedPct < 100) return false;
    if (hasPositiveExtra(q?.extra_left)) return false;
    if (isResetDatePassed(q?.resets_at)) return false;
    return true;
  }

  function museAccState(a: MuseAccount): { key: string; label: string; cls: string } {
    const now = Date.now() / 1000;
    const isQuota = /quota|credit|out of (usage|messages)|limit reached|exhaust/i.test(
      a.last_error || ""
    );
    if (!a.enabled) return { key: "disabled", label: "Tắt", cls: "mute" };
    if (a.status === "video_locked") return { key: "video_locked", label: "Khóa video", cls: "err" };
    if (a.status === "invalid") return { key: "invalid", label: "Lỗi phiên", cls: "err" };
    if (a.status === "cooling" && a.cooldown_until > now) {
      return isQuota
        ? { key: "quota", label: "Hết hạn mức", cls: "warn" }
        : { key: "cooling", label: "Tạm chờ", cls: "info" };
    }
    if (isQuotaExhausted(a.meta?.quota)) {
      return { key: "quota", label: "Hết hạn mức", cls: "warn" };
    }
    return { key: "active", label: "Hoạt động", cls: "ok" };
  }

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

  async function handleToggle(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      input.checked = account.enabled;
      return;
    }
    actionBusy = true;
    try {
      await api.muse_update_account(account.id, !account.enabled);
      dispatch("updated", { id: account.id });
    } catch (err) {
      input.checked = account.enabled;
      showToast("error", `Lỗi cập nhật: ${formatApiError(err)}`);
    } finally {
      actionBusy = false;
    }
  }

  async function handleProxySelectChange(e: Event) {
    const selectEl = e.currentTarget as HTMLSelectElement;
    const nextPid = selectEl.value;
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        quotaBusy,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      selectEl.value = account.proxy_id || "";
      return;
    }
    const prevPid = account.proxy_id || "";
    selectedProxyId = nextPid;
    proxySaving = true;
    try {
      const res = await api.muse_update_account(account.id, undefined, undefined, undefined, nextPid);
      if (res?.account) {
        account = res.account;
        prevPropProxyId = account.proxy_id || "";
        selectedProxyId = prevPropProxyId;
      }
      const pxName = proxies.find((p) => p.id === nextPid)?.name;
      showToast(
        "success",
        nextPid
          ? `Đã gán proxy "${pxName || nextPid}" và khởi động lại kết nối cho tài khoản.`
          : "Đã chuyển tài khoản về kết nối Trực tiếp."
      );
      dispatch("updated", { id: account.id });
    } catch (err) {
      selectedProxyId = prevPid;
      selectEl.value = prevPid;
      showToast("error", `Lỗi đổi proxy: ${formatApiError(err)}`);
    } finally {
      proxySaving = false;
    }
  }

  async function handleRefreshQuota() {
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        quotaBusy,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      return;
    }
    quotaBusy = true;
    quotaErr = "";
    try {
      const res = await api.muse_account_quota(account.id, true);
      if (res?.quota?.error || res?.quota?.found === false) {
        quotaErr = res?.quota?.error || "Không đọc được hạn mức từ trang Settings → Usage của muse.ai.";
        showToast("warn", `Không đọc được hạn mức: ${quotaErr}`);
      } else {
        showToast("success", "Đã làm mới hạn mức.");
      }
      dispatch("refresh", { id: account.id });
    } catch (e) {
      quotaErr = formatApiError(e);
      showToast("error", `Lỗi đọc hạn mức: ${quotaErr}`);
    } finally {
      quotaBusy = false;
    }
  }

  async function handleRenew() {
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      return;
    }
    actionBusy = true;
    try {
      const res = await api.muse_renew_account(account.id);
      if (res?.ok === false) {
        showToast("error", `Gia hạn phiên thất bại: ${res.error || "Không rõ nguyên nhân"}`);
      } else {
        showToast("success", "Đã gia hạn phiên thành công.");
      }
      dispatch("updated", { id: account.id });
    } catch (e) {
      showToast("error", `Lỗi gia hạn: ${formatApiError(e)}`);
    } finally {
      actionBusy = false;
    }
  }

  async function handleReset() {
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      return;
    }
    actionBusy = true;
    try {
      await api.muse_reset_account(account.id);
      showToast("success", "Đã đặt lại trạng thái tài khoản về hoạt động.");
      dispatch("updated", { id: account.id });
    } catch (e) {
      showToast("error", `Lỗi đặt lại tài khoản: ${formatApiError(e)}`);
    } finally {
      actionBusy = false;
    }
  }

  async function handleOpenLogin() {
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      return;
    }
    actionBusy = true;
    try {
      const res = await api.muse_open_login(account.id);
      showToast("info", res.message || "Đang mở trình duyệt đăng nhập...");
      dispatch("openLogin", { id: account.id });
    } catch (e) {
      showToast("error", `Không thể mở trình duyệt: ${formatApiError(e)}`);
    } finally {
      actionBusy = false;
    }
  }

  async function handleOpenView() {
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      return;
    }
    actionBusy = true;
    try {
      const res = await api.muse_open_view(account.id);
      showToast("info", res.message || "Đang mở trình duyệt xem hội thoại...");
      dispatch("openView", { id: account.id });
    } catch (e) {
      showToast("error", `Không thể mở xem web: ${formatApiError(e)}`);
    } finally {
      actionBusy = false;
    }
  }

  function requestDelete() {
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      return;
    }
    deleteConfirmOpen = true;
  }

  async function confirmDelete() {
    if (
      isAccountActionBlocked({
        actionBusy,
        proxySaving,
        running,
        accountBusy: Boolean(account.busy),
      })
    ) {
      return;
    }
    actionBusy = true;
    try {
      await api.muse_delete_account(account.id);
      deleteConfirmOpen = false;
      showToast("success", "Đã xóa tài khoản.");
      dispatch("deleted", { id: account.id });
    } catch (e) {
      showToast("error", `Lỗi xóa tài khoản: ${formatApiError(e)}`);
    } finally {
      actionBusy = false;
    }
  }

  $: st = museAccState(account);
  $: q = account.meta?.quota;
  $: pct = museQuotaPct(q);
  $: exp = museCookieExpiry(account);
</script>

<article class="acc-card s-{st.key}">
  <!-- Top header -->
  <div class="acc-header">
    <div class="acc-identity">
      <div class="acc-title-line">
        <h3 class="acc-name" title={account.label || account.id}>{account.label || account.id}</h3>
        <span class="acc-badge {st.cls}">{st.label}</span>
      </div>
      <div class="acc-id">{account.id}</div>
    </div>

    <label class="acc-switch" title={account.enabled ? "Đang bật — bấm để tắt" : "Đang tắt — bấm để bật"}>
      <input
        type="checkbox"
        checked={account.enabled}
        disabled={actionBlocked}
        aria-label={account.enabled
          ? `Tắt tài khoản ${account.label || account.id}`
          : `Bật tài khoản ${account.label || account.id}`}
        on:change={handleToggle}
      />
      <span class="slider"></span>
    </label>
  </div>

  <!-- Quota section -->
  {#if q && (q.found || pct != null || q.plan || q.error)}
    <div class="acc-quota">
      <div class="qrail">
        <b class={museQuotaBarClass(pct)} style="width:{pct != null ? Math.min(100, Math.max(0, pct)) : 0}%"></b>
      </div>
      <div class="qmeta">
        <span>
          {q.plan ? q.plan.replace(/\s*plan\s*$/i, "") : "Gói chuẩn"}{pct != null ? ` · ${pct}% đã dùng` : ""}
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
    <div class="acc-quota empty">Chưa có dữ liệu hạn mức — bấm "Làm mới quota" để đọc.</div>
  {/if}

  {#if quotaErr}
    <div class="acc-error-line selectable">{quotaErr}</div>
  {/if}

  <!-- Proxy dropdown row (#30 proxy_id assignment + lifecycle lock) -->
  <div
    class="acc-proxy-row"
    title={account.busy
      ? "Tài khoản đang bận xử lý hoặc đang mở trình duyệt — không thể đổi proxy lúc này"
      : "Chọn proxy đã lưu cho tài khoản này"}
  >
    <div class="proxy-icon-wrap" title="Proxy của tài khoản">
      <Icons name="globe" size={14} />
    </div>
    <select
      class="proxy-select"
      value={selectedProxyId}
      aria-label={`Chọn proxy cho tài khoản ${account.label || account.id}`}
      on:change={handleProxySelectChange}
      disabled={proxyOrQuotaBlocked}
    >
      <option value="">Trực tiếp (không dùng proxy)</option>
      {#each proxies as p (p.id)}
        <option value={p.id}>{formatProxyOptionLabel(p)}</option>
      {/each}
    </select>
    {#if proxySaving}
      <span class="proxy-saving-tag">Đang áp dụng...</span>
    {:else if account.busy}
      <span class="proxy-busy-tag">Đang bận</span>
    {/if}
  </div>

  {#if antiLinkConflicts.length > 0}
    <div class="acc-anti-link-warn" role="alert">
      <Icons name="alert-triangle" size={14} />
      <span>
        ⚠️ Proxy / IP đầu ra này đang được dùng bởi tài khoản <b>{antiLinkConflicts.join(", ")}</b>. Dùng chung IP giữa nhiều tài khoản có nguy cơ bị nối dấu vết với nhau.
      </span>
    </div>
  {/if}

  <!-- Meta indicators -->
  <div class="acc-meta">
    {#if exp}
      <span class="acc-meta-item plain {exp.cls}">{exp.txt}</span>
    {/if}
    <span class="acc-meta-item">Thành công {account.success_count}</span>
    {#if account.fail_count > 0}
      <span class="acc-meta-item bad">Lỗi {account.fail_count}</span>
    {/if}
    {#if account.last_used_at > 0}
      <span class="acc-meta-item">Dùng {museTimeAgo(account.last_used_at)}</span>
    {/if}
  </div>

  {#if account.last_error}
    <div class="acc-last-error selectable" title="Chi tiết lỗi">{account.last_error}</div>
  {/if}

  <!-- Actions toolbar -->
  <div class="acc-actions">
    <!-- P1-5: Login button opens headed browser with profile -->
    <button
      type="button"
      class="btn-action primary"
      disabled={actionBlocked}
      on:click={handleOpenLogin}
      title="Mở trình duyệt có giao diện để đăng nhập tài khoản Muse và tự động bắt token"
    >
      <Icons name="log-in" size={14} />
      <span>Đăng nhập</span>
    </button>

    <!-- P1-5: View chat history on web -->
    <button
      type="button"
      class="btn-action"
      disabled={actionBlocked}
      on:click={handleOpenView}
      title="Mở trình duyệt xem lịch sử hội thoại trên web của tài khoản này"
    >
      <Icons name="eye" size={14} />
      <span>Xem trên web</span>
    </button>

    <button
      type="button"
      class="btn-action"
      disabled={proxyOrQuotaBlocked}
      on:click={handleRefreshQuota}
      title="Đọc lại hạn mức từ trang Settings → Usage của muse.ai"
    >
      <Icons name="refresh" size={14} />
      <span>{quotaBusy ? "Đang đọc..." : "Làm mới quota"}</span>
    </button>

    <button
      type="button"
      class="btn-action"
      disabled={actionBlocked}
      on:click={handleRenew}
      title="Gia hạn phiên muse.ai và làm mới cookie"
    >
      <Icons name="check" size={14} />
      <span>Gia hạn</span>
    </button>

    {#if account.status === "video_locked" || account.status === "cooling" || account.status === "invalid"}
      <button
        type="button"
        class="btn-action"
        disabled={actionBlocked}
        on:click={handleReset}
        title="Đặt lại trạng thái khóa/tạm chờ về hoạt động để thử lại"
      >
        <Icons name="redo" size={14} />
        <span>Đặt lại</span>
      </button>
    {/if}

    <button
      type="button"
      class="btn-action danger"
      disabled={actionBlocked}
      on:click={requestDelete}
      title="Xóa tài khoản khỏi danh sách"
    >
      <Icons name="trash" size={14} />
      <span>Xóa</span>
    </button>
  </div>
</article>

<Modal
  open={deleteConfirmOpen}
  title="Xác nhận xóa tài khoản"
  width="400px"
  disableClose={actionBusy}
  on:close={() => (deleteConfirmOpen = false)}
>
  <p style="margin: 0; line-height: 1.5;">
    Bạn có chắc muốn xóa tài khoản <b>{account.label || account.id}</b> khỏi danh sách không?
    Hồ sơ trình duyệt và cookie lưu cục bộ của tài khoản này sẽ bị xóa khỏi pool.
  </p>
  {#if running || Boolean(account.busy)}
    <p class="acc-delete-busy-warn" role="alert">
      Tài khoản đang bận hoặc batch đang chạy — không thể xóa lúc này.
    </p>
  {/if}
  <div slot="footer">
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
      disabled={actionBlocked}
      on:click={confirmDelete}
    >
      {actionBusy ? "Đang xóa..." : "Xóa tài khoản"}
    </button>
  </div>
</Modal>

<style>
  .acc-card {
    border: 1px solid var(--c-border);
    border-radius: 10px;
    background: #fff;
    padding: 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 12px;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.04);
    transition: border-color 0.15s, box-shadow 0.15s;
  }
  .acc-card:hover {
    border-color: var(--c-teal);
    box-shadow: 0 2px 8px rgba(14, 124, 134, 0.1);
  }
  .acc-card.s-err,
  .acc-card.s-video_locked,
  .acc-card.s-invalid {
    border-left: 4px solid var(--c-danger);
  }
  .acc-card.s-warn,
  .acc-card.s-quota {
    border-left: 4px solid var(--c-warn);
  }
  .acc-card.s-ok,
  .acc-card.s-active {
    border-left: 4px solid var(--c-teal);
  }
  .acc-card.s-mute,
  .acc-card.s-disabled {
    border-left: 4px solid var(--c-border);
    opacity: 0.85;
  }

  .acc-header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }
  .acc-identity {
    min-width: 0;
    flex: 1;
  }
  .acc-title-line {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .acc-name {
    margin: 0;
    font-size: var(--fs-label, 14px);
    font-weight: 700;
    color: var(--c-text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .acc-id {
    font-size: 11px;
    color: var(--c-muted);
    font-family: var(--font-mono, monospace);
    margin-top: 2px;
  }

  /* Badge styling */
  .acc-badge {
    font-size: 11px;
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 999px;
    white-space: nowrap;
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .acc-badge.ok {
    background: #e6f6f1;
    color: #0b7c52;
  }
  .acc-badge.warn {
    background: #fff4e5;
    color: #b76400;
  }
  .acc-badge.info {
    background: #e8f4fc;
    color: #1769aa;
  }
  .acc-badge.err {
    background: #fde8e8;
    color: #c5221f;
  }
  .acc-badge.mute {
    background: var(--c-surface-subtle);
    color: var(--c-muted);
  }

  /* Toggle Switch */
  .acc-switch {
    position: relative;
    display: inline-block;
    width: 36px;
    height: 20px;
    flex-shrink: 0;
    cursor: pointer;
  }
  .acc-switch input {
    opacity: 0;
    width: 0;
    height: 0;
  }
  .slider {
    position: absolute;
    inset: 0;
    background-color: var(--c-border);
    border-radius: 20px;
    transition: 0.2s;
  }
  .slider::before {
    position: absolute;
    content: "";
    height: 14px;
    width: 14px;
    left: 3px;
    bottom: 3px;
    background-color: white;
    border-radius: 50%;
    transition: 0.2s;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.2);
  }
  input:checked + .slider {
    background-color: var(--c-teal);
  }
  input:checked + .slider::before {
    transform: translateX(16px);
  }
  input:disabled + .slider {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* Quota Rail */
  .acc-quota {
    display: flex;
    flex-direction: column;
    gap: 4px;
    background: var(--c-surface-subtle, #f8fafb);
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--c-border-subtle, #edf2f4);
  }
  .acc-quota.empty {
    font-size: 11px;
    color: var(--c-muted);
    font-style: italic;
  }
  .qrail {
    height: 6px;
    border-radius: 999px;
    background: #e2e8f0;
    overflow: hidden;
  }
  .qrail b {
    display: block;
    height: 100%;
    background: var(--c-teal);
    border-radius: 999px;
    transition: width 0.3s ease;
  }
  .qrail b.high {
    background: var(--c-warn);
  }
  .qrail b.full {
    background: var(--c-danger);
  }
  .qmeta {
    display: flex;
    justify-content: space-between;
    font-size: 11px;
    color: var(--c-muted);
  }
  .qextra {
    font-size: 11px;
    color: var(--c-teal-dark, #0a5f67);
  }
  .qerr {
    font-size: 11px;
    color: var(--c-danger);
  }
  .acc-error-line {
    font-size: 11px;
    color: var(--c-danger);
    background: #fff0f0;
    padding: 4px 8px;
    border-radius: 4px;
    word-break: break-all;
  }

  /* Proxy dropdown row */
  .acc-proxy-row {
    display: flex;
    align-items: center;
    gap: 8px;
    background: #fff;
    border: 1px solid var(--c-border);
    border-radius: 6px;
    padding: 3px 8px;
  }
  .acc-proxy-row:focus-within {
    border-color: var(--c-teal);
  }
  .proxy-icon-wrap {
    color: var(--c-muted);
    display: flex;
    align-items: center;
  }
  .proxy-select {
    border: none;
    outline: none;
    background: transparent;
    font-size: 12px;
    color: var(--c-text);
    flex: 1;
    min-width: 0;
    padding: 4px 0;
    cursor: pointer;
  }
  .proxy-select:disabled {
    cursor: not-allowed;
    opacity: 0.65;
  }
  .proxy-saving-tag,
  .proxy-busy-tag {
    font-size: 10.5px;
    font-weight: 600;
    padding: 2px 6px;
    border-radius: 4px;
    background: var(--c-surface-subtle, #f3f6f8);
    color: var(--c-muted);
    white-space: nowrap;
  }
  .acc-anti-link-warn {
    display: flex;
    align-items: flex-start;
    gap: 7px;
    font-size: 11.5px;
    color: #92400e;
    background: #fffbeb;
    border: 1px solid #fcd34d;
    border-radius: 6px;
    padding: 6px 9px;
    line-height: 1.4;
  }

  /* Meta items */
  .acc-meta {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    font-size: 11px;
    color: var(--c-muted);
  }
  .acc-meta-item.bad {
    color: var(--c-danger);
    font-weight: 600;
  }
  .acc-meta-item.plain.err {
    color: var(--c-danger);
  }
  .acc-meta-item.plain.warn {
    color: var(--c-warn);
  }
  .acc-meta-item.plain.info {
    color: var(--c-teal);
  }

  .acc-last-error {
    font-size: 11px;
    color: var(--c-danger);
    background: #fdf2f2;
    padding: 6px 8px;
    border-radius: 4px;
    word-break: break-all;
    max-height: 48px;
    overflow-y: auto;
  }

  /* Actions */
  .acc-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    padding-top: 4px;
    border-top: 1px solid var(--c-border-subtle, #f0f4f6);
  }
  .btn-action {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 30px;
    padding: 0 10px;
    border-radius: 6px;
    font-size: 12px;
    font-weight: 500;
    background: var(--c-surface-subtle, #f3f6f8);
    border: 1px solid var(--c-border);
    color: var(--c-text);
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s, color 0.15s;
  }
  .btn-action:hover:not(:disabled) {
    background: var(--c-teal-light, #eaf6f7);
    border-color: var(--c-teal);
    color: var(--c-teal);
  }
  .btn-action.primary {
    background: var(--c-teal-light, #eaf6f7);
    border-color: rgba(14, 124, 134, 0.35);
    color: var(--c-teal-dark, #0e7c86);
    font-weight: 600;
  }
  .btn-action.primary:hover:not(:disabled) {
    background: var(--c-teal);
    color: #fff;
  }
  .btn-action.danger {
    margin-left: auto;
    color: var(--c-danger);
  }
  .btn-action.danger:hover:not(:disabled) {
    background: #fdf2f2;
    border-color: var(--c-danger);
    color: var(--c-danger);
  }
  .btn-action:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .acc-delete-busy-warn {
    margin: 10px 0 0;
    padding: 8px 10px;
    border-radius: 6px;
    font-size: 12px;
    line-height: 1.45;
    color: #92400e;
    background: #fffbeb;
    border: 1px solid #fcd34d;
  }
</style>
