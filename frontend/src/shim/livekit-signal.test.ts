import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { describe, expect, it } from "vitest";

import {
  REQUEST_FIELDS,
  RESPONSE_FIELDS,
  describeJoinRequest,
  describeRequest,
  describeResponse,
  localIdentityFromJoin,
  redactSignalUrl,
} from "./livekit-signal";

/**
 * The field tables are transcribed from a minified bundle, and a wrong name would make a
 * trace report a message that never arrived. So they are checked against livekit-client's
 * own descriptors rather than trusted.
 *
 * The installed livekit-client is *newer* than the one in the Element Call bundle we ship,
 * which is the point: this is the check that says whether an upgrade renumbered anything.
 * It asserts agreement on the numbers both know, not equal size.
 */
function descriptorFields(message: string): Map<number, string> {
  // Via `package.json` because the bundle itself is not a declared export subpath. This
  // reads the shipped file rather than importing the module: the descriptors are internal,
  // livekit-client exports none of them, and this needs the same text the browser runs.
  const require = createRequire(import.meta.url);
  const entry = require.resolve("livekit-client");
  const esm = readFileSync(join(dirname(entry), "livekit-client.esm.mjs"), "utf8");
  const start = esm.indexOf(`livekit.${message}"`);
  expect(start, `descriptor for livekit.${message}`).toBeGreaterThan(-1);
  const block = esm.slice(start, esm.indexOf("]", start));
  const fields = new Map<number, string>();
  for (const m of block.matchAll(/no:\s*(\d+),\s*name:\s*"(\w+)"/g)) {
    fields.set(Number(m[1]), m[2]);
  }
  expect(fields.size).toBeGreaterThan(5);
  return fields;
}

/** A length-delimited protobuf field: tag varint, length varint, then `body`. */
function field(number: number, body: number[]): number[] {
  const tag: number[] = [];
  let t = number * 8 + 2;
  do {
    let b = t & 0x7f;
    t >>>= 7;
    if (t > 0) b |= 0x80;
    tag.push(b);
  } while (t > 0);
  return [...tag, body.length, ...body];
}

describe("the field tables", () => {
  it("agrees with livekit-client on every SignalRequest number both know", () => {
    const actual = descriptorFields("SignalRequest");
    for (const [no, name] of actual) {
      if (no in REQUEST_FIELDS) expect(REQUEST_FIELDS[no], `field ${no}`).toBe(name);
    }
  });

  it("agrees with livekit-client on every SignalResponse number both know", () => {
    const actual = descriptorFields("SignalResponse");
    for (const [no, name] of actual) {
      if (no in RESPONSE_FIELDS) expect(RESPONSE_FIELDS[no], `field ${no}`).toBe(name);
    }
  });
});

describe("naming a signalling message", () => {
  it("names the three the reorder logic already depends on", () => {
    // Tags 10, 18 and 26 -- publisher offer, subscriber answer, and the SFU's offer. The
    // WebSocket shim's reordering keys on these bytes and works in a live call, so they are
    // the one part of the table confirmed by something other than reading.
    expect(describeRequest(new Uint8Array(field(1, [1, 2, 3])))).toBe("offer");
    expect(describeRequest(new Uint8Array(field(2, [1, 2, 3])))).toBe("answer");
    expect(describeResponse(new Uint8Array(field(3, [1, 2, 3])))).toBe("offer");
  });

  it("names a field whose tag does not fit in one byte", () => {
    // Field 16 encodes as 0x82 0x01, so anything reading only the first byte gets this
    // wrong -- which is what the shim's own reorder check does, deliberately, since it only
    // cares about fields 1 to 3.
    expect(describeRequest(new Uint8Array(field(16, [])))).toBe("ping_req");
    expect(describeResponse(new Uint8Array(field(26, [7])))).toBe(
      "subscribed_audio_codec_update",
    );
  });

  it("reports an unknown field by number rather than inventing a name", () => {
    expect(describeResponse(new Uint8Array(field(99, [1])))).toBe("field 99");
  });

  it("says empty rather than throwing on a zero-length message", () => {
    expect(describeRequest(new Uint8Array(0))).toBe("empty");
  });

  it("stops at a truncated message instead of scanning past the end", () => {
    // A length byte claiming more than is present. The scan reports what it has; the
    // alternative is a trace that says nothing precisely when a message arrived malformed.
    expect(describeResponse(new Uint8Array([26, 200, 1, 2]))).toBe("offer");
  });

  it("names a ping, which is a varint rather than a nested message", () => {
    // Field 14, wire type 0. Skipping it needs the varint length, not a length prefix.
    expect(describeRequest(new Uint8Array([14 * 8, 0x96, 0x01]))).toBe("ping");
  });
});

