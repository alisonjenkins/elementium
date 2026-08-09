/**
 * Report whether this webview can decode video itself.
 *
 * ## Why this is worth a module
 *
 * Today Rust decodes every remote video frame to RGBA and ships 3.7MB per frame per track to
 * the page. Measured on a real call, that path delivers about 14fps a track while the
 * backend sits idle -- the cost is entirely in moving the pixels, not in producing them.
 *
 * If the page can decode, none of that is necessary: Rust would depacketise and decrypt (the
 * two things only it can do) and hand over the *encoded* frame, around 20-60kB instead of
 * 3.7MB, which the webview decodes on the GPU. Roughly a hundredfold less data, our software
 * VP8 decoder gone, and its decode failures with it.
 *
 * Whether that is possible is a property of the runtime, not of the version number.
 * WebKitGTK has shipped WebCodecs enabled since 2.46 and this build reports 2.52 -- but a
 * feature can be compiled out, disabled by policy, or present and refuse the codec actually
 * in use. So it is measured rather than assumed, and the answer is in the log before any
 * code depends on it.
 *
 * Nothing here changes behaviour. It observes and reports.
 */

/** The codecs worth asking about: what the SFU negotiates with us today. */
const CANDIDATE_CODECS = [
  { label: "VP8", config: { codec: "vp08.00.10.08", codedWidth: 1280, codedHeight: 720 } },
  { label: "H.264", config: { codec: "avc1.42E01F", codedWidth: 1280, codedHeight: 720 } },
  { label: "AV1", config: { codec: "av01.0.04M.08", codedWidth: 1280, codedHeight: 720 } },
];

interface VideoDecoderLike {
  isConfigSupported?: (config: unknown) => Promise<{ supported?: boolean }>;
}

/**
 * Ask the runtime what it can decode and write the answer to the log.
 *
 * Deliberately never throws and never rejects: this is a diagnostic on the startup path, and
 * a probe that can break the application it is measuring is worse than no probe.
 */
export async function probeWebCodecs(): Promise<void> {
  const decoder = (globalThis as Record<string, unknown>)["VideoDecoder"] as
    | VideoDecoderLike
    | undefined;

  if (typeof decoder !== "function") {
    console.log(
      "[Elementium] WebCodecs VideoDecoder is not available in this webview; " +
        "remote video must keep being decoded natively and shipped as RGBA",
    );
    return;
  }

  if (typeof decoder.isConfigSupported !== "function") {
    console.log(
      "[Elementium] WebCodecs VideoDecoder exists but cannot be queried " +
        "(no isConfigSupported); treating it as unusable rather than guessing",
    );
    return;
  }

  const results: string[] = [];
  for (const candidate of CANDIDATE_CODECS) {
    try {
      // Sequential rather than parallel: three probes at startup is not worth the
      // concurrency, and a serial loop keeps the log lines in a fixed order.
      // eslint-disable-next-line no-await-in-loop
      const support = await decoder.isConfigSupported(candidate.config);
      results.push(`${candidate.label}=${support?.supported === true ? "yes" : "no"}`);
    } catch (e) {
      // A codec string this runtime dislikes throws rather than answering false.
      results.push(`${candidate.label}=error(${String(e).slice(0, 40)})`);
    }
  }

  console.log(
    `[Elementium] WebCodecs VideoDecoder is available: ${results.join(" ")}. ` +
      "Where a codec is supported, remote video can be decoded in the page from encoded " +
      "frames -- around 20-60kB each instead of 3.7MB of RGBA.",
  );
}

/** Install the probe. Safe in any frame; runs once per frame and never throws. */
export function setupWebCodecsProbe(): void {
  const w = globalThis as unknown as Record<string, unknown>;
  if (w["__elementium_webcodecs_probed"]) return;
  w["__elementium_webcodecs_probed"] = true;
  void probeWebCodecs().catch(() => {
    /* a diagnostic must never break startup */
  });
}
