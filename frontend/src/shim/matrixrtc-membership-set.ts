/**
 * Track Element Call's own view of who is currently in the MatrixRTC session, from inside
 * the widget frame, and log when that view changes.
 *
 * ## Why derive it instead of reading `MatrixRTCSession` directly
 *
 * The fault under investigation is: a key distributed once at join never reaches a later
 * joiner, and other participants' keys reach us tens of seconds late. matrix-js-sdk's
 * `RTCEncryptionManager` reacts to *its* `MatrixRTCSession`'s membership diff, so the ideal
 * instrumentation would listen to that session object directly. It is not reachable: Element
 * Call is a separate bundle running inside the widget iframe, and nothing in this codebase
 * exposes its `MatrixRTCSession` (or any Element Call internal) on `window` -- unlike
 * `mxMatrixClientPeg`, which Element Web does expose and which `membership-log.ts` already
 * uses. Reaching in from outside would mean poking at Element Call's minified module graph,
 * which breaks on every upstream rebuild and cannot be verified without one running call to
 * test against.
 *
 * What *is* reachable and already verified working (see `widget-api-log.ts`) is the widget
 * API traffic itself: every membership event Element Call's `RoomWidgetClient` learns about
 * arrives here first, as a `toWidget update_state` batch, before Element Call's own session
 * gets to react to it. So the question "does Element Call see a membership change" is
 * answered by capturing the same traffic and keeping our own running tally -- if a change
 * shows up here, it was available to Element Call; if Element Call still didn't rotate or
 * redistribute the key, the fault is downstream of this point, not in delivery.
 *
 * ## Why a running tally rather than diffing consecutive batches
 *
 * Each membership event is self-describing: a non-empty `memberships` array means that
 * member is live with that many devices, and an empty one means they left (see
 * `describeMembership` in `widget-api-log.ts` for why device count, not event type, is what
 * distinguishes the two). Applying each event to a map keyed by state key is correct whether
 * a given `update_state` batch is a full room-state snapshot or only the events that changed
 * -- either way every event it carries is current truth for that member. Diffing whole
 * batches against each other would instead require knowing which of those two shapes the
 * widget API is sending, and guessing wrong would report every member as having left the
 * moment an unrelated batch omitted them.
 */

/** Every spelling of the MatrixRTC membership event this stack has used. */
const MEMBERSHIP_EVENT_TYPES = new Set([
  "org.matrix.msc3401.call.member",
  "m.call.member",
  "m.rtc.member",
]);

/** One member's live-ness as declared by a single membership event. */
export interface MembershipEntry {
  stateKey: string;
  devices: number;
}

/** The result of folding one batch of membership events into the running tally. */
export interface MembershipDiff {
  /** State keys newly present with at least one device. */
  joined: string[];
  /** State keys that dropped to zero devices and were removed from the tally. */
  left: string[];
  /** Members whose device count changed without crossing zero either way. */
  updated: string[];
  /** How many members the tally now believes are live. */
  liveCount: number;
}

/**
 * Read one membership event's state key and device count, or `null` if it is not a
 * membership event.
 *
 * Mirrors `describeMembership` in `widget-api-log.ts`, but returns structured data instead of
 * a log line: this module needs the state key to track identity across batches, which a
 * pre-formatted string throws away.
 */
export function parseMembershipEntry(event: unknown): MembershipEntry | null {
  if (typeof event !== "object" || event === null) return null;
  const data = event as Record<string, unknown>;
  const type = data["type"];
  if (typeof type !== "string" || !MEMBERSHIP_EVENT_TYPES.has(type)) return null;
  const stateKey = typeof data["state_key"] === "string" ? data["state_key"] : null;
  if (stateKey === null) return null;
  const content = (data["content"] ?? {}) as Record<string, unknown>;
  const memberships = content["memberships"];
  const devices = Array.isArray(memberships)
    ? memberships.length
    : Object.keys(content).length === 0
      ? 0
      : 1;
  return { stateKey, devices };
}

/**
 * Every membership event in one widget API message, wherever it is carried.
 *
 * `update_state` carries a batch under `data.state`; `send_event` carries a single event at
 * the top level. Both are tried, matching `describeWidgetMessage`'s handling of the same two
 * shapes.
 */
