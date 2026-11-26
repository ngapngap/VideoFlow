<script lang="ts">
  import { onMount } from "svelte";
  import { api, type PluginInstance, type PluginSummary, type PluginStatus } from "./api";

  let plugins: PluginSummary[] = [];
  let busy = false;
  let error = "";
  let newIds: Record<string, string> = {};
  let statuses: Record<string, PluginStatus> = {};
  let capabilities: Record<string, any> = {};
  let quotas: Record<string, any> = {};
  let configs: Record<string, Record<string, unknown>> = {};
  let secrets: Record<string, Record<string, string>> = {};
  let openConfig = "";

  const key = (p: string, i: string) => `${p}:${i}`;
  const instanceId = (i: PluginInstance) => i.instance_id || "default";
  const obj = (v: unknown): Record<string, any> => v && typeof v === "object" ? v as Record<string, any> : {};

  async function refresh() {
    busy = true; error = "";
    try {
      plugins = await api.plugin_list();
      // Populate local config drafts from public values only. Secret values never leave the host.
      for (const p of plugins) for (const i of p.instances ?? []) {
        const k = key(p.manifest.id, instanceId(i));
        configs[k] ??= { ...(i.config ?? {}) };
        secrets[k] ??= {};
      }
    } catch (e) { error = String(e); }
    finally { busy = false; }
  }
  onMount(refresh);

  async function run<T>(fn: () => Promise<T>, done?: (v: T) => void) {
    busy = true; error = "";
    try { const v = await fn(); done?.(v); await refresh(); }
    catch (e) { error = String(e); }
    finally { busy = false; }
  }
  async function add(p: PluginSummary) {
    const id = (newIds[p.manifest.id] ?? "default").trim();
    if (!/^[a-z0-9-]+$/.test(id)) { error = "ID instance chỉ được gồm chữ thường, số và dấu gạch ngang."; return; }
    await run(() => api.plugin_add_instance(p.manifest.id, id));
  }
  async function toggleTrust(p: PluginSummary, i: PluginInstance) {
    await run(() => api.plugin_set_trust(p.manifest.id, instanceId(i), !i.trusted));
  }
  async function toggleEnabled(p: PluginSummary, i: PluginInstance) {
    await run(() => api.plugin_set_enabled(p.manifest.id, instanceId(i), !i.enabled));
  }
  async function lifecycle(p: PluginSummary, i: PluginInstance) {
    const k = key(p.manifest.id, instanceId(i));
    await run(() => (statuses[k]?.running || i.health && i.health !== "stopped") ? api.plugin_stop(p.manifest.id, instanceId(i)) : api.plugin_start(p.manifest.id, instanceId(i)), v => statuses[k] = v);
  }
  async function inspect(p: PluginSummary, i: PluginInstance) {
    const k = key(p.manifest.id, instanceId(i));
    busy = true;
    try {
      const id = p.manifest.id, iid = instanceId(i);
      const [s, c, q] = await Promise.all([api.plugin_status(id, iid), api.plugin_capabilities(id, iid), api.plugin_quota(id, iid)]);
      statuses[k] = s; capabilities[k] = c; quotas[k] = q;
    } catch (e) { error = String(e); }
    finally { busy = false; }
  }
  async function saveConfig(p: PluginSummary, i: PluginInstance) {
    const k = key(p.manifest.id, instanceId(i));
    const secretDrafts = secrets[k] ?? {};
    await run(() => api.plugin_save_config(p.manifest.id, instanceId(i), configs[k] ?? {}, secretDrafts));
    // Retain only unsaved secrets on failure; successful refresh never reads them back.
    if (!error) secrets[k] = {};
  }
  async function invokeAction(p: PluginSummary, i: PluginInstance, actionId: string, confirm?: string | boolean) {
    const prompt = typeof confirm === "string" ? confirm : `Chạy thao tác ${actionId}?`;
    if (confirm && !window.confirm(prompt)) return;
    await run(() => api.plugin_action(p.manifest.id, instanceId(i), actionId, {}));
    await inspect(p, i);
  }
  function schemaFields(p: PluginSummary) { return Object.entries(obj(p.manifest.config_schema).properties ?? {}); }
  function isSecret(schema: any) { return schema?.['x-secret'] === true || schema?.format === "password"; }
  function fieldValue(p: PluginSummary, i: PluginInstance, name: string, schema: any) {
    const k = key(p.manifest.id, instanceId(i));
    return isSecret(schema) ? (secrets[k]?.[name] ?? "") : (configs[k]?.[name] ?? schema?.default ?? "");
  }
  function secretConfigured(i: PluginInstance, name: string) { return (i.configured_secrets ?? []).includes(name); }
  function setField(p: PluginSummary, i: PluginInstance, name: string, schema: any, value: unknown) {
    const k = key(p.manifest.id, instanceId(i));
    if (isSecret(schema)) secrets[k] = { ...(secrets[k] ?? {}), [name]: String(value) };
    else configs[k] = { ...(configs[k] ?? {}), [name]: value };
  }
  function fieldInput(e: Event, p: PluginSummary, i: PluginInstance, name: string, schema: any) { setField(p, i, name, schema, (e.target as HTMLInputElement).value); }
  function fieldCheck(e: Event, p: PluginSummary, i: PluginInstance, name: string, schema: any) { setField(p, i, name, schema, (e.target as HTMLInputElement).checked); }
  function fieldSelect(e: Event, p: PluginSummary, i: PluginInstance, name: string, schema: any) { setField(p, i, name, schema, (e.target as HTMLSelectElement).value); }
  function fieldInteger(e: Event, p: PluginSummary, i: PluginInstance, name: string, schema: any) { setField(p, i, name, schema, Number((e.target as HTMLInputElement).value)); }
  function pretty(v: unknown) { return JSON.stringify(v, null, 2); }
  function dynamicOptions(p: PluginSummary, i: PluginInstance, schema: any): string[] {
    const path = String(schema?.['x-dynamic-enum'] ?? "");
    const m = /^capabilities\.([^.]+)\.models\[\]\.id$/.exec(path);
    if (!m) return [];
    const k = key(p.manifest.id, instanceId(i));
    const list = capabilities[k]?.[m[1]]?.models;
    return Array.isArray(list) ? list.map((x: any) => String(x.id)).filter(Boolean) : [];
  }
  function retirementWarning(date: unknown): boolean {
    if (typeof date !== "string") return false;
    const ms = Date.parse(date);
    return Number.isFinite(ms) && ms >= Date.now() && ms - Date.now() <= 7 * 86400000;
  }
  function supportedSchema(s: any): boolean {
    return ["string", "integer", "boolean"].includes(s?.type) || Array.isArray(s?.enum) || Boolean(s?.["x-dynamic-enum"]);
  }
  function configToggle(e: Event, k: string) {
    openConfig = (e.currentTarget as HTMLDetailsElement).open ? k : "";
  }
