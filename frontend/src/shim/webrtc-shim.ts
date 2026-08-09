/**
 * WebRTC shim that intercepts RTCPeerConnection and routes to the Rust backend.
 *
 * This replaces the browser's WebRTC implementation with Tauri IPC calls
 * to the native str0m-based WebRTC engine.
 */

import { invoke } from "@tauri-apps/api/core";
import { interceptE2eeWorkerMessage, noteLocalIdentity } from "./e2ee-bridge";
import { createCanvasTrack } from "./canvas-track";
import { frameBytes } from "./frame-payload";
import { TRACK_SOURCE } from "./media-devices";
import {
  describeJoinRequest,
  describeRequest,
  describeResponse,
  localIdentityFromJoin,
  redactSignalUrl,
  signalBytes,
} from "./livekit-signal";

/**
 * What `create_offer` and `create_answer` actually return over the Tauri IPC.
 *
 * The Rust `SessionDescription` renames its field to `type` for exactly this reason, so
 * the key is `type` and never `sdpType`. This was declared as `{ sdpType, sdp }` for a
 * long time, which type-checks and is always `undefined`: the description reached
 * livekit-client with no type at all, proto3 dropped the empty field, and the SFU rejected
 * the negotiation with `STATE_MISMATCH`. It was invisible because the WebSocket shim
 * injected the missing field back in on the way out.
 */
interface NativeSessionDescription {
  type?: RTCSdpType;
  sdp: string;
}

/**
 * A description with its type guaranteed, whatever the native side returned.
 *
 * The type is not decoration: livekit-client copies it into the protobuf, and a
 * description without one is refused by the SFU rather than diagnosed. `expected` is what
 * this call site asked for, so a native side that stops sending the field degrades to
 * still-correct rather than to silently untyped.
 */
function sessionDescription(
  desc: NativeSessionDescription,
  expected: RTCSdpType,
): RTCSessionDescriptionInit {
  if (desc.type !== expected) {
    console.error(
      `[Elementium] native ${expected} came back with type=${String(desc.type)}; using ${expected}`,
    );
  }
  return { type: desc.type ?? expected, sdp: desc.sdp };
}

interface PeerConnectionResult {
  id: string;
}

interface WebRtcEvent {
  type: string;
  pcId: string;
  state?: string;
  candidate?: string;
  mid?: string;
  kind?: string;
  /** Data channel label, on `dataChannel{Open,Message,Close}` events. */
  label?: string;
  /** Whether a `dataChannelMessage` payload was originally binary (`ArrayBuffer`) rather
   * than a string -- see `RTCDataChannel.onmessage` reconstruction in
   * `installDataChannel`. */
  binary?: boolean;
  /** Raw bytes of a `dataChannelMessage` payload, as a plain number array (how `Vec<u8>`
   * serializes over Tauri IPC). */
  data?: number[];
}

/**
 * Get the top-level window (for registering callbacks reachable by Rust's webview.eval()).
 * Falls back to current window if cross-origin.
 */
function getTopWindow(): Record<string, unknown> {
  try {
    // Same-origin check: accessing window.top properties throws if cross-origin
    if (window.top && window.top.document) {
      return window.top as unknown as Record<string, unknown>;
    }
  } catch {
    // Cross-origin — fall back to current window
  }
  return window as unknown as Record<string, unknown>;
}

/** Module-level PC registry: maps pcId → event handler callback */
const __pcRegistry = new Map<string, (event: WebRtcEvent) => void>();

/**
 * Events that arrived before their peer connection registered a handler.
 *
 * Rust begins forwarding as soon as the connection exists; this side registers only after
 * the creating `invoke()` resolves. The events that fall in that window are the earliest
 * ones, which are also the ones that cannot be recovered later: ICE candidates nothing
 * re-announces, and the first connection-state change.
 */
const __pendingEvents = new Map<string, WebRtcEvent[]>();

/**
 * How many events to hold for a connection that has not registered yet.
 *
 * A cap rather than an unbounded queue: a connection closed before it finished opening
 * never registers, and its events would otherwise accumulate for the life of the page.
 * Generous enough for a normal startup burst of candidates.
 */
const MAX_HELD_EVENTS = 64;

/**
 * Shape of the `get_transport_stats` Tauri command's result -- the Rust side of
 * `TransportStatsResult` in `src-tauri/src/commands/webrtc.rs`, field-for-field. Every
 * `| null` here is a real "not measured", not a stand-in for zero: see
 * `OutboundTransportStats` in `crates/elementium-webrtc/src/peer_connection.rs` for what
 * str0m does and doesn't report.
 */
interface RustOutboundStats {
  mid: string;
  kind: string | null;
  bytesSent: number;
  packetsSent: number;
  nackCount: number;
  pliCount: number;
  firCount: number;
  roundTripTimeMs: number | null;
  remotePacketsLost: number | null;
  remoteJitterRtpUnits: number | null;
}

interface RustInboundStats {
  mid: string;
  kind: string | null;
  bytesReceived: number;
  packetsReceived: number;
  nackCount: number;
  pliCount: number;
  firCount: number;
  lossFraction: number | null;
}

interface RustCandidatePairStats {
  roundTripTimeMs: number | null;
  localAddr: string | null;
  remoteAddr: string | null;
}

interface RustTransportStats {
  outbound: RustOutboundStats[];
  inbound: RustInboundStats[];
  candidatePair: RustCandidatePairStats | null;
}

/**
 * RTP clock rate (Hz) for a media kind, used to convert str0m's raw RTCP jitter (RTP
 * timestamp ticks) into the seconds the WebRTC stats spec wants.
 *
 * Fixed by the codecs this app negotiates, not something worth threading a lookup table
 * for: Opus is always 48 kHz (RFC 7587 §4) and both VP8 and H.264 are always 90 kHz
 * (RFC 6386 §2, RFC 6184 §2).
 */
const CLOCK_RATE_HZ: Record<string, number> = { audio: 48_000, video: 90_000 };

/**
 * Build a real `RTCStatsReport`-shaped `Map` from the Rust side's transport-stats
 * snapshot, for `RTCPeerConnection.getStats()`.
 *
 * Element Call's connection-quality indicators read this, which is the whole reason this
 * exists: before this, `getStats()` always resolved an empty `Map`, so the indicators
 * could never show anything but blank or a default "everything is fine" green.
 *
 * Only fields Rust actually reported are set. In particular: `ssrc` is left off every
 * entry (str0m's stats events do not carry it, and inventing one that has to be right
 * per-track was out of scope here); inbound entries have no `packetsLost` because str0m
 * only exposes a loss *fraction* for inbound media, not the cumulative count the spec
 * field means, so it is surfaced as `fractionLost` instead of mislabeled; inbound entries
 * have no `jitter` because str0m does not expose one for media we are receiving (only for
 * what a peer reports back about media we *send*, which is why outbound entries can have
 * one).
 */
