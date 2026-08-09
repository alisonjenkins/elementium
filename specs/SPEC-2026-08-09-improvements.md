# Improvements, from one evening of real calls — 2026-08-09

Written after a session in which remote video went from never working to working, and
outbound video went from bad to differently bad. Everything below is ordered by what the
measurements support, not by how interesting it is to build. Each item says what was
observed, what is inferred, and what would settle the difference — because three of tonight's
faults survived weeks of logs by looking like something else.

## The pattern worth naming first

Tonight's three worst bugs were all **silent failure paths**, and none of them was a hard
problem once seen:

| fault | why it hid |
|---|---|
| track events never delivered | the `catch` logged `Track event dispatch: video mid=Ccu`, which reads as a success |
| every remote track discarded by livekit | its error log is gated on the sid starting with `PA`, and ours never did |
| two of three remote streams killed | a 30s idle timeout treated the ordinary key-arrival wait as a dead track |

None was found by reading code. Each was found by making the silent path count something.
That is the general lesson and it should shape what gets built below: **every discard gets a
counter, and every counter gets into a periodic log line.** Constitution Principle II already
says instrumentation must distinguish cause from consequence; these were cases where there
was no instrumentation at all.

---

## P0 — Outbound video is decoded only at keyframes

**The strongest evidence in this file, and the worst remaining symptom.**

Observed: the receiver sends a PLI on a precise three-second cadence — 165 of them in one
call, answered by 163 keyframes. Our own counters are healthy: `captured 2100, sent 2099,
paced_out 0`, and 130kB/s transmitted, which is roughly ten times what keyframes alone would
cost. So delta frames are produced, encrypted and transmitted.

Inferred: the far end decodes each keyframe, fails on every interframe that follows, and asks
again. One picture per PLI is about twenty frames per minute, which is exactly what the far
end reports seeing.

The only transform between our encoder and their decoder is E2EE, and it is the one thing
that treats the two frame types differently: **10 unencrypted header bytes on a keyframe, 3
on a delta**. If our delta framing disagrees with what livekit's decryptor computes, GCM
authentication fails on every delta and succeeds on every keyframe. The picture is therefore
never black, which is why this has never looked like a crypto fault.

### Three theories eliminated, overnight, on primary sources

Recorded so none of them is investigated a fourth time. Every one of them was plausible and
matched the symptom; none survived contact with the source.

1. **E2EE delta framing.** livekit's `UNENCRYPTED_BYTES = {key: 10, delta: 3, audio: 1}`
   (`e2ee/constants.ts`), applied by `FrameCryptor.getUnencryptedBytes` to the whole
   depacketized frame. Our `framing()` computes the same numbers over the same bytes, and
   what reaches `encrypt_frame` is the bare VP8 frame starting with its frame tag. Existing
   tests already round-trip a delta under these rules.
2. **Missing VP8 PictureID.** True that we send none -- str0m's `Vp8Packetizer::default()`
   has `enable_picture_id: false`, private, no setter. But libwebrtc's
   `RtpFrameReferenceFinder::Impl::ManageFrame` dispatches to `RtpSeqNumOnlyRefFinder` when
   `pictureId == kNoPictureId`, a complete fallback rather than an error path, and LiveKit's
   VP8 munger only reads the I/L/T/K fields when the X bit is set and gates its logic on
   `pictureIdUsed`. RFC 7741 marks it optional and both implementations mean it. Now pinned
   by `crates/elementium-webrtc/tests/vp8_payload_descriptor.rs`.
3. **The send path generally.** Inspected directly: the payload type is chosen to permit
   fragmentation, RTP timestamps are derived from elapsed time on the 90kHz clock rather
   than from a frame counter, `writer.write()` is called exactly once per encoded frame, and
   the encoder runs `lag_in_frames: 0`, CBR, realtime deadline, error-resilient on.

### What that leaves

Everything measurable from our side is correct, and every remaining hypothesis is about what
the *receiver* does with what we send. That evidence does not exist in our logs and cannot
be obtained by reading more of our code — which is the strongest argument for the end-to-end
harness being built alongside this: a Playwright receiver whose `getStats()` and console we
control turns "they say it looks bad" into `framesDecoded` and `keyFramesDecoded` per second.

