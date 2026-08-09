/**
 * A constraint value is not always the plain scalar the shim used to assume.
 *
 * The DOM lets `deviceId` be a string, `{exact}`, `{ideal}`, or an array of those, and
 * `echoCancellation` and friends be `{exact: true}`. This shim cast both straight to
 * `string | undefined` and `boolean | undefined` and handed them to Rust, which declares
 * them as `Option<String>` and `Option<bool>` -- so an object form would fail IPC
 * deserialization and reject the whole `getUserMedia` call with `NotAllowedError`, taking
 * the camera and microphone down over what was only a device *preference*.
 *
 * It never fired because Element Call passes a plain string. matrix-js-sdk's `MediaHandler`
 * builds `{exact: ...}` and ships in the same bundle, so it was one code path away.
 *
 * These pin the shapes rather than the call: the extraction is the part that was wrong.
 */
import { describe, expect, it } from "vitest";

/** The production extractor, kept in step with `extractDeviceId`. */
function extractDeviceId(value: unknown): string | undefined {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return extractDeviceId(value[0]);
  if (typeof value === "object" && value !== null) {
    const obj = value as Record<string, unknown>;
    if ("exact" in obj) return extractDeviceId(obj["exact"]);
    if ("ideal" in obj) return extractDeviceId(obj["ideal"]);
  }
  return undefined;
}

/** The production extractor, kept in step with `extractBooleanConstraint`. */
function extractBooleanConstraint(value: unknown, fallback: boolean): boolean {
  if (typeof value === "boolean") return value;
  if (typeof value === "object" && value !== null) {
    const obj = value as Record<string, unknown>;
    if ("exact" in obj) return extractBooleanConstraint(obj["exact"], fallback);
    if ("ideal" in obj) return extractBooleanConstraint(obj["ideal"], fallback);
  }
  return fallback;
}

describe("deviceId constraint shapes", () => {
  it("takes a plain string, which is what Element Call sends", () => {
    expect(extractDeviceId("mic-3")).toBe("mic-3");
  });

  it("unwraps the {exact} form matrix-js-sdk builds", () => {
    expect(extractDeviceId({ exact: "mic-3" })).toBe("mic-3");
  });

  it("unwraps {ideal}, and prefers {exact} when both are present", () => {
    expect(extractDeviceId({ ideal: "mic-3" })).toBe("mic-3");
    // exact is a requirement where ideal is only a preference, so it wins.
    expect(extractDeviceId({ exact: "wanted", ideal: "other" })).toBe("wanted");
  });

  it("takes the first entry of an array, which is the most preferred", () => {
    expect(extractDeviceId(["first", "second"])).toBe("first");
    expect(extractDeviceId([{ exact: "first" }])).toBe("first");
  });

  it("reports no preference rather than passing something Rust cannot read", () => {
    expect(extractDeviceId(undefined)).toBeUndefined();
    expect(extractDeviceId({})).toBeUndefined();
    expect(extractDeviceId(42)).toBeUndefined();
  });
});

describe("boolean constraint shapes", () => {
  it("takes a plain boolean", () => {
    expect(extractBooleanConstraint(false, true)).toBe(false);
  });

  it("unwraps {exact} and {ideal}", () => {
    expect(extractBooleanConstraint({ exact: false }, true)).toBe(false);
    expect(extractBooleanConstraint({ ideal: false }, true)).toBe(false);
  });

  it("falls back when the value says nothing, rather than guessing false", () => {
    // These gate echo cancellation, noise suppression and gain control: defaulting them
    // off because a constraint was an unexpected shape would degrade every call.
    expect(extractBooleanConstraint(undefined, true)).toBe(true);
    expect(extractBooleanConstraint({}, true)).toBe(true);
  });
});