async function getTransportStatsReport(pcId: string | null): Promise<RTCStatsReport> {
  const entries = new Map<string, unknown>();
  if (!pcId) {
    return entries as unknown as RTCStatsReport;
  }

  let stats: RustTransportStats;
  try {
    stats = await invoke<RustTransportStats>("get_transport_stats", { pcId });
  } catch (err) {
    console.error(`[Elementium] getStats: get_transport_stats failed pcId=${pcId}`, err);
    return entries as unknown as RTCStatsReport;
  }

  for (const s of stats.outbound) {
    const id = `outbound-rtp-${s.mid}`;
    const entry: Record<string, unknown> = {
      id,
      type: "outbound-rtp",
      mid: s.mid,
      bytesSent: s.bytesSent,
      packetsSent: s.packetsSent,
      nackCount: s.nackCount,
      pliCount: s.pliCount,
      firCount: s.firCount,
    };
    if (s.kind) entry.kind = s.kind;
    if (s.roundTripTimeMs !== null) entry.roundTripTime = s.roundTripTimeMs / 1000;
    if (s.remotePacketsLost !== null) entry.packetsLost = s.remotePacketsLost;
    if (s.remoteJitterRtpUnits !== null && s.kind && s.kind in CLOCK_RATE_HZ) {
      entry.jitter = s.remoteJitterRtpUnits / CLOCK_RATE_HZ[s.kind];
    }
    entries.set(id, entry);
  }

  for (const s of stats.inbound) {
    const id = `inbound-rtp-${s.mid}`;
    const entry: Record<string, unknown> = {
      id,
      type: "inbound-rtp",
      mid: s.mid,
      bytesReceived: s.bytesReceived,
      packetsReceived: s.packetsReceived,
      nackCount: s.nackCount,
      pliCount: s.pliCount,
      firCount: s.firCount,
    };
    if (s.kind) entry.kind = s.kind;
    if (s.lossFraction !== null) entry.fractionLost = s.lossFraction;
    entries.set(id, entry);
  }

  if (stats.candidatePair) {
    const pair = stats.candidatePair;
    const id = "candidate-pair-selected";
    const entry: Record<string, unknown> = {
      id,
      type: "candidate-pair",
      // Only ever built when str0m reports a *selected* pair, so "succeeded" is the only
      // state this can honestly report -- there is nothing upstream to distinguish
      // "still checking" from "no pair" for this to also cover.
      state: "succeeded",
    };
    if (pair.roundTripTimeMs !== null) entry.currentRoundTripTime = pair.roundTripTimeMs / 1000;
    if (pair.localAddr) entry.localCandidateAddr = pair.localAddr;
    if (pair.remoteAddr) entry.remoteCandidateAddr = pair.remoteAddr;
    entries.set(id, entry);
  }

  return entries as unknown as RTCStatsReport;
}

/**
 * Shimmed RTCPeerConnection that delegates to the Rust backend.
 *
 * Extends EventTarget for proper event dispatching. Element Web's
 * matrix-js-sdk relies on onicecandidate, ontrack, etc. callbacks.
 */
class ElementiumRTCPeerConnection extends EventTarget {
  private pcId: string | null = null;
  private _connectionState: RTCPeerConnectionState = "new";
  private _iceConnectionState: RTCIceConnectionState = "new";
  private _iceGatheringState: RTCIceGatheringState = "new";
  private _signalingState: RTCSignalingState = "stable";
  private _localDescription: RTCSessionDescription | null = null;
  private _remoteDescription: RTCSessionDescription | null = null;
  private _initPromise: Promise<void>;
  private _senders: RTCRtpSender[] = [];
  private _transceivers: RTCRtpTransceiver[] = [];
  private _dataChannelIdCounter = 0;
  private _hasVideo = false;
  // Tracked for passing to create_offer so the SDP includes the right m-lines
  private _pendingDataChannels: { label: string; ordered?: boolean; maxRetransmits?: number; maxPacketLifeTime?: number; protocol?: string }[] = [];
  /**
   * Every `RTCDataChannel` this connection knows about, keyed by label -- the only
   * identifier Rust has for a channel (str0m's `ChannelId` never crosses the IPC
   * boundary). Populated both by local `createDataChannel()` calls and by
   * `dataChannelOpen` events for channels the *remote* peer created, which this side only
   * learns about once str0m's DCEP handshake completes -- see `handleBackendEvent`.
   */
  private _dataChannels = new Map<string, RTCDataChannel>();
  private _pendingTransceivers: {
    kind: string;
    direction: string;
    trackId?: string;
    source?: string;
  }[] = [];

  // Event handler properties (on* style)
  onconnectionstatechange: ((this: RTCPeerConnection, ev: Event) => void) | null = null;
  ondatachannel: ((this: RTCPeerConnection, ev: RTCDataChannelEvent) => void) | null = null;
  onicecandidate: ((this: RTCPeerConnection, ev: RTCPeerConnectionIceEvent) => void) | null = null;
  onicecandidateerror: ((this: RTCPeerConnection, ev: Event) => void) | null = null;
  oniceconnectionstatechange: ((this: RTCPeerConnection, ev: Event) => void) | null = null;
  onicegatheringstatechange: ((this: RTCPeerConnection, ev: Event) => void) | null = null;
  onnegotiationneeded: ((this: RTCPeerConnection, ev: Event) => void) | null = null;
  onsignalingstatechange: ((this: RTCPeerConnection, ev: Event) => void) | null = null;
  ontrack: ((this: RTCPeerConnection, ev: RTCTrackEvent) => void) | null = null;

  constructor(configuration?: RTCConfiguration) {
    super();
    this._initPromise = this.init(configuration);
  }

  private async init(configuration?: RTCConfiguration) {
    try {
      const handle = await invoke<PeerConnectionResult>("create_peer_connection", {
        config: configuration ? {
          iceServers: configuration.iceServers?.map(server => ({
            urls: Array.isArray(server.urls) ? server.urls : [server.urls],
            username: server.username,
            credential: server.credential as string | undefined,
          })),
        } : null,
      });
      this.pcId = handle.id;
      console.log(`[Elementium] PeerConnection created: ${this.pcId}`);

      // Register in global PC registry (Rust pushes events via webview.eval())
      __pcRegistry.set(this.pcId, (event: WebRtcEvent) => this.handleBackendEvent(event));
      // Anything that arrived while this connection was being created, in arrival order.
      const held = __pendingEvents.get(this.pcId);
      if (held) {
        __pendingEvents.delete(this.pcId);
        console.log(`[Elementium] replaying ${held.length} held event(s) for ${this.pcId}`);
        for (const event of held) this.handleBackendEvent(event);
      }
    } catch (e) {
      console.error("[Elementium] Failed to create peer connection:", e);
    }
  }

  /** Wait for initialization to complete before using the PC. */
  private async ensureReady(): Promise<void> {
    await this._initPromise;
    if (!this.pcId) throw new DOMException("PeerConnection not initialized", "InvalidStateError");
  }

  private handleBackendEvent(event: WebRtcEvent) {
    switch (event.type) {
      case "iceConnectionStateChange":
        this._iceConnectionState = event.state as RTCIceConnectionState;
        this.fireEvent("iceconnectionstatechange", this.oniceconnectionstatechange);
        break;

      case "connectionStateChange":
        this._connectionState = event.state as RTCPeerConnectionState;
        this.fireEvent("connectionstatechange", this.onconnectionstatechange);
        break;

      case "iceCandidate":
        if (event.candidate) {
          const candidateInit: RTCIceCandidateInit = {
            candidate: event.candidate,
            sdpMid: "0",
            sdpMLineIndex: 0,
          };
          const iceEvent = new RTCPeerConnectionIceEvent("icecandidate", {
            candidate: new RTCIceCandidate(candidateInit),
          });
          this.dispatchEvent(iceEvent);
          this.onicecandidate?.call(this as unknown as RTCPeerConnection, iceEvent);
        }
        break;

      case "iceGatheringComplete": {
        this._iceGatheringState = "complete";
        const nullEvent = new RTCPeerConnectionIceEvent("icecandidate", {
          candidate: null,
        });
        this.dispatchEvent(nullEvent);
        this.onicecandidate?.call(this as unknown as RTCPeerConnection, nullEvent);
        this.fireEvent("icegatheringstatechange", this.onicegatheringstatechange);
        break;
      }

      case "connected":
        this._connectionState = "connected";
        this._iceConnectionState = "connected";
        this.fireEvent("connectionstatechange", this.onconnectionstatechange);
        this.fireEvent("iceconnectionstatechange", this.oniceconnectionstatechange);
        break;

      case "remoteTrackAdded":
        console.log(`[Elementium] Remote track: mid=${event.mid} kind=${event.kind}`);
        this.emitTrackEvent(event.mid!, event.kind!);
        break;

      case "dataChannelOpen":
        this.handleDataChannelOpen(event.label!);
        break;

      case "dataChannelMessage":
        this.handleDataChannelMessage(event.label!, event.binary ?? false, event.data ?? []);
        break;

      case "dataChannelClose":
        this.handleDataChannelClose(event.label!);
        break;
    }
  }

