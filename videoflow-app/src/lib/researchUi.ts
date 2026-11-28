import type { ResearchIdea, ResearchVariant } from "./researchApi";

export function parseOptionalThreshold(value: string): { value: number | null; error: string | null } {
  const trimmed = value.trim();
  if (!trimmed) return { value: null, error: null };
  if (!/^\d+$/.test(trimmed)) return { value: null, error: "Nhập số nguyên không âm hoặc để trống." };
  const parsed = Number(trimmed);
  if (!Number.isSafeInteger(parsed) || parsed > 2_147_483_647) {
    return { value: null, error: "Giá trị tối đa là 2.147.483.647." };
  }
  return { value: parsed, error: null };
}

export function ideaReady(idea: ResearchIdea): boolean {
  return idea.status === "ready" && Boolean(idea.asset && idea.content?.trim());
}

export function variantReadyForTransfer(variant: ResearchVariant): boolean {
  return ["pass","ready"].includes(variant.status) && variant.check_result?.verdict === "PASS";
}

export function safeHttpUrl(value: string | null | undefined): string | null {
  if (!value) return null;
  try {
    const url = new URL(value);
    return url.protocol === "https:" || url.protocol === "http:" ? url.href : null;
  } catch {
    return null;
  }
}

export function sourceDisplayValue(value: string | null | undefined): string {
  return value == null || value.trim() === "" ? "—" : value;
}
