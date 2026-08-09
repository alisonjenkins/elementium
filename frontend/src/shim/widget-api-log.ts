/**
 * Record the widget API traffic between Element Web and the Element Call iframe.
 *
 * ## Why
 *
 * Element Call runs as a widget. In that mode its Matrix client is a `RoomWidgetClient`,
 * which owns no network connection: every room-state read and every to-device send is a
 * postMessage to Element Web, which performs it on the widget's behalf. So the widget API is
 * the whole of Element Call's view of the room, and the whole of its ability to distribute
 * an encryption key.
 *
 * That makes two questions answerable here and nowhere else:
 *
 *  - Does Element Call *see* a membership change? If no `update_state` carrying a MatrixRTC
 *    membership event arrives, its encryption manager cannot react to one, and no amount of
 *    looking at the encryption manager will explain why.
 *  - Does Element Call *distribute* a key when it should? A key rotation that never becomes a
 *    `send_to_device` reached nobody, however correct the rotation was.
 *
 * A real call showed our key sent exactly once, at join, and the far end's picture freezing
 * at the moment a participant joined ninety seconds later -- with nothing in the log to say
 * whether the join was unseen, seen and ignored, or seen and acted on unsuccessfully. Those
 * are three different bugs in three different places.
 *
 * ## What it does not log
 *
 * Never message content. A `send_to_device` for `m.room.encrypted` carries the encryption
 * keys this whole subsystem exists to protect, so this records the event type and how many
 * recipients it addressed, and nothing from `data.messages` beyond counting its keys.
 *
 * ## How both directions are captured with one listener
 *
 * A frame's `message` listener sees what is posted *to* it. Installing this in both frames
 * therefore covers both directions without wrapping anyone's `postMessage`: the widget frame
 * observes `toWidget`, the main frame observes `fromWidget`. Which frame a line came from is
 * already in the log -- see the frame tag in `console-bridge.ts`.
 */

/** Every spelling of the MatrixRTC membership event this stack has used. */
const MEMBERSHIP_EVENT_TYPES = new Set([
  "org.matrix.msc3401.call.member",
  "m.call.member",
  "m.rtc.member",
]);

/**
 * Actions worth a line every time, however many there are.
 *
 * Everything else is counted and reported once per action name. The widget API carries a
 * steady trickle of mute toggles, theme changes and capability negotiation that would bury
 * the two actions this module exists for.
 */
const ALWAYS_INTERESTING = new Set([
  "send_to_device",
  "update_state",
  "send_event",
  "org.matrix.msc3819.send_to_device",
]);

/** Action names already reported once, so the uninteresting ones say so and then stop. */
const seenActions = new Set<string>();

export interface WidgetMessage {
  api?: unknown;
  action?: unknown;
  data?: Record<string, unknown>;
}

/** One line to log, and whether it should be said only once per action name. */
export interface WidgetLogLine {
  text: string;
  oncePerAction: boolean;
  /** The dedupe key for `oncePerAction`. */
  key: string;
}

/**
 * How many (user, device) pairs a `send_to_device` addresses.
 *
 * The shape is `{messages: {userId: {deviceId: content}}}`. Only the two levels of keys are
 * walked -- the content at the leaf is the encrypted payload and is never touched.
 */
function recipientCount(data: Record<string, unknown> | undefined): number {
  const messages = data?.["messages"];
  if (typeof messages !== "object" || messages === null) return 0;
  let count = 0;
  for (const perUser of Object.values(messages as Record<string, unknown>)) {
    if (typeof perUser === "object" && perUser !== null) {
      count += Object.keys(perUser as Record<string, unknown>).length;
    }
  }
  return count;
}

/**
 * Describe a state update if it is a MatrixRTC membership event, else `null`.
 *
 * A membership event's content is an array of per-device memberships (or, under MSC4143, one
 * event per device with an empty content meaning departure), so the device count is what
 * distinguishes a join from a leave. Nothing else from the content is read.
 */
function describeMembership(data: Record<string, unknown> | undefined): string | null {
  const type = data?.["type"];
  if (typeof type !== "string" || !MEMBERSHIP_EVENT_TYPES.has(type)) return null;
  const content = (data?.["content"] ?? {}) as Record<string, unknown>;
  const memberships = content["memberships"];
  const devices = Array.isArray(memberships)
    ? memberships.length
    : Object.keys(content).length === 0
      ? 0
      : 1;
  const stateKey = typeof data?.["state_key"] === "string" ? data["state_key"] : "?";
  const sender = typeof data?.["sender"] === "string" ? data["sender"] : "?";
  return `${devices === 0 ? "LEFT" : "JOINED/UPDATED"} member=${stateKey} sender=${sender} devices=${devices}`;
}