  /**
   * The DCEP handshake for `label` completed on the Rust side.
   *
   * Two cases: `label` is a channel *this* side created with `createDataChannel()`, in
   * which case it already exists in `_dataChannels` with `readyState: "connecting"` and
   * just needs to flip to `"open"`; or `label` is one the *remote* peer created (this side
   * never called `createDataChannel()` for it), in which case this is the first this side
   * hears of it at all, and the DOM contract is to synthesize the channel here and fire
   * `ondatachannel` -- mirroring what a browser does when the far end negotiates a channel
   * in-band.
   */
  private handleDataChannelOpen(label: string) {
    let channel = this._dataChannels.get(label);
    if (!channel) {
      channel = this.createRemoteDataChannel(label);
      this._dataChannels.set(label, channel);
      const ev = new Event("datachannel") as unknown as RTCDataChannelEvent;
      Object.defineProperty(ev, "channel", { value: channel, enumerable: true });
      this.dispatchEvent(ev);
      this.ondatachannel?.call(this as unknown as RTCPeerConnection, ev);
    }
    (channel as unknown as { readyState: RTCDataChannelState }).readyState = "open";
    const openEv = new Event("open");
    channel.dispatchEvent(openEv);
    channel.onopen?.call(channel, openEv);
  }

  /**
   * A message arrived on `label`. Reconstructs the DOM shapes `onmessage` expects:
   * `binary` payloads become an `ArrayBuffer` (never a `Blob` -- nothing upstream in this
   * shim needs one, and every consumer here already expects `ArrayBuffer`), text payloads
   * are decoded back to a string with the same `TextEncoder`/`TextDecoder` pairing `send()`
   * uses on the way out.
   */
  private handleDataChannelMessage(label: string, binary: boolean, data: number[]) {
    const channel = this._dataChannels.get(label);
    if (!channel) {
      console.warn(`[Elementium] dataChannelMessage for unknown label=${label}, dropping`);
      return;
    }
    const bytes = new Uint8Array(data);
    const payload: string | ArrayBuffer = binary
      ? bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer
      : new TextDecoder().decode(bytes);
    const ev = new MessageEvent("message", { data: payload });
    channel.dispatchEvent(ev);
    channel.onmessage?.call(channel, ev);
  }

  /** The channel named `label` closed, locally or by the remote peer. */
  private handleDataChannelClose(label: string) {
    const channel = this._dataChannels.get(label);
    if (!channel) return;
    (channel as unknown as { readyState: RTCDataChannelState }).readyState = "closed";
    const ev = new Event("close");
    channel.dispatchEvent(ev);
    channel.onclose?.call(channel, ev);
    this._dataChannels.delete(label);
  }

  /**
   * Build the `RTCDataChannel` shape for a channel the *remote* peer created, learned
   * about only once its DCEP handshake completes (see `handleDataChannelOpen`). No SDP
   * negotiation of ours is involved -- str0m already has the channel, so this exists purely
   * to give JS an object with the same `send()`/event surface as one created locally.
   */
  private createRemoteDataChannel(label: string): RTCDataChannel {
    return this.buildDataChannel(label, {});
  }

  /**
   * Create and dispatch a synthetic RTCTrackEvent for a remote track.
   * For video tracks, creates a canvas-backed MediaStreamTrack that
   * fetches frames from the Rust backend via the custom protocol.
   */
  private emitTrackEvent(mid: string, kind: string) {
    // Create a MediaStream for this track
    const stream = new MediaStream();

    // For video tracks, create a canvas source that renders frames from Rust
    if (kind === "video" && this.pcId) {
      // Per track, not per connection: on an SFU every remote participant's video arrives
      // on one connection and is told apart only by mid, so a key without it makes two
      // people share a display slot and overwrite each other.
      const trackId = `${this.pcId}-${mid}`;
      const canvas = document.createElement("canvas");
      // Fixed at 720p and never resized afterwards. `captureStream` fixes the resulting
      // track's frame size when it is called; resizing the canvas later leaves the track
      // describing one geometry while the backing store holds another, and rows are then
      // read at the wrong stride -- the picture shears into horizontal bands whose colours
      // rotate as the byte offset drifts through the RGBA quad. Incoming frames of any
      // other size are scaled into this one instead.
      canvas.width = 1280;
      canvas.height = 720;

      // Manually driven, so a frame is only ever emitted once a draw has completed --
      // see `createCanvasTrack`.
      const canvasTrack = createCanvasTrack(canvas);
      const videoTrack = canvasTrack.track;
      if (videoTrack) {
        stream.addTrack(videoTrack);

        // Start rendering frames from Rust onto this canvas
        this.startVideoFrameFetch(canvas, trackId, canvasTrack.present);
      }
    }

    // Dispatch the track event
    try {
      const trackEvent = new RTCTrackEvent("track", {
        track: stream.getTracks()[0] || new MediaStreamTrack(),
        streams: [stream],
        receiver: {} as RTCRtpReceiver,
        transceiver: {} as RTCRtpTransceiver,
      });
      this.dispatchEvent(trackEvent);
      this.ontrack?.call(this as unknown as RTCPeerConnection, trackEvent);
    } catch (e) {
      // RTCTrackEvent may not be constructable in all environments
      console.log(`[Elementium] Track event dispatch: ${kind} mid=${mid}`);
    }
  }

  /**
   * Fetch video frames from the Rust backend via Tauri IPC and render onto a canvas.
   */
  private startVideoFrameFetch(
    canvas: HTMLCanvasElement,
    trackId: string,
    present: () => void = () => {},
  ) {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    let running = true;
    let timerId: ReturnType<typeof setTimeout> | null = null;
    let scratch: HTMLCanvasElement | null = null;
    let attempts = 0;
    let failures = 0;

    const fetchLoop = async () => {
      if (!running) {
        // Said out loud, because a loop that stops is indistinguishable from a track that
        // never had frames: one remote camera in a three-way call was polled a handful of
        // times and then never again, and nothing recorded that it had stopped.
        console.log(
          `[Elementium] remote video fetch stopped for ${trackId} after ${attempts} attempts`,
        );
        return;
      }
      attempts += 1;
      const started = Date.now();

      try {
        // The same coercion the self-view needs: this is the remote participant's
        // renderer, and it is a second copy of the same loop -- which is how the black
        // tile survived the first fix.
        const buf = frameBytes(await invoke<unknown>("get_video_frame", { trackId }));
        if (buf && buf.byteLength > 8) {
          const view = new DataView(buf);
          const width = view.getUint32(0, true);
          const height = view.getUint32(4, true);

          if (width > 1 && height > 1) {
            const rgba = new Uint8ClampedArray(buf, 8);
            const imageData = new ImageData(rgba, width, height);
            if (canvas.width === width && canvas.height === height) {
              ctx.putImageData(imageData, 0, 0);
            } else {
              // Scale into the track's fixed geometry rather than resizing it. Aspect ratio
              // is preserved and the remainder left black, so a 4:3 sender is letterboxed
              // rather than stretched.
              if (!scratch) scratch = document.createElement("canvas");
              if (scratch.width !== width || scratch.height !== height) {
                scratch.width = width;
                scratch.height = height;
              }
              const sctx = scratch.getContext("2d");
              if (sctx) {
                sctx.putImageData(imageData, 0, 0);
                const scale = Math.min(canvas.width / width, canvas.height / height);
                const dw = Math.round(width * scale);
                const dh = Math.round(height * scale);
                const dx = Math.round((canvas.width - dw) / 2);
                const dy = Math.round((canvas.height - dh) / 2);
                ctx.fillStyle = "#000";
                ctx.fillRect(0, 0, canvas.width, canvas.height);
                ctx.drawImage(scratch, dx, dy, dw, dh);
              }
            }
            // Draw complete: publish it as one frame.
            present();
          }
        }
      } catch (e) {
        // Counted, not ignored. A loop that fails every time still reschedules, so a silent
        // catch turns "this track never renders" into a stream of nothing with no record of
        // why -- which is exactly the state one remote camera was found in.
        failures += 1;
        if (failures === 1 || failures % 100 === 0) {
          console.warn(
            `[Elementium] remote video fetch failed for ${trackId} ` +
              `(${failures} of ${attempts} attempts): ${String(e).slice(0, 120)}`,
          );
        }
      }

      if (running) {
        // Scheduled against the target period, not slept for a fixed amount after the work:
        // the previous form made the real period "IPC time + 33ms", so a 30ms fetch halved
        // the frame rate.
        timerId = setTimeout(fetchLoop, Math.max(0, 33 - (Date.now() - started)));
      }
    };

    timerId = setTimeout(fetchLoop, 33);

    // Store cleanup function (called on close)
    const originalClose = this.close.bind(this);
    this.close = () => {
      running = false;
      if (timerId !== null) clearTimeout(timerId);
      originalClose();
    };
  }

