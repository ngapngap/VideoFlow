<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { api, type MuseAccount, type MuseStatusView } from "../lib/api";
  import AccountCard from "../lib/AccountCard.svelte";
  import Icons from "../lib/Icons.svelte";
  import { running, showToast } from "../lib/store";

  let accounts: MuseAccount[] = [];
  let loading = false;
  let statusView: MuseStatusView | null = null;
  let startingEngine = false;

  // Form Thêm tài khoản
  let newLabel = "";
  let newProxy = "";
  let showManualCookies = false;
  let cookieSess = "";
  let cookieDevice = "";
  let cookieVml = "";
  let allowDuplicate = false;
  let dupPending = false;
  let addBusy = false;
  let addErr = "";

  let refreshTimer: ReturnType<typeof setInterval> | null = null;

  async function loadData() {
    loading = true;
    try {
      statusView = await api.muse_sidecar_status();
      if (statusView.sidecar.running) {
        const res = await api.muse_list_accounts();
        accounts = res.data ?? [];
      } else {
        accounts = [];
      }
    } catch {
      accounts = [];
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    loadData();
    refreshTimer = setInterval(() => {
      // Tự động cập nhật danh sách (hữu ích khi người dùng đăng nhập trên trình duyệt headed)
      if (statusView?.sidecar.running) {
        api.muse_list_accounts().then((res) => {
          accounts = res.data ?? [];
        }).catch(() => {});
      }
    }, 5000);
  });

  onDestroy(() => {
    if (refreshTimer) clearInterval(refreshTimer);
  });

  async function startEngine() {
    if (startingEngine || $running) return;
    startingEngine = true;
    try {
      await api.muse_start_sidecar();
      showToast("success", "Đã khởi động engine VideoFlow Muse.");
      await loadData();
    } catch (e) {
      showToast("error", `Không thể khởi động engine: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      startingEngine = false;
    }
  }

  async function stopEngine() {
    if ($running) return;
    try {
      await api.muse_stop_sidecar();
      showToast("info", "Đã dừng engine VideoFlow Muse.");
      await loadData();
    } catch (e) {
      showToast("error", `Không thể dừng engine: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  async function handleAddAndLogin() {
    if (addBusy || $running) return;
    addBusy = true;
    addErr = "";
    try {
      // 1. Tự khởi động engine nếu chưa chạy
      if (!statusView?.sidecar.running) {
        await api.muse_start_sidecar();
      }

      // 2. Tạo tài khoản mới
      const label = newLabel.trim() || `Tài khoản Muse ${accounts.length + 1}`;
      const res = await api.muse_add_cookie(label, {}, allowDuplicate);
      const acc = res.account;

      // 3. Nếu có proxy, cập nhật proxy
      if (newProxy.trim()) {
        await api.muse_update_account(acc.id, undefined, undefined, newProxy.trim());
      }

      newLabel = "";
      newProxy = "";
      showToast("success", `Đã thêm tài khoản "${label}". Đang mở trình duyệt đăng nhập...`);

      // 4. Mở trình duyệt có giao diện để đăng nhập ngay
      await api.muse_open_login(acc.id);
      await loadData();
    } catch (e) {
      addErr = e instanceof Error ? e.message : String(e);
      showToast("error", `Lỗi thêm tài khoản: ${addErr}`);
    } finally {
      addBusy = false;
    }
  }

  async function handleAddManualCookies() {
    if (addBusy || $running) return;
    const sess = cookieSess.trim();
    const dev = cookieDevice.trim();
    if (!sess || !dev) {
      addErr = "Vui lòng nhập đầy đủ hatch_sess và hatch_native_auth_device.";
      return;
    }

    addBusy = true;
    addErr = "";
    const cookies: Record<string, string> = {
      hatch_sess: sess,
      hatch_native_auth_device: dev,
    };
    if (cookieVml.trim()) {
      cookies["hatch_vml"] = cookieVml.trim();
    }

    try {
      const label = newLabel.trim() || `Tài khoản Muse ${accounts.length + 1}`;
      const res = await api.muse_add_cookie(label, cookies, allowDuplicate);
      const acc = res.account;

      if (newProxy.trim()) {
        await api.muse_update_account(acc.id, undefined, undefined, newProxy.trim());
      }

      newLabel = "";
      newProxy = "";
      cookieSess = "";
      cookieDevice = "";
      cookieVml = "";
      dupPending = false;
      showManualCookies = false;
      showToast("success", `Đã thêm tài khoản "${label}".`);
      await loadData();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (/trùng|belongs to account/i.test(msg) && !allowDuplicate) {
        dupPending = true;
        addErr = "Cookie trùng session với tài khoản đã có. Tick 'Cho phép trùng session' nếu bạn muốn tiếp tục.";
      } else {
        addErr = msg;
      }
      showToast("error", `Lỗi thêm cookie: ${addErr}`);
    } finally {
      addBusy = false;
    }
  }

  async function handleDownloadExtension() {
    try {
      const path = await api.muse_download_extension();
      showToast("success", `Đã tải extension: ${path}`);
    } catch (e) {
      showToast("error", `Lỗi tải extension: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  function museAccStateKey(a: MuseAccount): string {
    const now = Date.now() / 1000;
    const isQuota = /quota|credit|out of (usage|messages)|limit reached|exhaust/i.test(
      a.last_error || ""
    );
    if (!a.enabled) return "disabled";
    if (a.status === "video_locked") return "video_locked";
    if (a.status === "invalid") return "invalid";
    if (a.status === "cooling" && a.cooldown_until > now) {
      return isQuota ? "quota" : "cooling";
    }
    return "active";
  }

  $: counts = (() => {
    const c: Record<string, number> = {
      active: 0,
      quota: 0,
      cooling: 0,
      video_locked: 0,
      invalid: 0,
      disabled: 0,
    };
    for (const a of accounts) {
      const k = museAccStateKey(a);
      if (c[k] !== undefined) c[k] += 1;
    }
    return c;
  })();

  $: sortedAccounts = (() => {
    const order: Record<string, number> = {
      video_locked: 0,
      quota: 1,
      invalid: 2,
      cooling: 3,
      active: 4,
      disabled: 5,
    };
    return [...accounts].sort(
      (a, b) =>
        (order[museAccStateKey(a)] ?? 9) - (order[museAccStateKey(b)] ?? 9) ||
        (a.label || a.id).localeCompare(b.label || b.id)
    );
  })();
</script>

<div class="accounts-screen">
  <!-- Top Engine Status Banner -->
  <div class="engine-bar">
    <div class="engine-info">
      <span class="status-indicator" class:running={statusView?.sidecar.running}></span>
      <div class="engine-text">
        <span class="engine-title">
          Sidecar Engine: {statusView?.sidecar.running ? "Đang hoạt động" : "Đã dừng"}
        </span>
        {#if statusView?.sidecar.running}
          <span class="engine-sub">Port {statusView.sidecar.cfg_port || 18610} · Profile riêng từng tài khoản</span>
        {:else}
          <span class="engine-sub">Khởi động engine để kiểm tra hạn mức và đăng nhập tài khoản</span>
        {/if}
      </div>
    </div>

    <div class="engine-actions">
      {#if statusView?.sidecar.running}
        <button
          type="button"
          class="btn-secondary"
          disabled={$running}
          on:click={stopEngine}
          title="Dừng tiến trình sidecar engine"
        >
          <Icons name="stop" size={14} />
          <span>Dừng engine</span>
        </button>
      {:else}
        <button
          type="button"
          class="btn-primary"
          disabled={startingEngine || $running}
          on:click={startEngine}
          title="Khởi động tiến trình sidecar engine"
        >
          <Icons name="play" size={14} />
          <span>{startingEngine ? "Đang khởi động..." : "Khởi động engine"}</span>
        </button>
      {/if}
      <button
        type="button"
        class="btn-ghost"
        disabled={loading}
        on:click={loadData}
        title="Làm mới trạng thái"
      >
        <Icons name="refresh" size={15} />
      </button>
    </div>
  </div>

  <!-- Form thêm tài khoản ở phía trên (UI-1 requirement) -->
  <section class="section-card add-account-card">
    <div class="section-header">
      <Icons name="video-source" size={18} />
      <h2 class="section-title">Thêm tài khoản Muse mới</h2>
    </div>

    <div class="add-form-grid">
      <div class="form-group">
        <label class="form-label" for="acc-new-label">Tên tài khoản (nhãn gợi nhớ)</label>
        <input
          id="acc-new-label"
          class="form-input"
          placeholder="Ví dụ: Tài khoản Muse 01"
          bind:value={newLabel}
          disabled={addBusy || $running}
          autocomplete="off"
        />
      </div>

      <div class="form-group">
        <label class="form-label" for="acc-new-proxy">Proxy riêng (tùy chọn - P2-3)</label>
        <input
          id="acc-new-proxy"
          class="form-input"
          placeholder="http://user:pass@ip:port hoặc socks5://..."
          bind:value={newProxy}
          disabled={addBusy || $running}
          spellcheck="false"
          autocomplete="off"
        />
      </div>
    </div>

    <div class="add-actions-row">
      <!-- Nút Đăng nhập Headed (P1-5 tự động bắt token) -->
      <button
        type="button"
        class="btn-primary"
        disabled={addBusy || $running}
        on:click={handleAddAndLogin}
        title="Tạo tài khoản và mở trình duyệt có giao diện để bạn đăng nhập trực tiếp (tự bắt cookie & token)"
      >
        <Icons name="log-in" size={15} />
        <span>{addBusy ? "Đang xử lý..." : "Thêm & Mở trình duyệt đăng nhập"}</span>
      </button>

      <button
        type="button"
        class="btn-ghost toggle-manual-btn"
        on:click={() => (showManualCookies = !showManualCookies)}
      >
        <Icons name={showManualCookies ? "chevron-down" : "chevron-right"} size={14} />
        <span>{showManualCookies ? "Ẩn nhập cookie thủ công" : "Nhập cookie thủ công (nâng cao)"}</span>
      </button>

      <button
        type="button"
        class="btn-ghost download-ext-btn"
        disabled={$running}
        on:click={handleDownloadExtension}
        title="Tải extension xuất cookie cho Edge/Chrome"
      >
        <Icons name="link" size={14} />
        <span>Tải extension import</span>
      </button>
    </div>

    <!-- Khối nhập cookie thủ công nếu người dùng muốn dán trực tiếp -->
    {#if showManualCookies}
      <div class="manual-cookie-box">
        <div class="form-group">
          <label class="form-label" for="cookie-sess">
            <code>hatch_sess</code> <span class="req">* bắt buộc</span>
          </label>
          <input
            id="cookie-sess"
            class="form-input code"
            placeholder="Dán giá trị cookie hatch_sess"
            bind:value={cookieSess}
            disabled={addBusy || $running}
            spellcheck="false"
            autocomplete="off"
          />
        </div>

        <div class="form-group">
          <label class="form-label" for="cookie-device">
            <code>hatch_native_auth_device</code> <span class="req">* bắt buộc</span>
          </label>
          <input
            id="cookie-device"
            class="form-input code"
            placeholder="Dán giá trị cookie hatch_native_auth_device"
            bind:value={cookieDevice}
            disabled={addBusy || $running}
            spellcheck="false"
            autocomplete="off"
          />
        </div>

        <div class="form-group">
          <label class="form-label" for="cookie-vml">
            <code>hatch_vml</code> <span class="opt">(tùy chọn)</span>
          </label>
          <input
            id="cookie-vml"
            class="form-input code"
            placeholder="Dán giá trị cookie hatch_vml nếu có"
            bind:value={cookieVml}
            disabled={addBusy || $running}
            spellcheck="false"
            autocomplete="off"
          />
        </div>

        <div class="manual-save-row">
          <button
            type="button"
            class="btn-secondary"
            disabled={addBusy || $running || !cookieSess.trim() || !cookieDevice.trim()}
            on:click={handleAddManualCookies}
          >
            <Icons name="check" size={14} />
            <span>Lưu tài khoản từ cookie</span>
          </button>

          {#if dupPending}
            <label class="dup-allow-checkbox">
              <input type="checkbox" bind:checked={allowDuplicate} />
              <span>Cho phép trùng session (chỉ khi chắc chắn)</span>
            </label>
          {/if}
        </div>
      </div>
    {/if}

    {#if addErr}
      <div class="form-error-banner">{addErr}</div>
    {/if}
  </section>

  <!-- Danh sách thẻ tài khoản ở phía dưới (UI-1 requirement) -->
  <section class="section-card accounts-list-card">
    <div class="list-header-row">
      <div class="list-title-wrap">
        <h2 class="section-title">Danh sách tài khoản ({accounts.length})</h2>
      </div>

      <!-- Thống kê trạng thái -->
      <div class="pool-badges">
        {#if counts.active > 0}
          <span class="pool-badge ok">Hoạt động <b>{counts.active}</b></span>
        {/if}
        {#if counts.quota > 0}
          <span class="pool-badge warn">Hết hạn mức <b>{counts.quota}</b></span>
        {/if}
        {#if counts.cooling > 0}
          <span class="pool-badge info">Tạm chờ <b>{counts.cooling}</b></span>
        {/if}
        {#if counts.video_locked > 0}
          <span class="pool-badge err">Khóa video <b>{counts.video_locked}</b></span>
        {/if}
        {#if counts.invalid > 0}
          <span class="pool-badge err">Lỗi phiên <b>{counts.invalid}</b></span>
        {/if}
        {#if counts.disabled > 0}
          <span class="pool-badge mute">Đang tắt <b>{counts.disabled}</b></span>
        {/if}
      </div>
    </div>

    {#if loading && accounts.length === 0}
      <div class="empty-state">Đang tải danh sách tài khoản...</div>
    {:else if accounts.length === 0}
      <div class="empty-state">
        <Icons name="video-source" size={36} />
        <p class="empty-title">Chưa có tài khoản Muse nào trong pool</p>
        <p class="empty-desc">
          Nhập tên và bấm <b>"Thêm & Mở trình duyệt đăng nhập"</b> ở trên. Cửa sổ trình duyệt sẽ mở để bạn đăng nhập một lần, hệ thống sẽ tự động bắt cookies và token.
        </p>
      </div>
    {:else}
      <div class="accounts-grid">
        {#each sortedAccounts as account (account.id)}
          <AccountCard
            {account}
            running={$running}
            on:refresh={loadData}
            on:updated={loadData}
            on:deleted={loadData}
          />
        {/each}
      </div>
    {/if}
  </section>
</div>

<style>
  .accounts-screen {
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-width: 1200px;
    margin: 0 auto;
  }

  /* Engine bar */
  .engine-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 18px;
    background: var(--c-card, #fff);
    border: 1px solid var(--c-border);
    border-radius: 10px;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.04);
  }
  .engine-info {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .status-indicator {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: var(--c-border);
    box-shadow: 0 0 0 2px #fff;
  }
  .status-indicator.running {
    background: #0b7c52;
    box-shadow: 0 0 0 2px rgba(11, 124, 82, 0.2);
  }
  .engine-title {
    font-size: 13px;
    font-weight: 700;
    color: var(--c-text);
  }
  .engine-sub {
    font-size: 11px;
    color: var(--c-muted);
  }
  .engine-text {
    display: flex;
    flex-direction: column;
  }
  .engine-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  /* Section cards */
  .section-card {
    background: var(--c-card, #fff);
    border: 1px solid var(--c-border);
    border-radius: 10px;
    padding: 18px 20px;
    display: flex;
    flex-direction: column;
    gap: 14px;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.04);
  }
  .section-header {
    display: flex;
    align-items: center;
    gap: 10px;
    color: var(--c-teal-dark, #0a5f67);
  }
  .section-title {
    margin: 0;
    font-size: 15px;
    font-weight: 700;
    color: var(--c-text);
  }

  /* Add Form */
  .add-form-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 14px;
  }
  @media (max-width: 768px) {
    .add-form-grid {
      grid-template-columns: 1fr;
    }
  }
  .form-group {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .form-label {
    font-size: 12px;
    font-weight: 600;
    color: var(--c-text);
  }
  .form-input {
    height: 36px;
    padding: 0 12px;
    border-radius: 6px;
    border: 1px solid var(--c-border);
    background: #fff;
    font-size: 13px;
    color: var(--c-text);
    outline: none;
    transition: border-color 0.15s;
  }
  .form-input:focus {
    border-color: var(--c-teal);
  }
  .form-input.code {
    font-family: var(--font-mono, monospace);
    font-size: 12px;
  }
  .req {
    color: var(--c-danger);
    font-weight: 500;
    font-size: 11px;
  }
  .opt {
    color: var(--c-muted);
    font-weight: 400;
    font-size: 11px;
  }

  .add-actions-row {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    margin-top: 4px;
  }
  .download-ext-btn {
    margin-left: auto;
  }

  /* Manual cookies box */
  .manual-cookie-box {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 14px;
    background: var(--c-surface-subtle, #f8fafb);
    border: 1px dashed var(--c-border);
    border-radius: 8px;
    margin-top: 6px;
  }
  .manual-save-row {
    display: flex;
    align-items: center;
    gap: 14px;
    flex-wrap: wrap;
  }
  .dup-allow-checkbox {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--c-warn);
    cursor: pointer;
  }

  .form-error-banner {
    padding: 8px 12px;
    border-radius: 6px;
    background: #fdf2f2;
    border: 1px solid #f9d2d2;
    color: var(--c-danger);
    font-size: 12px;
  }

  /* List & Grid */
  .list-header-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--c-border-subtle, #edf2f4);
  }
  .pool-badges {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .pool-badge {
    font-size: 11px;
    padding: 2px 8px;
    border-radius: 999px;
    white-space: nowrap;
  }
  .pool-badge.ok {
    background: #e6f6f1;
    color: #0b7c52;
  }
  .pool-badge.warn {
    background: #fff4e5;
    color: #b76400;
  }
  .pool-badge.info {
    background: #e8f4fc;
    color: #1769aa;
  }
  .pool-badge.err {
    background: #fde8e8;
    color: #c5221f;
  }
  .pool-badge.mute {
    background: var(--c-surface-subtle);
    color: var(--c-muted);
  }

  .accounts-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(360px, 1fr));
    gap: 14px;
  }

  .empty-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    text-align: center;
    padding: 40px 20px;
    color: var(--c-muted);
    gap: 10px;
  }
  .empty-title {
    font-size: 14px;
    font-weight: 700;
    color: var(--c-text);
    margin: 0;
  }
  .empty-desc {
    font-size: 12px;
    max-width: 500px;
    margin: 0;
    line-height: 1.5;
  }
</style>
