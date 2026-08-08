/**
 * Name the LiveKit signalling messages passing over the WebSocket, in both directions.
 *
 * The peer-connection trace cannot see any of this. On Element Web v1.12.25 the publisher
 * offer is created *before* the signalling socket opens and travels inside the join request
 * as a query parameter, so the whole negotiation happens on the WebSocket and the peer
 * connection is only told the outcome. "What does the SFU reply to an offer carried in the
 * join request" is therefore a question only this can answer.
 *
 * Field numbers are read off the `livekit.SignalRequest` / `livekit.SignalResponse`
 * descriptors in the shipped Element Call bundle rather than guessed, because a wrong name
 * here is worse than no name: it would make a trace read as though a message we never
 * received had arrived.
 *
 * **Kinds and sizes only, never payloads.** These messages carry SDP, access tokens and
 * participant identities, and a trace is pasted into issues and logs.
 */

/** `livekit.SignalRequest`, the oneof this client sends. */
export const REQUEST_FIELDS: Record<number, string> = {
  1: "offer",
  2: "answer",
  3: "trickle",
  4: "add_track",
  5: "mute",
  6: "subscription",
  7: "track_setting",
  8: "leave",
  10: "update_layers",
  11: "subscription_permission",
  12: "sync_state",
  13: "simulate",
  14: "ping",
  15: "update_metadata",
  16: "ping_req",
  17: "update_audio_track",
  18: "update_video_track",
  // 19 and above exist only in newer livekit-client than the shipped bundle uses. Carried
  // so a trace taken after an Element Web upgrade still names what it sees rather than
  // reporting a bare field number at the moment the trace is most needed.
  19: "publish_data_track_request",
  20: "unpublish_data_track_request",
  21: "update_data_subscription",
  22: "store_data_blob_request",
  23: "get_data_blob_request",
};

/** `livekit.SignalResponse`, the oneof the SFU sends. */
export const RESPONSE_FIELDS: Record<number, string> = {
  1: "join",
  2: "answer",
  3: "offer",
  4: "trickle",
  5: "update",
  6: "track_published",
  8: "leave",
  9: "mute",
  10: "speakers_changed",
  11: "room_update",
  12: "connection_quality",
  13: "stream_state_update",
  14: "subscribed_quality_update",
  15: "subscription_permission_update",
  16: "refresh_token",
  17: "track_unpublished",
  18: "pong",
  19: "reconnect",
  20: "pong_resp",
  21: "subscription_response",
  22: "request_response",
  23: "track_subscribed",
  24: "room_moved",
  25: "media_sections_requirement",
  26: "subscribed_audio_codec_update",
  27: "publish_data_track_response",
  28: "unpublish_data_track_response",
  29: "data_track_subscriber_handles",
  30: "store_data_blob_response",
  31: "get_data_blob_response",
};

/** `livekit.JoinRequest` — what protocol 17 packs into the connection URL. */
const JOIN_REQUEST_FIELDS: Record<number, string> = {
  1: "client_info",
  2: "connection_settings",
  3: "metadata",
  4: "participant_attributes",
  5: "add_track_requests",
  6: "publisher_offer",
  7: "reconnect",
  8: "reconnect_reason",
  9: "participant_sid",
  10: "sync_state",
};

/** `livekit.WrappedJoinRequest.Compression`. */
const COMPRESSION = ["NONE", "GZIP"];

/** `livekit.LeaveRequest.Action` — what the SFU is asking the client to do. */
const LEAVE_ACTION = ["DISCONNECT", "RESUME", "RECONNECT"];

/**
 * `livekit.DisconnectReason`. Worth naming because a `leave` from the SFU is otherwise
 * indistinguishable from a call that simply ended, and the reason is the whole finding:
 * `STATE_MISMATCH` says it rejected the negotiation, `JOIN_FAILURE` says it never got that
 * far. Neither is secret.
 */
const DISCONNECT_REASON = [
  "UNKNOWN_REASON",
  "CLIENT_INITIATED",
  "DUPLICATE_IDENTITY",
  "SERVER_SHUTDOWN",
  "PARTICIPANT_REMOVED",
  "ROOM_DELETED",
  "STATE_MISMATCH",
  "JOIN_FAILURE",
  "MIGRATION",
  "SIGNAL_CLOSE",
  "ROOM_CLOSED",
  "USER_UNAVAILABLE",
  "USER_REJECTED",
  "SIP_TRUNK_FAILURE",
  "CONNECTION_TIMEOUT",
  "MEDIA_FAILURE",
  "AGENT_ERROR",
];

interface Varint {
  value: number;
  bytesRead: number;
}

function readVarint(bytes: Uint8Array, offset: number): Varint {
  let value = 0;
  let shift = 0;
  let bytesRead = 0;
  while (offset + bytesRead < bytes.length) {
    const b = bytes[offset + bytesRead];
    bytesRead++;
    // Beyond 28 bits the shift would overflow a JS bitwise operation. Every field number
    // and length this reads is far below that, and a value that is not is not something to
    // guess at, so it stops rather than reporting a wrapped number.
    if (shift <= 28) value += (b & 0x7f) * 2 ** shift;
    if ((b & 0x80) === 0) break;
    shift += 7;
  }
  return { value, bytesRead };
}

