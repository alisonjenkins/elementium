/**
 * WebRTC shim that intercepts RTCPeerConnection and routes to the Rust backend.
 *
 * This replaces the browser's WebRTC implementation with Tauri IPC calls
 * to the native str0m-based WebRTC engine.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

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
  private _unlisten: UnlistenFn | null = null;
  private _senders: RTCRtpSender[] = [];
  private _transceivers: RTCRtpTransceiver[] = [];
  private _dataChannelIdCounter = 0;
  private _hasVideo = false;

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

      // Listen for WebRTC events from the Rust backend
      this._unlisten = await listen<WebRtcEvent>("webrtc-event", (event) => {
        if (event.payload.pcId === this.pcId) {
          this.handleBackendEvent(event.payload);
        }
      });
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
    }
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
      const trackId = `${this.pcId}-video`;
      const canvas = document.createElement("canvas");
      canvas.width = 640;
      canvas.height = 480;

      // Use captureStream to get a real MediaStreamTrack from the canvas
      const canvasStream = canvas.captureStream(30);
      const videoTrack = canvasStream.getVideoTracks()[0];
      if (videoTrack) {
        stream.addTrack(videoTrack);

        // Start rendering frames from Rust onto this canvas
        this.startVideoFrameFetch(canvas, trackId);
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
   * Fetch video frames from the Rust backend and render onto a canvas.
   */
  private startVideoFrameFetch(canvas: HTMLCanvasElement, trackId: string) {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    let running = true;

    const fetchLoop = async () => {
      if (!running) return;

      try {
        const resp = await fetch(`elementium://localhost/video-frame/${trackId}`);
        if (resp.ok) {
          const width = parseInt(resp.headers.get("X-Frame-Width") || "0", 10);
          const height = parseInt(resp.headers.get("X-Frame-Height") || "0", 10);

          if (width > 1 && height > 1) {
            if (canvas.width !== width || canvas.height !== height) {
              canvas.width = width;
              canvas.height = height;
            }
            const buf = await resp.arrayBuffer();
            const imageData = new ImageData(
              new Uint8ClampedArray(buf),
              width,
              height,
            );
            ctx.putImageData(imageData, 0, 0);
          }
        }
      } catch {
        // Frame fetch failed, skip
      }

      if (running) {
        requestAnimationFrame(fetchLoop);
      }
    };

    requestAnimationFrame(fetchLoop);

    // Store cleanup function (called on close)
    const originalClose = this.close.bind(this);
    this.close = () => {
      running = false;
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
    const desc = await invoke<{ sdpType: string; sdp: string }>("create_offer", {
      pcId: this.pcId,
      includeVideo: this._hasVideo,
    });
    const init: RTCSessionDescriptionInit = { type: desc.sdpType as RTCSdpType, sdp: desc.sdp };
    return init;
  }

  async createAnswer(_options?: RTCAnswerOptions): Promise<RTCSessionDescriptionInit> {
    await this.ensureReady();
    const desc = await invoke<{ sdpType: string; sdp: string }>("create_answer", {
      pcId: this.pcId,
    });
    const init: RTCSessionDescriptionInit = { type: desc.sdpType as RTCSdpType, sdp: desc.sdp };
    return init;
  }

  async setLocalDescription(description?: RTCSessionDescriptionInit): Promise<void> {
    await this.ensureReady();
    if (!description) return;
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

    const result = await invoke<{ sdpType: string; sdp: string } | null>(
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
        this._localDescription = new RTCSessionDescription({
          type: result.sdpType as RTCSdpType,
          sdp: result.sdp,
        });
      }
    } else {
      this._signalingState = "stable";
    }
    this.fireEvent("signalingstatechange", this.onsignalingstatechange);
  }

  async addIceCandidate(candidate?: RTCIceCandidateInit | null): Promise<void> {
    await this.ensureReady();
    if (!candidate?.candidate) return;
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
      invoke("close_peer_connection", { pcId: this.pcId }).catch(console.error);
      this._connectionState = "closed";
      this._iceConnectionState = "closed";
      this._signalingState = "closed";
      this._unlisten?.();
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
    console.log(`[Elementium] addTransceiver called: kind=${kind}`);
    if (kind === "video") {
      this._hasVideo = true;
    }

    const mid = String(this._transceivers.length);
    const direction = init?.direction ?? "sendrecv";

    const sender = {
      track,
      dtmf: null,
      transport: null,
      transform: null as unknown,
      replaceTrack: async (newTrack: MediaStreamTrack | null) => {
        (sender as Record<string, unknown>).track = newTrack;
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
        (transceiver as Record<string, unknown>).direction = dir;
      },
      stop: () => {
        (transceiver as Record<string, unknown>).stopped = true;
        (transceiver as Record<string, unknown>).currentDirection = null;
      },
      setCodecPreferences: () => {},
    } as unknown as RTCRtpTransceiver;

    this._transceivers.push(transceiver);
    this._senders.push(sender);
    return transceiver;
  }

  createDataChannel(label: string, dataChannelDict?: RTCDataChannelInit): RTCDataChannel {
    console.log(`[Elementium] createDataChannel called: label=${label}`);
    const channelId = dataChannelDict?.id ?? this._dataChannelIdCounter++;

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
      send: (_data: string | Blob | ArrayBuffer | ArrayBufferView) => {},
      close: () => {
        channel.readyState = "closed";
        const ev = new Event("close");
        target.dispatchEvent(ev);
        channel.onclose?.call(channel as unknown as RTCDataChannel, ev);
      },
    });

    // Fire open event asynchronously (classic script-order: after current microtask)
    setTimeout(() => {
      channel.readyState = "open";
      const ev = new Event("open");
      target.dispatchEvent(ev);
      channel.onopen?.call(channel as unknown as RTCDataChannel, ev);
    }, 0);

    return channel as unknown as RTCDataChannel;
  }

  getConfiguration(): RTCConfiguration { return {}; }
  setConfiguration(_configuration?: RTCConfiguration): void {}

  getSenders(): RTCRtpSender[] { return this._senders; }
  getReceivers(): RTCRtpReceiver[] { return []; }
  getTransceivers(): RTCRtpTransceiver[] { return [...this._transceivers]; }

  async getStats(_selector?: MediaStreamTrack | null): Promise<RTCStatsReport> {
    return new Map() as unknown as RTCStatsReport;
  }

  restartIce(): void {
    console.log("[Elementium] restartIce called");
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
          const m = msg as Record<string, unknown> | null;
          if (m && typeof m === "object") {
            interceptE2eeMessage(m);
          }
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

/** Safe helper to invoke a Tauri command, catching both sync throws and async rejections. */
function safeInvoke(cmd: string, args: Record<string, unknown>): void {
  try {
    invoke(cmd, args).catch((e: unknown) =>
      console.warn(`[Elementium] IPC ${cmd} rejected:`, e),
    );
  } catch (e) {
    console.warn(`[Elementium] IPC ${cmd} unavailable:`, e);
  }
}

/** Extract and forward E2EE key/init messages to the Rust backend. */
function interceptE2eeMessage(m: Record<string, unknown>): void {
  // livekit-client may nest data under m.data
  const data = (m.data && typeof m.data === "object" ? m.data : m) as Record<string, unknown>;
  const kind = (m.kind ?? m.type ?? "") as string;

  if (kind === "setKey") {
    const participantIdentity = ((data.participantIdentity ?? data.participantId ?? "") as string);
    const keyIndex = (data.keyIndex ?? 0) as number;
    const keyData = data.key;

    if (keyData && (keyData instanceof ArrayBuffer || keyData instanceof Uint8Array)) {
      const keyArray = Array.from(
        keyData instanceof Uint8Array ? keyData : new Uint8Array(keyData),
      );
      console.log(
        `[Elementium] E2EE key received for participant ${participantIdentity} index=${keyIndex} len=${keyArray.length}`,
      );
      safeInvoke("e2ee_set_key", {
        participant: participantIdentity,
        keyIndex,
        keyMaterial: keyArray,
      });
    }
  }

  if (kind === "init") {
    const keyProviderOptions = (data.keyProviderOptions ?? null) as Record<string, unknown> | null;
    console.log("[Elementium] E2EE Worker init intercepted", keyProviderOptions);
    safeInvoke("e2ee_init", { options: keyProviderOptions });
  }
}

/**
 * Install the WebRTC shim, replacing the global RTCPeerConnection.
 */
export function setupWebRtcShim(): void {
  const w = window as unknown as Record<string, unknown>;
  w.RTCPeerConnection = ElementiumRTCPeerConnection;
  w.webkitRTCPeerConnection = ElementiumRTCPeerConnection;

  // Stub RTCRtpScriptTransform for E2EE support detection
  if (typeof w.RTCRtpScriptTransform === "undefined") {
    w.RTCRtpScriptTransform = ElementiumRTCRtpScriptTransform;
  }

  console.log("[Elementium] RTCPeerConnection shim installed");
}
