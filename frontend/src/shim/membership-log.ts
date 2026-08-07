/**
 * Log MatrixRTC membership changes into the same stream as everything else.
 *
 * Two faults are reported as "someone joined or left and then nobody could hear each
 * other". Neither is diagnosable without knowing *which* join or leave a silence followed,
 * and until now nothing recorded membership at all: the native log contains keys, frames and
 * SDP, and not one line saying a participant arrived.
 *
 * Element Call rotates its encryption key on every leaver, and on a joiner when the current
 * key is more than ten seconds old. So a rotation is nearly always a consequence of a
 * membership event, and attributing a silence to the rotation without knowing which event
 * caused it is guessing. With both in one timestamped stream it is reading.
 *
 * ## Why it reads Element Web's client rather than Element Call's
 *
 * Element Call runs in a widget iframe and manages the session, but the membership *state*
 * lives in the room, and Element Web already holds a synced client for it. `mxMatrixClientPeg`
 * is that client. Reaching for it couples this to an Element Web internal, which is why every
 * step here is optional: if the peg is absent, or the client never arrives, this logs once
 * that it gave up and the rest of the application is unaffected.
 */

/** Every spelling of the membership event this stack has used. */
const MEMBERSHIP_EVENT_TYPES = new Set([
  "org.matrix.msc3401.call.member",
  "m.call.member",
  "m.rtc.member",
]);

/** How long to wait for Element Web to finish creating its client before giving up. */
const CLIENT_WAIT_MS = 60_000;
const POLL_MS = 1_000;

interface MatrixEventLike {
  getType?: () => string;
  getStateKey?: () => string | undefined;
  getSender?: () => string | undefined;
  getRoomId?: () => string | undefined;
  getContent?: () => Record<string, unknown>;
}

interface ClientLike {
  on?: (event: string, handler: (...args: unknown[]) => void) => void;
}

/**
 * How many devices a member declares in this event.
 *
 * The membership event carries an array of per-device memberships, so leaving is not a
 * separate event type -- it is this array becoming empty. A reader who does not know that
 * sees a "join" at the moment someone leaves.
 */
function membershipCount(content: Record<string, unknown>): number {
  const memberships = content["memberships"];
  if (Array.isArray(memberships)) return memberships.length;
  // MSC4143 replaced the array with one event per device, where an empty content is the
  // leave. Treat a content with no application/device as a departure.
  return Object.keys(content).length === 0 ? 0 : 1;
}

function handleStateEvent(event: MatrixEventLike): void {
  const type = event.getType?.();
  if (!type || !MEMBERSHIP_EVENT_TYPES.has(type)) return;

  const content = event.getContent?.() ?? {};
  const count = membershipCount(content);
  console.log(
    `[Elementium] MatrixRTC membership: ${count === 0 ? "LEFT" : "JOINED/UPDATED"} ` +
      `room=${event.getRoomId?.() ?? "?"} member=${event.getStateKey?.() ?? "?"} ` +
      `sender=${event.getSender?.() ?? "?"} devices=${count} type=${type}. ` +
      `Element Call rotates its key on every leaver, and on a joiner when the current key ` +
      `is over ten seconds old -- so a key change just after this line is caused by it.`,
  );
}

/** Poll for Element Web's client, then subscribe. Gives up quietly. */
function whenClientReady(use: (client: ClientLike) => void): void {
  const deadline = Date.now() + CLIENT_WAIT_MS;
  const tick = () => {
    const peg = (window as unknown as Record<string, unknown>)["mxMatrixClientPeg"] as
      | { get?: () => ClientLike | null }
      | undefined;
    const client = peg?.get?.();
    if (client?.on) {
      use(client);
      return;
    }
    if (Date.now() > deadline) {
      console.log(
        "[Elementium] MatrixRTC membership logging unavailable: no Matrix client after " +
          `${CLIENT_WAIT_MS}ms. Key rotations will not be attributable to a join or leave.`,
      );
      return;
    }
    setTimeout(tick, POLL_MS);
  };
  tick();
}

/** Install membership logging. Safe to call in any frame; only the main window has a client. */
export function setupMembershipLog(): void {
  const w = window as unknown as Record<string, unknown>;
  if (w["__elementium_membership_logged"]) return;
  w["__elementium_membership_logged"] = true;

  whenClientReady((client) => {
    // `RoomState.events` fires for every state event in every joined room. Filtering by type
    // here rather than subscribing more narrowly keeps this independent of which room the
    // call is in -- which is not known when this installs.
    client.on?.("RoomState.events", (...args: unknown[]) => {
      try {
        handleStateEvent(args[0] as MatrixEventLike);
      } catch {
        /* a diagnostic must never break the client's event loop */
      }
    });
    console.log("[Elementium] MatrixRTC membership logging active");
  });
}