The next step is therefore not another theory. It is to run the harness, read the receiver's
own numbers, and let them say which half of the link is at fault.

### The harness now exists, and the fault does not reproduce in it

`just test-app-call` puts Elementium in a real call against the local stack and measures the
far end. Result:

    framesDecoded 30.1/s, keyFramesDecoded 1% of them

That is healthy video, from the same send path that gives a remote participant twenty frames
a minute. So the fault is **not in the send path alone** -- which agrees with all three
theories above having been disproved -- and is a property of the configuration, not the code
in isolation.

What differs between the passing test and the failing call, in rough order of suspicion:

1. **The SFU and the network between us and it.** The test uses a LiveKit container on
   loopback; the failing calls go to a remote LiveKit 1.12.0 over the internet. Loss, MTU and
   the SFU's own forwarding decisions all exist in one and not the other.
2. **Who the receiver is.** The test's far end is Element Web driven by Playwright. The
   failing reports come from other people's clients and from a browser on the same machine.
3. **Participant count.** One remote participant in the test; three or four in the failing
   calls, which is when an SFU starts making forwarding choices rather than relaying.
4. **The key path.** In the test Element Call distributes and rotates over to-device and
   Elementium adopts each key -- sixteen in one call. The single-distribution fault (P1a) was
   observed against the real deployment, so the two configurations do not exercise the same
   code.

The cheapest next experiment is to point the harness at the real SFU rather than the local
one. If it still reads 30/s, the fault is in the receiver or the participant mix; if it drops
to keyframes only, it is the network or that SFU, and everything on our side is exonerated.
Either answer is worth more than another reading of our own code.

## P1 — Encryption keys: one fault outbound, one inbound

Same subsystem, same root, two visible failures. Both trace to this client's MatrixRTC
membership handling rather than to any crypto code — the keyring, ratchet and framing have
all been verified correct.

**P1a. We distribute our key once, at join, and never again.** Confirmed across three calls:
exactly one `fromWidget send_to_device io.element.call.encryption_keys` per call, at join,
addressed to the participants present at that moment. Anyone who joins later is never sent
our key, so from their side our video freezes on the last frame they could decrypt and our
audio stops — while our encoder runs perfectly. This is `M5`.

Reviewed against `RTCEncryptionManager`: a join re-distributes the existing key inside a
10-second grace period and rotates outside it. Ours was 91 seconds old at the membership
change that broke it, so both a rotation and a distribution were due, and neither happened.

**P1b. Other participants' keys reach us 25–58 seconds after joining.** Measured at 36s, 25s,
and 55s on three separate calls. Their video is undecryptable for that whole period. Key
acquisition in this protocol is push-only — there is no request mechanism — so a long gap
means remote clients saw our membership event late.

Both point at the same place, and the room carries dozens of stale `LEFT` membership events
for our user, one per past device, which is the obvious candidate for a broken
changed-memberships diff.

Next: instrument `MatrixRTCSession`'s membership callbacks in the widget frame to establish
whether `onMembershipsUpdate` fires at all after the initial join, and whether the stale
entries break the diff. The widget-API recorder added tonight already shows what reaches
Element Call; this is about what it does with it.

## P2 — Outbound audio quality

Least well understood item here, and I have already been wrong about it twice tonight — once
blaming a stereo downmix that turned out to handle a silent channel correctly, once blaming
AGC clipping that turned out to be capped per buffer.

What is measured: 3750 of 3750 frames captured and sent, zero skipped, 22–34kbps, real speech
in the peaks. What is not known is what it sounds like at the far end, and the four
candidates need different fixes:

- **quiet** — the AGC riding at 7.2–7.7× on a low-level input; a device gain problem
- **pumping** — the AGC's envelope behaviour
- **robotic or choppy** — pacing, or the far end's jitter buffer
- **distorted** — something not yet found

Next: use the existing audio dump to capture what we actually encode, and listen to it. That
converts a four-way guess into an observation. Guessing further without it is how the last
two hours went.

## P3 — A mid-call device change can orphan the microphone

