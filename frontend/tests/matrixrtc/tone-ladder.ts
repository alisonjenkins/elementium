/**
 * A known signal, and the arithmetic for deciding whether it survived a call.
 *
 * Every audio measurement in this repository until now answered "was there audio": packets,
 * samples, decoded frames, concealment. None of them can tell a voice from a chainsaw, and
 * the fault that prompted this -- "the mic audio is bad" -- reads healthy on all of them.
 * The only way to answer "is it the *same* audio" is to transmit something recognisable and
 * look for it at the far end.
 *
 * # Why a ladder of distinct tones, and not one continuous tone
 *
 * A single steady tone detects dropouts and gross distortion and nothing else. It cannot see
 * reordering, it cannot see a stretch of audio being repeated, and a receiver that is stuck
 * replaying its last 20ms forever looks perfect. A *sequence* of distinct tones, cycling in a
 * fixed order, turns each of those into an observable:
 *
 * - a tone that never appears is a band that did not survive;
 * - tones appearing out of the cycle order are reordering, or a chunk skipped;
 * - a stretch where no expected tone dominates is a gap, measurable in milliseconds;
 * - a tone that persists past its slot is a receiver repeating itself.
 *
 * It is also immune to "it sounded fine to me": the verdict is a list of frequencies and a
 * gap in milliseconds, not an opinion.
 *
 * # Why these frequencies
 *
 * The three ladders below interleave on one geometric grid of ratio 1.15, so that
 *
 * - within a ladder, consecutive tones are a factor of 1.52 apart -- far wider than any
 *   analysis bin, and never an exact octave, so a harmonic of one tone is never mistaken for
 *   another tone of the same ladder;
 * - between ladders, the nearest two tones are 15% apart (45Hz at the bottom of the range),
 *   which is four analysis bins at the resolution used here.
 *
 * The separation between ladders is what lets a receiver work out *who* it is listening to
 * from the audio alone. That matters more than it sounds: at the far end there is no reliable
 * way to attribute an inbound RTP stream to a participant without trusting the very signalling
 * the test is meant to check. Here the signal identifies its own sender.
 *
 * Everything in this module is pure -- no browser, no filesystem, no clock -- so it is unit
 * tested (`tone-ladder.test.ts`) rather than only exercised by a twelve-minute call.
 */

/** One participant's signal: a fixed cycle of tones, each held for `toneMs`. */
export interface Ladder {
  /** Who transmits it. Only ever used in reports. */
  readonly name: string;
  /** The tones, in the order they are transmitted, then repeated from the start. */
  readonly tones: readonly number[];
  /** How long each tone is held, in milliseconds. */
  readonly toneMs: number;
}

/** The geometric grid the three ladders are drawn from. Ratio 1.15, base 300Hz. */
const GRID = Array.from({ length: 18 }, (_, i) => Math.round(300 * 1.15 ** i));

/** Take every third grid slot starting at `offset`, so the three ladders interleave. */
const everyThird = (offset: number): number[] => GRID.filter((_, i) => i % 3 === offset);

/**
 * How long each tone is held.
 *
 * 400ms is a compromise. Shorter makes a gap easier to attribute to a particular tone and
 * shortens the cycle, but Opus needs a few frames to settle on a new fundamental and the
 * analysis window is 40ms, so a slot must be many windows long for a run to be unambiguous.
 * Longer would make the whole cycle too slow to see twice inside a ten-second measurement.
 */
const TONE_MS = 400;

/**
 * The three signals in play: one per participant, so a receiver can tell them apart.
 *
 * Elementium takes the lowest ladder because it is the one transmitted through a real capture
 * device and a real encoder, and the low end is where a resampling fault shows first.
 */
export const LADDERS = {
  elementium: { name: "elementium", tones: everyThird(0), toneMs: TONE_MS },
  peerB: { name: "peerB", tones: everyThird(1), toneMs: TONE_MS },
  peerC: { name: "peerC", tones: everyThird(2), toneMs: TONE_MS },
} as const satisfies Record<string, Ladder>;

/** Every frequency any ladder uses, for a receiver that does not yet know who is talking. */
export const ALL_TONES: readonly number[] = Object.values(LADDERS)
  .flatMap((l) => l.tones)
  .sort((a, b) => a - b);

