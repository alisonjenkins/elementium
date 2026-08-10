/**
 * The analyser, checked against signals whose damage is known exactly.
 *
 * The call test built on this takes minutes, needs a homeserver, an SFU and a sound server,
 * and reports one verdict at the end. If the analyser is wrong, that verdict is wrong in a
 * way nothing in the run would reveal -- a test that cannot see a dropout passes a call that
 * had one. So the instrument is calibrated here, in milliseconds, against a clean signal and
 * against signals broken on purpose: a hole cut out of the middle, and the cycle played
 * backwards.
 */
import { describe as suite, expect, test } from "vitest";
import {
  ALL_TONES,
  LADDERS,
  assess,
  describe,
  goertzelPower,
  identify,
  ladderWav,
  observePcm,
  toneIndex,
  type Ladder,
} from "./tone-ladder";

const RATE = 48_000;

/** The samples out of a generated WAV, as f32 mono. */
function pcmOf(wav: Uint8Array): Float32Array {
  const view = new DataView(wav.buffer, wav.byteOffset, wav.byteLength);
  const count = view.getUint32(40, true) / 2;
  const out = new Float32Array(count);
  for (let i = 0; i < count; i++) out[i] = view.getInt16(44 + i * 2, true) / 32768;
  return out;
}

/** Observe a block of PCM exactly as the call test does. */
const observe = (pcm: Float32Array) => observePcm(pcm, RATE, ALL_TONES);

suite("the tone ladders", () => {
  test("no two ladders share a frequency, and none is within 30Hz of another", () => {
    const sorted = [...ALL_TONES].sort((a, b) => a - b);
    const closest = Math.min(
      ...sorted.slice(1).map((f, i) => f - (sorted[i] as number)),
    );
    expect(
      closest,
      `two tones are ${closest}Hz apart; the classifier's tolerance is 25Hz, so they would ` +
        `be confusable and a stream could be attributed to the wrong sender.`,
    ).toBeGreaterThan(30);
  });

  test("a ladder's own tones are never octaves of each other", () => {
    // A harmonic of one tone landing exactly on another would make a clean sine look like
    // two tones at once, which reads as a spurious out-of-order transition.
    for (const ladder of Object.values(LADDERS)) {
      for (const a of ladder.tones) {
        for (const b of ladder.tones) {
          if (a >= b) continue;
          expect(Math.abs(b - 2 * a), `${a}Hz and ${b}Hz are an octave apart`).toBeGreaterThan(25);
        }
      }
    }
  });
});

suite("goertzelPower", () => {
  test("peaks at the frequency actually present", () => {
    const n = 1920; // 40ms
    const pcm = new Float32Array(n);
    for (let i = 0; i < n; i++) pcm[i] = Math.sin((2 * Math.PI * 1056 * i) / RATE);
    const onTone = goertzelPower(pcm, RATE, 1056);
    const offTone = goertzelPower(pcm, RATE, 1214);
    expect(onTone).toBeGreaterThan(offTone * 100);
  });
});

suite("assess", () => {
  test("an undamaged ladder reports every tone, in order, with no gap", () => {
    const pcm = pcmOf(ladderWav(LADDERS.elementium, { cycles: 3 }));
    const verdict = assess(observe(pcm), LADDERS.elementium);

    expect(verdict.tonesSeen, describe(verdict)).toBe(LADDERS.elementium.tones.length);
    expect(verdict.missing).toEqual([]);
    expect(verdict.orderErrors, describe(verdict)).toEqual([]);
    // One analysis window (40ms) may straddle each tone boundary and be discarded; two of
    // those back to back is the most a clean signal can produce.
    expect(verdict.longestGapMs, describe(verdict)).toBeLessThanOrEqual(120);
    expect(verdict.coverage, describe(verdict)).toBeGreaterThan(0.9);
    for (const m of verdict.measured) expect(m.median).toBe(m.nominal);
  });

  test("a hole cut out of the middle is reported as a gap of that length", () => {
    const pcm = pcmOf(ladderWav(LADDERS.elementium, { cycles: 3 }));
    // 600ms of silence starting one and a half tones in, which also removes a whole tone
    // slot -- exactly what a stalled jitter buffer does.
    const from = Math.round(0.6 * RATE);
    const to = Math.round(1.2 * RATE);
    pcm.fill(0, from, to);

    const verdict = assess(observe(pcm), LADDERS.elementium);
    expect(verdict.longestGapMs, describe(verdict)).toBeGreaterThan(500);
    expect(verdict.longestGapMs, describe(verdict)).toBeLessThan(750);
    expect(verdict.longestGapAtMs, describe(verdict)).toBeGreaterThan(500);
  });

  test("the cycle played backwards is reported as out of order, not as missing", () => {
    const backwards: Ladder = {
      name: "backwards",
      tones: [...LADDERS.elementium.tones].reverse(),
      toneMs: LADDERS.elementium.toneMs,
    };
    const pcm = pcmOf(ladderWav(backwards, { cycles: 3 }));
    const verdict = assess(observe(pcm), LADDERS.elementium);

    // Every tone is still present -- "was there audio in every band" cannot see this at all.
    expect(verdict.tonesSeen).toBe(LADDERS.elementium.tones.length);
    expect(verdict.missing).toEqual([]);
    // Every transition is wrong, which is the whole point of transmitting a sequence.
    expect(verdict.orderErrors.length, describe(verdict)).toBeGreaterThan(10);
  });

  test("silence is not mistaken for a signal", () => {
    const verdict = assess(observe(new Float32Array(RATE * 2)), LADDERS.elementium);
    expect(verdict.tonesSeen).toBe(0);
    expect(verdict.runs).toEqual([]);
  });

  test("noise is not mistaken for a signal", () => {
    // A maximum bin always exists; without a prominence threshold, noise would classify as
    // whichever tone it happened to lean towards.
    const pcm = new Float32Array(RATE);
    let seed = 1;
    for (let i = 0; i < pcm.length; i++) {
      seed = (seed * 1103515245 + 12345) & 0x7fffffff;
      pcm[i] = (seed / 0x3fffffff - 1) * 0.3;
    }
    const verdict = assess(observe(pcm), LADDERS.elementium);
    expect(verdict.coverage, describe(verdict)).toBeLessThan(0.1);
  });
});

suite("identify", () => {
  test("a stream is attributed to its sender by its content alone", () => {
    for (const expected of Object.values(LADDERS)) {
      const pcm = pcmOf(ladderWav(expected, { cycles: 2 }));
      const best = identify(observe(pcm), Object.values(LADDERS));
      expect(best?.ladder.name).toBe(expected.name);
    }
  });
});

suite("toneIndex", () => {
  test("accepts a peak a little off and rejects one far off", () => {
    const first = LADDERS.elementium.tones[0] as number;
    expect(toneIndex(LADDERS.elementium, first + 10)).toBe(0);
    expect(toneIndex(LADDERS.elementium, first + 60)).toBeNull();
  });
});
