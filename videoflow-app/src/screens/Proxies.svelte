<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { api, type MuseAccount, type ProxyInput, type ProxyItem } from "../lib/api";
  import { mergeSingleTestedProxy } from "../lib/asyncGuard";
  import Icons from "../lib/Icons.svelte";
  import Modal from "../lib/Modal.svelte";
  import {
    buildProxyInput,
    formatHostPort,
    isProxyBusy,
    parseProxyPaste,
    resolveLiveProxy,
    validateProxyHost,
  } from "../lib/proxyInput";
  import {
    formatApiError,
    museAccounts,
    refreshMuseState,
    running,
    showToast,
    startMusePolling,
  } from "../lib/store";

  let proxies: ProxyItem[] = [];
  let loading = true;
  let loadSeq = 0;
  let destroyed = false;
  let stopMusePolling: (() => void) | null = null;

  // Test states
  let testingIds = new Set<string>();
  let testingAll = false;

  // Add / Edit modal
  let formModalOpen = false;
  let editingId: string | null = null;
  let editingHasPassword = false;
  let clearPassword = false;
  let formName = "";
  let formKind: "http" | "socks5" = "http";
  let formHost = "";
  let formPort = 8080;
  let formUsername = "";
  let formPassword = "";
  let formNote = "";
  let showPassword = false;
  let formSaving = false;
  let formErr = "";

  // Delete confirmation modal
  let deleteTarget: ProxyItem | null = null;
  let deleting = false;

  onMount(() => {
    stopMusePolling = startMusePolling();
    void loadData();
  });

  onDestroy(() => {
    destroyed = true;
    stopMusePolling?.();
    stopMusePolling = null;
  });

  async function loadData() {
    if (destroyed) return;
    const seq = ++loadSeq;
    try {
      const [pxList] = await Promise.all([
        api.proxy_list(),
        refreshMuseState({ force: true }),
      ]);
      if (destroyed || seq !== loadSeq) return;
      proxies = pxList;
    } catch (e) {
      if (!destroyed && seq === loadSeq) {
        showToast("error", `Không thể tải danh sách proxy: ${formatApiError(e)}`);
      }
    } finally {
      if (!destroyed && seq === loadSeq) {
        loading = false;
      }
    }
  }

  $: accountMap = new Map($museAccounts.map((a) => [a.id, a]));
  $: liveDeleteTarget = resolveLiveProxy(deleteTarget, proxies);
  $: deleteTargetBusy = isProxyBusy(liveDeleteTarget, $running, accountMap);

  // Map exit_ip -> danh sách account IDs đang dùng exit_ip đó (qua mọi proxy)
  $: exitIpAccountsMap = (() => {
    const m = new Map<string, { accId: string; label: string; proxyName: string }[]>();
    for (const p of proxies) {
      const ip = (p.exit_ip || "").trim();
      if (!ip) continue;
      for (const accId of p.assigned_account_ids) {
        const acc = accountMap.get(accId);
        const label = acc?.label || accId;
        const arr = m.get(ip) ?? [];
        arr.push({ accId, label, proxyName: p.name });
        m.set(ip, arr);
      }
    }
    return m;
  })();

  function accountDisplayName(accId: string, accMap: Map<string, MuseAccount>): string {
    const a = accMap.get(accId);
    return a?.label ? `${a.label} (${accId})` : accId;
  }

  function getSharedIpConflict(
    p: ProxyItem,
    ipMap: Map<string, { accId: string; label: string; proxyName: string }[]>
  ): string[] {
    if (p.assigned_account_ids.length === 0) return [];
    const ip = (p.exit_ip || "").trim();
    if (!ip) return [];
    const entries = ipMap.get(ip) ?? [];
    if (entries.length <= 1) return [];
    return entries.map((e) => e.label);
  }

  function openCreateModal() {
    if ($running || testingAll) return;
    editingId = null;
    editingHasPassword = false;
    clearPassword = false;
    formName = `Proxy ${proxies.length + 1}`;
    formKind = "http";
    formHost = "";
    formPort = 8080;
    formUsername = "";
    formPassword = "";
    formNote = "";
    showPassword = false;
    formErr = "";
    formModalOpen = true;
  }

  function openEditModal(p: ProxyItem) {
    if (isProxyBusy(p, $running, accountMap)) {
      showToast(
        "warn",
        "Không thể sửa proxy khi đang chạy batch hoặc tài khoản gắn proxy đang bận."
      );
      return;
    }
    editingId = p.id;
    editingHasPassword = Boolean(p.has_password);
    clearPassword = false;
    formName = p.name;
    formKind = p.kind === "socks5" ? "socks5" : "http";
    formHost = p.host;
    formPort = p.port;
    formUsername = p.username || "";
    formPassword = "";
    formNote = p.note || "";
    showPassword = false;
    formErr = "";
    formModalOpen = true;
  }

  /** Hỗ trợ dán nhanh chuỗi `host:port`, `host:port:user:pass`, `[ipv6]:port` hoặc URL vào ô Host */
  function handleHostPaste(e: ClipboardEvent) {
    const raw = e.clipboardData?.getData("text") ?? "";
    const parsed = parseProxyPaste(raw);
    if (!parsed) return;

    e.preventDefault();
    if (parsed.kind) formKind = parsed.kind;
    formHost = parsed.host;
    if (parsed.port !== undefined) formPort = parsed.port;
    if (parsed.username !== undefined) formUsername = parsed.username;
    if (parsed.password !== undefined) {
      formPassword = parsed.password; // NEVER .trim()
      clearPassword = false;
    }
  }

  async function handleSaveProxy() {
    if (formSaving || $running) return;
    formErr = "";
    const name = formName.trim();
    const port = Number(formPort);

    if (!name) {
      formErr = "Vui lòng nhập tên gợi nhớ cho proxy.";
      return;
    }
    const hostCheck = validateProxyHost(formHost);
    if (!hostCheck.ok) {
      formErr = hostCheck.error;
      return;
    }
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      formErr = "Port phải là số nguyên từ 1 đến 65535.";
      return;
    }

    const input: ProxyInput = buildProxyInput({
      name,
      kind: formKind,
      host: hostCheck.host,
      port,
      username: formUsername,
      password: formPassword,
      note: formNote,
      isEditing: Boolean(editingId),
      clearPassword,
    });

    formSaving = true;
    try {
      if (editingId) {
        await api.proxy_update(editingId, input);
        showToast(
          "success",
          `Đã cập nhật proxy "${name}" và đồng bộ xuống các tài khoản liên kết.`
        );
      } else {
        await api.proxy_create(input);
        showToast("success", `Đã thêm proxy "${name}".`);
      }
      formModalOpen = false;
      await loadData();
    } catch (e) {
      formErr = formatApiError(e);
      showToast("error", `Lỗi lưu proxy: ${formErr}`);
    } finally {
      formSaving = false;
    }
  }

  function requestDeleteProxy(p: ProxyItem) {
    if (isProxyBusy(p, $running, accountMap)) {
      showToast(
        "warn",
        "Không thể xoá proxy khi đang chạy batch hoặc tài khoản gắn proxy đang bận."
      );
      return;
    }
    deleteTarget = p;
  }

  async function confirmDeleteProxy() {
    const target = resolveLiveProxy(deleteTarget, proxies);
    if (!target || deleting) return;
    if (isProxyBusy(target, $running, accountMap)) {
      showToast(
        "warn",
        "Không thể xoá proxy khi đang chạy batch hoặc tài khoản gắn proxy đang bận."
      );
      return;
    }
    deleting = true;
    try {
      const res = await api.proxy_delete(target.id);
      deleteTarget = null;
      const count = res.unassigned_accounts?.length ?? 0;
      if (count > 0) {
        showToast(
          "info",
          `Đã xoá proxy "${target.name}" và chuyển ${count} tài khoản về kết nối Trực tiếp.`
        );
      } else {
        showToast("success", `Đã xoá proxy "${target.name}".`);
      }
      await loadData();
    } catch (e) {
      showToast("error", `Lỗi xoá proxy: ${formatApiError(e)}`);
    } finally {
      deleting = false;
    }
  }

  async function handleTestOne(p: ProxyItem) {
    if (destroyed || testingIds.has(p.id) || testingAll) return;
    testingIds = new Set([...testingIds, p.id]);
    try {
      const updated = await api.proxy_test(p.id);
      if (destroyed) return;
      proxies = mergeSingleTestedProxy(proxies, updated);
      if (updated.last_status === "alive") {
        showToast(
          "success",
          `Proxy "${updated.name}" sống · IP: ${updated.exit_ip || "không rõ"} · ${updated.latency_ms ?? 0}ms`
        );
      } else {
        showToast(
          "error",
          `Proxy "${updated.name}" thất bại: ${updated.last_error || "Không kết nối được tới muse.ai"}`
        );
      }
    } catch (e) {
      if (destroyed) return;
      showToast("error", `Lỗi kiểm tra proxy "${p.name}": ${formatApiError(e)}`);
    } finally {
      if (!destroyed) {
        const next = new Set(testingIds);
        next.delete(p.id);
        testingIds = next;
      }
    }
  }

  async function handleTestAll() {
    if (destroyed || testingAll || testingIds.size > 0 || proxies.length === 0) return;
    const seq = loadSeq;
    testingAll = true;
    try {
      const updated = await api.proxy_test_all();
      if (destroyed || seq !== loadSeq) return;
      const aliveCount = updated.filter((p) => p.last_status === "alive").length;
      const deadCount = updated.filter((p) => p.last_status === "dead").length;
      showToast(
        updated.length > 0 && aliveCount === updated.length
          ? "success"
          : updated.length > 0 && deadCount === updated.length
            ? "error"
            : "info",
        `Đã kiểm tra ${updated.length} proxy: ${aliveCount} sống, ${deadCount} lỗi.`
      );
      await loadData();
    } catch (e) {
      if (!destroyed && seq === loadSeq) {
        showToast("error", `Lỗi kiểm tra tất cả proxy: ${formatApiError(e)}`);
      }
    } finally {
      if (!destroyed) {
        testingAll = false;
      }
    }
  }

  function statusBadge(st: string): { label: string; cls: string } {
    if (st === "alive") return { label: "Sống (Alive)", cls: "ok" };
    if (st === "dead") return { label: "Lỗi (Dead)", cls: "err" };
    return { label: "Chưa test", cls: "mute" };
  }

  function formatTestedTime(iso: string | null | undefined): string {
    if (!iso) return "Chưa kiểm tra";
    const ts = Date.parse(iso);
    if (Number.isNaN(ts)) return iso;
    const diffSec = Math.max(0, Math.floor((Date.now() - ts) / 1000));
    if (diffSec < 60) return "vừa xong";
    if (diffSec < 3600) return `${Math.floor(diffSec / 60)} phút trước`;
    if (diffSec < 86400) return `${Math.floor(diffSec / 3600)} giờ trước`;
    return new Date(ts).toLocaleString("vi-VN");
  }

  $: countAlive = proxies.filter((p) => p.last_status === "alive").length;
  $: countDead = proxies.filter((p) => p.last_status === "dead").length;
  $: countUntested = proxies.filter((p) => p.last_status === "untested").length;