/**
 * Energy at one frequency in one block of samples, by the Goertzel algorithm.
 *
 * Goertzel rather than an FFT because only a handful of frequencies are ever of interest and
 * they are known in advance: this is a few multiplies per sample per frequency, needs no
 * library, and -- unlike a bin of an FFT -- is evaluated at the exact frequency asked for
 * rather than at whatever bin centre happens to be nearest.
 *
 * The result is a power, in arbitrary units, comparable only against other calls of this
 * function on the same block.
 */
export function goertzelPower(
  samples: Float32Array,
  sampleRate: number,
  frequency: number,
): number {
  const k = (2 * Math.PI * frequency) / sampleRate;
  const coeff = 2 * Math.cos(k);
  let s1 = 0;
  let s2 = 0;
  for (let i = 0; i < samples.length; i++) {
    const s0 = (samples[i] ?? 0) + coeff * s1 - s2;
    s2 = s1;
    s1 = s0;
  }
  // Normalised by block length so blocks of different sizes are comparable.
  const n = Math.max(1, samples.length);
  return (s1 * s1 + s2 * s2 - coeff * s1 * s2) / (n * n);
}

/** One instant of a receiver's view: which tone dominated, and how strongly. */
export interface Observation {
  /** Milliseconds since measurement began. */
  atMs: number;
  /** The frequency carrying the most energy, in Hz. */
  hz: number;
  /** That frequency's level, in dB, on whatever scale the observer used. */
  db: number;
  /** The level everything else sat at, in the same dB scale. */
  floorDb: number;
}

/** How far from a nominal tone a measured peak may be and still count as that tone. */
const TOLERANCE_HZ = 25;

/**
 * How far above the surrounding spectrum a peak must sit to count as a tone at all.
 *
 * A silent stream still has a maximum bin -- noise has a peak too -- so "which bin was
 * highest" is not on its own evidence of anything. 12dB is well below the ~40dB a pure tone
 * stands out by, and well above the few dB of an unremarkable noise floor.
 */
const MIN_PROMINENCE_DB = 12;

/** Index of the ladder tone `hz` corresponds to, or `null` if it is none of them. */
export function toneIndex(ladder: Ladder, hz: number): number | null {
  let best: number | null = null;
  let bestErr = TOLERANCE_HZ;
  ladder.tones.forEach((f, i) => {
    const err = Math.abs(f - hz);
    if (err <= bestErr) {
      bestErr = err;
      best = i;
    }
  });
  return best;
}

/** A stretch of consecutive observations that all showed the same tone. */
export interface Run {
  tone: number;
  hz: number;
  fromMs: number;
  toMs: number;
}

/** What a receiver actually got, and whether it is the signal that was sent. */
export interface Verdict {
  ladder: string;
  /** How many of the ladder's tones were seen at all. */
  tonesSeen: number;
  /** The ladder's tones that never appeared, in Hz. */
  missing: number[];
  /** Median measured frequency for each tone seen, against its nominal value. */
  measured: { nominal: number; median: number | null }[];
  /** Transitions that did not follow the cycle, e.g. tone 3 straight to tone 5. */
  orderErrors: { fromMs: number; from: number; to: number }[];
  /** The longest stretch, inside the signal, where no ladder tone dominated. */
  longestGapMs: number;
  /** When that gap began. */
  longestGapAtMs: number;
  /** How much of the measured span carried a ladder tone. */
  coverage: number;
  /** How long the analysis considered, after trimming lead-in and tail silence. */
  spanMs: number;
  runs: Run[];
}

/** Median of a list, or `null` when it is empty. */
function median(xs: number[]): number | null {
  if (xs.length === 0) return null;
  const sorted = [...xs].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0
    ? ((sorted[mid - 1] ?? 0) + (sorted[mid] ?? 0)) / 2
    : (sorted[mid] ?? 0);
}

/**
 * Turn a stream of observations into a statement about whether the ladder arrived.
 *
 * Leading and trailing stretches with no tone are trimmed before anything is measured: a
 * receiver that starts observing before the sender is publishing has not experienced a
 * dropout, and counting the wait as a gap would make every run fail for the wrong reason.
 *
 * Runs shorter than `minRunSamples` observations are discarded rather than treated as the
 * signal changing tone. Two consecutive tones meet at a discontinuity, and an analysis window
 * straddling that boundary sees both -- a single stray observation there is the instrument,
 * not the audio.
 */