/**
 * Every MatrixRTC membership event in an `update_state` batch, described.
 *
 * The batch also carries room name, avatars and topic changes, which are content and are
 * neither read nor logged.
 */
function collectMemberships(state: unknown): string[] {
  if (!Array.isArray(state)) return [];
  const out: string[] = [];
  for (const event of state) {
    if (typeof event !== "object" || event === null) continue;
    const described = describeMembership(event as Record<string, unknown>);
    if (described !== null) out.push(described);
  }
  return out;
}

/**
 * Turn one widget API message into the line worth logging, or `null` if it carries nothing.
 *
 * Pure, and separate from the listener, so the guarantee that matters here -- that no
 * message content ever reaches the log -- is testable without a DOM, a widget or a real
 * `send_to_device` full of live keys.
 */
export function describeWidgetMessage(message: WidgetMessage): WidgetLogLine | null {
  if (message.api !== "toWidget" && message.api !== "fromWidget") return null;
  const api = message.api;
  const action = typeof message.action === "string" ? message.action : "?";

  if (action === "send_to_device" || action === "org.matrix.msc3819.send_to_device") {
    const type = typeof message.data?.["type"] === "string" ? message.data["type"] : "?";
    const encrypted = String(message.data?.["encrypted"] ?? "?");
    // The two directions carry different shapes and want different facts. Outbound is a
    // `messages` map of recipients -- the number of them is the whole question, since a
    // rotation that addressed nobody reached nobody. Inbound is one delivered event with a
    // `sender`, and counting its (absent) recipients reported `recipients=0` on a message
    // that was working perfectly. Neither branch reads content: it is key material.
    if (api === "toWidget") {
      const sender = typeof message.data?.["sender"] === "string" ? message.data["sender"] : "?";
      return {
        text:
          `[Elementium] widget API toWidget send_to_device type=${type} from=${sender} ` +
          `encrypted=${encrypted} (a key arriving from someone else)`,
        oncePerAction: false,
        key: `${api}:${action}`,
      };
    }
    return {
      text:
        `[Elementium] widget API fromWidget send_to_device type=${type} ` +
        `recipients=${recipientCount(message.data)} encrypted=${encrypted}. ` +
        `This is how Element Call distributes an encryption key; its absence after a ` +
        `membership change means nobody was sent one.`,
      oncePerAction: false,
      key: `${api}:${action}`,
    };
  }

  // `update_state` delivers a batch under `data.state`, not a single event -- reading
  // `data.type` on it yields nothing, which is how every membership change this was built to
  // catch was logged as `type=-`. `send_event` does carry the event at the top level, so
  // both shapes are tried.
  const memberships = [
    ...collectMemberships(message.data?.["state"]),
    ...(describeMembership(message.data) === null
      ? []
      : [describeMembership(message.data) as string]),
  ];
  if (memberships.length > 0) {
    return {
      text:
        `[Elementium] widget API ${api} ${action}: MatrixRTC membership ` +
        `${memberships.join("; ")}. Element Call rotates its key on a leaver, and on a ` +
        `joiner when the current key is over ten seconds old -- so a fromWidget ` +
        `send_to_device should follow this line.`,
      oncePerAction: false,
      key: `${api}:${action}`,
    };
  }

  if (ALWAYS_INTERESTING.has(action)) {
    const type = typeof message.data?.["type"] === "string" ? message.data["type"] : "-";
    return {
      text: `[Elementium] widget API ${api} ${action} type=${type}`,
      oncePerAction: false,
      key: `${api}:${action}`,
    };
  }

  // Negotiation and UI chatter. That it happens at all is worth knowing once; after that it
  // would bury the two actions this module exists for.
  return {
    text: `[Elementium] widget API ${api} ${action} (further occurrences not logged)`,
    oncePerAction: true,
    key: `${api}:${action}`,
  };
}

function report(message: WidgetMessage): void {
  const line = describeWidgetMessage(message);
  if (line === null) return;
  if (line.oncePerAction) {
    if (seenActions.has(line.key)) return;
    seenActions.add(line.key);
  }
  console.log(line.text);
}

/**
 * Install widget API logging. Safe to call in any frame; each frame observes the direction
 * addressed to it.
 */
export function setupWidgetApiLog(): void {
  const w = window as unknown as Record<string, unknown>;
  if (w["__elementium_widget_api_logged"]) return;
  w["__elementium_widget_api_logged"] = true;

  // Capture phase, so this runs before the widget API's own handler and still sees a message
  // that handler stops propagating.
  window.addEventListener(
    "message",
    (event: MessageEvent) => {
      try {
        const data = event.data as WidgetMessage | undefined;
        if (typeof data !== "object" || data === null) return;
        report(data);
      } catch {
        /* a diagnostic must never break the widget API's message handling */
      }
    },
    true,
  );
}