</script>

<section class="plugins">
  <div class="head"><div><h2>Plugin providers</h2><p>Quản lý gói, instance, quyền tin cậy, cấu hình và trạng thái.</p></div>
    <button class="btn-secondary" disabled={busy} on:click={refresh}>{busy ? "Đang tải…" : "Làm mới"}</button>
  </div>
  {#if error}<div class="notice bad">{error}</div>{/if}
  {#if plugins.length === 0 && !busy}<div class="notice">Chưa tìm thấy plugin. Plugin được đặt trong thư mục cài đặt hoặc AppData.</div>{/if}
  {#each plugins as p (p.manifest.id)}
    <article class="plugin-card">
      <div class="plugin-head"><div><h3>{p.manifest.name} <small>{p.manifest.version}</small></h3><div class="muted">{p.manifest.id} · Plugin API {p.manifest.plugin_api_version}{p.bundled ? " · đi kèm ứng dụng" : " · do người dùng cài"}</div></div></div>
      {#if p.error}<div class="notice bad">{p.error}</div>{/if}
      {#if p.manifest.permissions?.length}<div class="muted">Quyền khai báo: {p.manifest.permissions.join(", ")}</div>{/if}
      <div class="add-row"><input class="input" placeholder="ID instance (vd account-a)" bind:value={newIds[p.manifest.id]} />
        <button class="btn-secondary" disabled={busy} on:click={() => add(p)}>Thêm instance</button></div>
      {#each p.instances ?? [] as i (instanceId(i))}
        {@const iid = instanceId(i)}{@const k = key(p.manifest.id, iid)}{@const st = statuses[k]}
        <div class="instance">
          <div class="instance-top"><div><strong>{iid}</strong>
            <span class:good={i.trusted} class="badge">{i.trusted ? "Đã tin cậy" : "Chưa tin cậy"}</span>
            <span class:good={i.enabled} class="badge">{i.enabled ? "Đang bật" : "Đã tắt"}</span>
            {#if st}<span class="badge" class:good={st.running && st.health === "ok"}>{st.running ? st.health : "đã dừng"}</span>{/if}
          </div><div class="actions">
            <button class="btn-secondary" disabled={busy} on:click={() => toggleTrust(p, i)}>{i.trusted ? "Thu hồi tin cậy" : "Tin cậy"}</button>
            <button class="btn-secondary" disabled={busy} on:click={() => toggleEnabled(p, i)}>{i.enabled ? "Tắt" : "Bật"}</button>
            <button class="btn-secondary" disabled={busy || !i.enabled || (!i.trusted && !i.bundled)} on:click={() => lifecycle(p, i)}>{st?.running ? "Dừng" : "Khởi động"}</button>
            <button class="btn-secondary" disabled={busy} on:click={() => inspect(p, i)}>Trạng thái</button>
          </div></div>
          {#if i.reason || st?.reason}<div class="notice">{i.reason ?? st?.reason}</div>{/if}
          {#if i.restart_required}<div class="notice">Cấu hình đã lưu; cần khởi động lại instance để áp dụng.</div>{/if}
          {#if st?.checks}<div class="checks">{#each Object.entries(st.checks) as [name, check]}<span><b>{name}</b>: {check.status}{check.message ? ` — ${check.message}` : ""}</span>{/each}</div>{/if}
          {#if capabilities[k]}
            <details><summary>Capabilities{capabilities[k].stale ? " · dữ liệu cũ" : ""}</summary>
              {#each (capabilities[k].kinds ?? []) as kind (kind)}
                <div class="field-group-title">{kind} · {(capabilities[k][kind]?.modes ?? []).join(", ")}</div>
                {#each (capabilities[k][kind]?.models ?? []) as model (model.id)}
                  <div class="cap-model"><strong>{model.label ?? model.id}</strong> <code>{model.id}</code>
                    {#if model.aspects?.length}<span>Aspect: {model.aspects.join(", ")}</span>{/if}
                    {#if model.durations?.length}<span>Thời lượng: {model.durations.join(", ")} giây</span>{/if}
                    {#if model.retires_on}<span class:retiring={retirementWarning(model.retires_on)}>Nghỉ ngày {model.retires_on}{retirementWarning(model.retires_on) ? " · còn tối đa 7 ngày" : ""}</span>{/if}
                  </div>
                {/each}
              {/each}
              <details><summary>JSON đầy đủ</summary><pre>{pretty(capabilities[k])}</pre></details>
            </details>
          {/if}
          {#if quotas[k]}<details><summary>Quota</summary><pre>{pretty(quotas[k])}</pre></details>{/if}
          <details open={openConfig === k} on:toggle={(e) => configToggle(e, k)}>
            <summary>Cấu hình instance</summary>
            {#if schemaFields(p).length === 0}<div class="muted">Plugin không khai báo trường cấu hình.</div>{/if}
            {#each schemaFields(p) as [name, rawSchema] (name)}
              {@const schema = obj(rawSchema)}
              <div class="label">{schema.title ?? name}{schema.description ? ` — ${schema.description}` : ""}</div>
              {#if !supportedSchema(schema)}<div class="notice bad">Kiểu cấu hình “{schema.type ?? "chưa khai báo"}” chưa được hỗ trợ; trường này cần cập nhật UI plugin.</div>
              {:else if isSecret(schema)}<input class="input full" type="password" autocomplete="new-password" placeholder={secretConfigured(i, name) ? "Đã lưu · nhập để thay thế" : "Chưa cấu hình"} value={fieldValue(p, i, name, schema)} on:input={(e) => fieldInput(e, p, i, name, schema)} />
              {:else if schema.type === "boolean"}<label class="check"><input type="checkbox" checked={Boolean(fieldValue(p, i, name, schema))} on:change={(e) => fieldCheck(e, p, i, name, schema)} /> Bật</label>
              {:else if Array.isArray(schema.enum) || dynamicOptions(p, i, schema).length > 0}<select class="input full" value={String(fieldValue(p, i, name, schema))} on:change={(e) => fieldSelect(e, p, i, name, schema)}><option value="">— Chọn —</option>{#each (schema.enum ?? dynamicOptions(p, i, schema)) as option}<option value={option}>{option}</option>{/each}</select>
              {:else if schema.type === "integer"}<input class="input" type="number" min={schema.minimum} max={schema.maximum} value={Number(fieldValue(p, i, name, schema))} on:input={(e) => fieldInteger(e, p, i, name, schema)} />
              {:else}<input class="input full" type="text" value={String(fieldValue(p, i, name, schema))} on:input={(e) => fieldInput(e, p, i, name, schema)} />{/if}
              {#if schema['x-dynamic-enum']}<small class="muted">Danh sách lựa chọn lấy từ capabilities sau khi khởi động và làm mới trạng thái.</small>{/if}
            {/each}
            <button class="btn-primary" disabled={busy} on:click={() => saveConfig(p, i)}>Lưu cấu hình</button>
          </details>
          {#if p.manifest.actions?.length}<div class="action-row"><strong>Thao tác</strong>{#each p.manifest.actions as action (action.id)}<button class="btn-secondary" disabled={busy || (action.scope === "job")} title={action.scope === "job" ? "Thao tác này chỉ khả dụng trên job tương ứng." : action.label} on:click={() => invokeAction(p, i, action.id, action.confirm)}>{action.label}</button>{/each}</div>{/if}
          <button class="remove" disabled={busy} on:click={() => window.confirm(`Xóa instance ${iid}? Thư mục dữ liệu không bị xóa.`) && run(() => api.plugin_remove_instance(p.manifest.id, iid))}>Xóa instance</button>
        </div>
      {/each}
    </article>
  {/each}
</section>

<style>
  .plugins{display:flex;flex-direction:column;gap:12px}.head,.plugin-head,.instance-top{display:flex;justify-content:space-between;align-items:center;gap:12px}.head h2,.plugin-head h3{margin:0}.head p{margin:4px 0 0;color:var(--c-muted)}.plugin-card{border:1px solid var(--c-border);border-radius:10px;padding:16px;background:var(--c-card);display:flex;flex-direction:column;gap:12px}.plugin-head small{font-weight:400;color:var(--c-muted)}.muted{color:var(--c-muted);font-size:var(--fs-hint)}.add-row,.actions,.action-row{display:flex;align-items:center;gap:8px;flex-wrap:wrap}.add-row input{max-width:240px}.instance{border-top:1px solid var(--c-border);padding-top:12px;display:flex;flex-direction:column;gap:10px}.instance-top{align-items:flex-start}.instance-top>div:first-child{display:flex;align-items:center;gap:6px;flex-wrap:wrap}.badge{border-radius:99px;background:var(--c-surface-subtle);padding:3px 8px;color:var(--c-muted);font-size:12px}.badge.good{color:var(--c-teal)}.notice{padding:9px 11px;border-radius:7px;background:var(--c-surface-subtle);color:var(--c-text)}.notice.bad{color:var(--c-danger)}.checks{display:flex;flex-wrap:wrap;gap:8px;color:var(--c-muted);font-size:12px}.cap-model{display:flex;gap:8px;flex-wrap:wrap;align-items:center;padding:6px 0;color:var(--c-muted)}.cap-model strong{color:var(--c-text)}.retiring{color:var(--c-danger)}details{border-top:1px solid var(--c-border);padding-top:8px}summary{cursor:pointer;font-weight:600;margin-bottom:8px}pre{white-space:pre-wrap;overflow-wrap:anywhere;max-height:280px;overflow:auto;background:var(--c-surface-subtle);padding:10px;border-radius:6px}.label{display:block;margin:8px 0 4px}.full{width:100%}.check{display:flex;gap:8px;align-items:center}.remove{align-self:flex-start;color:var(--c-danger);background:none;padding:5px 0}
</style>