  private fireEvent(
    type_: string,
    handler: ((this: RTCPeerConnection, ev: Event) => void) | null,
  ) {
    const ev = new Event(type_);
    this.dispatchEvent(ev);
    handler?.call(this as unknown as RTCPeerConnection, ev);
  }

  get connectionState(): RTCPeerConnectionState { return this._connectionState; }
  get iceConnectionState(): RTCIceConnectionState { return this._iceConnectionState; }
  get iceGatheringState(): RTCIceGatheringState { return this._iceGatheringState; }
  get signalingState(): RTCSignalingState { return this._signalingState; }
  get localDescription(): RTCSessionDescription | null { return this._localDescription; }
  get remoteDescription(): RTCSessionDescription | null { return this._remoteDescription; }
  get currentLocalDescription(): RTCSessionDescription | null { return this._localDescription; }
  get currentRemoteDescription(): RTCSessionDescription | null { return this._remoteDescription; }
  get pendingLocalDescription(): RTCSessionDescription | null { return null; }
  get pendingRemoteDescription(): RTCSessionDescription | null { return null; }
  get canTrickleIceCandidates(): boolean | null { return true; }
  get sctp(): RTCSctpTransport | null { return null; }

  async createOffer(_options?: RTCOfferOptions): Promise<RTCSessionDescriptionInit> {
    await this.ensureReady();
    console.log(`[Elementium] createOffer: pcId=${this.pcId} hasVideo=${this._hasVideo} dc=${this._pendingDataChannels.length} tc=${this._pendingTransceivers.length}`);
    const desc = await invoke<NativeSessionDescription>("create_offer", {
      pcId: this.pcId,
      includeVideo: this._hasVideo,
      dataChannels: this._pendingDataChannels.length > 0 ? this._pendingDataChannels : null,
      transceivers: this._pendingTransceivers.length > 0 ? this._pendingTransceivers : null,
    });
    // Clear pending lists after they've been applied
    this._pendingDataChannels = [];
    this._pendingTransceivers = [];
    console.log(`[Elementium] createOffer result: pcId=${this.pcId} sdpLen=${desc.sdp.length}`);
    console.log("[Elementium] createOffer raw SDP:\n" + desc.sdp);
    return sessionDescription(desc, "offer");
  }

  async createAnswer(_options?: RTCAnswerOptions): Promise<RTCSessionDescriptionInit> {
    await this.ensureReady();
    console.log(`[Elementium] createAnswer: pcId=${this.pcId}`);
    const desc = await invoke<NativeSessionDescription>("create_answer", {
      pcId: this.pcId,
    });
    console.log(`[Elementium] createAnswer result: pcId=${this.pcId} sdpLen=${desc.sdp.length}`);
    return sessionDescription(desc, "answer");
  }

  async setLocalDescription(description?: RTCSessionDescriptionInit): Promise<void> {
    await this.ensureReady();
    if (!description) {
      // The implicit form: the browser generates the offer or answer itself from the
      // signaling state. Implemented rather than refused, because the alternatives are both
      // bad — returning silently makes a caller that used it look like one that never
      // negotiated (no offer on the wire, nothing in the log), and throwing breaks a caller
      // that is using the API exactly as specified.
      //
      // Which one to generate is decided by the same rule the spec gives: an offer unless
      // a remote offer is outstanding, in which case the reply is an answer.
      const implicit =
        this._signalingState === "have-remote-offer"
          ? await this.createAnswer()
          : await this.createOffer();
      await this.setLocalDescription(implicit);
      return;
    }
    console.log(`[Elementium] setLocalDescription: pcId=${this.pcId} type=${description.type}`);
    console.log("[Elementium] setLocalDescription SDP (post-munge):\n" + (description.sdp ?? "(no sdp)"));
    await invoke("set_local_description", {
      pcId: this.pcId,
      description: { type: description.type, sdp: description.sdp },
    });
    this._localDescription = new RTCSessionDescription(description);
    this._signalingState = description.type === "offer" ? "have-local-offer" : "stable";
    this.fireEvent("signalingstatechange", this.onsignalingstatechange);
  }

  async setRemoteDescription(description: RTCSessionDescriptionInit): Promise<void> {
    await this.ensureReady();
    console.log(`[Elementium] setRemoteDescription: pcId=${this.pcId} type=${description.type} sdpLen=${description.sdp?.length ?? 0}`);
    console.log("[Elementium] setRemoteDescription SDP:\n" + (description.sdp ?? "(no sdp)"));

    const result = await invoke<NativeSessionDescription | null>(
      "set_remote_description",
      {
        pcId: this.pcId,
        description: { type: description.type, sdp: description.sdp },
      },
    );

    this._remoteDescription = new RTCSessionDescription(description);

    if (description.type === "offer") {
      this._signalingState = "have-remote-offer";
      if (result) {
        this._localDescription = new RTCSessionDescription(
          sessionDescription(result, "answer"),
        );
      }
    } else {
      this._signalingState = "stable";
    }
    this.fireEvent("signalingstatechange", this.onsignalingstatechange);
  }

  async addIceCandidate(candidate?: RTCIceCandidateInit | null): Promise<void> {
    await this.ensureReady();
    if (!candidate?.candidate) return;
    console.log(`[Elementium] addIceCandidate: pcId=${this.pcId} candidate=${candidate.candidate.substring(0, 80)} sdpMid=${candidate.sdpMid} sdpMLineIndex=${candidate.sdpMLineIndex}`);
    await invoke("add_ice_candidate", {
      pcId: this.pcId,
      candidate: {
        candidate: candidate.candidate,
        sdpMid: candidate.sdpMid,
        sdpMLineIndex: candidate.sdpMLineIndex,
      },
    });
  }

  close(): void {
    if (this.pcId) {
      console.log(`[Elementium] close: pcId=${this.pcId}`);
      __pcRegistry.delete(this.pcId);
      invoke("close_peer_connection", { pcId: this.pcId }).catch(console.error);
      this._connectionState = "closed";
      this._iceConnectionState = "closed";
      this._signalingState = "closed";
    }
  }