</script>

<div class="proxies-page">
  <!-- Top Header Banner -->
  <section class="card hero-card">
    <div class="hero-top">
      <div class="hero-info">
        <div class="hero-title-row">
          <Icons name="globe" size={20} />
          <h2>Quản lý Proxy (HTTP / SOCKS5)</h2>
        </div>
        <p class="hero-desc">
          Lưu trữ tập trung tại <code>proxies.db</code>. Kiểm tra kết nối thực tế tới <b>https://muse.ai</b>, truy vấn Exit IP đầu ra và gán độc lập cho từng tài khoản Muse để chống liên kết dấu vết IP.
        </p>
      </div>

      <div class="hero-actions">
        <button
          type="button"
          class="btn-secondary"
          disabled={loading || testingAll || testingIds.size > 0}
          on:click={loadData}
          title="Tải lại danh sách proxy"
        >
          <Icons name="refresh" size={14} />
          <span>Làm mới</span>
        </button>

        <button
          type="button"
          class="btn-secondary"
          disabled={proxies.length === 0 || testingAll || testingIds.size > 0}
          on:click={handleTestAll}
          title="Kiểm tra kết nối thực tế tới muse.ai cho toàn bộ proxy"
        >
          <Icons name="beaker" size={14} />
          <span>{testingAll ? "Đang test tất cả..." : "Test tất cả"}</span>
        </button>

        <button
          type="button"
          class="btn-primary"
          disabled={$running || testingAll}
          on:click={openCreateModal}
          title="Thêm cấu hình proxy mới"
        >
          <Icons name="folder-plus" size={14} />
          <span>+ Thêm Proxy</span>
        </button>
      </div>
    </div>

    <div class="stats-strip">
      <div class="stat-pill">
        <span class="lbl">Tổng số</span>
        <b>{proxies.length}</b>
      </div>
      <div class="stat-pill ok">
        <span class="lbl">Sống (Alive)</span>
        <b>{countAlive}</b>
      </div>
      <div class="stat-pill err">
        <span class="lbl">Lỗi (Dead)</span>
        <b>{countDead}</b>
      </div>
      <div class="stat-pill mute">
        <span class="lbl">Chưa kiểm tra</span>
        <b>{countUntested}</b>
      </div>
    </div>
  </section>

  <!-- Proxy Grid / List -->
  <section class="list-section">
    {#if loading && proxies.length === 0}
      <div class="empty-state">Đang tải danh sách proxy...</div>
    {:else if proxies.length === 0}
      <div class="empty-state">
        <Icons name="globe" size={30} />
        <p>Chưa có proxy nào được lưu.</p>
        <span class="hint">
          Bấm <b>"+ Thêm Proxy"</b> để tạo proxy HTTP hoặc SOCKS5, bấm <b>"Test"</b> để kiểm tra kết nối tới <code>muse.ai</code>, sau đó gán cho tài khoản ở tab <b>Tài khoản</b>.
        </span>
      </div>
    {:else}
      <div class="proxy-grid">
        {#each proxies as p (p.id)}
          {@const badge = statusBadge(p.last_status)}
          {@const isTesting = testingIds.has(p.id) || testingAll}
          {@const busy = isProxyBusy(p, $running, accountMap)}
          {@const ipConflictNames = getSharedIpConflict(p, exitIpAccountsMap)}
          <article class="proxy-card s-{p.last_status}">
            <div class="p-header">
              <div class="p-title-wrap">
                <div class="p-title-line">
                  <span class="kind-tag {p.kind}">{p.kind.toUpperCase()}</span>
                  <h3 class="p-name" title={p.name}>{p.name}</h3>
                </div>
                <div class="p-endpoint selectable">
                  {formatHostPort(p.host, p.port)}
                  {#if p.username}
                    <span class="auth-pill" title={`Tài khoản xác thực: ${p.username}`}>
                      · user: {p.username}{p.has_password ? " (có mật khẩu)" : ""}
                    </span>
                  {:else if p.has_password}
                    <span class="auth-pill" title="Proxy có mật khẩu xác thực">
                      · (có mật khẩu)
                    </span>
                  {/if}
                </div>
              </div>

              <span class="status-badge {badge.cls}">{badge.label}</span>
            </div>

            <!-- Test metrics -->
            <div class="p-metrics">
              <div class="metric-item">
                <span class="m-lbl">Exit IP:</span>
                <b class="m-val mono selectable">{p.exit_ip || "—"}</b>
              </div>
              <div class="metric-item">
                <span class="m-lbl">Độ trễ:</span>
                <b class="m-val">{p.latency_ms != null ? `${p.latency_ms} ms` : "—"}</b>
              </div>
              <div class="metric-item">
                <span class="m-lbl">Kiểm tra:</span>
                <span class="m-val">{formatTestedTime(p.tested_at)}</span>
              </div>
            </div>

            {#if p.last_error}
              <div class="p-error selectable" title="Chi tiết lỗi kiểm tra gần nhất">
                {p.last_error}
              </div>
            {/if}

            {#if p.note}
              <div class="p-note">{p.note}</div>
            {/if}

            <!-- Assigned accounts -->
            <div class="p-accounts">
              <span class="acc-lbl">Tài khoản đang gán ({p.assigned_account_ids.length}):</span>
              {#if p.assigned_account_ids.length === 0}
                <span class="acc-empty">Chưa gán cho tài khoản nào</span>
              {:else}
                <div class="acc-chips">
                  {#each p.assigned_account_ids as accId (accId)}
                    <span class="acc-chip">{accountDisplayName(accId, accountMap)}</span>
                  {/each}
                </div>
              {/if}
            </div>

            <!-- Anti-Link Warning if >1 account uses this proxy or shares its exit_ip -->
            {#if p.assigned_account_ids.length > 1}
              <div class="anti-link-warn">
                <Icons name="alert-triangle" size={14} />
                <span>
                  Proxy này đang dùng chung cho <b>{p.assigned_account_ids.length} tài khoản</b> ({p.assigned_account_ids.map((id) => accountDisplayName(id, accountMap)).join(", ")}). Dùng chung IP có nguy cơ bị nối dấu vết giữa các tài khoản.
                </span>
              </div>
            {:else if ipConflictNames.length > 1}
              <div class="anti-link-warn">
                <Icons name="alert-triangle" size={14} />
                <span>
                  IP đầu ra <b>{p.exit_ip}</b> đang trùng giữa các tài khoản: <b>{ipConflictNames.join(", ")}</b>.
                </span>
              </div>
            {/if}

            <!-- Actions -->
            <div class="p-actions">
              <button
                type="button"
                class="btn-action primary"
                disabled={isTesting}
                on:click={() => handleTestOne(p)}
                title="Kiểm tra kết nối thực tế qua proxy tới https://muse.ai và lấy Exit IP"
              >
                <Icons name="beaker" size={14} />
                <span>{isTesting ? "Đang test..." : "Test kết nối"}</span>
              </button>

              <button
                type="button"
                class="btn-action"
                disabled={busy || isTesting}
                on:click={() => openEditModal(p)}
                title={busy
                  ? "Không thể sửa khi đang chạy batch hoặc tài khoản đang bận"
                  : "Chỉnh sửa cấu hình proxy"}
              >
                <Icons name="settings" size={14} />
                <span>Sửa</span>
              </button>

              <button
                type="button"
                class="btn-action danger"
                disabled={busy || isTesting}
                on:click={() => requestDeleteProxy(p)}
                title={busy
                  ? "Không thể xoá khi đang chạy batch hoặc tài khoản đang bận"
                  : "Xoá proxy này"}
              >
                <Icons name="trash" size={14} />
                <span>Xoá</span>
              </button>
            </div>
          </article>
        {/each}
      </div>
    {/if}
  </section>
</div>

<!-- Modal Thêm / Sửa Proxy -->
<Modal
  open={formModalOpen}
  title={editingId ? "Chỉnh sửa cấu hình Proxy" : "Thêm Proxy mới"}
  width="500px"
  disableClose={formSaving}
  on:close={() => (formModalOpen = false)}
>
  <div class="form-body">
    <div class="form-row two-col">
      <div class="field">
        <label for="px-name">Tên gợi nhớ *</label>
        <input
          id="px-name"
          type="text"
          placeholder="VD: Proxy US-01"
          bind:value={formName}
          disabled={formSaving}
        />
      </div>

      <div class="field">
        <label for="px-kind">Loại giao thức *</label>
        <select id="px-kind" bind:value={formKind} disabled={formSaving}>
          <option value="http">HTTP Proxy</option>
          <option value="socks5">SOCKS5 Proxy</option>
        </select>
      </div>
    </div>

    <div class="form-row host-port-row">
      <div class="field">
        <label for="px-host">Host / IP * (hỗ trợ dán nhanh host:port hoặc URL)</label>
        <input
          id="px-host"
          type="text"
          placeholder="198.51.100.10 hoặc http://user:pass@ip:port"
          bind:value={formHost}
          on:paste={handleHostPaste}
          disabled={formSaving}
          spellcheck="false"
        />
      </div>

      <div class="field port-field">
        <label for="px-port">Port *</label>
        <input
          id="px-port"
          type="number"
          min="1"
          max="65535"
          bind:value={formPort}
          disabled={formSaving}
        />
      </div>
    </div>

    <div class="form-row two-col">
      <div class="field">
        <label for="px-user">Username (tùy chọn)</label>
        <input
          id="px-user"
          type="text"
          placeholder="Để trống nếu không cần auth"
          bind:value={formUsername}
          disabled={formSaving}
          spellcheck="false"
          autocomplete="off"
        />
      </div>

      <div class="field">
        <label for="px-pass">Password (tùy chọn)</label>
        <div class="pass-input-wrap">
          {#if showPassword}
            <input
              id="px-pass"
              type="text"
              placeholder={editingId && editingHasPassword
                ? clearPassword
                  ? "Sẽ xoá mật khẩu đã lưu"
                  : "Để trống để giữ nguyên mật khẩu đã lưu"
                : "Mật khẩu proxy"}
              bind:value={formPassword}
              disabled={formSaving || clearPassword}
              spellcheck="false"
              autocomplete="off"
            />
          {:else}
            <input
              id="px-pass"
              type="password"
              placeholder={editingId && editingHasPassword
                ? clearPassword
                  ? "Sẽ xoá mật khẩu đã lưu"
                  : "Để trống để giữ nguyên mật khẩu đã lưu"
                : "Mật khẩu proxy"}
              bind:value={formPassword}
              disabled={formSaving || clearPassword}
              spellcheck="false"
              autocomplete="off"
            />
          {/if}
          <button
            type="button"
            class="btn-eye"
            disabled={formSaving || clearPassword}
            on:click={() => (showPassword = !showPassword)}
            title={showPassword ? "Ẩn mật khẩu" : "Hiện mật khẩu"}
            aria-label={showPassword ? "Ẩn mật khẩu" : "Hiện mật khẩu"}
          >
            <Icons name={showPassword ? "eye-off" : "eye"} size={14} />
          </button>
        </div>
        {#if editingId && editingHasPassword}
          <label class="clear-pass-toggle">
            <input
              type="checkbox"
              bind:checked={clearPassword}
              disabled={formSaving}
              on:change={() => {
                if (clearPassword) formPassword = "";
              }}
            />
            <span>Xoá mật khẩu đã lưu</span>
          </label>
        {/if}
      </div>
    </div>

    <div class="field">
      <label for="px-note">Ghi chú (tùy chọn)</label>
      <input
        id="px-note"
        type="text"
        placeholder="VD: Nhà cung cấp, khu vực, ngày hết hạn..."
        bind:value={formNote}
        disabled={formSaving}
      />
    </div>

    {#if formErr}
      <div class="form-err">{formErr}</div>
    {/if}
  </div>

  <div slot="footer" class="modal-footer-actions">
    <button
      type="button"
      class="btn-secondary"
      disabled={formSaving}
      on:click={() => (formModalOpen = false)}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-primary"
      disabled={formSaving || $running}
      on:click={handleSaveProxy}
    >
      {formSaving ? "Đang lưu..." : editingId ? "Lưu thay đổi" : "Thêm Proxy"}
    </button>
  </div>
</Modal>

<!-- Modal Xác nhận xoá Proxy -->
<Modal
  open={liveDeleteTarget !== null}
  title="Xác nhận xoá Proxy"
  width="460px"
  disableClose={deleting}
  on:close={() => (deleteTarget = null)}
>
  {#if liveDeleteTarget}
    <div class="del-body">
      {#if liveDeleteTarget.assigned_account_ids.length > 0}
        <p>
          Proxy <b>{liveDeleteTarget.name}</b> (<code>{formatHostPort(liveDeleteTarget.host, liveDeleteTarget.port)}</code>) đang được gán cho
          <b>{liveDeleteTarget.assigned_account_ids.length} tài khoản</b>:
          <b>{liveDeleteTarget.assigned_account_ids.map((id) => accountDisplayName(id, accountMap)).join(", ")}</b>.
        </p>
        <p>
          Xác nhận xoá proxy này và chuyển các tài khoản đó về kết nối <b>Trực tiếp (không dùng proxy)</b>?
        </p>
      {:else}
        <p>
          Bạn có chắc chắn muốn xoá proxy <b>{liveDeleteTarget.name}</b> (<code>{formatHostPort(liveDeleteTarget.host, liveDeleteTarget.port)}</code>) khỏi hệ thống?
        </p>
      {/if}
      {#if deleteTargetBusy}
        <p class="proxy-delete-busy-warn" role="alert">
          Không thể xoá proxy khi đang chạy batch hoặc tài khoản gắn proxy đang bận.
        </p>
      {/if}
    </div>
  {/if}
  <div slot="footer" class="modal-footer-actions">
    <button
      type="button"
      class="btn-secondary"
      disabled={deleting}
      on:click={() => (deleteTarget = null)}
    >
      Hủy
    </button>
    <button
      type="button"
      class="btn-danger"
      disabled={deleting || deleteTargetBusy}
      on:click={confirmDeleteProxy}
    >
      {deleting ? "Đang xoá..." : "Xác nhận xoá"}
    </button>
  </div>
</Modal>

<style>
  .proxies-page {
    display: flex;
    flex-direction: column;
    gap: 16px;
    padding-bottom: 24px;
  }

  .card {
    background: var(--c-card);
    border: 1px solid var(--c-border);
    border-radius: 12px;
    padding: 16px 18px;
    box-shadow: 0 1px 2px rgba(15, 23, 42, 0.03);
  }

  .hero-top {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
    flex-wrap: wrap;
  }

  .hero-title-row {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--c-teal);
  }

  .hero-title-row h2 {
    margin: 0;
    font-size: 16px;
    font-weight: 700;
    color: var(--c-text);
  }

  .hero-desc {
    margin: 6px 0 0;
    font-size: 12.5px;
    color: var(--c-muted);
    max-width: 720px;
    line-height: 1.45;
  }

  .hero-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .hero-actions button {
    height: var(--h-btn-secondary);
  }

  .stats-strip {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-top: 14px;
    padding-top: 12px;
    border-top: 1px solid var(--c-border);
  }

  .stat-pill {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
    border-radius: 8px;
    background: var(--c-page);
    border: 1px solid var(--c-border);
    font-size: 12px;
  }
  .stat-pill .lbl {
    color: var(--c-muted);
  }
  .stat-pill b {
    font-size: 13px;
    color: var(--c-text);
  }
  .stat-pill.ok b {
    color: var(--c-green);
  }
  .stat-pill.err b {
    color: var(--c-danger);
  }

  .proxy-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(360px, 1fr));
    gap: 14px;
  }

  .proxy-card {
    background: var(--c-card);
    border: 1px solid var(--c-border);
    border-radius: 12px;
    padding: 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    box-shadow: 0 1px 2px rgba(15, 23, 42, 0.03);
  }
  .proxy-card.s-alive {
    border-left: 3px solid var(--c-green);
  }
  .proxy-card.s-dead {
    border-left: 3px solid var(--c-danger);
  }

  .p-header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 10px;
  }

  .p-title-line {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .kind-tag {
    font-size: 10.5px;
    font-weight: 700;
    padding: 2px 7px;
    border-radius: 5px;
    background: rgba(14, 124, 134, 0.12);
    color: var(--c-teal);
  }
  .kind-tag.socks5 {
    background: rgba(79, 70, 229, 0.12);
    color: #4338ca;
  }

  .p-name {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--c-text);
  }

  .p-endpoint {
    margin-top: 3px;
    font-family: var(--font-mono, monospace);
    font-size: 12px;
    color: var(--c-muted);
    word-break: break-all;
  }

  .auth-pill {
    font-family: var(--font-sans, sans-serif);
    font-size: 11px;
    color: var(--c-muted);
  }

  .status-badge {
    font-size: 11px;
    font-weight: 600;
    padding: 3px 8px;
    border-radius: 999px;
    white-space: nowrap;
  }
  .status-badge.ok {
    background: rgba(30, 142, 62, 0.12);
    color: var(--c-green);
  }
  .status-badge.err {
    background: rgba(197, 34, 31, 0.12);
    color: var(--c-danger);
  }
  .status-badge.mute {
    background: var(--c-page);
    color: var(--c-muted);
    border: 1px solid var(--c-border);
  }

  .p-metrics {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 8px;
    background: var(--c-page);
    border: 1px solid var(--c-border);
    border-radius: 8px;
    padding: 8px 10px;
    font-size: 11.5px;
  }
  .metric-item {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .m-lbl {
    color: var(--c-muted);
    font-size: 10.5px;
  }
  .m-val {
    color: var(--c-text);
  }
  .m-val.mono {
    font-family: var(--font-mono, monospace);
  }

  .p-error {
    font-size: 11.5px;
    color: var(--c-danger);
    background: rgba(197, 34, 31, 0.06);
    border: 1px solid rgba(197, 34, 31, 0.2);
    border-radius: 6px;
    padding: 6px 8px;
    word-break: break-word;
  }

  .p-note {
    font-size: 11.5px;
    color: var(--c-muted);
    font-style: italic;
  }

  .p-accounts {
    display: flex;
    flex-direction: column;
    gap: 5px;
    font-size: 11.5px;
  }
  .acc-lbl {
    color: var(--c-muted);
    font-weight: 500;
  }
  .acc-empty {
    color: var(--c-muted);
    font-style: italic;
  }
  .acc-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
  }
  .acc-chip {
    background: rgba(14, 124, 134, 0.1);
    color: var(--c-teal);
    border: 1px solid rgba(14, 124, 134, 0.25);
    padding: 2px 8px;
    border-radius: 999px;
    font-size: 11px;
    font-weight: 500;
  }

  .anti-link-warn {
    display: flex;
    align-items: flex-start;
    gap: 7px;
    font-size: 11.5px;
    color: #92400e;
    background: #fffbeb;
    border: 1px solid #fcd34d;
    border-radius: 8px;
    padding: 7px 10px;
    line-height: 1.4;
  }

  .p-actions {
    display: flex;
    align-items: center;
    gap: 6px;
    padding-top: 8px;
    border-top: 1px solid var(--c-border);
    margin-top: auto;
  }

  .btn-action {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 5px 10px;
    border-radius: 6px;
    border: 1px solid var(--c-border);
    background: var(--c-page);
    color: var(--c-text);
    font-size: 11.5px;
    font-weight: 500;
    cursor: pointer;
  }
  .btn-action:hover:not(:disabled) {
    border-color: var(--c-teal);
    color: var(--c-teal);
  }
  .btn-action.primary {
    background: var(--c-teal);
    color: #fff;
    border-color: var(--c-teal);
  }
  .btn-action.primary:hover:not(:disabled) {
    background: var(--c-teal-dark);
    color: #fff;
  }
  .btn-action.danger {
    margin-left: auto;
    color: var(--c-danger);
    border-color: rgba(197, 34, 31, 0.25);
  }
  .btn-action.danger:hover:not(:disabled) {
    background: rgba(197, 34, 31, 0.08);
    border-color: var(--c-danger);
  }
  .btn-action:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .empty-state {
    background: var(--c-card);
    border: 1px dashed var(--c-border);
    border-radius: 12px;
    padding: 36px 20px;
    text-align: center;
    color: var(--c-muted);
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
  }
  .empty-state p {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--c-text);
  }
  .empty-state .hint {
    font-size: 12.5px;
    max-width: 520px;
    line-height: 1.45;
  }

  .form-body {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .form-row.two-col {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px;
  }
  .form-row.host-port-row {
    display: grid;
    grid-template-columns: 1fr 110px;
    gap: 10px;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .field label:not(.clear-pass-toggle) {
    font-size: 11.5px;
    font-weight: 600;
    color: var(--c-muted);
  }
  .field input:not([type="checkbox"]),
  .field select {
    padding: 7px 10px;
    border-radius: 7px;
    border: 1px solid var(--c-border);
    background: var(--c-page);
    color: var(--c-text);
    font-size: 12.5px;
    outline: none;
  }
  .field input:not([type="checkbox"]):focus,
  .field select:focus {
    border-color: var(--c-teal);
  }

  .pass-input-wrap {
    position: relative;
    display: flex;
    align-items: center;
  }
  .pass-input-wrap input {
    width: 100%;
    padding-right: 32px;
  }
  .btn-eye {
    position: absolute;
    right: 6px;
    background: transparent;
    border: none;
    color: var(--c-muted);
    cursor: pointer;
    padding: 4px;
    display: flex;
    align-items: center;
  }
  .clear-pass-toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 11.5px;
    font-weight: 500;
    color: var(--c-muted);
    margin-top: 3px;
    cursor: pointer;
  }
  .clear-pass-toggle input[type="checkbox"] {
    width: auto;
    height: auto;
    margin: 0;
    padding: 0;
    cursor: pointer;
    accent-color: var(--c-teal);
  }

  .form-err {
    font-size: 12px;
    color: var(--c-danger);
    background: rgba(197, 34, 31, 0.08);
    border: 1px solid rgba(197, 34, 31, 0.25);
    border-radius: 6px;
    padding: 8px 10px;
  }

  .del-body p {
    margin: 0 0 10px;
    font-size: 13px;
    line-height: 1.5;
    color: var(--c-text);
  }
  .del-body p.proxy-delete-busy-warn {
    margin: 10px 0 0;
    padding: 8px 10px;
    border-radius: 6px;
    font-size: 12px;
    line-height: 1.45;
    color: #92400e;
    background: #fffbeb;
    border: 1px solid #fcd34d;
  }

  .modal-footer-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
