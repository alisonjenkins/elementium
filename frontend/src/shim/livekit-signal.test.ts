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
