import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  createSeqGuard,
  isAccountActionBlocked,
  mergeSingleTestedProxy,
  runWithLoadingGuard,
  shouldSyncSelectedProxy,
} from "../src/lib/asyncGuard.js";

describe("createSeqGuard", () => {
  it("tracks current sequence across concurrent requests and invalidates on close/destroy", () => {
    const guard = createSeqGuard();
    const s1 = guard.next();
    assert.equal(guard.isCurrent(s1), true);

    const s2 = guard.next();
    assert.equal(guard.isCurrent(s1), false);
    assert.equal(guard.isCurrent(s2), true);

    guard.invalidate();
    assert.equal(guard.isCurrent(s2), false);
  });
});

describe("shouldSyncSelectedProxy", () => {
  it("syncs only when not saving and prop proxy_id actually changed", () => {
    assert.equal(shouldSyncSelectedProxy("p1", "p2", false), true);
    assert.equal(shouldSyncSelectedProxy("p1", "p1", false), false);
    assert.equal(shouldSyncSelectedProxy("p1", "p2", true), false);
  });
});

describe("isAccountActionBlocked", () => {
  it("blocks when any busy/running flag including accountBusy is true", () => {
    assert.equal(isAccountActionBlocked({}), false);
    assert.equal(isAccountActionBlocked({ accountBusy: true }), true);
    assert.equal(isAccountActionBlocked({ actionBusy: true }), true);
    assert.equal(isAccountActionBlocked({ proxySaving: true }), true);
    assert.equal(isAccountActionBlocked({ quotaBusy: true }), true);
    assert.equal(isAccountActionBlocked({ running: true }), true);
  });
});

describe("mergeSingleTestedProxy", () => {
  it("updates matching proxy by id and never resurrects deleted proxies", () => {
    const p1 = { id: "p1", name: "One", last_status: "untested" };
    const p2 = { id: "p2", name: "Two", last_status: "untested" };
    const updatedP1 = { id: "p1", name: "One", last_status: "alive" };
    const deletedP3 = { id: "p3", name: "Three", last_status: "alive" };

    assert.deepEqual(mergeSingleTestedProxy([p1, p2], updatedP1), [updatedP1, p2]);
    assert.deepEqual(mergeSingleTestedProxy([p1, p2], deletedP3), [p1, p2]);
  });
});

function deferred<T>() {
  let resolve!: (val: T) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe("runWithLoadingGuard", () => {
  it("clears loading after a silent reload overlaps an in-flight non-silent load", async () => {
    const guard = createSeqGuard();
    let loading = false;
    let applied: string | null = null;
    const errors: unknown[] = [];

    const first = deferred<string>();
    const second = deferred<string>();

    const p1 = runWithLoadingGuard(
      guard,
      false,
      (v) => {
        loading = v;
      },
      async (seq) => {
        const res = await first.promise;
        if (guard.isCurrent(seq)) applied = res;
        return res;
      },
      (err) => errors.push(err)
    );
    assert.equal(loading, true);

    const p2 = runWithLoadingGuard(
      guard,
      true,
      (v) => {
        loading = v;
      },
      async (seq) => {
        const res = await second.promise;
        if (guard.isCurrent(seq)) applied = res;
        return res;
      },
      (err) => errors.push(err)
    );
    assert.equal(loading, true);

    first.resolve("stale");
    await p1;
    assert.equal(applied, null);
    assert.equal(loading, true);

    second.resolve("fresh");
    await p2;
    assert.equal(applied, "fresh");
    assert.equal(loading, false);
    assert.equal(errors.length, 0);
  });

  it("clears loading even when the overlapping silent reload rejects", async () => {
    const guard = createSeqGuard();
    let loading = false;
    const errors: unknown[] = [];

    const first = deferred<string>();
    const second = deferred<string>();

    const p1 = runWithLoadingGuard(
      guard,
      false,
      (v) => {
        loading = v;
      },
      async () => first.promise,
      (err) => errors.push(err)
    );
    const p2 = runWithLoadingGuard(
      guard,
      true,
      (v) => {
        loading = v;
      },
      async () => second.promise,
      (err) => errors.push(err)
    );

    first.resolve("stale");
    await p1;
    assert.equal(loading, true);

    second.reject(new Error("silent refresh failed"));
    await p2;
    assert.equal(loading, false);
    assert.equal(errors.length, 1);
  });

  it("clears loading when fast silent reload finishes before slow non-silent load", async () => {
    const guard = createSeqGuard();
    let loading = false;
    const errors: unknown[] = [];

    const slowFirst = deferred<string>();
    const fastSecond = deferred<string>();

    const p1 = runWithLoadingGuard(
      guard,
      false,
      (v) => {
        loading = v;
      },
      async () => slowFirst.promise,
      (err) => errors.push(err)
    );
    const p2 = runWithLoadingGuard(
      guard,
      true,
      (v) => {
        loading = v;
      },
      async () => fastSecond.promise,
      (err) => errors.push(err)
    );

    fastSecond.resolve("latest");
    await p2;
    assert.equal(loading, false);

    slowFirst.reject(new Error("stale failure"));
    await p1;
    assert.equal(loading, false);
    assert.equal(errors.length, 0);
  });
});
