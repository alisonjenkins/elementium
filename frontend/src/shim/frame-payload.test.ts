/**
 * A video frame has to survive whichever IPC transport the webview ended up using.
 *
 * `get_video_frame` returns raw bytes. Tauri's custom-protocol IPC hands those to the page
 * as an `ArrayBuffer`; its postMessage fallback -- which WebKitGTK forces whenever the page
 * is served over http, because it refuses a custom-scheme fetch from an http origin --
 * hands over the same bytes as an ordinary array of numbers.
 *
 * Reading `.byteLength` off the second shape gives `undefined`, and `undefined > 8` is
 * false, so every frame was skipped without an error, without a counter, and without a log
 * line: 29fps fetched and `drawn=0`. These pin the coercion that removes the class, and the
 * prototype walk that finds `enabled` on a canvas capture track.
 */
import { describe, expect, it } from "vitest";

import { frameBytes } from "./frame-payload";

/** The production prototype walk, kept in step with `enabledDescriptor`. */
function enabledDescriptor(track: object): PropertyDescriptor | undefined {
  let proto: object | null = Object.getPrototypeOf(track) as object | null;
  while (proto) {
    const descriptor = Object.getOwnPropertyDescriptor(proto, "enabled");
    if (descriptor) return descriptor;
    proto = Object.getPrototypeOf(proto) as object | null;
  }
  return undefined;
}

/** A frame as the backend writes it: width, height, then RGBA. */
function frame(width: number, height: number): number[] {
  const header = [width & 0xff, width >> 8, 0, 0, height & 0xff, height >> 8, 0, 0];
  return header.concat(new Array(width * height * 4).fill(0x7f));
}

describe("video frame payloads", () => {
  it("reads a frame that arrived as an array of numbers", () => {
    const buf = frameBytes(frame(2, 2));
    expect(buf).not.toBeNull();
    const view = new DataView(buf as ArrayBuffer);
    expect(view.getUint32(0, true)).toBe(2);
    expect(view.getUint32(4, true)).toBe(2);
    expect((buf as ArrayBuffer).byteLength - 8).toBe(2 * 2 * 4);
  });

  it("passes an ArrayBuffer through untouched", () => {
    const original = new Uint8Array(frame(1, 1)).buffer;
    expect(frameBytes(original)).toBe(original);
  });

  it("reads a frame that arrived as a typed-array view", () => {
    // A view into a larger buffer must contribute only its own bytes, or the geometry
    // check disagrees with the payload and the picture shears.
    const backing = new Uint8Array([0xff, 0xff].concat(frame(1, 1)));
    const view = new Uint8Array(backing.buffer, 2, backing.byteLength - 2);
    const buf = frameBytes(view);
    expect(buf).not.toBeNull();
    expect(new DataView(buf as ArrayBuffer).getUint32(0, true)).toBe(1);
    expect((buf as ArrayBuffer).byteLength - 8).toBe(4);
  });

  it("reports anything else as unusable rather than guessing", () => {
    expect(frameBytes(null)).toBeNull();
    expect(frameBytes(undefined)).toBeNull();
    expect(frameBytes("not a frame")).toBeNull();
    expect(frameBytes({ data: [1, 2, 3] })).toBeNull();
  });
});

describe("finding the enabled accessor", () => {
  it("finds it on a plain track, where it is one level up", () => {
    class Track {}
    Object.defineProperty(Track.prototype, "enabled", { get: () => true, set: () => {} });
    expect(enabledDescriptor(new Track())?.set).toBeDefined();
  });

  it("finds it on a canvas capture track, where it is two levels up", () => {
    // This is the case that was missed: the camera track comes from
    // `canvas.captureStream()`, so its immediate prototype is a subclass that does not
    // declare `enabled`, and video mute silently never reached the backend.
    class Track {}
    Object.defineProperty(Track.prototype, "enabled", { get: () => true, set: () => {} });
    class CanvasTrack extends Track {}
    expect(enabledDescriptor(new CanvasTrack())?.set).toBeDefined();
  });

  it("reports nothing when no prototype declares it", () => {
    class Bare {}
    expect(enabledDescriptor(new Bare())).toBeUndefined();
  });
});