describe("the protocol-17 join request", () => {
  /** A `WrappedJoinRequest` carrying an uncompressed `JoinRequest`, base64 as in the URL. */
  function joinUrl(inner: number[], compression?: number): string {
    const wrapped = [
      ...(compression === undefined ? [] : [8, compression]),
      ...field(2, inner),
    ];
    const b64 = Buffer.from(Uint8Array.from(wrapped)).toString("base64");
    return `ws://sfu.invalid/rtc/v1?access_token=secret&join_request=${encodeURIComponent(b64)}`;
  }

  it("names the publisher offer that protocol 17 packs into the URL", () => {
    const inner = [...field(1, [1]), ...field(6, [1, 2, 3, 4, 5])];
    const described = describeJoinRequest(joinUrl(inner));
    expect(described).toContain("client_info(1B)");
    expect(described).toContain("publisher_offer(5B)");
  });

  it("returns null when the URL carries no join request", () => {
    // How a protocol-16 client connects. The absence is the finding, so it must not be
    // reported as a parse failure.
    expect(describeJoinRequest("ws://sfu.invalid/rtc?access_token=secret")).toBeNull();
  });

  it("does not scan a compressed payload as protobuf", () => {
    const described = describeJoinRequest(joinUrl([1, 2, 3], 1));
    expect(described).toContain("compression=GZIP");
    expect(described).not.toContain("client_info");
  });

  it("never repeats the access token or any field value", () => {
    const inner = [...field(6, [...Buffer.from("v=0 a=rtpmap")])];
    const described = describeJoinRequest(joinUrl(inner)) ?? "";
    expect(described).not.toContain("secret");
    expect(described).not.toContain("rtpmap");
  });
});

describe("a leave from the SFU", () => {
  it("names the reason and the action, which is the whole finding", () => {
    // SignalResponse.leave (field 8) holding reason=STATE_MISMATCH, action=RECONNECT.
    // The kind alone cannot distinguish a refused negotiation from a call that ended.
    const leave = [/* reason */ 2 * 8, 6, /* action */ 3 * 8, 2];
    expect(describeResponse(new Uint8Array(field(8, leave)))).toBe(
      "leave reason=STATE_MISMATCH action=RECONNECT",
    );
  });

  it("still names the message when the leave carries no reason", () => {
    expect(describeResponse(new Uint8Array(field(8, [])))).toBe("leave");
  });
});

describe("logging a signalling URL", () => {
  it("removes the access token and the join request", () => {
    const url =
      "ws://localhost:7880/rtc/v1?access_token=eyJhbGciOiJIUzI1NiJ9.secret&join_request=CAES";
    const safe = redactSignalUrl(url);
    expect(safe).not.toContain("secret");
    expect(safe).not.toContain("CAES");
    expect(safe).toContain("localhost:7880/rtc/v1");
  });

  it("redacts in the text when the URL is not absolute", () => {
    // `URL` refuses a relative URL without a base, and supplying a placeholder base would
    // put a hostname that is not the server's into the log.
    expect(redactSignalUrl("/rtc/v1?access_token=secret&x=1")).toBe(
      "/rtc/v1?access_token=<redacted>&x=1",
    );
  });
});

/**
 * The identity the SFU assigns is what end-to-end encryption keys against, and without it
 * the encryptor refuses every outbound frame. A real call lost 44,743 consecutive audio
 * frames to exactly that, so the parse is worth pinning rather than assuming.
 */
describe("the local identity in a join response", () => {
  /** A length-delimited protobuf field. */
  const msg = (number: number, body: number[]): number[] => [
    (number << 3) | 2,
    body.length,
    ...body,
  ];
  const text = (s: string): number[] => Array.from(new TextEncoder().encode(s));

  /** `JoinResponse.participant` is field 2; `ParticipantInfo.identity` is field 2 of that. */
  const joinResponse = (identity: string): Uint8Array =>
    new Uint8Array(
      msg(1, msg(2, [...msg(1, text("PA_sid")), ...msg(2, text(identity))])),
    );

  it("reads the identity the SFU assigned", () => {
    expect(localIdentityFromJoin(joinResponse("@ali:example.org:DEVICE"))).toBe(
      "@ali:example.org:DEVICE",
    );
  });

  it("ignores every other kind of message", () => {
    // An `offer` (field 3) carries no identity, and most traffic is not a join.
    expect(localIdentityFromJoin(new Uint8Array(msg(3, text("v=0"))))).toBeNull();
    expect(localIdentityFromJoin(new Uint8Array())).toBeNull();
  });

  it("reports nothing rather than guessing when the field is absent", () => {
    // A join whose participant has a sid but no identity yet.
    const partial = new Uint8Array(msg(1, msg(2, msg(1, text("PA_sid")))));
    expect(localIdentityFromJoin(partial)).toBeNull();
  });

  it("refuses bytes that are not text", () => {
    // A wrong identity is worse than none: it encrypts under a key no peer looks for.
    const invalid = new Uint8Array(msg(1, msg(2, msg(2, [0xff, 0xfe, 0xfd]))));
    expect(localIdentityFromJoin(invalid)).toBeNull();
  });
});
