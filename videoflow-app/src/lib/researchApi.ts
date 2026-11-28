import { invoke } from "@tauri-apps/api/core";

export type ResearchStatus = "queued" | "running" | "completed" | "ready" | "partial" | "failed" | "stopped" | "cancelled" | string;
export type ResearchBucket = "qualified" | "missing";

export interface ResearchFilters {
  min_likes: number | null;
  min_views: number | null;
  country: string;
  require_buy_link: boolean;
}

export interface ResearchSource {
  source_id: string;
  reel_id: string;
  url: string;
  thumb: string | null;
  caption: string | null;
  coverage: "full" | "excerpt" | "unknown" | string;
  likes: string | null;
  views: string | null;
  country: string | null;
  market_evidence: string | null;
  buy_url: string | null;
  cta: string | null;
  idea: string | null;
  filter_status: "qualified" | "missing" | string;
  missing_fields: string[];
  selected: boolean;
  accept_missing_reason: string | null;
}

export interface ResearchIdea {
  idea_id: string;
  source_id: string;
  revision: number;
  status: string;
  asset_path?: string | null;
  asset_hash?: string | null;
  asset_mime?: string | null;
  asset: string | null;
  title: string | null;
  content: string | null;
  visual_brief: string | null;
  error: string | null;
  history?: Array<{ revision: number; title?: string | null; content?: string | null; created_at?: string | null }>;
}

export interface ResearchVariant {
  ordinal: number;
  title: string;
  conflict: string;
  reveal: string;
  payoff: string;
  image_prompt: string;
  video_prompt: string;
  status: string;
  check_result: { verdict?: string; code?: string; evidence?: string; change?: string | null } | null;
  attempt: number;
  version?: number;
}

export interface ResearchBatch {
  card?: {asset?:string;title:string;content:string};
  batch_id: string;
  request_id?: string;
  session_id: string;
  idea_id: string;
  revision: number;
  n: number;
  status: ResearchStatus;
  seq?: number;
  variants: ResearchVariant[];
  check_summary?: string | null;
  error?: string | null;
  created_at?: string | null;
  prompt_source?: string | null;
  prompt_model?: string | null;
}

export interface ResearchSelection {
  idea_id: string;
  revision: number;
  version?: number;
}

export interface ResearchSnapshot {
  session_id: string;
  seq: number;
  status: ResearchStatus;
  topic: string;
  filters: ResearchFilters;
  reported_total: number | null;
  counts: Record<string, number>;
  source_selection_ids: string[];
  selection_version?: number;
  ideas: ResearchIdea[];
  selection: ResearchSelection | null;
  selection_version_current?: number;
  batches: ResearchBatch[];
}

export interface ResearchHistoryItem {
  session_id: string;
  topic: string;
  status: ResearchStatus;
  created_at: string;
  counts: Record<string, number>;
}

export interface ResearchProviderSnapshot {
  source: "gateway" | "internal";
  base_url?: string;
  model?: string;
  timeout_s: number;
}

interface ActionContracts {
  search_start: { request: { request_id: string; topic: string; filters: ResearchFilters }; response: { session_id: string; status: ResearchStatus; seq: number; counts: Record<string, number> } };
  search_get: { request: { session_id: string }; response: ResearchSnapshot };
  search_list: { request: { cursor?: string | null; limit?: number }; response: { items: ResearchHistoryItem[]; next_cursor: string | null } };
  search_sources: { request: { session_id: string; cursor?: string | null; limit: number; bucket: ResearchBucket }; response: { items: ResearchSource[]; total: number; next_cursor: string | null } };
  set_sources: { request: { session_id: string; source_ids: Array<{ source_id: string; accept_missing: boolean; reason?: string }>; expected_selection_version?: number }; response: { source_selection_ids: string[]; selection_version: number } };
  prepare_ideas: { request: { session_id: string; source_ids: string[]; request_id: string }; response: { operation_id: string; status: ResearchStatus; ideas: ResearchIdea[] } };
  idea_get: { request: { session_id: string; idea_id: string }; response: { idea: ResearchIdea } };
  idea_update: { request: { session_id: string; idea_id: string; expected_revision: number; content?: string; image_b64?: string; filename?: string; request_id: string }; response: { idea: ResearchIdea } };
  select_idea: { request: { session_id: string; idea_id: string; revision: number; expected_version: number }; response: { selection: ResearchSelection; version: number } };
  develop_start: { request: { request_id: string; session_id: string; idea_id: string; revision: number; n: number; prompt_language: string; constraints: string }; response: { batch_id: string; status: ResearchStatus } };
  batch_get: { request: { batch_id: string }; response: ResearchBatch };
  batch_resume: { request: { batch_id: string }; response: ResearchBatch };
  batch_stop: { request: { batch_id: string }; response: { status: ResearchStatus } };
  search_stop: { request: { session_id: string }; response: { status: ResearchStatus } };
  search_resume: { request: { session_id: string }; response: { status: ResearchStatus } };
  variant_edit: { request: { batch_id: string; ordinal: number; expected_version: number; image_prompt?: string; video_prompt?: string; request_id: string }; response: { variant: ResearchVariant } };
  develop_repair: { request: { batch_id: string; request_id: string }; response: { batch: ResearchBatch } };
}

export type ResearchAction = keyof ActionContracts;

export function research<K extends ResearchAction>(
  action: K,
  payload: ActionContracts[K]["request"]
): Promise<ActionContracts[K]["response"]> {
  return invoke<any>("research", { action, payload }).then(result =>
    action === "batch_get" || action === "batch_resume" ? result.batch : result);
}

export interface TransferReceipt {
  batch_id: string;
  request_id: string;
  added: number;
  existing: number;
  status: "committed" | string;
}

export const researchTransfer = {
  commit: (args: { batchId: string; ordinals: number[]; requestId: string; expectedProject: string }) =>
    invoke<TransferReceipt>("research_transfer", args),
};

export function openResearchUrl(url: string): Promise<void> {
  return invoke<void>("research_open_url", { url });
}
