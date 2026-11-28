import { writable } from "svelte/store";

/** Keeps the current research session and batch available while screens unmount. */
export const researchSessionId = writable<string | null>(null);
export const researchBatchId = writable<string | null>(null);