When the camera became available mid-call, livekit unpublished both tracks, closed the
publisher connection, logged `could not createOffer with closed peer connection`, and never
built a replacement. For the following 45 seconds the microphone captured into nothing:
`captured_frames 2250, encoded_frames 1, sent_frames 0, skipped_not_connected 2249`.

The existing inheritance path found nothing to inherit because the connection it would have
inherited was the one being closed, and no new connection was created for
`adopt_idle_pipelines` to fire on.

Two separable pieces of work, and the second is worth doing even if the first is slow:

1. Establish why livekit closed the publisher and did not rebuild it. Plausibly shares a
   cause with P1, since both involve this client's reaction to a membership change.
2. Make the state impossible to hold silently: a capture pipeline whose
   `skipped_not_connected` climbs into the thousands should warn, loudly, naming the
   consequence. This is the cheap half and follows the pattern at the top of this file.

## P4 — Codec coverage in the runtime

The WebCodecs probe found `VP8=yes, VP9=no, H.264=no, AV1=no`, and `MediaSource` found
`webm/vp8=yes, webm/vp9=yes, mp4/avc1=no`. So this build has **no H.264 decoder at all**, in
either surface.

That is fine today because we negotiate VP8, and it is why the page-side decode path works.
It stops being fine the moment an SFU picks H.264 — which the code already supports encoding
and which earlier commits in this repo went to some trouble to get right.

The fix is packaging, not architecture: add the GStreamer plugin set to the Nix runtime
closure. That gets hardware decode through the path a `<video>` element already uses.
A WASM decoder would also fill the gap, and is worth considering *only* if the packaging
route fails, because it is software-only and slower than what the GPU can do.

The same measurement explains why VP9 is offered by MediaSource but not WebCodecs, which is
worth knowing before anyone tries to negotiate VP9.

## P5 — The frame rate is not configurable by a person

Requested directly, mid-call, having watched the frame rate be bad with no way to change it.

Two mechanisms exist and neither is reachable: `ELEMENTIUM_MAX_FPS` needs a shell and a
restart, and the page's own `frameRate` constraint only started reaching the backend today —
before the `camelCase` fix it arrived as `None` for the life of the project, so the encoder
has always run at the compiled-in 30 regardless of what anything asked for.

Wanted: a persisted setting, applied to a running encoder without a restart, clamped honestly
against the 120fps ceiling, and reconciled with the page's own constraint so the two do not
fight. Note there is a *second* rate limiter inside `pipewire_capture` halving a 60fps camera
to 30, which any such setting has to reach as well.

## P6 — Structural and instrumentation debt

Small individually, and each one cost real time tonight.

- **The main window runs a stale shim bundle.** Zero log lines are tagged `[main]` while the
  widget frame's are tagged correctly, and widget-API lines appear twice — once tagged, once
  not. One frame is not reliably running the build you just made, which undermines every
  measurement taken from it. Likely `prepare-build` refreshing one shim copy and not another.
- **Debug logging is on by default.** 7,533 of 6,156 lines in 53 seconds were `UDP received`
  and `str0m event`; one call produced 30MB. That is a cost inside the I/O loop, and it
  buries everything else.
- **No PLI when a page-side stream attaches.** A stream opening mid-call waits for the
  sender's next natural keyframe. Cheap to fix and removes seconds from every remote tile.
- **The negotiation rewrite (`N1`) has never been accepted or declined.** It was proposed
  after seven consecutive builds each fixed a different negotiation fault by treating the
  newest symptom as the specification. It should be decided rather than left.

## What I would build, in order

1. **P0's measurement.** One throttled log line, one call. It is the worst symptom and the
   cheapest next step, and everything else about outbound video is guesswork until it lands.
2. **P1's measurement**, in the same build — membership callbacks in the widget frame. Two
   answers from one restart.
3. Whichever of P0/P1 the data indicts, then its regression test.
4. **P3's second half** and **P6's logging default** — both small, both remove a class of
   silent failure.
5. **P4 packaging**, before anyone needs H.264 in a hurry.
6. **P5**, which is a feature rather than a fault, and the only item here a user asked for
   by name.

P2 stays unscheduled until there is a symptom to work from. It is the one place where more
thinking will not help and thirty seconds of listening will.
