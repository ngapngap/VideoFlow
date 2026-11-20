import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  createSeqGuard,
  isAccountActionBlocked,
  mergeSingleTestedProxy,
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