export function collectMembershipEntries(data: Record<string, unknown> | undefined): MembershipEntry[] {
  const out: MembershipEntry[] = [];
  const state = data?.["state"];
  if (Array.isArray(state)) {
    for (const event of state) {
      const entry = parseMembershipEntry(event);
      if (entry !== null) out.push(entry);
    }
  }
  const top = parseMembershipEntry(data);
  if (top !== null) out.push(top);
  return out;
}

/**
 * Fold one batch of membership entries into a running tally, in place, and report what
 * changed.
 *
 * Pure apart from mutating the supplied map -- the caller owns the map's lifetime so the
 * listener can keep one for the life of the call while this stays unit-testable with a fresh
 * map per test.
 */
export function applyMembershipBatch(tally: Map<string, number>, entries: MembershipEntry[]): MembershipDiff {
  const joined: string[] = [];
  const left: string[] = [];
  const updated: string[] = [];
  for (const { stateKey, devices } of entries) {
    const before = tally.get(stateKey);
    if (devices === 0) {
      if (before !== undefined) {
        tally.delete(stateKey);
        left.push(stateKey);
      }
      continue;
    }
    if (before === undefined) {
      tally.set(stateKey, devices);
      joined.push(stateKey);
    } else if (before !== devices) {
      tally.set(stateKey, devices);
      updated.push(stateKey);
    }
  }
  return { joined, left, updated, liveCount: tally.size };
}

/** Nothing changed, so there is nothing worth a log line. */
function isNoOp(diff: MembershipDiff): boolean {
  return diff.joined.length === 0 && diff.left.length === 0 && diff.updated.length === 0;
}

/**
 * Describe a membership diff as one log line, or `null` if nothing changed.
 *
 * Bounds volume the same way the count reported for a `send_to_device` does: this fires only
 * on an actual change to the tally, not once per `update_state` batch, so a call sitting idle
 * with a steady membership does not add a line per heartbeat.
 */
export function describeMembershipDiff(diff: MembershipDiff): string | null {
  if (isNoOp(diff)) return null;
  const parts: string[] = [];
  if (diff.joined.length > 0) parts.push(`joined=[${diff.joined.join(",")}]`);
  if (diff.left.length > 0) parts.push(`left=[${diff.left.join(",")}]`);
  if (diff.updated.length > 0) parts.push(`device-count-changed=[${diff.updated.join(",")}]`);
  return (
    `[Elementium] MatrixRTC live membership tally (widget frame): ${parts.join(" ")} ` +
    `live=${diff.liveCount}. This is Element Call's own delivered view of the session -- if ` +
    `a join or leave shows up here, it reached Element Call, so a missing key ` +
    `rotation/distribution after this line is not a delivery failure.`
  );
}

/**
 * Install the tally listener. Safe to call in any frame: membership events only arrive as
 * `toWidget` traffic, which only the widget iframe receives as a `message` event -- installed
 * in the main frame this listens correctly but never sees a matching message, the same way
 * `widget-api-log.ts`'s two directions naturally separate by frame.
 */
export function setupMatrixRtcMembershipSetLog(): void {
  const w = window as unknown as Record<string, unknown>;
  if (w["__elementium_membership_set_logged"]) return;
  w["__elementium_membership_set_logged"] = true;

  const tally = new Map<string, number>();

  window.addEventListener(
    "message",
    (event: MessageEvent) => {
      try {
        const message = event.data as { api?: unknown; action?: unknown; data?: Record<string, unknown> } | undefined;
        if (typeof message !== "object" || message === null) return;
        if (message.api !== "toWidget") return;
        if (message.action !== "update_state" && message.action !== "send_event") return;
        const entries = collectMembershipEntries(message.data);
        if (entries.length === 0) return;
        const diff = applyMembershipBatch(tally, entries);
        const line = describeMembershipDiff(diff);
        if (line !== null) console.log(line);
      } catch {
        /* a diagnostic must never break the widget API's message handling */
      }
    },
    true,
  );
}
