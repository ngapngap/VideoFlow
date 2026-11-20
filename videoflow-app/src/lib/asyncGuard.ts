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