interface Field {
  number: number;
  wireType: number;
  /** Where this field's value starts, and how long it is. */
  start: number;
  length: number;
}

/**
 * The top-level fields of a protobuf message, without decoding any of their contents.
 *
 * Returns what it managed to read rather than throwing on a malformed tail: a trace that
 * says "offer" for a message it could only partly parse is more use than one that says
 * nothing because byte 400 was unexpected.
 */
function topLevelFields(bytes: Uint8Array): Field[] {
  const fields: Field[] = [];
  let pos = 0;
  while (pos < bytes.length) {
    const tag = readVarint(bytes, pos);
    if (tag.bytesRead === 0) break;
    pos += tag.bytesRead;
    const number = Math.floor(tag.value / 8);
    const wireType = tag.value % 8;
    if (number === 0) break;
    let length: number;
    switch (wireType) {
      case 0: {
        length = readVarint(bytes, pos).bytesRead;
        break;
      }
      case 1:
        length = 8;
        break;
      case 2: {
        const len = readVarint(bytes, pos);
        pos += len.bytesRead;
        length = len.value;
        break;
      }
      case 5:
        length = 4;
        break;
      default:
        // Groups (3, 4) and anything else: livekit uses none of them, and guessing a length
        // would desynchronise the scan and invent fields for the rest of the message.
        return fields;
    }
    if (pos + length > bytes.length) {
      fields.push({ number, wireType, start: pos, length: bytes.length - pos });
      break;
    }
    fields.push({ number, wireType, start: pos, length });
    pos += length;
  }
  return fields;
}

/**
 * The reason and action inside a `LeaveRequest`, named.
 *
 * `leave` is the one message whose kind alone is not enough. It is how the SFU reports that
 * it is ending the session, and "why" is the difference between a call that finished and a
 * negotiation it refused.
 */
function leaveDetail(bytes: Uint8Array, leave: Field): string {
  const body = bytes.subarray(leave.start, leave.start + leave.length);
  const parts: string[] = [];
  for (const f of topLevelFields(body)) {
    // reason (2) and action (3); can_reconnect (1) and regions (4) say less than these do.
    if (f.number === 2 || f.number === 3) {
      const value = readVarint(body, f.start).value;
      const names = f.number === 2 ? DISCONNECT_REASON : LEAVE_ACTION;
      parts.push(`${f.number === 2 ? "reason" : "action"}=${names[value] ?? value}`);
    }
  }
  return parts.length === 0 ? "" : ` ${parts.join(" ")}`;
}

/** The name of the oneof case set in a signalling message, e.g. `offer`, `join`. */
export function describeSignal(bytes: Uint8Array, names: Record<number, string>): string {
  const fields = topLevelFields(bytes);
  if (fields.length === 0) return "empty";
  return fields
    .map((f) => {
      const name = names[f.number] ?? `field ${f.number}`;
      return name === "leave" ? `leave${leaveDetail(bytes, f)}` : name;
    })
    .join("+");
}

export function describeRequest(bytes: Uint8Array): string {
  return describeSignal(bytes, REQUEST_FIELDS);
}

export function describeResponse(bytes: Uint8Array): string {
  return describeSignal(bytes, RESPONSE_FIELDS);
}

/**
 * Which parts of the join request this client packed into the connection URL.
 *
 * Names the fields that are present and their sizes; the values are an SDP offer and the
 * participant's identity, so none of them are logged.
 *
 * Returns null when the URL carries no join request, which is how a protocol-16 client
 * connects — so "no join request" is itself the answer to which flow is in use.
 */
export function describeJoinRequest(url: string): string | null {
  let param: string | null;
  try {
    param = new URL(url, "http://invalid.invalid").searchParams.get("join_request");
  } catch {
    return null;
  }
  if (param === null || param === "") return null;

  let wrapped: Uint8Array;
  try {
    const raw = atob(param.replace(/-/g, "+").replace(/_/g, "/"));
    wrapped = Uint8Array.from(raw, (c) => c.charCodeAt(0));
  } catch {
    return "join_request present, but not base64";
  }

  // WrappedJoinRequest: field 1 compression (enum), field 2 join_request (bytes).
  const outer = topLevelFields(wrapped);
  const compressionField = outer.find((f) => f.number === 1);
  const compression =
    compressionField === undefined
      ? 0
      : readVarint(wrapped, compressionField.start).value;
  const payload = outer.find((f) => f.number === 2);
  if (payload === undefined) return `join_request wrapper with no payload`;
  if (compression !== 0) {
    // Nothing here decompresses gzip, and a gzip payload scanned as protobuf would produce
    // confident nonsense. Which compression was used is the useful half anyway.
    return `join_request ${payload.length} bytes, compression=${COMPRESSION[compression] ?? compression}`;
  }

  const inner = wrapped.subarray(payload.start, payload.start + payload.length);
  const parts = topLevelFields(inner).map(
    (f) => `${JOIN_REQUEST_FIELDS[f.number] ?? `field ${f.number}`}(${f.length}B)`,
  );
  return `join_request ${payload.length} bytes: ${parts.join(" ")}`;
}

/** The bytes of a `send`/`message` payload, or null for text and Blob. */
export function signalBytes(data: unknown): Uint8Array | null {
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) {
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  }
  return null;
}
