import { describe, expect, it } from "vitest";

import {
  DISTRIBUTION_DEADLINE_MS,
  checkOverdue,
  newDistributionWatch,
  noteDistribution,
  noteMembershipChange,
} from "./key-distribution-watch";

/**
 * The decision is a pure function of instants, so every case here is checked without a clock,
 * a widget or a call -- the same way the native `NotConnectedWatch` is tested. What is being
 * pinned is not the wording but the arithmetic: when an expectation is armed, when it is
 * satisfied, and that it is reported exactly once.
 */
describe("noteMembershipChange", () => {
  it("arms an expectation from the first change", () => {
    const watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    expect(watch.pendingSince).toBe(1_000);
    expect(watch.cause).toBe("JOINED alice");
  });

  it("does not restart the clock for a change arriving while one is pending", () => {
    // Several people joining at once is the case worth catching, and restarting the deadline
    // on each of them is how an expectation never expires.
    let watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    watch = noteMembershipChange(watch, 5_000, "JOINED bob");
    expect(watch.pendingSince).toBe(1_000);
    expect(watch.furtherChanges).toBe(1);
    expect(watch.cause).toBe("JOINED alice");
  });
});

describe("checkOverdue", () => {
  it("says nothing while nothing is pending", () => {
    expect(checkOverdue(newDistributionWatch(), 1_000_000).line).toBeNull();
  });

  it("says nothing before the deadline", () => {
    const watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    const step = checkOverdue(watch, 1_000 + DISTRIBUTION_DEADLINE_MS - 1);
    expect(step.line).toBeNull();
    expect(step.watch.reported).toBe(false);
  });

  it("reports once the deadline has passed, and names the change it was waiting on", () => {
    const watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    const step = checkOverdue(watch, 1_000 + DISTRIBUTION_DEADLINE_MS);
    expect(step.line).toContain("JOINED alice");
    expect(step.watch.reported).toBe(true);
  });

  it("reports an overdue expectation exactly once, however long it stays overdue", () => {
    let watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    let step = checkOverdue(watch, 1_000 + DISTRIBUTION_DEADLINE_MS);
    expect(step.line).not.toBeNull();
    watch = step.watch;
    for (const now of [20_000, 40_000, 600_000]) {
      step = checkOverdue(watch, now);
      expect(step.line, `at ${now}`).toBeNull();
      watch = step.watch;
    }
  });

  it("counts the changes that piled up behind the one it is reporting", () => {
    let watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    watch = noteMembershipChange(watch, 2_000, "JOINED bob");
    watch = noteMembershipChange(watch, 3_000, "LEFT carol");
    const step = checkOverdue(watch, 1_000 + DISTRIBUTION_DEADLINE_MS);
    expect(step.line).toContain("2 further membership change(s)");
  });
});

describe("noteDistribution", () => {
  it("says nothing when no membership change is waiting on one", () => {
    // Keys are also sent on the initial join and on rotation timers. A line for each would
    // bury the case this exists for.
    const step = noteDistribution(newDistributionWatch(), 1_000);
    expect(step.line).toBeNull();
    expect(step.watch.pendingSince).toBeNull();
  });

  it("clears the expectation and records how long it took", () => {
    const watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    const step = noteDistribution(watch, 1_340);
    expect(step.watch.pendingSince).toBeNull();
    expect(step.line).toContain("340ms");
  });

  it("distinguishes a late distribution from a timely one", () => {
    let watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    watch = checkOverdue(watch, 1_000 + DISTRIBUTION_DEADLINE_MS).watch;
    const step = noteDistribution(watch, 30_000);
    expect(step.line).toContain("Late, not missing");
  });

  it("re-arms cleanly, so the next membership change is watched too", () => {
    let watch = noteMembershipChange(newDistributionWatch(), 1_000, "JOINED alice");
    watch = noteDistribution(watch, 1_100).watch;
    watch = noteMembershipChange(watch, 90_000, "JOINED bob");
    expect(watch.pendingSince).toBe(90_000);
    expect(watch.furtherChanges).toBe(0);
    expect(checkOverdue(watch, 90_000 + DISTRIBUTION_DEADLINE_MS).line).toContain("JOINED bob");
  });
});
