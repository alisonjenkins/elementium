/**
 * Pins the contract `ipc-error.ts` exists to hold: a coded envelope parses and maps to a
 * `DOMException` only where the shipped Element Call bundle is evidenced to branch on that
 * name (see `ipcErrorToRejection`'s doc), a plain non-JSON string -- an unconverted command,
 * or an older backend paired with this frontend -- still rejects sensibly rather than
 * crashing the caller's `catch` block, and malformed JSON never throws out of the parser.
 */
import { describe, expect, it } from "vitest";

import { ipcErrorMessage, ipcErrorToRejection, parseIpcError } from "./ipc-error";

describe("parseIpcError", () => {
  it("parses a well-formed envelope", () => {
    const raw = JSON.stringify({ code: "picker_cancelled", message: "declined", id: "" });
    expect(parseIpcError(raw)).toEqual({ code: "picker_cancelled", message: "declined", id: "" });
  });

  it("returns null for a legacy plain-string rejection instead of throwing", () => {
    // This is what every unconverted command still rejects with, and what a newer
    // frontend gets from an older backend during development -- it must not crash here.
    expect(() => parseIpcError("Peer connection not found")).not.toThrow();
    expect(parseIpcError("Peer connection not found")).toBeNull();
  });

  it("returns null for malformed JSON instead of throwing", () => {
    expect(() => parseIpcError("{not valid json")).not.toThrow();
    expect(parseIpcError("{not valid json")).toBeNull();
  });

  it("returns null for JSON that parses but is not the envelope shape", () => {
    expect(parseIpcError(JSON.stringify({ foo: "bar" }))).toBeNull();
    expect(parseIpcError(JSON.stringify(["code", "message", "id"]))).toBeNull();
    expect(parseIpcError(JSON.stringify(42))).toBeNull();
  });

  it("returns null for a non-string rejection instead of throwing", () => {
    expect(() => parseIpcError(new TypeError("invoke plumbing broke"))).not.toThrow();
    expect(parseIpcError(new TypeError("invoke plumbing broke"))).toBeNull();
    expect(parseIpcError(undefined)).toBeNull();
  });
});

describe("ipcErrorMessage", () => {
  it("prefers the envelope's message when there is one", () => {
    const raw = JSON.stringify({ code: "capture_failed", message: "portal unreachable", id: "" });
    expect(ipcErrorMessage(raw)).toBe("portal unreachable");
  });

  it("falls back to an Error's own message", () => {
    expect(ipcErrorMessage(new Error("boom"))).toBe("boom");
  });

  it("falls back to String() for anything else", () => {
    expect(ipcErrorMessage("Peer connection not found")).toBe("Peer connection not found");
  });
});

describe("ipcErrorToRejection", () => {
  it("maps picker_cancelled to a DOMException named NotAllowedError", () => {
    const raw = JSON.stringify({ code: "picker_cancelled", message: "declined", id: "" });
    const rejection = ipcErrorToRejection(raw, "fallback");
    expect(rejection).toBeInstanceOf(DOMException);
    expect((rejection as DOMException).name).toBe("NotAllowedError");
    expect(rejection.message).toBe("declined");
  });

  it("does not fabricate a DOMException name for a code with no evidenced mapping", () => {
    const raw = JSON.stringify({ code: "capture_failed", message: "portal unreachable", id: "" });
    const rejection = ipcErrorToRejection(raw, "fallback");
    expect(rejection).not.toBeInstanceOf(DOMException);
    expect(rejection.message).toBe("portal unreachable");
  });

  it("uses the caller's fallback message for a legacy plain-string rejection", () => {
    const rejection = ipcErrorToRejection("Peer connection not found", "fallback message");
    expect(rejection).not.toBeInstanceOf(DOMException);
    expect(rejection.message).toBe("fallback message");
  });

  it("never throws for a malformed-JSON rejection", () => {
    expect(() => ipcErrorToRejection("{not valid json", "fallback")).not.toThrow();
  });
});
