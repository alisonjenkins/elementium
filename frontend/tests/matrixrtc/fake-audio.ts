/**
 * Give Elementium a known microphone, and quiet speakers, without touching the machine.
 *
 * Both are environment variables on the application's own process. Nothing here changes a
 * setting, loads a module, or leaves anything behind: if the run is killed halfway through,
 * the developer's sound server is exactly as it was.
 *
 * # The microphone
 *
 * `ELEMENTIUM_FAKE_MIC` points the native capture path at a WAV file instead of a device --
 * the same thing Chromium's `--use-file-for-fake-audio-capture` does for the browser
 * participants in these tests, and for the same reason. See `crates/elementium-media/src/
 * fake_mic.rs`, including what it does not prove.
 *
 * An earlier version of this file did it the other way: a `PulseAudio` null sink playing the
 * signal, with the session's default source pointed at its monitor. It is recorded here
 * because it looks like the obvious approach and is not. It did retarget ALSA's `default` --
 * verified by capturing through it -- and it did not retarget Elementium, which opens the
 * device it was asked for by id. The measurement that settled it: `capture-raw` peaked at
 * 0.0006 on one channel and exactly 0.0 on the other, which is a hardware microphone in a
 * quiet room. It also reconfigured the developer's microphone and speakers for the length of
 * a test and restored them only on a clean exit, which is not a thing a test should do.
 *
 * # The speakers
 *
 * Elementium plays what it receives, and what it receives here is two continuous tone ladders.
 * `ALSA_CONFIG_PATH` is per-process: pointing it at a config that includes the real one and
 * then sends *playback only* to ALSA's null device silences this one process, and leaves
 * capture -- and every other process -- untouched. Best effort: if the system ALSA config
 * cannot be located the test still runs, and says that the tones will be audible.
 */
import { access, writeFile } from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { ladderWav, type Ladder } from "./tone-ladder";

/**
 * Write `ladder` to a WAV and return the environment that makes it Elementium's microphone.
 *
 * A whole number of cycles, so the point where the file loops is also a point where the
 * signal would have moved to its next tone anyway -- a loop seam in the middle of a tone
 * would be a discontinuity the test would then have to explain away.
 */
export async function fakeMicEnv(ladder: Ladder, cycles = 10): Promise<Record<string, string>> {
  const wavPath = path.join(os.tmpdir(), `elementium-${ladder.name}-ladder.wav`);
  await writeFile(wavPath, ladderWav(ladder, { cycles }));
  return { ELEMENTIUM_FAKE_MIC: wavPath };
}

/**
 * Where alsa-lib keeps the top-level config this machine's ALSA setup is built from.
 *
 * Located through `LD_LIBRARY_PATH`, which in this project's nix shell holds the alsa-lib
 * store path. There is no portable way to ask alsa-lib for it, and guessing `/usr/share`
 * would be wrong on exactly the machine this runs on.
 */
async function systemAlsaConf(): Promise<string | null> {
  const candidates = (process.env["LD_LIBRARY_PATH"] ?? "")
    .split(":")
    .filter((dir) => dir.includes("alsa-lib"))
    .map((dir) => path.join(path.dirname(dir), "share/alsa/alsa.conf"))
    .concat(["/usr/share/alsa/alsa.conf"]);
  for (const candidate of candidates) {
    try {
      await access(candidate);
      return candidate;
    } catch {
      /* try the next one */
    }
  }
  return null;
}

/**
 * Environment that keeps one process's audio playback silent, or `{}` if it cannot be built.
 *
 * `type asym` is what splits the two directions: playback goes to ALSA's null device, capture
 * is left as it was. Only this process sees it.
 */
export async function silentPlaybackEnv(): Promise<Record<string, string>> {
  const conf = await systemAlsaConf();
  if (conf === null) return {};
  const confPath = path.join(os.tmpdir(), "elementium-silent-playback.conf");
  await writeFile(
    confPath,
    [
      `<${conf}>`,
      "pcm.!default {",
      "    type asym",
      '    playback.pcm "null"',
      "    capture.pcm { type pipewire }",
      "}",
      "",
    ].join("\n"),
  );
  return { ALSA_CONFIG_PATH: confPath };
}