  addTrack(track: MediaStreamTrack, ..._streams: MediaStream[]): RTCRtpSender {
    console.log(`[Elementium] addTrack called: kind=${track.kind}`);
    if (track.kind === "video") {
      this._hasVideo = true;
    }
    const sender = { track, replaceTrack: async () => {} } as unknown as RTCRtpSender;
    this._senders.push(sender);
    return sender;
  }

  removeTrack(sender: RTCRtpSender): void {
    console.log("[Elementium] removeTrack called");
    this._senders = this._senders.filter(s => s !== sender);
  }

  addTransceiver(
    trackOrKind: MediaStreamTrack | string,
    init?: RTCRtpTransceiverInit,
  ): RTCRtpTransceiver {
    const kind = typeof trackOrKind === "string" ? trackOrKind : trackOrKind.kind;
    const track = typeof trackOrKind === "string" ? null : trackOrKind;
    const direction = init?.direction ?? "sendrecv";
    console.log(`[Elementium] addTransceiver called: kind=${kind} direction=${direction}`);
    if (kind === "video") {
      this._hasVideo = true;
    }
    // Track for passing to create_offer
    // The track's id must reach the SDP as the msid track id: livekit-client sends this
    // same value as the `cid` of its AddTrackRequest, and the SFU pairs a published track
    // with an m-line by matching the two. Without it the SFU falls back to guessing by
    // media kind.
    // `source` distinguishes the camera from the screen share. The track's own id is a
    // browser UUID that means nothing to the backend, so without this both video
    // transceivers are labelled the camera and the two tracks contend for one m-line.
    const source = track
      ? (track as unknown as Record<string, unknown>)[TRACK_SOURCE]
      : undefined;
    this._pendingTransceivers.push({
      kind,
      direction,
      trackId: track?.id,
      source: typeof source === "string" ? source : undefined,
    });

    const mid = String(this._transceivers.length);

    const sender = {
      track,
      dtmf: null,
      transport: null,
      transform: null as unknown,
      /**
       * Swap the track this sender reports, which is all it has to do here.
       *
       * In a browser this re-points an encoder at a different source. In this app the
       * encoder lives in Rust, keyed by track identity (camera, screen share) rather than
       * by the `MediaStreamTrack` object the page holds — and a caller only reaches this
       * having already obtained the replacement from `getUserMedia`, which restarts the
       * capture pipeline for that key and inherits its peer connection. The media on the
       * wire has therefore already changed by the time this runs.
       *
       * It looked like the reason device switching did not work, and it was not: the cause
       * was `getUserMedia` ignoring `device_id`, so the "new" track came from the same
       * camera as the old one. That is fixed in the Rust capture path. Left as a field
       * assignment deliberately, with this note, so the next person does not add a second
       * mechanism for something that already happened.
       */
      replaceTrack: async (newTrack: MediaStreamTrack | null) => {
        (sender as unknown as Record<string, unknown>).track = newTrack;
      },
      getParameters: () => ({
        codecs: [],
        headerExtensions: [],
        rtcp: { cname: "", reducedSize: false },
        encodings: init?.sendEncodings ?? [{}],
        transactionId: "",
      }),
      setParameters: async (params: RTCRtpSendParameters) => params,
      getStats: async () => new Map() as unknown as RTCStatsReport,
      setStreams: () => {},
    } as unknown as RTCRtpSender;

    const receiver = {
      track: null,
      transport: null,
      transform: null as unknown,
      getParameters: () => ({
        codecs: [],
        headerExtensions: [],
        rtcp: { cname: "", reducedSize: false },
      }),
      getStats: async () => new Map() as unknown as RTCStatsReport,
      getContributingSources: () => [],
      getSynchronizationSources: () => [],
    } as unknown as RTCRtpReceiver;

    const transceiver = {
      mid,
      sender,
      receiver,
      direction,
      currentDirection: null as string | null,
      stopped: false,
      setDirection: (dir: RTCRtpTransceiverDirection) => {
        (transceiver as unknown as Record<string, unknown>).direction = dir;
      },
      stop: () => {
        (transceiver as unknown as Record<string, unknown>).stopped = true;
        (transceiver as unknown as Record<string, unknown>).currentDirection = null;
      },
      setCodecPreferences: () => {},
    } as unknown as RTCRtpTransceiver;

    this._transceivers.push(transceiver);
    this._senders.push(sender);
    return transceiver;
  }

  createDataChannel(label: string, dataChannelDict?: RTCDataChannelInit): RTCDataChannel {
    console.log(`[Elementium] createDataChannel called: label=${label} ordered=${dataChannelDict?.ordered} maxRetransmits=${dataChannelDict?.maxRetransmits}`);
    // Track for passing to create_offer so the SDP includes m=application. The channel
    // does not become usable until Rust reports the DCEP handshake complete
    // (`dataChannelOpen`, handled in `handleBackendEvent`) -- str0m's own docs say
    // `Rtc::channel()` returns `None` for a fresh `ChannelId` on *either* side, so there is
    // no shortcut that lets this side start writing before that.
    this._pendingDataChannels.push({
      label,
      ordered: dataChannelDict?.ordered,
      maxRetransmits: dataChannelDict?.maxRetransmits ?? undefined,
      maxPacketLifeTime: dataChannelDict?.maxPacketLifeTime ?? undefined,
      protocol: dataChannelDict?.protocol,
    });

    const channel = this.buildDataChannel(label, dataChannelDict);
    this._dataChannels.set(label, channel);
    return channel;
  }

