import { describe, expect, it } from "vitest";

import {
  applyMembershipBatch,
  collectMembershipEntries,
  describeMembershipDiff,
  parseMembershipEntry,
} from "./matrixrtc-membership-set";

/**
 * This module exists to answer, from inside the widget frame, whether Element Call's own
 * delivered view of the session changed after the initial join -- something nothing in this
 * codebase could previously say, because `membership-log.ts` only sees the main frame's
 * client and the widget frame has none. It is a pure fold over widget API traffic, so every
 * case here can be checked without a DOM, an iframe or a running call.
 */
describe("parseMembershipEntry", () => {
  it("reads a join as a state key with a positive device count", () => {
    const entry = parseMembershipEntry({
      type: "org.matrix.msc3401.call.member",
      state_key: "_@alice:example.org_DEVICEONE",
      content: { memberships: [{ device_id: "DEVICEONE" }] },
    });
    expect(entry).toEqual({ stateKey: "_@alice:example.org_DEVICEONE", devices: 1 });
  });

  it("reads a leave (empty content) as a zero device count", () => {
    const entry = parseMembershipEntry({
      type: "m.rtc.member",
      state_key: "_@alice:example.org_DEVICEONE",
      content: {},
    });
    expect(entry).toEqual({ stateKey: "_@alice:example.org_DEVICEONE", devices: 0 });
  });

  it("recognises every spelling of the membership event this stack has used", () => {
    for (const type of ["org.matrix.msc3401.call.member", "m.call.member", "m.rtc.member"]) {
      const entry = parseMembershipEntry({ type, state_key: "k", content: { memberships: [{}] } });
      expect(entry, type).not.toBeNull();
    }
  });

  it("ignores non-membership events and malformed input without throwing", () => {
    expect(parseMembershipEntry({ type: "m.room.topic", state_key: "", content: {} })).toBeNull();
    expect(parseMembershipEntry(null)).toBeNull();
    expect(parseMembershipEntry("nonsense")).toBeNull();
    expect(parseMembershipEntry({ type: "m.rtc.member" })).toBeNull(); // no state_key
  });
});

describe("collectMembershipEntries", () => {
  it("finds membership events inside an update_state batch, skipping unrelated state", () => {
    const entries = collectMembershipEntries({
      state: [
        { type: "m.room.name", state_key: "", content: { name: "General" } },
        {
          type: "org.matrix.msc3401.call.member",
          state_key: "_@bob:example.org_D1",
          content: { memberships: [{}] },
        },
      ],
    });
    expect(entries).toEqual([{ stateKey: "_@bob:example.org_D1", devices: 1 }]);
  });

  it("finds a single top-level membership event, as send_event carries it", () => {
    const entries = collectMembershipEntries({
      type: "m.call.member",
      state_key: "_@carol:example.org_D1",
      content: { memberships: [{}, {}] },
    });
    expect(entries).toEqual([{ stateKey: "_@carol:example.org_D1", devices: 2 }]);
  });

  it("returns nothing for undefined or unrelated data, and never throws", () => {
    expect(collectMembershipEntries(undefined)).toEqual([]);
    expect(collectMembershipEntries({ state: "not-an-array" })).toEqual([]);
    expect(collectMembershipEntries({ state: [null, 42, "x"] })).toEqual([]);
  });
});

describe("applyMembershipBatch", () => {
  it("records a first sighting as a join", () => {
    const tally = new Map<string, number>();
    const diff = applyMembershipBatch(tally, [{ stateKey: "a", devices: 1 }]);
    expect(diff.joined).toEqual(["a"]);
    expect(diff.left).toEqual([]);
    expect(diff.liveCount).toBe(1);
    expect(tally.get("a")).toBe(1);
  });

  it("records a member dropping to zero devices as a leave, and removes it from the tally", () => {
    const tally = new Map([["a", 1]]);
    const diff = applyMembershipBatch(tally, [{ stateKey: "a", devices: 0 }]);
    expect(diff.left).toEqual(["a"]);
    expect(diff.liveCount).toBe(0);
    expect(tally.has("a")).toBe(false);
  });

  it("reports a device count change on an already-live member as neither a join nor a leave", () => {
    const tally = new Map([["a", 1]]);
    const diff = applyMembershipBatch(tally, [{ stateKey: "a", devices: 2 }]);
    expect(diff.joined).toEqual([]);
    expect(diff.left).toEqual([]);
    expect(diff.updated).toEqual(["a"]);
    expect(tally.get("a")).toBe(2);
  });

  it("is a no-op when a batch repeats an unchanged member -- this is what bounds log volume", () => {
    const tally = new Map([["a", 1]]);
    const diff = applyMembershipBatch(tally, [{ stateKey: "a", devices: 1 }]);
    expect(diff.joined).toEqual([]);
    expect(diff.left).toEqual([]);
    expect(diff.updated).toEqual([]);
  });

  it("ignores a leave for a member it never saw join, rather than going negative", () => {
    const tally = new Map<string, number>();
    const diff = applyMembershipBatch(tally, [{ stateKey: "ghost", devices: 0 }]);
    expect(diff.left).toEqual([]);
    expect(diff.liveCount).toBe(0);
  });

  it("folds a whole batch -- a stale LEFT alongside a genuine JOIN in one message", () => {
    const tally = new Map([["stale-device", 1]]);
    const diff = applyMembershipBatch(tally, [
      { stateKey: "stale-device", devices: 0 },
      { stateKey: "new-member", devices: 1 },
    ]);
    expect(diff.left).toEqual(["stale-device"]);
    expect(diff.joined).toEqual(["new-member"]);
    expect(diff.liveCount).toBe(1);
  });
});

describe("describeMembershipDiff", () => {
  it("is null when nothing changed, so idle calls do not add a log line per heartbeat", () => {
    expect(describeMembershipDiff({ joined: [], left: [], updated: [], liveCount: 3 })).toBeNull();
  });

  it("names who joined and left and the resulting live count", () => {
    const text = describeMembershipDiff({ joined: ["b"], left: ["a"], updated: [], liveCount: 2 });
    expect(text).toContain("joined=[b]");
    expect(text).toContain("left=[a]");
    expect(text).toContain("live=2");
  });

  it("never needs message content to describe a change", () => {
    const text = describeMembershipDiff({ joined: ["@alice:example.org_D1"], left: [], updated: [], liveCount: 1 });
    // Only identity and counts appear; nothing about content shape leaks in because none was
    // ever passed in.
    expect(text).not.toContain("ciphertext");
  });
});
