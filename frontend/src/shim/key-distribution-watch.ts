/**
 * Check that a membership change is followed by a key distribution, and say so when it is not.
 *
 * ## Why this exists
 *
 * Both halves of the question are already logged. `membership-log.ts` records every MatrixRTC
 * join and leave the main frame's Matrix client sees; `widget-api-log.ts` records every
 * `fromWidget send_to_device`, which is Element Call's only route for handing anybody a key.
 * Its comment even says a `send_to_device` "should follow this line" -- and then nothing
 * checks that it did. A person has to read two interleaved streams and notice an absence.
 *
 * M5 in `specs/BACKLOG-2026-08-09-media.md` is exactly that absence: in the incident our key
 * was distributed once, at join, and never again. A participant joined ninety seconds later,
 * we sent nothing, and their picture of us froze -- because a peer that never received our
 * key cannot decrypt our frames however well our encoder is running. The log contained the
 * join and the absence of a send, and neither line said anything was wrong.
 *
 * This turns that absence into a statement. A membership change arms an expectation; a
 * distribution satisfies it; an expectation that outlives its deadline is reported once,
 * naming what it was waiting on.
 *
 * ## Why the main frame, and only the main frame
 *
 * The two signals are not visible in the same frame. `toWidget` traffic arrives at the widget
 * iframe, `fromWidget` traffic at the main window -- so the widget frame can see the
 * membership change and never the distribution. Arming from the widget frame's tally would
 * report every distribution as missing. The main frame is the one place both are observable:
 * membership through Element Web's own client, distribution as the widget's outbound message.
 *
 * ## What it does not claim
 *
 * An overdue expectation is not proof of the fault. Element Call re-distributes an existing
 * key inside a ten-second grace period and rotates outside it, and a membership *update* that
 * changes nothing it cares about legitimately produces no send. So this reports what was
 * observed and what was expected, and leaves the reading to whoever is looking -- the value
 * is that the absence now has a line of its own instead of being invisible.
 */

/**
 * How long a membership change may go unanswered before it is worth a line.
 *
 * Element Call's rollout is gated on its own session update plus a to-device round trip, and
 * its grace period for reusing an existing key is ten seconds. Twelve leaves room for both
 * without reporting a call that is merely being slow.
 */
export const DISTRIBUTION_DEADLINE_MS = 12_000;

/** An expectation of a key distribution, armed by a membership change. */
export interface DistributionWatch {
  /** When the oldest unanswered membership change was seen, or `null` if none is pending. */
  pendingSince: number | null;
  /** What that change was, quoted back in the message. */
  cause: string | null;
  /** How many further changes arrived while it was still pending. */
  furtherChanges: number;
  /** Whether this expectation has already been reported overdue, so it is reported once. */
  reported: boolean;
}

/** A state transition, with the line it produced (or `null` if it produced none). */
export interface WatchStep {
  watch: DistributionWatch;
  line: string | null;
}

export function newDistributionWatch(): DistributionWatch {
  return { pendingSince: null, cause: null, furtherChanges: 0, reported: false };
}

/**
 * Record a membership change, arming an expectation.
 *
 * A change arriving while one is already pending does not restart the clock. The question is
 * whether *any* distribution followed, and resetting the deadline on each of a flurry of
 * membership events is how an expectation never expires: several people joining at once is
 * precisely the case worth catching.
 */
export function noteMembershipChange(
  watch: DistributionWatch,
  at: number,
  cause: string,
): DistributionWatch {
  if (watch.pendingSince !== null) {
    return { ...watch, furtherChanges: watch.furtherChanges + 1 };
  }
  return { pendingSince: at, cause, furtherChanges: 0, reported: false };
}

/**
 * Record that Element Call sent a key to somebody, satisfying any pending expectation.
 *
 * A distribution with nothing pending is not worth a line: keys are also sent on rotation
 * timers and on the initial join, and reporting each one would bury the case this exists for.
 */
export function noteDistribution(watch: DistributionWatch, at: number): WatchStep {
  if (watch.pendingSince === null) return { watch, line: null };
  const elapsed = Math.max(0, at - watch.pendingSince);
  const cleared = newDistributionWatch();
  const cause = watch.cause ?? "?";
  return {
    watch: cleared,
    line: watch.reported
      ? `[Elementium] a key distribution finally followed the membership change reported ` +
        `overdue above (${cause}), ${elapsed}ms after it. Late, not missing -- anyone whose ` +
        `media was undecryptable in that window should recover now.`
      : `[Elementium] a key distribution followed the membership change (${cause}) after ` +
        `${elapsed}ms. This is the path M5 is about working; the line is here so that its ` +
        `absence is an absence of something, and not of nothing.`,
  };
}

/**
 * Report an expectation that has outlived its deadline. Reports once per expectation.
 */
export function checkOverdue(watch: DistributionWatch, now: number): WatchStep {
  if (watch.pendingSince === null || watch.reported) return { watch, line: null };
  const elapsed = now - watch.pendingSince;
  if (elapsed < DISTRIBUTION_DEADLINE_MS) return { watch, line: null };
  const also =
    watch.furtherChanges > 0
      ? ` ${watch.furtherChanges} further membership change(s) arrived in the same window.`
      : "";
  return {
    watch: { ...watch, reported: true },
    line:
      `[Elementium] no key was distributed in the ${elapsed}ms since a MatrixRTC membership ` +
      `change (${watch.cause ?? "?"}).${also} Element Call distributes keys only as a ` +
      `fromWidget send_to_device, and none has been seen. If a participant joined, they hold ` +
      `no key of ours and our video will be frozen for them -- see M5.`,
  };
}

/** The watch this frame is keeping, or `null` until it is installed. */
let watch: DistributionWatch | null = null;

/** How often to test the deadline. A second: the deadline is measured in tens of them. */
const POLL_MS = 1_000;

/** Arm the watch from a membership change observed in this frame. */
export function armKeyDistributionWatch(cause: string): void {
  if (watch === null) return;
  watch = noteMembershipChange(watch, Date.now(), cause);
}

/** Satisfy the watch from a key distribution observed in this frame. */
export function noteKeyDistributed(): void {
  if (watch === null) return;
  const step = noteDistribution(watch, Date.now());
  watch = step.watch;
  if (step.line !== null) console.log(step.line);
}

/**
 * Install the watch. Call in the main frame only -- see the note above on why the widget
 * frame cannot see a distribution and would report every one of them missing.
 */
export function setupKeyDistributionWatch(): void {
  const w = window as unknown as Record<string, unknown>;
  if (w["__elementium_key_distribution_watched"]) return;
  w["__elementium_key_distribution_watched"] = true;

  watch = newDistributionWatch();
  // Never cleared: it is one comparison a second and it has to outlive every membership
  // change in the call, which is the whole life of the page.
  setInterval(() => {
    if (watch === null) return;
    const step = checkOverdue(watch, Date.now());
    watch = step.watch;
    if (step.line !== null) console.error(step.line);
  }, POLL_MS);
}
