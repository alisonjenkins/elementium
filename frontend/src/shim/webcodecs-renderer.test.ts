import { describe, expect, it } from "vitest";

import { takeFrames } from "./webcodecs-renderer";

/**
 * The framing is the whole correctness of this path. A VP8 interframe references the one
 * before it, so a reader that loses a frame boundary -- or silently drops a frame -- hands
 * the decoder a broken reference chain and produces the frozen picture this project has
 * already diagnosed twice from other causes.
 *
 * A chunk off the network has no relationship to a frame boundary: one read can carry half a
 * frame, or three frames and a fragment. These pin that.
 */
describe("takeFrames", () => {
  const HEADER = 13;

  /** Build the wire form Rust writes: `[u32 len][u8 keyframe][u64 timestamp][payload]`. */
  function wire(payload: number[], keyframe: boolean, timestamp: number): Uint8Array {
    const out = new Uint8Array(HEADER + payload.length);
    const view = new DataView(out.buffer);
    view.setUint32(0, payload.length, true);
    view.setUint8(4, keyframe ? 1 : 0);
    view.setBigUint64(5, BigInt(timestamp), true);
    out.set(payload, HEADER);
    return out;
  }

  function join(...parts: Uint8Array[]): Uint8Array {
    const total = parts.reduce((n, p) => n + p.byteLength, 0);
    const out = new Uint8Array(total);
    let at = 0;
    for (const p of parts) {
      out.set(p, at);
      at += p.byteLength;
    }
    return out;
  }

  it("reads a whole frame and reports nothing left over", () => {
    const { frames, rest } = takeFrames(wire([1, 2, 3], true, 1000));
    expect(frames).toHaveLength(1);
    expect(frames[0]?.keyframe).toBe(true);
    expect(frames[0]?.timestamp).toBe(1000);
    expect(Array.from(frames[0]?.data ?? [])).toEqual([1, 2, 3]);
    expect(rest.byteLength).toBe(0);
  });

  it("reads several frames out of one chunk, in order", () => {
    const { frames, rest } = takeFrames(
      join(wire([1], true, 0), wire([2], false, 33_333), wire([3], false, 66_666)),
    );
    expect(frames.map((f) => f.timestamp)).toEqual([0, 33_333, 66_666]);
    expect(frames.map((f) => f.keyframe)).toEqual([true, false, false]);
    expect(rest.byteLength).toBe(0);
  });

  /** The case that loses sync if the length prefix is misread. */
  it("keeps a partial frame as remainder rather than emitting it", () => {
    const full = wire([9, 9, 9, 9], false, 5);
    const { frames, rest } = takeFrames(full.subarray(0, full.byteLength - 2));
    expect(frames).toHaveLength(0);
    expect(rest.byteLength).toBe(full.byteLength - 2);
  });

  it("keeps a header split across chunks as remainder", () => {
    const { frames, rest } = takeFrames(new Uint8Array([0, 0, 0]));
    expect(frames).toHaveLength(0);
    expect(rest.byteLength).toBe(3);
  });

  /**
   * The realistic case: a read boundary lands in the middle of the second frame. The first
   * must come out now and the second must survive intact to be completed by the next read.
   */
  it("emits what is complete and carries the fragment forward", () => {
    const first = wire([1, 1], true, 0);
    const second = wire([2, 2, 2, 2], false, 100);
    const buffer = join(first, second.subarray(0, 5));

    const { frames, rest } = takeFrames(buffer);
    expect(frames).toHaveLength(1);
    expect(frames[0]?.timestamp).toBe(0);

    // The next read completes it, with nothing lost across the boundary.
    const completed = new Uint8Array(rest.byteLength + (second.byteLength - 5));
    completed.set(rest, 0);
    completed.set(second.subarray(5), rest.byteLength);
    const next = takeFrames(completed);
    expect(next.frames).toHaveLength(1);
    expect(Array.from(next.frames[0]?.data ?? [])).toEqual([2, 2, 2, 2]);
    expect(next.rest.byteLength).toBe(0);
  });

  it("handles an empty buffer and a zero-length frame without looping", () => {
    expect(takeFrames(new Uint8Array(0)).frames).toHaveLength(0);
    const { frames, rest } = takeFrames(wire([], false, 7));
    expect(frames).toHaveLength(1);
    expect(frames[0]?.data.byteLength).toBe(0);
    expect(rest.byteLength).toBe(0);
  });
});