export function assess(
  observations: readonly Observation[],
  ladder: Ladder,
  options: { minRunSamples?: number } = {},
): Verdict {
  const minRun = options.minRunSamples ?? 2;
  const classified = observations.map((o) => ({
    ...o,
    tone: o.db - o.floorDb >= MIN_PROMINENCE_DB ? toneIndex(ladder, o.hz) : null,
  }));

  const first = classified.findIndex((c) => c.tone !== null);
  let last = -1;
  for (let i = classified.length - 1; i >= 0; i--) {
    if (classified[i]?.tone !== null) {
      last = i;
      break;
    }
  }
  const inSignal = first === -1 ? [] : classified.slice(first, last + 1);

  // Runs first, so a one-sample excursion at a tone boundary can be dropped before any
  // ordering or gap conclusion is drawn from it.
  const raw: { tone: number | null; from: number; to: number; hzs: number[] }[] = [];
  for (const c of inSignal) {
    const tail = raw[raw.length - 1];
    if (tail && tail.tone === c.tone) {
      tail.to = c.atMs;
      if (c.tone !== null) tail.hzs.push(c.hz);
    } else {
      raw.push({ tone: c.tone, from: c.atMs, to: c.atMs, hzs: c.tone === null ? [] : [c.hz] });
    }
  }
  const kept = raw.filter((r) => r.tone === null || r.hzs.length >= minRun);

  const runs: Run[] = kept
    .filter((r) => r.tone !== null)
    .map((r) => ({
      tone: r.tone as number,
      hz: median(r.hzs) ?? 0,
      fromMs: r.from,
      toMs: r.to,
    }));

  // A gap is the distance between the end of one accepted tone run and the start of the next.
  // Measured that way rather than by counting null observations, because the discarded short
  // runs above must still count as time in which the signal was absent.
  let longestGapMs = 0;
  let longestGapAtMs = 0;
  for (let i = 1; i < runs.length; i++) {
    const gap = (runs[i]?.fromMs ?? 0) - (runs[i - 1]?.toMs ?? 0);
    if (gap > longestGapMs) {
      longestGapMs = gap;
      longestGapAtMs = runs[i - 1]?.toMs ?? 0;
    }
  }

  const n = ladder.tones.length;
  const orderErrors: Verdict["orderErrors"] = [];
  for (let i = 1; i < runs.length; i++) {
    const from = runs[i - 1]!;
    const to = runs[i]!;
    if (to.tone !== (from.tone + 1) % n) {
      orderErrors.push({ fromMs: from.toMs, from: from.tone, to: to.tone });
    }
  }

  const seen = new Set(runs.map((r) => r.tone));
  const spanMs =
    inSignal.length > 0 ? (inSignal[inSignal.length - 1]?.atMs ?? 0) - (inSignal[0]?.atMs ?? 0) : 0;
  const covered = runs.reduce((acc, r) => acc + (r.toMs - r.fromMs), 0);

  return {
    ladder: ladder.name,
    tonesSeen: seen.size,
    missing: ladder.tones.filter((_, i) => !seen.has(i)),
    measured: ladder.tones.map((nominal, i) => ({
      nominal,
      median: median(runs.filter((r) => r.tone === i).map((r) => r.hz)),
    })),
    orderErrors,
    longestGapMs,
    longestGapAtMs,
    coverage: spanMs > 0 ? covered / spanMs : 0,
    spanMs,
    runs,
  };
}

/** A verdict as lines a person can read, so a failure says what was wrong. */
export function describe(v: Verdict): string {
  const measured = v.measured
    .map((m) => `${m.nominal}Hz->${m.median === null ? "absent" : `${m.median.toFixed(0)}Hz`}`)
    .join(" ");
  const order =
    v.orderErrors.length === 0
      ? "in order"
      : v.orderErrors
          .slice(0, 5)
          .map((e) => `tone ${e.from}->${e.to} at ${(e.fromMs / 1000).toFixed(1)}s`)
          .join(", ");
  return (
    `${v.ladder}: ${v.tonesSeen}/${v.measured.length} tones over ${(v.spanMs / 1000).toFixed(1)}s\n` +
    `    measured ${measured}\n` +
    `    ${v.orderErrors.length} out-of-order transitions (${order})\n` +
    `    longest gap ${v.longestGapMs.toFixed(0)}ms at ${(v.longestGapAtMs / 1000).toFixed(1)}s, ` +
    `coverage ${(v.coverage * 100).toFixed(0)}%`
  );
}