  /**
   * Build the `RTCDataChannel` object shared by both `createDataChannel()` (local) and
   * `createRemoteDataChannel()` (remote-initiated). `readyState` starts `"connecting"` in
   * both cases and only ever moves to `"open"`/`"closed"` from a real backend event --
   * this used to fire `"open"` unconditionally after a `setTimeout(0)` regardless of
   * whether SDP negotiation, let alone the DCEP handshake, had happened, which is how
   * `send()` being a no-op went unnoticed: every caller saw a channel that claimed to be
   * open immediately.
   */
  private buildDataChannel(label: string, dataChannelDict?: RTCDataChannelInit): RTCDataChannel {
    const channelId = dataChannelDict?.id ?? this._dataChannelIdCounter++;
    // Read at send() time, not captured now: `createDataChannel()` can be called before
    // `init()`'s `create_peer_connection` call resolves, while `this.pcId` is still null.
    // By the time a channel can actually be open (a real `dataChannelOpen` backend event),
    // `pcId` is guaranteed to be set -- the event could not have arrived otherwise.
    const getPcId = () => this.pcId;

    const target = new EventTarget();
    const channel = Object.assign(target, {
      label,
      id: channelId,
      ordered: dataChannelDict?.ordered ?? true,
      protocol: dataChannelDict?.protocol ?? "",
      readyState: "connecting" as RTCDataChannelState,
      bufferedAmount: 0,
      bufferedAmountLowThreshold: 0,
      maxPacketLifeTime: dataChannelDict?.maxPacketLifeTime ?? null,
      maxRetransmits: dataChannelDict?.maxRetransmits ?? null,
      negotiated: dataChannelDict?.negotiated ?? false,
      binaryType: "arraybuffer" as BinaryType,
      onopen: null as ((ev: Event) => void) | null,
      onmessage: null as ((ev: MessageEvent) => void) | null,
      onclose: null as ((ev: Event) => void) | null,
      onerror: null as ((ev: Event) => void) | null,
      onbufferedamountlow: null as ((ev: Event) => void) | null,
      send: (data: string | Blob | ArrayBuffer | ArrayBufferView) => {
        if (channel.readyState !== "open") {
          // Matches the DOM: `send()` on a channel that is not open throws rather than
          // silently dropping, which is the shape of bug this whole feature exists to fix
          // -- a caller that thinks its message left when it did not.
          throw new DOMException(
            `Failed to execute 'send' on 'RTCDataChannel': RTCDataChannel.readyState is not 'open'`,
            "InvalidStateError",
          );
        }
        if (data instanceof Blob) {
          // Not supported: nothing between here and str0m's `Channel::write` (Tauri IPC,
          // `Vec<u8>`) has a notion of a `Blob`, and turning one into bytes requires an
          // async `arrayBuffer()` read that `send()`'s synchronous DOM signature has no
          // way to await. Every other payload shape (`string`, `ArrayBuffer`,
          // `ArrayBufferView`) is supported.
          console.warn(
            `[Elementium] RTCDataChannel.send(Blob) is not supported by this shim; ` +
              `message on label=${label} was dropped. Send an ArrayBuffer or string instead.`,
          );
          return;
        }
        let bytes: Uint8Array;
        let binary: boolean;
        if (typeof data === "string") {
          bytes = new TextEncoder().encode(data);
          binary = false;
        } else if (data instanceof ArrayBuffer) {
          bytes = new Uint8Array(data);
          binary = true;
        } else {
          // ArrayBufferView (typed array / DataView).
          bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
          binary = true;
        }
        invoke("send_data_channel_message", {
          pcId: getPcId(),
          label,
          binary,
          data: Array.from(bytes),
        }).catch((e) => {
          console.error(`[Elementium] send_data_channel_message failed: label=${label}`, e);
          // Also raised on the channel, because that is where a caller looks. `send()` is
          // synchronous and cannot reject, so the DOM's only route for a transport failure
          // is an `error` event -- and a console line is not a route at all for code trying
          // to decide whether to retry.
          const errorEvent = new Event("error");
          const handler = (channel as unknown as Record<string, unknown>)["onerror"];
          if (typeof handler === "function") {
            (handler as (ev: Event) => void).call(channel, errorEvent);
          }
          channel.dispatchEvent?.(errorEvent);
        });
      },
      close: () => {
        // Local-only: there is no Rust command to tear down the underlying SCTP stream
        // from this side (str0m's `close_data_channel` lives behind its lower-level
        // `DirectApi`, not the SDP-negotiated path this connection uses), so this marks
        // the JS-visible state closed without notifying the peer. A close initiated by the
        // *remote* side still arrives correctly, via the `dataChannelClose` backend event.
        if (channel.readyState === "closed") return;
        channel.readyState = "closed";
        const ev = new Event("close");
        target.dispatchEvent(ev);
        channel.onclose?.call(channel as unknown as RTCDataChannel, ev);
      },
    });

    return channel as unknown as RTCDataChannel;
  }

  getConfiguration(): RTCConfiguration { return {}; }
  setConfiguration(_configuration?: RTCConfiguration): void {}

  getSenders(): RTCRtpSender[] { return this._senders; }
  getReceivers(): RTCRtpReceiver[] { return []; }
  getTransceivers(): RTCRtpTransceiver[] { return [...this._transceivers]; }

  async getStats(_selector?: MediaStreamTrack | null): Promise<RTCStatsReport> {
    return getTransportStatsReport(this.pcId);
  }

  restartIce(): void {
    // Marks the connection as needing renegotiation and fires `negotiationneeded`, which is
    // what the DOM specifies -- the restart itself rides on the offer the page then makes.
    // This used to only log, so a call could not recover from a network change short of a
    // full reconnect.
    void invoke("restart_ice", { pcId: this.pcId })
      .then(() => {
        this.fireEvent("negotiationneeded", this.onnegotiationneeded);
      })
      .catch((e) => {
        console.error(`[Elementium] restart_ice(${this.pcId}) failed:`, e);
      });
  }
}

/**
 * RTCRtpScriptTransform shim that intercepts the E2EE Worker's messages
 * and forwards encryption keys to the Rust backend via Tauri IPC.
 *
 * Element Call creates a Web Worker that handles E2EE frame encryption.
 * The Worker receives `init` and `setKey` messages. We intercept these
 * to extract key material and forward it to Rust, where encryption
 * happens natively in the media pipeline.
 */
class ElementiumRTCRtpScriptTransform {
  constructor(worker: Worker, options?: unknown, _transfer?: Transferable[]) {
    try {
      // Wrap the worker's postMessage to intercept E2EE messages
      const origPostMessage = worker.postMessage.bind(worker);
      worker.postMessage = function (msg: unknown, transferOrOptions?: Transferable[] | StructuredSerializeOptions) {
        // Intercept E2EE messages — wrapped in try/catch so the real
        // postMessage always fires even if our interception code fails.
        try {
          interceptE2eeWorkerMessage(msg);
        } catch (e) {
          console.warn("[Elementium] E2EE intercept error (non-fatal):", e);
        }

        // Always forward to the real worker
        if (Array.isArray(transferOrOptions)) {
          return origPostMessage(msg, transferOrOptions);
        }
        return origPostMessage(msg, transferOrOptions as StructuredSerializeOptions);
      };
    } catch (e) {
      // If wrapping fails (e.g. postMessage not writable), fall through silently
      console.warn("[Elementium] RTCRtpScriptTransform shim setup failed (non-fatal):", e);
    }

    void options;
  }
}


/**
 * Install the WebRTC shim, replacing the global RTCPeerConnection.
 */
/**
 * Record every property read and method call on the peer connection, in order.
 *
 * Off unless asked for. It exists because "which call does the far end make that we do not
 * answer" is otherwise unanswerable: livekit-client is a minified bundle, our shim only logs
 * the calls it already knew about, and a call we never implemented logs nothing at all —
 * which is indistinguishable from a call that was never made.
 *
 * That is not hypothetical. On Element Web v1.12.25 livekit-client stops somewhere between
 * `createOffer` and `setLocalDescription`, and reading its source did not settle where.
 *
 * Enabled by `ELEMENTIUM_TRACE_PC=1` on `just app-join`, which the patch script forwards.
 */
let pcCounter = 0;

function tracePeerConnection<T extends object>(pc: T, pcId: string): T {
  return new Proxy(pc, {
    get(target, prop, receiver) {
      const value = Reflect.get(target, prop, receiver) as unknown;
      const name = String(prop);
      // Symbols and the event-handler slots are read constantly and say nothing.
      if (typeof prop === "symbol" || name.startsWith("on")) return value;
      if (typeof value === "function") {
        return (...args: unknown[]) => {
          console.log(`[PCTrace] ${pcId}.${name}(${args.length} args)`);
          return (value as (...a: unknown[]) => unknown).apply(target, args);
        };
      }
      console.log(`[PCTrace] ${pcId}.${name} -> ${safeTraceValue(value)}`);
      return value;
    },
  });
}

/** A property value rendered short enough to read in a log, and never a payload. */
function safeTraceValue(value: unknown): string {
  if (value === null || value === undefined) return String(value);
  if (typeof value === "object") return Array.isArray(value) ? `[${value.length}]` : "{...}";
  return String(value).slice(0, 40);
}

