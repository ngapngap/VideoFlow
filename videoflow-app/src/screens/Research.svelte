<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { createEventDispatcher, onDestroy, onMount } from "svelte";
  import { get } from "svelte/store";
  import { api, toAssetUrl, type ProjectInfo, type SettingsView } from "../lib/api";
  import {
    research,
    researchTransfer,
    type ResearchBatch,
    type ResearchBucket,
    type ResearchFilters,
    type ResearchHistoryItem,
    type ResearchIdea,
    type ResearchSnapshot,
    type ResearchSource,
    type ResearchVariant,
    type TransferReceipt,
  } from "../lib/researchApi";
  import { researchBatchId, researchSessionId } from "../lib/researchStore";
  import { ideaReady, parseOptionalThreshold, safeHttpUrl, sourceDisplayValue, variantReadyForTransfer } from "../lib/researchUi";
  import { project, showToast, formatApiError } from "../lib/store";

  type Pane = "sources" | "ideas" | "batches";
  const dispatch = createEventDispatcher<{ configurePrompt: void }>();

  let pane: Pane = "sources";
  let topic = "";
  let settingsView: SettingsView | null = null;
  let snapshot: ResearchSnapshot | null = null;
  let currentBatch: ResearchBatch | null = null;
  let currentBatchId = "";
  let history: ResearchHistoryItem[] = [];
  let sourceRows: ResearchSource[] = [];
  let sourceBucket: ResearchBucket = "qualified";
  let sourceTotal = 0;
  let sourceCursor: string | null = null;
  let nextSourceCursor: string | null = null;
  let priorSourceCursors: Array<string | null> = [];
  let selectedSourceIds: string[] = [];
  let selectionVersion = 0;
  let missingReasons: Record<string, string> = {};
  let exceptionReason = "";
  let selectedIdeaDetails: ResearchIdea | null = null;
  let selectedSourceDetails: ResearchSource | null = null;
  let detailIdea: ResearchIdea | null = null;
  let detailDraft = "";
  let detailSaving = false;
  let uploadingIdeaId = "";
  let filterModalOpen = false;
  let historyOpen = false;
  let detailOpen = false;
  let transferOpen = false;
  let transferBusy = false;
  let transferReceipt: TransferReceipt | null = null;
  let selectedOrdinals: number[] = [];
  let variantDrafts: Record<number, { image_prompt: string; video_prompt: string }> = {};
  let savingVariant = 0;
  let pageLoading = false;
  let startingSearch = false;
  let stoppingSearch = false;
  let preparingIdeas = false;
  let startingBatch = false;
  let loadingBatch = false;
  let actionError = "";
  let countVariants = 5;
  let promptLanguage = "vi";
  let constraints = "";
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  let refreshInFlight = false;
  let destroyed = false;
  let minLikesDraft = "500";
  let minViewsDraft = "30000";
  let countryDraft = "";
  let requireBuyLinkDraft = false;
  let topicError = "";
  let likesError = "";
  let viewsError = "";

  $: filters = settingsView?.settings
    ? ({
        min_likes: settingsView.settings.reels_min_likes,
        min_views: settingsView.settings.reels_min_views,
        country: settingsView.settings.reels_country || "",
        require_buy_link: settingsView.settings.reels_require_buy_link,
      } satisfies ResearchFilters)
    : ({ min_likes: 500, min_views: 30000, country: "", require_buy_link: false } satisfies ResearchFilters);
  $: readyIdeas = (snapshot?.ideas ?? []).filter(ideaReady);
  $: selectedIdea = snapshot?.selection
    ? snapshot.ideas.find((idea) => idea.idea_id === snapshot?.selection?.idea_id) ?? null
    : null;
  $: selectedIdeaIsCurrent = Boolean(selectedIdea && selectedIdea.revision === snapshot?.selection?.revision && ideaReady(selectedIdea));
  $: qualifiedCount = Number(snapshot?.counts.qualified ?? snapshot?.counts.eligible ?? 0);
  $: missingCount = Number(snapshot?.counts.missing ?? snapshot?.counts.incomplete ?? 0);
  $: sourceBusy = isActive(snapshot?.status);
  $: batchBusy = isActive(currentBatch?.status);
  $: promptSourceText = settingsView?.settings.prompt_source === "internal"
    ? "VideoFlow Muse · muse-chat"
    : `Gateway ngoài${settingsView?.settings.endpoint_prompt_model ? ` · ${settingsView.settings.endpoint_prompt_model}` : ""}`;
  $: batchReadyOrdinals = (currentBatch?.variants ?? []).filter(variantReadyForTransfer).map((variant) => variant.ordinal);
  $: selectedTransferOrdinals = selectedOrdinals.filter((ordinal) => batchReadyOrdinals.includes(ordinal));
  $: validResearchForm = !topicError && !likesError && !viewsError && topic.trim().length >= 1 && topic.trim().length <= 1000;

  function makeRequestId(): string {
    return typeof crypto !== "undefined" && "randomUUID" in crypto
      ? crypto.randomUUID()
      : `vf-${Date.now()}-${Math.random().toString(16).slice(2)}`;
  }

  function isActive(status: string | undefined): boolean {
    return Boolean(status && ["queued", "searching", "preparing", "watching", "parsing", "submitting", "running", "submitted", "processing", "generating", "checking", "repairing", "stopping"].includes(status.toLowerCase()));
  }

  function statusLabel(status?: string | null): string {
    const labels: Record<string, string> = {
      queued: "Đang chờ", running: "Đang chạy", submitted: "Đã gửi", processing: "Đang xử lý",
      searching: "Đang tìm", preparing: "Đang chuẩn bị", generating: "Đang tạo prompt", checking: "Đang kiểm tra",
      ready: "Sẵn sàng", completed: "Hoàn tất", partial: "Một phần đạt", failed: "Thất bại",
      stopped: "Đã dừng", cancelled: "Đã hủy", error: "Lỗi", needs_review: "Cần xem lại",
    };
    return labels[(status ?? "").toLowerCase()] ?? status ?? "Chưa chạy";
  }

  function assetUrl(asset: string | null | undefined, key?: string | number): string {
    if (!asset) return "";
    if (/^https?:\/\//i.test(asset) || asset.startsWith("data:")) return asset;
    return toAssetUrl(asset, key);
  }

  function applyThresholdDrafts(): void {
    const likes = parseOptionalThreshold(minLikesDraft);
    const views = parseOptionalThreshold(minViewsDraft);
    likesError = likes.error ?? "";
    viewsError = views.error ?? "";
    if (!settingsView || likes.error || views.error) return;
    settingsView.settings.reels_min_likes = likes.value;
    settingsView.settings.reels_min_views = views.value;
    settingsView.settings.reels_country = countryDraft.trim().toUpperCase();
    settingsView.settings.reels_require_buy_link = requireBuyLinkDraft;
  }

  function validateTopic(): void {
    const value = topic.trim();
    topicError = value.length === 0
      ? "Nhập chủ đề tìm kiếm."
      : value.length > 1000
        ? "Chủ đề tối đa 1.000 ký tự."
        : "";
  }

  onMount(async () => {
    destroyed = false;
    try {
      settingsView = await api.get_settings();
      minLikesDraft = settingsView.settings.reels_min_likes == null ? "" : String(settingsView.settings.reels_min_likes);
      minViewsDraft = settingsView.settings.reels_min_views == null ? "" : String(settingsView.settings.reels_min_views);
      countryDraft = settingsView.settings.reels_country ?? "";
      requireBuyLinkDraft = settingsView.settings.reels_require_buy_link;
    } catch (error) {
      actionError = formatApiError(error);
    }
    const sessionId = get(researchSessionId);
    if (sessionId) await loadSession(sessionId);
    const batchId = get(researchBatchId);
    if (batchId) await loadBatch(batchId);
    if (!sessionId) void loadHistory();
    pollTimer = setInterval(() => void poll(), 1400);
  });

  onDestroy(() => {
    destroyed = true;
    if (pollTimer) clearInterval(pollTimer);
  });

  async function poll(): Promise<void> {
    if (refreshInFlight || destroyed) return;
    const sessionId = get(researchSessionId);
    const batchId = get(researchBatchId);
    if (!sessionId && !batchId) return;
    refreshInFlight = true;
    try {
      if (sessionId && (isActive(snapshot?.status) || snapshot?.ideas.some(i => i.status === "preparing"))) {
        const wasSearching = isActive(snapshot?.status);
        await refreshSnapshot(sessionId);
        if (wasSearching && !isActive(snapshot?.status)) await loadSources(null);
      }
      if (batchId && isActive(currentBatch?.status)) await refreshBatch(batchId);
    } catch (error) { actionError = formatApiError(error); } finally {
      refreshInFlight = false;
    }
  }

  async function refreshSnapshot(sessionId: string): Promise<void> {
    const next = await research("search_get", { session_id: sessionId });
    if (destroyed || get(researchSessionId) !== sessionId) return;
    if (!snapshot || next.seq >= snapshot.seq) {
      snapshot = next;
      selectedSourceIds = [...next.source_selection_ids];
      selectionVersion = Number(next.selection_version ?? next.selection_version_current ?? selectionVersion);
    }
  }

  async function refreshBatch(batchId: string): Promise<void> {
    const next = await research("batch_get", { batch_id: batchId });
    if (destroyed || get(researchBatchId) !== batchId) return;
    if (!currentBatch || Number(next.seq ?? 0) >= Number(currentBatch.seq ?? 0)) {
      currentBatch = next;
      if (currentBatch.status !== "running" && currentBatch.status !== "queued") {
        selectedOrdinals = next.variants.filter(variantReadyForTransfer).map((variant) => variant.ordinal);
      }
      variantDrafts = Object.fromEntries(next.variants.map((variant) => [variant.ordinal, {
        image_prompt: variant.image_prompt,
        video_prompt: variant.video_prompt,
      }]));
    }
  }

  async function loadSession(sessionId: string): Promise<void> {
    actionError = "";
    try {
      const next = await research("search_get", { session_id: sessionId });
      snapshot = next;
      topic = next.topic;
      selectedSourceIds = [...next.source_selection_ids];
      selectionVersion = Number(next.selection_version ?? next.selection_version_current ?? 0);
      researchSessionId.set(sessionId);
      if (next.batches?.length) {
        const lastBatch = next.batches[next.batches.length - 1];
        currentBatchId = lastBatch.batch_id;
        researchBatchId.set(lastBatch.batch_id);
        await loadBatch(lastBatch.batch_id);
      }
      pane = "sources";
      sourceBucket = "qualified";
      priorSourceCursors = [];
      sourceCursor = null;
      await loadSources(null);
    } catch (error) {
      actionError = formatApiError(error);
    }
  }

  async function loadHistory(): Promise<void> {
    try {
      const result = await research("search_list", { cursor: null, limit: 30 });
      history = result.items;
    } catch (error) {
      actionError = formatApiError(error);
    }
  }

  async function openHistory(): Promise<void> {
    historyOpen = true;
    await loadHistory();
  }

  async function loadSources(cursor: string | null): Promise<void> {
    const sessionId = get(researchSessionId);
    if (!sessionId) return;
    pageLoading = true;
    actionError = "";
    try {
      const result = await research("search_sources", {
        session_id: sessionId,
        cursor,
        limit: 20,
        bucket: sourceBucket,
      });
      sourceRows = result.items;
      sourceTotal = result.total;
      sourceCursor = cursor;
      nextSourceCursor = result.next_cursor;
      for (const row of result.items) {
        if (row.accept_missing_reason) missingReasons[row.source_id] = row.accept_missing_reason;
      }
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      pageLoading = false;
    }
  }

  async function nextSourcePage(): Promise<void> {
    if (!nextSourceCursor) return;
    priorSourceCursors = [...priorSourceCursors, sourceCursor];
    await loadSources(nextSourceCursor);
  }

  async function previousSourcePage(): Promise<void> {
    if (!priorSourceCursors.length) return;
    const previous = [...priorSourceCursors];
    const cursor = previous.pop() ?? null;
    priorSourceCursors = previous;
    await loadSources(cursor);
  }

  async function changeBucket(bucket: ResearchBucket): Promise<void> {
    if (sourceBucket === bucket) return;
    sourceBucket = bucket;
    priorSourceCursors = [];
    await loadSources(null);
  }

  async function startSearch(): Promise<void> {
    validateTopic();
    applyThresholdDrafts();
    if (!validResearchForm || !settingsView || sourceBusy || startingSearch) return;
    startingSearch = true;
    actionError = "";
    try {
      const requestFilters: ResearchFilters = {
        min_likes: settingsView.settings.reels_min_likes,
        min_views: settingsView.settings.reels_min_views,
        country: settingsView.settings.reels_country || "",
        require_buy_link: settingsView.settings.reels_require_buy_link,
      };
      const response = await research("search_start", {
        request_id: makeRequestId(),
        topic: topic.trim(),
        filters: requestFilters,
      });
      researchSessionId.set(response.session_id);
      researchBatchId.set(null);
      snapshot = {
        session_id: response.session_id,
        seq: response.seq,
        status: response.status,
        topic: topic.trim(),
        filters: requestFilters,
        reported_total: null,
        counts: response.counts ?? {},
        source_selection_ids: [],
        selection_version: 0,
        ideas: [],
        selection: null,
        batches: [],
      };
      selectedSourceIds = [];
      selectionVersion = 0;
      sourceBucket = "qualified";
      sourceRows = [];
      sourceTotal = 0;
      pane = "sources";
      await refreshSnapshot(response.session_id);
      if (!isActive(snapshot?.status)) await loadSources(null);
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      startingSearch = false;
    }
  }

  async function stopSearch(): Promise<void> {
    const sessionId = get(researchSessionId);
    if (!sessionId || stoppingSearch) return;
    stoppingSearch = true;
    try {
      await research("search_stop", { session_id: sessionId });
      await refreshSnapshot(sessionId);
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      stoppingSearch = false;
    }
  }

  async function resumeSearch(): Promise<void> {
    const sessionId = get(researchSessionId);
    if (!sessionId) return;
    try {
      await research("search_resume", { session_id: sessionId });
      await refreshSnapshot(sessionId);
    } catch (error) {
      actionError = formatApiError(error);
    }
  }

  async function saveSourceSelection(nextRecords: Array<{ source_id: string; accept_missing: boolean; reason?: string }>): Promise<void> {
    if (!snapshot) return;
    try {
      const result = await research("set_sources", {
        session_id: snapshot.session_id,
        source_ids: nextRecords,
        expected_selection_version: selectionVersion,
      });
      selectedSourceIds = [...result.source_selection_ids];
      selectionVersion = result.selection_version;
      snapshot = { ...snapshot, source_selection_ids: selectedSourceIds, selection_version: selectionVersion };
    } catch (error) {
      actionError = formatApiError(error);
      await refreshSnapshot(snapshot.session_id);
    }
  }

  function currentSourceRecords(): Array<{ source_id: string; accept_missing: boolean; reason?: string }> {
    return selectedSourceIds.map((sourceId) => {
      const reason = missingReasons[sourceId];
      return reason
        ? { source_id: sourceId, accept_missing: true, reason }
        : { source_id: sourceId, accept_missing: false };
    });
  }

  async function toggleQualified(source: ResearchSource): Promise<void> {
    const selected = selectedSourceIds.includes(source.source_id);
    const next = selected
      ? selectedSourceIds.filter((id) => id !== source.source_id)
      : [...selectedSourceIds, source.source_id];
    if (selected) delete missingReasons[source.source_id];
    selectedSourceIds = next;
    await saveSourceSelection(currentSourceRecords());
  }

  async function acceptMissing(source: ResearchSource): Promise<void> {
    const reason = exceptionReason.trim();
    if (reason.length < 3) {
      actionError = "Ghi lý do ngoại lệ (ít nhất 3 ký tự) trước khi chọn dòng thiếu dữ liệu.";
      return;
    }
    missingReasons[source.source_id] = reason;
    exceptionReason = "";
    if (!selectedSourceIds.includes(source.source_id)) selectedSourceIds = [...selectedSourceIds, source.source_id];
    await saveSourceSelection(currentSourceRecords());
  }

  async function clearSourceSelection(): Promise<void> {
    selectedSourceIds = [];
    missingReasons = {};
    await saveSourceSelection([]);
  }

  async function prepareIdeas(sourceIds = selectedSourceIds): Promise<void> {
    const sessionId = get(researchSessionId);
    if (!sessionId || !sourceIds.length || preparingIdeas) return;
    preparingIdeas = true;
    actionError = "";
    try {
      const response = await research("prepare_ideas", {
        session_id: sessionId,
        source_ids: sourceIds,
        request_id: makeRequestId(),
      });
      if (snapshot) {
        const current = new Map(snapshot.ideas.map((idea) => [idea.idea_id, idea]));
        response.ideas.forEach((idea) => current.set(idea.idea_id, idea));
        snapshot = { ...snapshot, ideas: [...current.values()] };
      }
      pane = "ideas";
      await refreshSnapshot(sessionId);
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      preparingIdeas = false;
    }
  }

  async function retryIdea(idea: ResearchIdea): Promise<void> {
    await prepareIdeas([idea.source_id]);
  }

  async function inspectSource(source: ResearchSource): Promise<void> {
    selectedSourceDetails = source;
    detailOpen = true;
  }

  async function inspectIdea(idea: ResearchIdea): Promise<void> {
    detailOpen = true;
    detailSaving = false;
    try {
      const result = await research("idea_get", { session_id: snapshot?.session_id ?? "", idea_id: idea.idea_id });
      detailIdea = result.idea;
      detailDraft = result.idea.content ?? "";
    } catch (error) {
      detailIdea = idea;
      detailDraft = idea.content ?? "";
      actionError = formatApiError(error);
    }
  }

  async function saveIdeaContent(): Promise<void> {
    const idea = detailIdea;
    const sessionId = get(researchSessionId);
    if (!idea || !sessionId || detailSaving) return;
    detailSaving = true;
    try {
      const result = await research("idea_update", {
        session_id: sessionId,
        idea_id: idea.idea_id,
        expected_revision: idea.revision,
        content: detailDraft,
        request_id: makeRequestId(),
      });
      detailIdea = result.idea;
      if (snapshot) snapshot = { ...snapshot, ideas: snapshot.ideas.map((x) => x.idea_id === idea.idea_id ? result.idea : x) };
      showToast("success", `Đã tạo revision ${result.idea.revision} cho thẻ ý tưởng.`);
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      detailSaving = false;
    }
  }

  async function handleIdeaImage(event: Event, idea: ResearchIdea): Promise<void> {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = "";
    if (!file || !snapshot) return;
    if (!file.type.startsWith("image/")) {
      actionError = "Chọn tệp ảnh PNG, JPEG hoặc WebP.";
      return;
    }
    if (file.size > 20 * 1024 * 1024) {
      actionError = "Ảnh phải nhỏ hơn 20 MiB.";
      return;
    }
    uploadingIdeaId = idea.idea_id;
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      let binary = "";
      for (let offset = 0; offset < bytes.length; offset += 0x8000) {
        binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
      }
      const result = await research("idea_update", {
        session_id: snapshot.session_id,
        idea_id: idea.idea_id,
        expected_revision: idea.revision,
        image_b64: "data:" + file.type + ";base64," + btoa(binary),
        filename: file.name,
        request_id: makeRequestId(),
      });
      snapshot = { ...snapshot, ideas: snapshot.ideas.map((x) => x.idea_id === idea.idea_id ? result.idea : x) };
      showToast("success", `Đã gắn ảnh vào revision ${result.idea.revision}.`);
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      uploadingIdeaId = "";
    }
  }

  async function chooseIdea(idea: ResearchIdea): Promise<void> {
    if (!snapshot || !ideaReady(idea)) return;
    try {
      const result = await research("select_idea", {
        session_id: snapshot.session_id,
        idea_id: idea.idea_id,
        revision: idea.revision,
        expected_version: snapshot.selection_version_current ?? 0,
      });
      snapshot = { ...snapshot, selection: result.selection, selection_version_current: result.version };
    } catch (error) {
      actionError = formatApiError(error);
      await refreshSnapshot(snapshot.session_id);
    }
  }

  async function startBatch(): Promise<void> {
    if (!snapshot || !selectedIdeaIsCurrent || !selectedIdea || !settingsView || startingBatch) return;
    if (settingsView.settings.prompt_source === "gateway" &&
      (!settingsView.settings.endpoint_prompt_base_url.trim() || !settingsView.settings.endpoint_prompt_model.trim())) {
      actionError = "Cấu hình endpoint prompt trước khi triển khai.";
      dispatch("configurePrompt");
      return;
    }
    startingBatch = true;
    actionError = "";
    try {
      const response = await research("develop_start", {
        request_id: makeRequestId(),
        session_id: snapshot.session_id,
        idea_id: selectedIdea.idea_id,
        revision: selectedIdea.revision,
        n: Math.max(2, Math.min(10, Number(countVariants) || 5)),
        prompt_language: promptLanguage.trim(),
        constraints: constraints.trim(),
      });
      currentBatchId = response.batch_id;
      researchBatchId.set(response.batch_id);
      await loadBatch(response.batch_id);
      pane = "batches";
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      startingBatch = false;
    }
  }

  async function loadBatch(batchId: string): Promise<void> {
    currentBatchId = batchId;
    researchBatchId.set(batchId);
    loadingBatch = true;
    try {
      const batch = await research("batch_get", { batch_id: batchId });
      currentBatch = batch;
      selectedOrdinals = batch.variants.filter(variantReadyForTransfer).map((variant) => variant.ordinal);
      variantDrafts = Object.fromEntries(batch.variants.map((variant) => [variant.ordinal, {
        image_prompt: variant.image_prompt,
        video_prompt: variant.video_prompt,
      }]));
      pane = "batches";
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      loadingBatch = false;
    }
  }

  async function stopBatch(): Promise<void> {
    if (!currentBatchId || !batchBusy) return;
    try {
      await research("batch_stop", { batch_id: currentBatchId });
      await refreshBatch(currentBatchId);
    } catch (error) {
      actionError = formatApiError(error);
    }
  }

  async function resumeBatch(batch: ResearchBatch): Promise<void> {
    try {
      await research("batch_resume", { batch_id: batch.batch_id });
      await loadBatch(batch.batch_id);
    } catch (error) {
      actionError = formatApiError(error);
    }
  }

  function toggleVariant(variant: ResearchVariant): void {
    if (!variantReadyForTransfer(variant)) return;
    selectedOrdinals = selectedOrdinals.includes(variant.ordinal)
      ? selectedOrdinals.filter((ordinal) => ordinal !== variant.ordinal)
      : [...selectedOrdinals, variant.ordinal];
  }

  async function saveVariant(variant: ResearchVariant): Promise<void> {
    const draft = variantDrafts[variant.ordinal];
    if (!currentBatch || !draft || savingVariant) return;
    savingVariant = variant.ordinal;
    try {
      const result = await research("variant_edit", {
        batch_id: currentBatch.batch_id,
        ordinal: variant.ordinal,
        expected_version: variant.version ?? 0,
        image_prompt: draft.image_prompt,
        video_prompt: draft.video_prompt,
        request_id: makeRequestId(),
      });
      currentBatch = {
        ...currentBatch,
        variants: currentBatch.variants.map((item) => item.ordinal === variant.ordinal ? result.variant : item),
      };
      await refreshBatch(currentBatch.batch_id);
      showToast("success", `Đã lưu cặp prompt ${variant.ordinal}.`);
    } catch (error) {
      actionError = formatApiError(error);
      await refreshBatch(currentBatch.batch_id);
    } finally {
      savingVariant = 0;
    }
  }

  let transferProject = "";
  let transferId = "";
  function openTransfer(): void {
    if (!selectedTransferOrdinals.length || !$project || !currentBatch) return;
    transferReceipt = null;
    transferProject = $project.path;
    transferId = makeRequestId();
    transferOpen = true;
  }

  async function confirmTransfer(): Promise<void> {
    if (!currentBatch || !$project || !selectedTransferOrdinals.length || transferBusy) return;
    transferBusy = true;
    actionError = "";
    const expectedProject = transferProject;
    try {
      transferReceipt = await researchTransfer.commit({
        batchId: currentBatch.batch_id,
        ordinals: selectedTransferOrdinals,
        requestId: transferId,
        expectedProject,
      });
      showToast("success", `Đã đưa ${transferReceipt.added} dòng vào ${$project.name}; ${transferReceipt.existing} dòng đã có.`);
    } catch (error) {
      actionError = formatApiError(error);
    } finally {
      transferBusy = false;
    }
  }

  async function reloadCurrentPage(): Promise<void> {
    if (snapshot) await loadSources(sourceCursor);
  }