/**
 * Which ladder a set of observations is most likely to be, judged only by what was heard.
 *
 * The point of interleaving the three ladders on one grid: an inbound stream can be
 * attributed to its sender from its own content, without trusting the signalling that says
 * who it came from.
 */
export function identify(observations: readonly Observation[], ladders: readonly Ladder[]) {
  const scored = ladders.map((ladder) => ({ ladder, verdict: assess(observations, ladder) }));
  scored.sort((a, b) => b.verdict.tonesSeen - a.verdict.tonesSeen || b.verdict.coverage - a.verdict.coverage);
  return scored[0];
}

/**
 * The ladder as a 16-bit mono WAV, `cycles` repetitions long.
 *
 * Generated here rather than by shelling out to ffmpeg: the whole value of this signal is
 * that it is known exactly, and a generator in the same module as the analyser cannot drift
 * from it. It is also what lets the round trip -- generate, analyse -- be a unit test.
 *
 * Each tone is faded in and out over 5ms. A hard join between two sines is a step
 * discontinuity, which is broadband: it puts energy in every bin for one analysis window and
 * would show up as a spurious out-of-order transition at every tone boundary.
 */
export function ladderWav(
  ladder: Ladder,
  options: { cycles: number; sampleRate?: number; amplitude?: number },
): Uint8Array {
  const sampleRate = options.sampleRate ?? 48_000;
  const amplitude = options.amplitude ?? 0.5;
  const perTone = Math.round((ladder.toneMs / 1000) * sampleRate);
  const fade = Math.round(0.005 * sampleRate);
  const total = perTone * ladder.tones.length * options.cycles;

  const bytes = new Uint8Array(44 + total * 2);
  const view = new DataView(bytes.buffer);
  const ascii = (offset: number, s: string) => {
    for (let i = 0; i < s.length; i++) view.setUint8(offset + i, s.charCodeAt(i));
  };
  ascii(0, "RIFF");
  view.setUint32(4, 36 + total * 2, true);
  ascii(8, "WAVEfmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true); // PCM
  view.setUint16(22, 1, true); // mono
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  ascii(36, "data");
  view.setUint32(40, total * 2, true);

  let at = 0;
  for (let cycle = 0; cycle < options.cycles; cycle++) {
    for (const hz of ladder.tones) {
      // Phase restarts per tone; the fade makes the restart inaudible and spectrally clean.
      for (let i = 0; i < perTone; i++) {
        const envelope = Math.min(1, i / fade, (perTone - 1 - i) / fade);
        const v = amplitude * envelope * Math.sin((2 * Math.PI * hz * i) / sampleRate);
        view.setInt16(44 + at * 2, Math.max(-32768, Math.min(32767, Math.round(v * 32767))), true);
        at++;
      }
    }
  }
  return bytes;
}

/**
 * Observe a block of PCM the way a browser's `AnalyserNode` observes a live stream.
 *
 * The same shape of measurement, so one implementation of [`assess`] judges both the far end
 * (a real analyser, in a real browser, on a live track) and Elementium's own inbound audio
 * (raw f32 written to disk by `ELEMENTIUM_AUDIO_DUMP`). Two analyses that disagreed about
 * what counts as a gap would be two tests, not one measurement from two ends.
 *
 * `floorDb` is the median across the candidate frequencies rather than a true spectral floor:
 * with several well-separated candidates and only one tone present at a time, the median is
 * one of the absent ones, which is exactly the level a peak should be judged against.
 */
export function observePcm(
  samples: Float32Array,
  sampleRate: number,
  candidates: readonly number[],
  windowMs = 40,
): Observation[] {
  const per = Math.max(1, Math.round((windowMs / 1000) * sampleRate));
  const out: Observation[] = [];
  for (let start = 0; start + per <= samples.length; start += per) {
    const block = samples.subarray(start, start + per);
    const powers = candidates.map((hz) => goertzelPower(block, sampleRate, hz));
    let best = 0;
    for (let i = 1; i < powers.length; i++) if ((powers[i] ?? 0) > (powers[best] ?? 0)) best = i;
    const db = (p: number) => 10 * Math.log10(Math.max(p, 1e-20));
    out.push({
      atMs: (start / sampleRate) * 1000,
      hz: candidates[best] ?? 0,
      db: db(powers[best] ?? 0),
      floorDb: db(median(powers.filter((_, i) => i !== best)) ?? 1e-20),
    });
  }
  return out;
}
