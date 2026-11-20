import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  createSeqGuard,
  isAccountActionBlocked,
  isRowLockedForBatch,
  mergeSingleTestedProxy,
  pruneSelectedKeys,
  runStartupProjectAutoOpen,
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

describe("pruneSelectedKeys", () => {
  it("drops completed, skipped, max-attempts-exhausted, and missing rows while keeping prompt-modified rows", () => {
    const rows = [
      { row_key: "r-ok", status: "Đang chờ", attempts_count: 0 },
      { row_key: "r-done", status: "Đã tạo video", attempts_count: 1 },
      { row_key: "r-skip", status: "Bỏ qua", attempts_count: 0 },
      { row_key: "r-maxed", status: "Lỗi tạo video", attempts_count: 3, prompt_modified: false },
      { row_key: "r-edited", status: "Lỗi tạo video", attempts_count: 3, prompt_modified: true },
    ];
    assert.equal(isRowLockedForBatch(rows[0], 3), false);
    assert.equal(isRowLockedForBatch(rows[1], 3), true);
    assert.equal(isRowLockedForBatch(rows[2], 3), true);
    assert.equal(isRowLockedForBatch(rows[3], 3), true);
    assert.equal(isRowLockedForBatch(rows[4], 3), false);

    const initial = new Set(["r-ok", "r-done", "r-skip", "r-maxed", "r-edited", "r-missing"]);
    const pruned = pruneSelectedKeys(initial, rows, 3);
    assert.deepEqual(Array.from(pruned), ["r-ok", "r-edited"]);
    assert.equal(initial.size, 6);

    const unchanged = pruneSelectedKeys(pruned, rows, 3);
    assert.equal(unchanged, pruned);

    // Lowering maxAttempts reactively locks rows that hit the new threshold unless prompt_modified
    const tightenedRows = [
      ...rows,
      { row_key: "r-border", status: "Lỗi tạm thời", attempts_count: 2 },
    ];
    const withBorder = new Set(["r-ok", "r-border", "r-edited"]);
    assert.deepEqual(Array.from(pruneSelectedKeys(withBorder, tightenedRows, 2)), [
      "r-ok",
      "r-edited",
    ]);
  });
});

describe("runStartupProjectAutoOpen", () => {
  it("opens the first existing recent project when no manual project load occurs", async () => {
    let current: { path: string } | null = null;
    const seq = 1;
    const applied: string[] = [];

    const opened = await runStartupProjectAutoOpen({
      hasCurrentProject: () => current !== null,
      getProjectSeq: () => seq,
      listRecentProjects: async () => [
        { path: "E:\\Missing", exists: false },
        { path: "E:\\Recent1", exists: true },
      ],
      loadProject: async (path) => ({ path }),
      applyProject: (info) => {
        current = info;
        applied.push(info.path);
      },
    });

    assert.equal(opened, true);
    assert.deepEqual(applied, ["E:\\Recent1"]);
    assert.deepEqual(current, { path: "E:\\Recent1" });
  });

  it("aborts before listRecentProjects when a project is already open", async () => {
    let listCalls = 0;
    let loadCalls = 0;
    const opened = await runStartupProjectAutoOpen({
      hasCurrentProject: () => true,
      getProjectSeq: () => 1,
      listRecentProjects: async () => {
        listCalls += 1;
        return [{ path: "E:\\Recent1", exists: true }];
      },
      loadProject: async (path) => {
        loadCalls += 1;
        return { path };
      },
      applyProject: () => {},
    });

    assert.equal(opened, false);
    assert.equal(listCalls, 0);
    assert.equal(loadCalls, 0);
  });

  it("aborts before loadProject when user initiates a manual switch while listing recents", async () => {
    let current: { path: string } | null = null;
    let seq = 1;
    let loadCalls = 0;
    const gate = deferred<void>();

    const bootPromise = runStartupProjectAutoOpen({
      hasCurrentProject: () => current !== null,
      getProjectSeq: () => seq,
      listRecentProjects: async () => {
        await gate.promise;
        return [{ path: "E:\\Recent1", exists: true }];
      },
      loadProject: async (path) => {
        loadCalls += 1;
        return { path };
      },
      applyProject: (info) => {
        current = info;
      },
    });

    // Manual switch starts (in flight, current still null until manual load finishes)
    seq += 1;
    gate.resolve(undefined);

    const opened = await bootPromise;
    assert.equal(opened, false);
    assert.equal(loadCalls, 0);
    assert.equal(current, null);
  });

  it("aborts auto-open without overwriting when user manually opens a project mid-bootstrap", async () => {
    let current: { path: string } | null = null;
    let seq = 1;
    const gate = deferred<void>();

    const bootPromise = runStartupProjectAutoOpen({
      hasCurrentProject: () => current !== null,
      getProjectSeq: () => seq,
      listRecentProjects: async () => [{ path: "E:\\Recent1", exists: true }],
      loadProject: async (path) => {
        await gate.promise;
        return { path };
      },
      applyProject: (info) => {
        current = info;
      },
    });

    // User manually opens another project while bootstrap loadProject is in flight
    await Promise.resolve();
    seq += 1;
    current = { path: "E:\\ManualChoice" };

    gate.resolve(undefined);
    const opened = await bootPromise;
    assert.equal(opened, false);
    assert.deepEqual(current, { path: "E:\\ManualChoice" });
  });
});
