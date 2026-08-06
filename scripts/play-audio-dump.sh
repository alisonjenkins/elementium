#!/usr/bin/env bash
# Wrap the raw PCM dumps written when ELEMENTIUM_AUDIO_DUMP is enabled as .wav files,
# and report their levels.
#
# The dumps are headerless little-endian f32, so sample rate and channel count are not in
# the file and have to be supplied. Everything here is 48kHz stereo unless a device forced
# otherwise -- check the "Audio capture started" log line, which records the real rate.
#
# Deliberately depends on nothing but python3 (no ffmpeg/sox): the resulting .wav opens
# directly in Audacity.
#
# Usage:
#   scripts/play-audio-dump.sh            # convert every dump found
#   scripts/play-audio-dump.sh <file>     # convert one
set -euo pipefail

RATE="${ELEMENTIUM_DUMP_RATE:-48000}"
CHANNELS="${ELEMENTIUM_DUMP_CHANNELS:-2}"

python3 - "$RATE" "$CHANNELS" "$@" <<'PY'
import array, math, struct, sys, glob, os

rate = int(sys.argv[1])
channels = int(sys.argv[2])
args = sys.argv[3:]

paths = args if args else sorted(glob.glob("/tmp/elementium_audio_dump_*.f32le"))
if not paths:
    print("No dumps found. Enable capture and reproduce:", file=sys.stderr)
    print("  touch /tmp/ELEMENTIUM_AUDIO_DUMP   # then restart the app", file=sys.stderr)
    sys.exit(1)

if not args:
    print("Outbound bisection points (compare in order to localise a fault):")
    print("  capture-raw         what the microphone produced, untouched")
    print("  capture-encoder-in  after resample/reframe, as handed to Opus")
    print("  capture-loopback    encoded then decoded -- what the far end should hear")
    print()


def wav_header(n_bytes):
    # 32-bit IEEE float (WAVE_FORMAT_IEEE_FLOAT = 3), which Audacity imports without
    # prompting for a format. Float rather than 16-bit PCM on purpose: these captures can
    # sit at -60 dBFS, where 16-bit quantisation would itself become the loudest thing in
    # the file and make a quiet-but-clean signal look broken.
    #
    # Non-PCM formats are specified to carry a `fact` chunk giving the sample count; many
    # readers tolerate its absence, some do not, so it is included.
    bits = 32
    block_align = channels * bits // 8
    byte_rate = rate * block_align
    fmt_chunk = b"fmt " + struct.pack(
        "<IHHIIHH", 16, 3, channels, rate, byte_rate, block_align, bits
    )
    fact_chunk = b"fact" + struct.pack("<II", 4, n_bytes // block_align if block_align else 0)
    body = b"WAVE" + fmt_chunk + fact_chunk + b"data" + struct.pack("<I", n_bytes)
    return b"RIFF" + struct.pack("<I", len(body) + n_bytes) + body


def dbfs(x):
    return "-inf" if x <= 0 else f"{20 * math.log10(x):.1f}"


for src in paths:
    raw = open(src, "rb").read()
    # Truncate any partial trailing sample (process may have been killed mid-write).
    usable = len(raw) - (len(raw) % (4 * channels))
    samples = array.array("f")
    samples.frombytes(raw[:usable])
    if sys.byteorder != "little":
        samples.byteswap()

    dst = src[:-len(".f32le")] + ".wav" if src.endswith(".f32le") else src + ".wav"
    with open(dst, "wb") as out:
        out.write(wav_header(usable))
        out.write(raw[:usable])

    n = len(samples)
    if n:
        peak = max(abs(s) for s in samples)
        rms = math.sqrt(sum(s * s for s in samples) / n)
        secs = n / (rate * channels)
        print(f"{os.path.basename(src)}")
        print(f"    -> {dst}")
        print(f"    {secs:.1f}s  peak {peak:.4f} ({dbfs(peak)} dBFS)  rms {rms:.5f} ({dbfs(rms)} dBFS)")
    else:
        print(f"{os.path.basename(src)}\n    -> {dst}\n    EMPTY")
PY