export function setupWebRtcShim(): void {
  const w = window as unknown as Record<string, unknown>;
  const trace = Boolean(
    (w["__ELEMENTIUM_AUTOJOIN"] as Record<string, unknown> | undefined)?.["tracePc"],
  );
  w.RTCPeerConnection = trace
    ? new Proxy(ElementiumRTCPeerConnection, {
        construct(target, args: ConstructorParameters<typeof ElementiumRTCPeerConnection>) {
          const pc = new target(...args);
          return tracePeerConnection(pc, String(pcCounter++));
        },
      })
    : ElementiumRTCPeerConnection;
  w.webkitRTCPeerConnection = w.RTCPeerConnection;

  // Register global event handler on window.top so Rust's webview.eval() can reach it.
  // Rust calls: window.__elementium_webrtc_event({type,pcId,...})
  // This dispatches to the correct PC's handler via the module-level registry.
  const topWin = getTopWindow();
  topWin.__elementium_webrtc_event = (payload: WebRtcEvent) => {
    console.log(`[Elementium] eval event: type=${payload.type} pcId=${payload.pcId} registry_size=${__pcRegistry.size} has_handler=${__pcRegistry.has(payload.pcId)}`);
    const handler = __pcRegistry.get(payload.pcId);
    if (handler) {
      handler(payload);
      return;
    }
    // Held rather than dropped.
    //
    // Rust starts forwarding events the moment the connection exists, while this side only
    // registers its handler once the `invoke()` that created it has resolved. Anything in
    // that window used to vanish — and what arrives first is exactly what matters most:
    // the initial ICE candidates and the first connection-state change. Losing a candidate
    // costs connectivity that nothing later re-announces, and each state change is the only
    // notification of a transition that will not repeat.
    //
    // Replayed in arrival order when the handler appears, and capped so a pcId that never
    // registers — a connection closed before it finished opening — cannot grow forever.
    const held = __pendingEvents.get(payload.pcId) ?? [];
    if (held.length < MAX_HELD_EVENTS) {
      held.push(payload);
      __pendingEvents.set(payload.pcId, held);
    } else {
      console.warn(
        `[Elementium] dropping an event for unregistered pc ${payload.pcId}: ` +
          `${MAX_HELD_EVENTS} already held`,
      );
    }
  };

  // Stub RTCRtpScriptTransform for E2EE support detection
  if (typeof w.RTCRtpScriptTransform === "undefined") {
    w.RTCRtpScriptTransform = ElementiumRTCRtpScriptTransform;
  }

  // Polyfill RTCSessionDescription if missing (WebKitGTK lacks it).
  // livekit-client uses `new RTCSessionDescription({type, sdp})` for SDP munging.
  if (typeof w.RTCSessionDescription === "undefined") {
    w.RTCSessionDescription = class RTCSessionDescription {
      readonly type: RTCSdpType;
      readonly sdp: string;
      constructor(init: RTCSessionDescriptionInit) {
        this.type = init.type!;
        this.sdp = init.sdp ?? "";
      }
      toJSON(): RTCSessionDescriptionInit {
        return { type: this.type, sdp: this.sdp };
      }
    } as unknown as typeof globalThis.RTCSessionDescription;
  }

  // Polyfill RTCIceCandidate if missing (WebKitGTK may lack it).
  if (typeof w.RTCIceCandidate === "undefined") {
    w.RTCIceCandidate = class RTCIceCandidate {
      readonly candidate: string;
      readonly sdpMid: string | null;
      readonly sdpMLineIndex: number | null;
      readonly usernameFragment: string | null;
      constructor(init?: RTCIceCandidateInit) {
        this.candidate = init?.candidate ?? "";
        this.sdpMid = init?.sdpMid ?? null;
        this.sdpMLineIndex = init?.sdpMLineIndex ?? null;
        this.usernameFragment = init?.usernameFragment ?? null;
      }
      toJSON(): RTCIceCandidateInit {
        return {
          candidate: this.candidate,
          sdpMid: this.sdpMid,
          sdpMLineIndex: this.sdpMLineIndex,
          usernameFragment: this.usernameFragment,
        };
      }
    } as unknown as typeof globalThis.RTCIceCandidate;
  }

  // LiveKit signaling WebSocket interceptor.
  //
  // Fixes a race condition where our Tauri IPC latency causes the publisher
  // offer to be sent BEFORE the subscriber answer. The SFU expects its
  // subscriber offer to be answered before it will accept a publisher offer;
  // otherwise it responds with STATE_MISMATCH and disconnects.
  //
  // Protobuf SignalRequest field tags (first byte of each message):
  //   10 = field 1 (offer)   — publisher offer
  //   18 = field 2 (answer)  — subscriber answer
  //   26 = field 3 (trickle)
  //   ...
  // Protobuf SignalResponse field tags (first byte of each recv message):
  //   26 = field 3 (offer)   — subscriber offer from SFU
  //
  // Strategy: when a subscriber offer is received, buffer any outgoing
  // publisher offers until the subscriber answer has been sent.

  const PROTO_TAG_REQ_OFFER = 10;   // SignalRequest.offer (field 1, wire type 2)
  const PROTO_TAG_REQ_ANSWER = 18;  // SignalRequest.answer (field 2, wire type 2)
  const PROTO_TAG_RESP_OFFER = 26;  // SignalResponse.offer (field 3, wire type 2)

  // --- Protobuf patching helpers ---
  // livekit-client's proto3 encoding omits SessionDescription.type (field 1)
  // when it's the default empty string. The LiveKit SFU requires this field
  // to be present ("offer" or "answer"), sending STATE_MISMATCH without it.
  // We inject the type field into outgoing messages at the WebSocket level.

  function readVarint(bytes: Uint8Array, offset: number): { value: number; bytesRead: number } {
    let value = 0;
    let shift = 0;
    let bytesRead = 0;
    while (offset + bytesRead < bytes.length) {
      const b = bytes[offset + bytesRead];
      value |= (b & 0x7f) << shift;
      bytesRead++;
      if ((b & 0x80) === 0) break;
      shift += 7;
    }
    return { value, bytesRead };
  }

  function encodeVarint(value: number): number[] {
    const result: number[] = [];
    do {
      let byte = value & 0x7f;
      value >>>= 7;
      if (value > 0) byte |= 0x80;
      result.push(byte);
    } while (value > 0);
    return result;
  }

  // Inject `type` field (proto field 1) into a SessionDescription inside
  // a SignalRequest.offer (tag=10) or SignalRequest.answer (tag=18).
  function injectTypeField(rawBytes: Uint8Array): Uint8Array {
    if (rawBytes.length < 3) return rawBytes;
    const outerTag = rawBytes[0];
    if (outerTag !== PROTO_TAG_REQ_OFFER && outerTag !== PROTO_TAG_REQ_ANSWER) {
      return rawBytes;
    }
    const { value: innerLen, bytesRead: lenBytes } = readVarint(rawBytes, 1);
    const innerStart = 1 + lenBytes;
    if (innerStart >= rawBytes.length) return rawBytes;
    // If inner already starts with field 1 tag (10), type is present — skip
    if (rawBytes[innerStart] === 10) return rawBytes;
    // If inner doesn't start with field 2 tag (18 = sdp), unexpected — skip
    if (rawBytes[innerStart] !== 18) return rawBytes;

    const typeStr = outerTag === PROTO_TAG_REQ_OFFER ? "offer" : "answer";
    const typeEncoded = new TextEncoder().encode(typeStr);
    // Type field: tag=10 (field 1, wire type 2) + length byte + string bytes
    const typeFieldLen = 1 + 1 + typeEncoded.length;

    const newInnerLen = innerLen + typeFieldLen;
    const newLenBytes = encodeVarint(newInnerLen);

    const result = new Uint8Array(1 + newLenBytes.length + newInnerLen);
    let pos = 0;
    result[pos++] = outerTag;
    for (const b of newLenBytes) result[pos++] = b;
    // Injected type field
    result[pos++] = 10; // field 1, wire type 2
    result[pos++] = typeEncoded.length;
    result.set(typeEncoded, pos);
    pos += typeEncoded.length;
    // Original inner bytes (sdp + id)
    result.set(rawBytes.subarray(innerStart, innerStart + innerLen), pos);

    return result;
  }

  function patchOutgoingMessage(
    data: string | ArrayBufferLike | Blob | ArrayBufferView,
  ): { data: string | ArrayBufferLike | Blob | ArrayBufferView; patched: boolean } {
    let bytes: Uint8Array | null = null;
    if (data instanceof ArrayBuffer) {
      bytes = new Uint8Array(data);
    } else if (ArrayBuffer.isView(data)) {
      bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    }
    if (bytes === null) return { data, patched: false };
    const result = injectTypeField(bytes);
    if (result === bytes) return { data, patched: false };
    return { data: result.buffer, patched: true };
  }

  const OrigWebSocket = window.WebSocket;
  w.WebSocket = class MonitoredWebSocket extends OrigWebSocket {
    constructor(url: string | URL, protocols?: string | string[]) {
      super(url, protocols);
      const wsUrl = url.toString();
      // Never the raw URL: it carries the access token and the whole join request.
      const safeUrl = redactSignalUrl(wsUrl);
      const isLk = wsUrl.includes("livekit") || wsUrl.includes("matrixrtc") || wsUrl.includes("rtc");
      if (isLk) {
        console.log(`[Elementium] WebSocket opening: ${safeUrl}`);
        let recvCount = 0;
        let sendCount = 0;
        // Reorder state: buffer publisher offers while waiting for subscriber answer
        let awaitingSubscriberAnswer = false;
        let bufferedOffers: (string | ArrayBufferLike | Blob | ArrayBufferView)[] = [];
        let reorderTimer: ReturnType<typeof setTimeout> | null = null;

        if (trace) {
          // Which connection flow is in use, and what we packed into it. A protocol-16
          // client has no join request at all, so its absence is as informative as its
          // contents -- see `livekit-signal.ts`.
          const join = describeJoinRequest(wsUrl);
          console.log(
            `[LKSignal] connect: ${join ?? "no join_request (pre-protocol-17 flow)"}`,
          );
        }

        this.addEventListener("open", () => {
          console.log(`[Elementium] WebSocket opened: ${safeUrl}`);
        });
        this.addEventListener("close", (e) => {
          console.log(`[Elementium] WebSocket closed: ${safeUrl} code=${e.code} reason=${e.reason} wasClean=${e.wasClean}`);
          // Clean up on close
          awaitingSubscriberAnswer = false;
          bufferedOffers = [];
          if (reorderTimer !== null) { clearTimeout(reorderTimer); reorderTimer = null; }
        });
        this.addEventListener("error", () => {
          console.log(`[Elementium] WebSocket error: ${safeUrl}`);
        });

        // Intercept incoming messages — detect subscriber offers from SFU
        this.addEventListener("message", (e) => {
          const idx = recvCount++;
          const bytes = signalBytes(e.data);
          const firstByte = bytes !== null && bytes.length > 0 ? bytes[0] : -1;
          // The join response is where the SFU says who we are, and end-to-end encryption
          // cannot encrypt a single frame until it knows. Read here rather than in the
          // E2EE bridge because this is the only place the message passes through.
          if (bytes !== null) {
            const identity = localIdentityFromJoin(bytes);
            if (identity !== null) noteLocalIdentity(identity);
          }
          if (trace) {
            if (bytes !== null) {
              console.log(
                `[LKSignal] recv #${idx} ${describeResponse(bytes)} (${bytes.byteLength} bytes)`,
              );
            } else if (e.data instanceof Blob) {
              console.log(`[LKSignal] recv #${idx} blob (${e.data.size} bytes)`);
            } else {
              // Never the message body: signalling text carries tokens and identities.
              console.log(`[LKSignal] recv #${idx} text (${String(e.data).length} chars)`);
            }
          }

          // If SFU sent a subscriber offer, mark that we need to answer it
          // before sending any publisher offer.
          if (firstByte === PROTO_TAG_RESP_OFFER) {
            console.log(`[Elementium] SFU subscriber offer received — buffering publisher offers until answer sent`);
            awaitingSubscriberAnswer = true;
          }
        });

        // Intercept outgoing messages — reorder offers after answers
        const origSend = this.send.bind(this);
        this.send = (data: string | ArrayBufferLike | Blob | ArrayBufferView) => {
          const idx = sendCount++;

          // Inject missing `type` field into outgoing SessionDescription messages
          const patchCheck = patchOutgoingMessage(data);
          if (patchCheck.patched) {
            let tag = -1;
            if (data instanceof ArrayBuffer) {
              tag = new Uint8Array(data)[0];
            } else if (ArrayBuffer.isView(data)) {
              tag = new Uint8Array(data.buffer, data.byteOffset, data.byteLength)[0];
            }
            const typeStr = tag === PROTO_TAG_REQ_OFFER ? "offer" : "answer";
            console.log(`[Elementium] Injected type="${typeStr}" into outgoing SessionDescription`);
            data = patchCheck.data;
          }

          // Determine the protobuf tag of the outgoing message
          const outBytes = signalBytes(data);
          const firstByte = outBytes !== null && outBytes.length > 0 ? outBytes[0] : -1;
          if (trace) {
            if (outBytes !== null) {
              console.log(
                `[LKSignal] send #${idx} ${describeRequest(outBytes)} (${outBytes.byteLength} bytes)`,
              );
            } else if (data instanceof Blob) {
              console.log(`[LKSignal] send #${idx} blob (${data.size} bytes)`);
            } else {
              console.log(`[LKSignal] send #${idx} text (${String(data).length} chars)`);
            }
          }

          // Reorder logic: buffer publisher offers while subscriber answer is pending.
          //
          // The safety timer is armed *here*, when an offer is actually held, rather than
          // when the subscriber offer arrives. Arming it on arrival meant it could fire
          // while the buffer was still empty, do nothing, and leave no timer running for
          // an offer buffered afterwards -- stranding it indefinitely.
          if (firstByte === PROTO_TAG_REQ_OFFER && awaitingSubscriberAnswer) {
            console.log(`[Elementium] Buffering publisher offer (waiting for subscriber answer)`);
            bufferedOffers.push(data);
            if (reorderTimer === null) {
              reorderTimer = setTimeout(() => {
                reorderTimer = null;
                if (bufferedOffers.length === 0) return;
                console.warn(
                  `[Elementium] Reorder timeout — flushing ${bufferedOffers.length} buffered publisher offer(s)`,
                );
                const stranded = bufferedOffers;
                bufferedOffers = [];
                awaitingSubscriberAnswer = false;
                for (const buf of stranded) origSend(buf);
              }, 500);
            }
            return;
          }

          // Send the message
          origSend(data);

          // The subscriber offer has now been answered, so publisher offers may flow again.
          //
          // This must clear the flag whether or not anything is buffered. It used to be
          // guarded on `bufferedOffers.length > 0`, so an answer sent with an empty buffer
          // left the flag latched at `true` forever: every subsequent publisher offer was
          // buffered and never sent, the publisher transport never negotiated, and LiveKit
          // dropped into an endless "reconnecting" loop ~15s later.
          if (firstByte === PROTO_TAG_REQ_ANSWER) {
            awaitingSubscriberAnswer = false;
            if (reorderTimer !== null) { clearTimeout(reorderTimer); reorderTimer = null; }
            if (bufferedOffers.length > 0) {
              console.log(`[Elementium] Subscriber answer sent — will flush ${bufferedOffers.length} buffered publisher offer(s) after 100ms`);
              const toFlush = [...bufferedOffers];
              bufferedOffers = [];
              // Brief delay so the SFU processes the answer before the offer lands.
              setTimeout(() => {
                console.log(`[Elementium] Flushing ${toFlush.length} buffered publisher offer(s) now`);
                for (const buf of toFlush) origSend(buf);
              }, 100);
            }
          }
        };
      }
    }
  } as unknown as typeof WebSocket;

  console.log("[Elementium] RTCPeerConnection shim installed");
}