</script>

<div class="research-screen">
  <div class="research-topline">
    <div><p class="research-eyebrow">Nghiên cứu độc lập với project đang mở</p><h2>Tìm ý tưởng Reels</h2></div>
    <button class="btn-secondary" type="button" on:click={openHistory}>Lịch sử tìm kiếm</button>
  </div>

  {#if actionError}<div class="research-alert" role="alert">{actionError}</div>{/if}

  <nav class="research-tabs" aria-label="Các bước nghiên cứu">
    <button type="button" class:active={pane === "sources"} on:click={() => (pane = "sources")}>1 · Tìm nguồn</button>
    <button type="button" class:active={pane === "ideas"} disabled={!snapshot?.ideas.length} on:click={() => (pane = "ideas")}>2 · Duyệt ý tưởng</button>
    <button type="button" class:active={pane === "batches"} disabled={!snapshot?.batches.length && !currentBatch} on:click={() => (pane = "batches")}>3 · Lượt triển khai</button>
  </nav>

  {#if pane === "sources"}
    <section class="research-card">
      <div class="research-section-head"><div><h3>Tìm nguồn</h3><p>Đọc dữ liệu Reel theo nguồn và giữ riêng các trường chưa biết.</p></div>
        {#if sourceBusy}<button type="button" class="btn-secondary" disabled={stoppingSearch} on:click={stopSearch}>{stoppingSearch ? "Đang dừng…" : "Dừng tìm"}</button>
        {:else if snapshot && snapshot.status === "stopped"}<button type="button" class="btn-secondary" on:click={resumeSearch}>Tiếp tục tìm</button>{/if}
      </div>
      <form class="research-search-form" on:submit|preventDefault={startSearch}>
        <label class="research-field grow" for="research-topic">Chủ đề
          <input id="research-topic" class="input full" maxlength="1000" bind:value={topic} on:input={validateTopic} placeholder="Ví dụ: funny dog owner shirt" disabled={sourceBusy || startingSearch} />
          {#if topicError}<small class="field-error">{topicError}</small>{/if}
        </label>
        <button class="btn-ghost" type="button" on:click={() => (filterModalOpen = true)}>Bộ lọc</button>
        <button class="btn-primary" type="submit" disabled={!validResearchForm || startingSearch || sourceBusy || !settingsView}>{startingSearch ? "Đang bắt đầu…" : "Tìm trên Muse"}</button>
      </form>
      <div class="research-filter-summary">
        <span>Lượt thích trên <b>{filters.min_likes ?? "không giới hạn"}</b></span>
        <span>Lượt xem trên <b>{filters.min_views ?? "không giới hạn"}</b></span>
        <span>Thị trường <b>{filters.country || "Tất cả"}</b></span>
        <span>Link mua trong caption <b>{filters.require_buy_link ? "Bắt buộc" : "Không bắt buộc"}</b></span>
      </div>
      {#if snapshot}
        <div class="research-session-status">
          <div><b>{snapshot.topic}</b><span class:status-live={sourceBusy} class="status-chip">{statusLabel(snapshot.status)}</span></div>
          <span>Phiên {snapshot.session_id.slice(0, 8)} · seq {snapshot.seq}</span>
        </div>
        {#if sourceBusy}
          <div class="research-wait"><span class="spinner"></span><div><b>Đang tìm trên Muse…</b><small>Chỉ hiện những nguồn đã được parse và commit.</small></div></div>
        {:else}
          <div class="research-buckets" role="tablist" aria-label="Kết quả theo bộ lọc">
            <button type="button" class:active={sourceBucket === "qualified"} on:click={() => changeBucket("qualified")}>Đạt bộ lọc ({qualifiedCount})</button>
            <button type="button" class:active={sourceBucket === "missing"} on:click={() => changeBucket("missing")}>Thiếu dữ liệu ({missingCount})</button>
            <span>Đã chọn {selectedSourceIds.length} trên toàn phiên</span>
          </div>
          {#if pageLoading}<div class="research-empty">Đang tải trang nguồn…</div>
          {:else if !sourceRows.length}<div class="research-empty">Không có nguồn trong nhóm này.</div>
          {:else}
            {#if sourceBucket === "qualified"}<label><input type="checkbox" checked={sourceRows.every(r=>selectedSourceIds.includes(r.source_id))} on:change={async(e)=>{
              const visible = sourceRows.map(r=>r.source_id);
              selectedSourceIds = e.currentTarget.checked ? [...new Set([...selectedSourceIds,...visible])] : selectedSourceIds.filter(id=>!visible.includes(id));
              await saveSourceSelection(currentSourceRecords());
            }} /> Chọn các dòng đạt trên trang này</label>{/if}
            <div class="source-grid" aria-label="Kết quả nguồn Reel">
              {#each sourceRows as source (source.source_id)}
                <article class="source-row" class:source-missing={source.filter_status !== "qualified"}>
                  <div class="source-select">
                    {#if sourceBucket === "qualified"}
                      <input aria-label="Chọn nguồn đạt bộ lọc" type="checkbox" checked={selectedSourceIds.includes(source.source_id)} on:change={() => toggleQualified(source)} />
                    {:else}
                      <span aria-hidden="true" class="empty-checkbox"></span>
                    {/if}
                  </div>
                  <button class="source-thumb" type="button" on:click={() => inspectSource(source)} aria-label="Xem chi tiết nguồn">
                    {#if safeHttpUrl(source.thumb)}<img src={safeHttpUrl(source.thumb) ?? ""} alt="Ảnh Reel nguồn" loading="lazy" referrerpolicy="no-referrer" />{:else}<span>Chưa có ảnh</span>{/if}
                  </button>
                  <div class="source-main">
                    <div class="source-title-line"><b>{sourceDisplayValue(source.idea)}</b><span class="status-chip" class:status-good={source.filter_status === "qualified"}>{source.filter_status === "qualified" ? "Đạt" : "Thiếu dữ liệu"}</span></div>
                    <p class="source-caption">{sourceDisplayValue(source.caption)}</p>
                    {#if source.missing_fields?.length}<small class="missing-label">Thiếu: {source.missing_fields.join(", ")}</small>{/if}
                    {#if source.accept_missing_reason}<small class="exception-label">Ngoại lệ: {source.accept_missing_reason}</small>{/if}
                    <div class="source-meta"><span>Thích: {sourceDisplayValue(source.likes)}</span><span>Xem: {sourceDisplayValue(source.views)}</span><span>Thị trường: {sourceDisplayValue(source.country)}</span></div>
                    <div class="source-actions"><button type="button" class="btn-link" on:click={() => inspectSource(source)}>Xem chi tiết</button>
                      {#if safeHttpUrl(source.url)}<a class="btn-link" href={safeHttpUrl(source.url) ?? ""} on:click|preventDefault={() => invoke("research_open_url",{url:source.url}).catch(e=>actionError=formatApiError(e))}>Mở Reel</a>{/if}
                    </div>
                    {#if sourceBucket === "missing" && !selectedSourceIds.includes(source.source_id)}
                      <div class="exception-control"><input class="input" bind:value={exceptionReason} placeholder="Lý do chọn ngoại lệ" aria-label="Lý do chọn nguồn thiếu dữ liệu" />
                        <button type="button" class="btn-secondary" disabled={!exceptionReason.trim()} on:click={() => acceptMissing(source)}>Dùng dù thiếu dữ liệu</button></div>
                    {:else if sourceBucket === "missing"}
                      <button type="button" class="btn-ghost" on:click={() => toggleQualified(source)}>Bỏ chọn ngoại lệ</button>
                    {/if}
                  </div>
                </article>
              {/each}
            </div>
            <div class="research-source-footer"><button type="button" class="btn-ghost" disabled={!selectedSourceIds.length} on:click={clearSourceSelection}>Bỏ chọn tất cả</button>
              <div class="pager"><button type="button" class="btn-ghost" disabled={!priorSourceCursors.length || pageLoading} on:click={previousSourcePage}>‹ Trước</button><span>{sourceTotal} nguồn</span><button type="button" class="btn-ghost" disabled={!nextSourceCursor || pageLoading} on:click={nextSourcePage}>Tiếp ›</button></div>
              <button type="button" class="btn-primary" disabled={!selectedSourceIds.length || preparingIdeas || sourceBusy} on:click={() => prepareIdeas()}>{preparingIdeas ? "Đang chuẩn bị…" : `Chuẩn bị ý tưởng từ ${selectedSourceIds.length} dòng`}</button>
            </div>
          {/if}
        {/if}
      {:else}
        <div class="research-empty">Bắt đầu bằng cách nhập chủ đề. Công việc vẫn có thể mở lại từ Lịch sử.</div>
      {/if}
    </section>
  {:else if pane === "ideas"}
    <section class="research-card">
      <div class="research-section-head"><div><h3>Duyệt ý tưởng</h3><p>{readyIdeas.length}/{snapshot?.ideas.length ?? 0} thẻ sẵn sàng · Chọn đúng một thẻ để triển khai.</p></div>
        <button type="button" class="btn-secondary" on:click={() => (pane = "sources")}>Quay lại nguồn</button>
      </div>
      {#if selectedIdeaIsCurrent && selectedIdea}
        <div class="selected-idea-preview"><img src={assetUrl(selectedIdea.asset, selectedIdea.revision)} alt="Ảnh ý tưởng đang chọn" /><div><small>Đang chọn revision {selectedIdea.revision}</small><b>{selectedIdea.title ?? "Ý tưởng"}</b><p>{selectedIdea.content}</p>
          <div class="selected-idea-controls"><label>Số phương án <input class="input count-input" type="number" min="2" max="10" bind:value={countVariants} /></label>
            <label>Ngôn ngữ prompt <select class="input" bind:value={promptLanguage}><option value="vi">Tiếng Việt</option><option value="en">English</option></select></label>
          </div><label class="research-field">Ràng buộc sản phẩm và đầu ra<textarea class="input full" rows="3" bind:value={constraints}></textarea></label>
          <div class="prompt-source-row"><span>Nguồn tạo prompt: <b>{promptSourceText}</b></span><button type="button" class="btn-link" on:click={() => dispatch("configurePrompt")}>Cấu hình nguồn tạo prompt</button></div>
          <button class="btn-primary" type="button" disabled={startingBatch || !selectedIdeaIsCurrent} on:click={startBatch}>{startingBatch ? "Đang tạo lượt…" : "Bắt đầu triển khai"}</button>
        </div></div>
      {:else}
        <div class="research-empty">Chọn một thẻ sẵn sàng. Việc xem hoặc chọn thẻ không gửi yêu cầu tạo prompt.</div>
      {/if}
      <div class="idea-grid">
        {#each snapshot?.ideas ?? [] as idea (idea.idea_id)}
          <article class="idea-card" class:selected={snapshot?.selection?.idea_id === idea.idea_id && snapshot?.selection?.revision === idea.revision}>
            <div class="idea-image-wrap">
              {#if idea.asset}<img class="idea-image" src={assetUrl(idea.asset, idea.revision)} alt={idea.title ?? "Ảnh ý tưởng"} loading="lazy" referrerpolicy="no-referrer" />{:else}<div class="idea-image-placeholder">{idea.status === "failed" ? "Cần ảnh" : statusLabel(idea.status)}</div>{/if}
              {#if idea.status !== "ready"}<span class="status-chip">{statusLabel(idea.status)}</span>{/if}
            </div>
            <div class="idea-card-body"><small>Revision {idea.revision}</small><h4>{idea.title ?? "Đang chuẩn bị ý tưởng"}</h4><p>{idea.content ?? idea.error ?? "Đang chờ kết quả từ Muse."}</p>
              <div class="idea-card-actions"><button type="button" class="btn-secondary" on:click={() => inspectIdea(idea)}>Xem chi tiết</button>
                {#if idea.status === "failed"}<button type="button" class="btn-ghost" disabled={preparingIdeas} on:click={() => retryIdea(idea)}>Thử lại thẻ này</button>{/if}
              </div>
              <label class="idea-image-upload">Chọn ảnh từ máy<input type="file" accept="image/png,image/jpeg,image/webp" disabled={uploadingIdeaId === idea.idea_id} on:change={(event) => handleIdeaImage(event, idea)} /></label>
              <label class="radio-select"><input type="radio" name="selected-idea" disabled={!ideaReady(idea)} checked={snapshot?.selection?.idea_id === idea.idea_id && snapshot?.selection?.revision === idea.revision} on:change={() => chooseIdea(idea)} />
                <span>{ideaReady(idea) ? "Chọn ý tưởng" : "Chưa sẵn sàng"}</span></label>
            </div>
          </article>
        {/each}
      </div>
      {#if !snapshot?.ideas.length}<div class="research-empty">Chưa có thẻ. Chọn nguồn và bấm “Chuẩn bị ý tưởng từ X dòng”.</div>{/if}
    </section>
  {:else}
    <section class="research-card">
      <div class="research-section-head"><div><h3>Lượt triển khai</h3><p>GENERATE → CHECK → READY · Các cặp lỗi bị loại khỏi bàn giao.</p></div>
        {#if batchBusy}<button type="button" class="btn-secondary" on:click={stopBatch}>Dừng triển khai</button>{/if}
      </div>
      {#if snapshot?.batches.length}
        <div class="batch-history-list"><span>Lượt:</span>{#each snapshot.batches as item (item.batch_id)}<button type="button" class:active={item.batch_id === currentBatchId} on:click={() => loadBatch(item.batch_id)}>N={item.n} · {statusLabel(item.status)}</button>{/each}</div>
      {/if}
      {#if loadingBatch}<div class="research-empty">Đang tải trạng thái lượt…</div>
      {:else if currentBatch}
        {#if currentBatch.card}<div class="selected-idea-preview"><img alt="Ảnh đã duyệt" src={assetUrl(currentBatch.card.asset)} /><div><b>{currentBatch.card.title}</b><p>{currentBatch.card.content}</p></div></div>{/if}
        <div class="batch-summary"><div><span class="status-chip" class:status-good={currentBatch.status === "ready"}>{statusLabel(currentBatch.status)}</span><b>N={currentBatch.n} · revision {currentBatch.revision}</b><small>Batch {currentBatch.batch_id.slice(0, 8)} · {currentBatch.prompt_source ?? settingsView?.settings.prompt_source ?? "prompt"}{#if currentBatch.prompt_model} · {currentBatch.prompt_model}{/if}</small></div>
          <div class="batch-summary-metrics"><span>{currentBatch.variants.filter(variantReadyForTransfer).length} cặp đạt</span><span>{currentBatch.variants.length}/{currentBatch.n} có kết quả</span></div></div>
        {#if currentBatch.error}<div class="research-alert" role="status">{currentBatch.error}</div>{/if}
        {#if currentBatch.check_summary}<p class="check-summary">{currentBatch.check_summary}</p>{/if}
        <div class="variant-list">
          {#each currentBatch.variants as variant (variant.ordinal)}
            <article class="variant-card" class:variant-ready={variantReadyForTransfer(variant)}>
              <header><div><h4>Phương án {variant.ordinal}: {variant.title || "Đang tạo"}</h4><p>{variant.conflict}</p></div><span class="status-chip" class:status-good={variantReadyForTransfer(variant)}>{statusLabel(variant.status)} · {variant.check_result?.verdict ?? "CHECK chưa xong"}</span></header>
              {#if variant.check_result}<p class="variant-check"><b>{variant.check_result.code ?? "CHECK"}:</b> {variant.check_result.evidence ?? "Chưa có bằng chứng."}{#if variant.check_result.change}<br /><b>Thay đổi:</b> {variant.check_result.change}{/if}</p>{/if}
              <div class="paired-prompts"><label>Prompt tạo ảnh<textarea class="input full prompt-editor" rows="8" bind:value={variantDrafts[variant.ordinal].image_prompt} disabled={batchBusy}></textarea></label>
                <label>Prompt tạo video<textarea class="input full prompt-editor" rows="8" bind:value={variantDrafts[variant.ordinal].video_prompt} disabled={batchBusy}></textarea></label></div>
              <div class="variant-footer"><div class="payoff-summary"><b>Reveal:</b> {variant.reveal}<br /><b>Payoff:</b> {variant.payoff}</div>
                <div class="variant-actions"><button type="button" class="btn-secondary" disabled={batchBusy || savingVariant === variant.ordinal} on:click={() => saveVariant(variant)}>{savingVariant === variant.ordinal ? "Đang lưu…" : "Lưu cặp prompt"}</button>
                  <label class="variant-select"><input type="checkbox" checked={selectedOrdinals.includes(variant.ordinal)} disabled={!variantReadyForTransfer(variant)} on:change={() => toggleVariant(variant)} /> Đưa vào project</label></div></div>
            </article>
          {/each}
        </div>
        {#if currentBatch.status === "stopped" || currentBatch.status === "queued"}
          <button type="button" class="btn-secondary" on:click={() => currentBatch && resumeBatch(currentBatch)}>Tiếp tục lượt</button>
        {/if}
        <div class="transfer-bar"><div><b>Đưa vào project…</b><span>{selectedTransferOrdinals.length} cặp sẵn sàng · ảnh tham chiếu được giữ cùng batch</span></div>
          <button type="button" class="btn-primary" disabled={!$project || !selectedTransferOrdinals.length || batchBusy} on:click={openTransfer}>Xem trước và xác nhận</button></div>
      {:else}
        <div class="research-empty">Chưa có lượt triển khai. Chọn một ý tưởng sẵn sàng trong Duyệt ý tưởng.</div>
      {/if}
      {#if transferReceipt}<div class="transfer-success" role="status">Đã bàn giao: {transferReceipt.added} dòng mới, {transferReceipt.existing} dòng đã có trong project.</div>{/if}
    </section>
  {/if}

  {#if filterModalOpen}
    <div class="research-backdrop" role="presentation" on:click={(e) => e.target === e.currentTarget && (filterModalOpen = false)}>
      <section class="research-modal" role="dialog" aria-modal="true" aria-labelledby="filter-heading"><header><h3 id="filter-heading">Bộ lọc tìm nguồn</h3><button type="button" class="btn-ghost" on:click={() => (filterModalOpen = false)}>Đóng</button></header>
        <label class="research-field" for="filter-likes">Lượt thích trên<input id="filter-likes" class="input full" type="number" min="0" max="2147483647" bind:value={minLikesDraft} on:input={applyThresholdDrafts} />{#if likesError}<small class="field-error">{likesError}</small>{/if}</label>
        <label class="research-field" for="filter-views">Lượt xem trên<input id="filter-views" class="input full" type="number" min="0" max="2147483647" bind:value={minViewsDraft} on:input={applyThresholdDrafts} />{#if viewsError}<small class="field-error">{viewsError}</small>{/if}</label>
        <label class="research-field" for="filter-country">Thị trường mục tiêu<input id="filter-country" class="input full" maxlength="2" bind:value={countryDraft} on:input={applyThresholdDrafts} placeholder="Tất cả hoặc mã ISO 2 chữ cái" /></label>
        <label class="research-check"><input type="checkbox" bind:checked={requireBuyLinkDraft} on:change={applyThresholdDrafts} /> Yêu cầu URL mua và CTA trong caption của chính Reel</label>
        <p class="hint">Giá trị trong Settings được dùng làm mặc định. Thay đổi bộ lọc ở đây chỉ ảnh hưởng lần tìm kế tiếp.</p>
        <footer><button type="button" class="btn-secondary" on:click={() => { minLikesDraft = "500"; minViewsDraft = "30000"; countryDraft = ""; requireBuyLinkDraft = false; applyThresholdDrafts(); }}>Khôi phục mặc định</button><button type="button" class="btn-primary" disabled={Boolean(likesError || viewsError)} on:click={() => { applyThresholdDrafts(); filterModalOpen = false; }}>Dùng bộ lọc</button></footer>
      </section>
    </div>
  {/if}

  {#if historyOpen}
    <div class="research-backdrop" role="presentation" on:click={(e) => e.target === e.currentTarget && (historyOpen = false)}><section class="research-modal" role="dialog" aria-modal="true" aria-labelledby="history-heading">
      <header><h3 id="history-heading">Lịch sử tìm kiếm</h3><button type="button" class="btn-ghost" on:click={() => (historyOpen = false)}>Đóng</button></header>
      {#if !history.length}<p class="research-empty">Chưa có phiên tìm kiếm.</p>{:else}<div class="history-list">{#each history as item (item.session_id)}<button type="button" on:click={async () => { await loadSession(item.session_id); historyOpen = false; }}><span><b>{item.topic}</b><small>{item.created_at}</small></span><span class="status-chip">{statusLabel(item.status)}</span></button>{/each}</div>{/if}
    </section></div>
  {/if}

  {#if detailOpen}
    <div class="research-backdrop" role="presentation" on:click={(e) => e.target === e.currentTarget && (detailOpen = false)}><section class="research-modal detail-modal" role="dialog" aria-modal="true" aria-labelledby="detail-heading">
      <header><h3 id="detail-heading">{detailIdea ? "Chi tiết thẻ ý tưởng" : "Chi tiết nguồn Reel"}</h3><button type="button" class="btn-ghost" on:click={() => { detailOpen = false; detailIdea = null; selectedSourceDetails = null; }}>Đóng</button></header>
      {#if detailIdea}
        {#if detailIdea.asset}<img class="detail-image" src={assetUrl(detailIdea.asset, detailIdea.revision)} alt="Ảnh thẻ ý tưởng" />{/if}
        <p><b>{detailIdea.title}</b> · revision {detailIdea.revision}</p><label class="research-field">Nội dung ý tưởng<textarea class="input full" rows="8" bind:value={detailDraft} disabled={detailSaving}></textarea></label>
        {#if detailIdea.history?.length}<details><summary>Lịch sử revision</summary>{#each detailIdea.history as rev}<p>Revision {rev.revision} · {rev.title ?? ""} · {rev.content ?? ""}</p>{/each}</details>{/if}
        <footer><button type="button" class="btn-primary" disabled={detailSaving || detailDraft === (detailIdea.content ?? "")} on:click={saveIdeaContent}>{detailSaving ? "Đang lưu…" : "Lưu thành revision mới"}</button></footer>
      {:else if selectedSourceDetails}
        {#if safeHttpUrl(selectedSourceDetails.thumb)}<img class="detail-image" src={safeHttpUrl(selectedSourceDetails.thumb) ?? ""} alt="Ảnh Reel nguồn" />{/if}
        <p><b>Caption nguyên văn</b></p><pre class="source-full-text">{sourceDisplayValue(selectedSourceDetails.caption)}</pre>
        <dl class="source-detail-grid"><dt>Lượt thích</dt><dd>{sourceDisplayValue(selectedSourceDetails.likes)}</dd><dt>Lượt xem</dt><dd>{sourceDisplayValue(selectedSourceDetails.views)}</dd><dt>Thị trường</dt><dd>{sourceDisplayValue(selectedSourceDetails.country)}</dd><dt>Chứng cứ thị trường</dt><dd>{sourceDisplayValue(selectedSourceDetails.market_evidence)}</dd><dt>Link mua</dt><dd>{sourceDisplayValue(selectedSourceDetails.buy_url)}</dd><dt>CTA</dt><dd>{sourceDisplayValue(selectedSourceDetails.cta)}</dd></dl>
        {#if safeHttpUrl(selectedSourceDetails.url)}<a href={safeHttpUrl(selectedSourceDetails.url) ?? ""} on:click|preventDefault={() => selectedSourceDetails && invoke("research_open_url",{url:selectedSourceDetails.url}).catch(e=>actionError=formatApiError(e))}>Mở Reel nguồn</a>{/if}
      {/if}
    </section></div>
  {/if}

  {#if transferOpen && currentBatch}
    <div class="research-backdrop" role="presentation" on:click={(e) => e.target === e.currentTarget && !transferBusy && (transferOpen = false)}><section class="research-modal transfer-modal" role="dialog" aria-modal="true" aria-labelledby="transfer-heading">
      <header><h3 id="transfer-heading">Xác nhận đưa vào project</h3><button type="button" class="btn-ghost" disabled={transferBusy} on:click={() => (transferOpen = false)}>Đóng</button></header>
      <p>Project đích: <b>{$project?.name}</b></p><p class="subtle selectable">{transferProject}</p><div class="transfer-image-row"><img src={assetUrl(currentBatch.card?.asset, currentBatch.revision)} alt="Ảnh tham chiếu" /><span>Ảnh tham chiếu revision {currentBatch.revision} · {selectedTransferOrdinals.length} dòng mới/đã đối soát</span></div>
      {#each currentBatch.variants.filter((variant) => selectedTransferOrdinals.includes(variant.ordinal)) as variant (variant.ordinal)}<article class="transfer-preview-row"><h4>{variant.ordinal}. {variant.title}</h4><p><b>Prompt ảnh</b><br />{variantDrafts[variant.ordinal]?.image_prompt ?? variant.image_prompt}</p><p><b>Prompt video</b><br />{variantDrafts[variant.ordinal]?.video_prompt ?? variant.video_prompt}</p></article>{/each}
      {#if transferReceipt}<div class="transfer-success">Bàn giao hoàn tất: {transferReceipt.added} dòng mới, {transferReceipt.existing} dòng đã có.</div>{/if}
      <footer><button type="button" class="btn-secondary" disabled={transferBusy} on:click={() => (transferOpen = false)}>Hủy</button><button type="button" class="btn-primary" disabled={transferBusy || !selectedTransferOrdinals.length || !$project} on:click={confirmTransfer}>{transferBusy ? "Đang ghi vào project…" : "Xác nhận bàn giao"}</button></footer>
    </section></div>
  {/if}
</div>

<style>
  .research-screen { display:flex; flex-direction:column; gap:14px; padding:18px 20px 32px; color:var(--c-text); }
  .research-topline,.research-section-head,.research-session-status,.research-buckets,.research-source-footer,.research-section-head>div,.research-session-status>div,.source-title-line,.source-actions,.source-meta,.source-detail-grid,.variant-card>header,.variant-footer,.variant-actions,.transfer-bar,.batch-summary,.batch-summary>div,.batch-summary-metrics,.batch-history-list,.selected-idea-controls,.prompt-source-row,.transfer-image-row { display:flex; align-items:center; justify-content:space-between; gap:12px; }
  .research-topline h2,.research-section-head h3 { margin:2px 0; }
  .research-eyebrow { margin:0; color:var(--c-teal); font-size:12px; font-weight:700; text-transform:uppercase; letter-spacing:.06em; }
  .research-tabs,.research-buckets { display:flex; flex-wrap:wrap; gap:8px; border-bottom:1px solid var(--c-border); padding-bottom:8px; }
  .research-tabs button,.research-buckets button,.batch-history-list button { border:1px solid var(--c-border); border-radius:8px; padding:8px 12px; background:var(--c-surface); color:var(--c-text); cursor:pointer; }
  .research-tabs button.active,.research-buckets button.active,.batch-history-list button.active { border-color:var(--c-teal); color:var(--c-teal-dark); background:var(--c-teal-bg); }
  .research-tabs button:disabled { opacity:.5; cursor:not-allowed; }
  .research-card { background:var(--c-surface); border:1px solid var(--c-border); border-radius:12px; padding:18px; box-shadow:var(--shadow-sm); }
  .research-section-head { margin-bottom:14px; }
  .research-section-head p,.research-buckets>span,.research-session-status>span { color:var(--c-muted); font-size:13px; }
  .research-search-form { display:flex; align-items:flex-end; gap:10px; }
  .research-field { display:flex; flex-direction:column; gap:6px; font-size:13px; font-weight:600; margin:8px 0; }
  .research-field.grow { flex:1; }
  .field-error { color:var(--c-danger); }
  .research-filter-summary { display:flex; flex-wrap:wrap; gap:8px; margin:12px 0; }
  .research-filter-summary span,.status-chip { display:inline-flex; gap:5px; border:1px solid var(--c-border); border-radius:999px; padding:4px 9px; color:var(--c-muted); background:var(--c-surface-subtle); font-size:12px; }
  .status-chip.status-good { color:var(--c-green); border-color:color-mix(in srgb,var(--c-green) 32%,transparent); background:var(--c-green-bg); }
  .status-chip.status-live { color:var(--c-warn-dark); background:var(--c-warn-bg); }
  .research-session-status { border-top:1px solid var(--c-border); padding:12px 0; margin-top:10px; }
  .research-wait,.research-empty { display:flex; align-items:center; gap:12px; justify-content:center; padding:28px; color:var(--c-muted); text-align:center; }
  .research-empty { display:block; }
  .spinner { width:20px; height:20px; border:2px solid var(--c-border); border-top-color:var(--c-teal); border-radius:50%; animation:research-spin .8s linear infinite; }
  @keyframes research-spin { to { transform:rotate(360deg); } }
  .source-grid { display:flex; flex-direction:column; gap:8px; }
  .source-row { display:grid; grid-template-columns:28px 100px minmax(0,1fr); gap:12px; border:1px solid var(--c-border); border-radius:10px; padding:10px; }
  .source-row.source-missing { border-left:3px solid var(--c-warn); }
  .source-select { padding-top:5px; }
  .source-thumb { width:100px; height:92px; display:grid; place-items:center; overflow:hidden; background:var(--c-surface-subtle); border:0; border-radius:7px; color:var(--c-muted); cursor:pointer; }
  .source-thumb img { width:100%; height:100%; object-fit:cover; }
  .source-main { min-width:0; }
  .source-title-line { justify-content:flex-start; align-items:flex-start; }
  .source-caption { color:var(--c-muted); margin:5px 0; white-space:pre-wrap; overflow-wrap:anywhere; max-height:3.2em; overflow:hidden; }
  .source-meta { justify-content:flex-start; flex-wrap:wrap; color:var(--c-muted); font-size:12px; margin:7px 0; }
  .source-actions { justify-content:flex-start; gap:12px; }
  .missing-label,.exception-label { display:block; color:var(--c-warn-dark); font-size:12px; margin:4px 0; }
  .exception-label { color:var(--c-teal-dark); }
  .exception-control { display:flex; gap:8px; margin-top:8px; }
  .exception-control .input { max-width:380px; }
  .research-source-footer { border-top:1px solid var(--c-border); padding-top:12px; margin-top:12px; }
  .pager { display:flex; align-items:center; gap:8px; font-size:12px; color:var(--c-muted); }
  .selected-idea-preview { display:grid; grid-template-columns:minmax(170px,250px) minmax(0,1fr); gap:16px; padding:14px; margin-bottom:16px; background:var(--c-surface-subtle); border:1px solid var(--c-border); border-radius:10px; }
  .selected-idea-preview>img { width:100%; border-radius:8px; object-fit:cover; max-height:250px; }
  .selected-idea-preview>div { display:flex; flex-direction:column; gap:8px; }
  .selected-idea-preview small,.idea-card-body>small { color:var(--c-muted); }
  .selected-idea-preview p,.idea-card-body p { white-space:pre-wrap; overflow-wrap:anywhere; }
  .selected-idea-controls { justify-content:flex-start; flex-wrap:wrap; }
  .selected-idea-controls label { display:flex; flex-direction:column; gap:5px; font-size:12px; }
  .selected-idea-controls .input { min-width:220px; }
  .count-input { width:90px; min-width:90px !important; }
  .prompt-source-row { justify-content:flex-start; flex-wrap:wrap; color:var(--c-muted); font-size:13px; }
  .idea-grid { display:grid; grid-template-columns:repeat(auto-fill,minmax(245px,1fr)); gap:14px; }
  .idea-card { border:1px solid var(--c-border); border-radius:11px; overflow:hidden; background:var(--c-surface); }
  .idea-card.selected { outline:2px solid var(--c-teal); }
  .idea-image-wrap { position:relative; height:190px; background:var(--c-surface-subtle); display:grid; place-items:center; }
  .idea-image { width:100%; height:100%; object-fit:cover; }
  .idea-image-wrap>.status-chip { position:absolute; top:8px; left:8px; }
  .idea-image-placeholder { color:var(--c-muted); }
  .idea-card-body { padding:12px; }
  .idea-card-body h4 { margin:3px 0; }
  .idea-card-body p { max-height:5em; overflow:hidden; color:var(--c-muted); font-size:13px; }
  .idea-card-actions { display:flex; gap:7px; flex-wrap:wrap; }
  .idea-image-upload { display:block; margin:9px 0; color:var(--c-teal-dark); font-size:12px; cursor:pointer; }
  .idea-image-upload input { display:block; max-width:100%; margin-top:4px; font-size:11px; }
  .radio-select,.variant-select,.research-check { display:flex; gap:8px; align-items:center; font-size:13px; padding-top:8px; }
  .batch-history-list { justify-content:flex-start; flex-wrap:wrap; margin:8px 0 12px; }
  .batch-summary { padding:12px; border:1px solid var(--c-border); border-radius:9px; background:var(--c-surface-subtle); }
  .batch-summary>div:first-child { flex-direction:column; align-items:flex-start; }
  .batch-summary small { color:var(--c-muted); }
  .batch-summary-metrics { color:var(--c-muted); font-size:13px; }
  .check-summary { color:var(--c-muted); font-size:13px; }
  .variant-list { display:flex; flex-direction:column; gap:12px; margin:14px 0; }
  .variant-card { border:1px solid var(--c-border); border-radius:11px; padding:14px; }
  .variant-card.variant-ready { border-left:4px solid var(--c-green); }
  .variant-card>header { align-items:flex-start; }
  .variant-card>header p { margin:3px 0; color:var(--c-muted); }
  .variant-check { background:var(--c-surface-subtle); border-radius:8px; padding:9px; font-size:12px; white-space:pre-wrap; }
  .paired-prompts { display:grid; grid-template-columns:1fr 1fr; gap:12px; }
  .paired-prompts label { display:flex; flex-direction:column; gap:5px; font-size:13px; font-weight:650; }
  .prompt-editor { font-size:12px; line-height:1.45; min-height:170px; font-family:ui-monospace,SFMono-Regular,Consolas,monospace; }
  .variant-footer { align-items:flex-start; margin-top:10px; }
  .payoff-summary { color:var(--c-muted); font-size:12px; white-space:pre-wrap; flex:1; }
  .variant-actions { flex-direction:column; align-items:flex-end; }
  .transfer-bar { margin-top:15px; padding:14px; border:1px solid var(--c-border); border-radius:10px; background:var(--c-surface-subtle); }
  .transfer-bar>div { display:flex; flex-direction:column; }
  .transfer-bar span { color:var(--c-muted); font-size:12px; }
  .research-alert,.transfer-success { border:1px solid color-mix(in srgb,var(--c-danger) 35%,transparent); border-radius:8px; padding:10px 12px; color:var(--c-danger); background:var(--c-danger-bg); }
  .transfer-success { color:var(--c-green); border-color:color-mix(in srgb,var(--c-green) 35%,transparent); background:var(--c-green-bg); }
  .research-backdrop { position:fixed; inset:0; z-index:80; display:grid; place-items:center; padding:20px; background:#08110dc9; }
  .research-modal { width:min(620px,100%); max-height:88vh; overflow:auto; padding:20px; border-radius:14px; border:1px solid var(--c-border); background:var(--c-surface); box-shadow:var(--shadow-lg); }
  .research-modal>header,.research-modal>footer { display:flex; align-items:center; justify-content:space-between; gap:10px; }
  .research-modal>header { border-bottom:1px solid var(--c-border); padding-bottom:10px; margin-bottom:12px; }
  .research-modal>header h3 { margin:0; }
  .research-modal>footer { justify-content:flex-end; margin-top:15px; }
  .research-modal .hint { color:var(--c-muted); font-size:12px; }
  .history-list { display:flex; flex-direction:column; gap:7px; }
  .history-list button { display:flex; align-items:center; justify-content:space-between; gap:10px; text-align:left; padding:10px; border:1px solid var(--c-border); border-radius:8px; background:var(--c-surface); color:var(--c-text); cursor:pointer; }
  .history-list button span:first-child { display:flex; flex-direction:column; }
  .history-list small { color:var(--c-muted); }
  .detail-modal { width:min(760px,100%); }
  .detail-image { display:block; max-width:100%; max-height:260px; margin:0 auto 12px; border-radius:8px; }
  .source-full-text { white-space:pre-wrap; overflow-wrap:anywhere; padding:10px; border:1px solid var(--c-border); border-radius:8px; font:inherit; }
  .source-detail-grid { display:grid; grid-template-columns:140px 1fr; justify-content:initial; align-items:start; }
  .source-detail-grid dt { color:var(--c-muted); }
  .source-detail-grid dd { margin:0; white-space:pre-wrap; overflow-wrap:anywhere; }
  .transfer-modal { width:min(800px,100%); }
  .transfer-image-row { justify-content:flex-start; margin:12px 0; }
  .transfer-image-row img { width:100px; height:100px; object-fit:cover; border-radius:8px; }
  .transfer-preview-row { max-height:220px; overflow:auto; border:1px solid var(--c-border); border-radius:8px; padding:10px; margin:8px 0; font-size:12px; }
  .transfer-preview-row p { white-space:pre-wrap; overflow-wrap:anywhere; }
  @media(max-width:800px) { .research-search-form,.research-source-footer,.transfer-bar,.batch-summary,.variant-footer { flex-direction:column; align-items:stretch; } .source-row { grid-template-columns:24px 75px minmax(0,1fr); gap:7px; } .source-thumb { width:75px; height:75px; } .paired-prompts,.selected-idea-preview { grid-template-columns:1fr; } .selected-idea-preview>img { max-height:200px; max-width:280px; } .variant-actions { align-items:flex-start; } }
</style>
