export interface SeqGuard {
  next(): number;
  invalidate(): number;
  isCurrent(seq: number): boolean;
}

/** Monotonic sequence guard to discard stale async results and cancel on close/destroy. */
export function createSeqGuard(): SeqGuard {
  let current = 0;
  return {
    next: () => ++current,
    invalidate: () => ++current,
    isCurrent: (seq: number) => seq === current,
  };
}

/**
 * Runs an async reload guarded by `guard` and clears `loading` whenever the latest
 * request settles, even if a silent reload superseded an earlier non-silent load.
 */
export async function runWithLoadingGuard<T>(
  guard: SeqGuard,
  silent: boolean,
  setLoading: (loading: boolean) => void,
  fn: (seq: number) => Promise<T>,
  onError?: (err: unknown) => void
): Promise<T | undefined> {
  const seq = guard.next();
  if (!silent) {
    setLoading(true);
  }
  try {
    return await fn(seq);
  } catch (err) {
    if (guard.isCurrent(seq)) {
      onError?.(err);
    }
    return undefined;
  } finally {
    if (guard.isCurrent(seq)) {
      setLoading(false);
    }
  }
}

/**
 * Determines whether AccountCard should sync `selectedProxyId` from an updated `account.proxy_id` prop.
 * Prevents snapping back while a proxy assignment request is still saving.
 */
export function shouldSyncSelectedProxy(
  prevPropProxyId: string,
  nextPropProxyId: string,
  proxySaving: boolean
): boolean {
  return !proxySaving && nextPropProxyId !== prevPropProxyId;
}

export interface AccountActionBlockFlags {
  actionBusy?: boolean;
  proxySaving?: boolean;
  quotaBusy?: boolean;
  running?: boolean;
  accountBusy?: boolean;
}

/** Returns true when any busy/running flag blocks mutating or launching an account action. */
export function isAccountActionBlocked(flags: AccountActionBlockFlags): boolean {
  return Boolean(
    flags.actionBusy ||
      flags.proxySaving ||
      flags.quotaBusy ||
      flags.running ||
      flags.accountBusy
  );
}

/**
 * Updates a single tested proxy inside an existing proxy list by matching `id`.
 * Never appends/resurrects a proxy that was already deleted from `proxies`.
 */
export function mergeSingleTestedProxy<T extends { id: string }>(
  proxies: readonly T[],
  updated: T
): T[] {
  let found = false;
  const next = proxies.map((p) => {
    if (p.id === updated.id) {
      found = true;
      return updated;
    }
    return p;
  });
  return found ? next : [...proxies];
}

export interface BatchSelectableRow {
  row_key: string;
  status: string;
  attempts_count: number;
  prompt_modified?: boolean;
}

/** Returns true when a row cannot be queued in a batch run (completed, skipped, or exhausted attempts without prompt edit). */
export function isRowLockedForBatch(row: BatchSelectableRow, maxAttempts: number): boolean {
  return (
    row.status === "Đã tạo video" ||
    row.status === "Bỏ qua" ||
    (row.attempts_count >= maxAttempts && !row.prompt_modified)
  );
}

/**
 * Drops selected keys missing from `rows` or locked by `isRowLockedForBatch`.
 * Returns the original `Set` reference when unchanged to avoid redundant reactive updates.
 */
export function pruneSelectedKeys(
  selected: Set<string>,
  rows: readonly BatchSelectableRow[],
  maxAttempts: number
): Set<string> {
  if (selected.size === 0) return selected;
  const eligible = new Set<string>();
  for (const row of rows) {
    if (!isRowLockedForBatch(row, maxAttempts)) {
      eligible.add(row.row_key);
    }
  }
  let changed = false;
  const next = new Set<string>();
  for (const key of selected) {
    if (eligible.has(key)) {
      next.add(key);
    } else {
      changed = true;
    }
  }
  return changed ? next : selected;
}

export interface StartupAutoOpenDeps<
  TRecent extends { path: string; exists: boolean },
  TProject,
> {
  hasCurrentProject: () => boolean;
  getProjectSeq: () => number;
  listRecentProjects: () => Promise<TRecent[]>;
  loadProject: (path: string) => Promise<TProject>;
  applyProject: (info: TProject) => void;
  isAborted?: () => boolean;
}

/**
 * Auto-opens the first existing recent project at startup only if no project was
 * opened or switched concurrently while metadata/project IPC calls were in flight.
 */
export async function runStartupProjectAutoOpen<
  TRecent extends { path: string; exists: boolean },
  TProject,
>(deps: StartupAutoOpenDeps<TRecent, TProject>): Promise<boolean> {
  const startSeq = deps.getProjectSeq();
  const canProceed = () =>
    !deps.isAborted?.() &&
    !deps.hasCurrentProject() &&
    deps.getProjectSeq() === startSeq;

  if (!canProceed()) return false;
  const recents = await deps.listRecentProjects();
  if (!canProceed()) return false;

  const firstExisting = recents.find((r) => r.exists);
  if (!firstExisting) return false;

  const info = await deps.loadProject(firstExisting.path);
  if (!canProceed()) return false;

  deps.applyProject(info);
  return true;
}

export interface CoalescedScheduler {
  schedule(): void;
  cancel(): void;
  flush(): void;
}

/**
 * Coalesces bursts of rapid notifications (e.g. concurrent `row_changed` ticks)
 * into a single trailing callback after `delayMs`, with deterministic cancellation on destroy/switch.
 */
export function createCoalescedScheduler(
  delayMs: number,
  fn: () => void
): CoalescedScheduler {
  let timer: ReturnType<typeof setTimeout> | null = null;
  return {
    schedule() {
      if (timer !== null) {
        clearTimeout(timer);
      }
      timer = setTimeout(() => {
        timer = null;
        fn();
      }, delayMs);
    },
    cancel() {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
    },
    flush() {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
        fn();
      }
    },
  };
}
