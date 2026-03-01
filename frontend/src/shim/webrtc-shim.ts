/**
 * WebRTC shim that intercepts RTCPeerConnection and routes to the Rust backend.
 *
 * This replaces the browser's WebRTC implementation with Tauri IPC calls
 * to the native str0m-based WebRTC engine.
 */

import { invoke } from "@tauri-apps/api/core";

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
  private _pendingTransceivers: { kind: string; direction: string }[] = [];

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
   * Fetch video frames from the Rust backend via Tauri IPC and render onto a canvas.
   */
  private startVideoFrameFetch(canvas: HTMLCanvasElement, trackId: string) {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    let running = true;
    let timerId: ReturnType<typeof setTimeout> | null = null;

    const fetchLoop = async () => {
      if (!running) return;

      try {
        const buf = await invoke<ArrayBuffer>("get_video_frame", { trackId });
        if (buf && buf.byteLength > 8) {
          const view = new DataView(buf);
          const width = view.getUint32(0, true);
          const height = view.getUint32(4, true);

          if (width > 1 && height > 1) {
            if (canvas.width !== width || canvas.height !== height) {
              canvas.width = width;
              canvas.height = height;
            }
            const rgba = new Uint8ClampedArray(buf, 8);
            const imageData = new ImageData(rgba, width, height);
            ctx.putImageData(imageData, 0, 0);
          }
        }
      } catch {
        // Frame fetch failed, skip
      }

      if (running) {
        timerId = setTimeout(fetchLoop, 33);
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
    const desc = await invoke<{ sdpType: string; sdp: string }>("create_offer", {
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
    const init: RTCSessionDescriptionInit = { type: desc.sdpType as RTCSdpType, sdp: desc.sdp };
    return init;
  }

  async createAnswer(_options?: RTCAnswerOptions): Promise<RTCSessionDescriptionInit> {
    await this.ensureReady();
    console.log(`[Elementium] createAnswer: pcId=${this.pcId}`);
    const desc = await invoke<{ sdpType: string; sdp: string }>("create_answer", {
      pcId: this.pcId,
    });
    console.log(`[Elementium] createAnswer result: pcId=${this.pcId} sdpLen=${desc.sdp.length}`);
    const init: RTCSessionDescriptionInit = { type: desc.sdpType as RTCSdpType, sdp: desc.sdp };
    return init;
  }

  async setLocalDescription(description?: RTCSessionDescriptionInit): Promise<void> {
    await this.ensureReady();
    if (!description) return;
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
    this._pendingTransceivers.push({ kind, direction });

    const mid = String(this._transceivers.length);

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
    console.log(`[Elementium] createDataChannel called: label=${label} ordered=${dataChannelDict?.ordered} maxRetransmits=${dataChannelDict?.maxRetransmits}`);
    const channelId = dataChannelDict?.id ?? this._dataChannelIdCounter++;
    // Track for passing to create_offer so the SDP includes m=application
    this._pendingDataChannels.push({
      label,
      ordered: dataChannelDict?.ordered,
      maxRetransmits: dataChannelDict?.maxRetransmits ?? undefined,
      maxPacketLifeTime: dataChannelDict?.maxPacketLifeTime ?? undefined,
      protocol: dataChannelDict?.protocol,
    });

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

  // Register global event handler on window.top so Rust's webview.eval() can reach it.
  // Rust calls: window.__elementium_webrtc_event({type,pcId,...})
  // This dispatches to the correct PC's handler via the module-level registry.
  const topWin = getTopWindow();
  topWin.__elementium_webrtc_event = (payload: WebRtcEvent) => {
    console.log(`[Elementium] eval event: type=${payload.type} pcId=${payload.pcId} registry_size=${__pcRegistry.size} has_handler=${__pcRegistry.has(payload.pcId)}`);
    const handler = __pcRegistry.get(payload.pcId);
    if (handler) {
      handler(payload);
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
      const isLk = wsUrl.includes("livekit") || wsUrl.includes("matrixrtc") || wsUrl.includes("rtc");
      if (isLk) {
        console.log(`[Elementium] WebSocket opening: ${wsUrl}`);
        let recvCount = 0;
        let sendCount = 0;
        // Reorder state: buffer publisher offers while waiting for subscriber answer
        let awaitingSubscriberAnswer = false;
        let bufferedOffers: (string | ArrayBufferLike | Blob | ArrayBufferView)[] = [];
        let reorderTimer: ReturnType<typeof setTimeout> | null = null;

        this.addEventListener("open", () => {
          console.log(`[Elementium] WebSocket opened: ${wsUrl}`);
        });
        this.addEventListener("close", (e) => {
          console.log(`[Elementium] WebSocket closed: ${wsUrl} code=${e.code} reason=${e.reason} wasClean=${e.wasClean}`);
          // Clean up on close
          awaitingSubscriberAnswer = false;
          bufferedOffers = [];
          if (reorderTimer !== null) { clearTimeout(reorderTimer); reorderTimer = null; }
        });
        this.addEventListener("error", () => {
          console.log(`[Elementium] WebSocket error: ${wsUrl}`);
        });

        // Intercept incoming messages — detect subscriber offers from SFU
        this.addEventListener("message", (e) => {
          const idx = recvCount++;
          let firstByte = -1;
          if (e.data instanceof ArrayBuffer) {
            const bytes = new Uint8Array(e.data);
            firstByte = bytes.length > 0 ? bytes[0] : -1;
            if (idx < 5) {
              console.log(`[Elementium] WS recv #${idx}: binary ${bytes.byteLength} bytes tag=${firstByte} FULL:`, Array.from(bytes));
            } else {
              console.log(`[Elementium] WS recv #${idx}: binary ${bytes.byteLength} bytes tag=${firstByte}`, bytes.slice(0, 32));
            }
          } else if (e.data instanceof Blob) {
            console.log(`[Elementium] WS recv #${idx}: blob ${e.data.size} bytes`);
          } else {
            const str = String(e.data);
            console.log(`[Elementium] WS recv #${idx}: text ${str.length} chars`, str.slice(0, 200));
          }

          // If SFU sent a subscriber offer, mark that we need to answer it
          // before sending any publisher offer.
          if (firstByte === PROTO_TAG_RESP_OFFER) {
            console.log(`[Elementium] SFU subscriber offer received — buffering publisher offers until answer sent`);
            awaitingSubscriberAnswer = true;
            // Safety timeout: don't hold offers forever (500ms max)
            if (reorderTimer !== null) clearTimeout(reorderTimer);
            reorderTimer = setTimeout(() => {
              if (awaitingSubscriberAnswer && bufferedOffers.length > 0) {
                console.warn(`[Elementium] Reorder timeout — flushing ${bufferedOffers.length} buffered publisher offer(s)`);
                for (const buf of bufferedOffers) origSend(buf);
                bufferedOffers = [];
                awaitingSubscriberAnswer = false;
              }
            }, 500);
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
          let firstByte = -1;
          if (data instanceof ArrayBuffer) {
            const bytes = new Uint8Array(data);
            firstByte = bytes.length > 0 ? bytes[0] : -1;
            if (idx < 5) {
              console.log(`[Elementium] WS send #${idx}: binary ${bytes.byteLength} bytes tag=${firstByte} FULL:`, Array.from(bytes));
            } else {
              console.log(`[Elementium] WS send #${idx}: binary ${bytes.byteLength} bytes tag=${firstByte}`, bytes.slice(0, 32));
            }
          } else if (data instanceof Blob) {
            console.log(`[Elementium] WS send #${idx}: blob ${data.size} bytes`);
          } else if (ArrayBuffer.isView(data)) {
            const bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
            firstByte = bytes.length > 0 ? bytes[0] : -1;
            if (idx < 5) {
              console.log(`[Elementium] WS send #${idx}: view ${bytes.byteLength} bytes tag=${firstByte} FULL:`, Array.from(bytes));
            } else {
              console.log(`[Elementium] WS send #${idx}: view ${bytes.byteLength} bytes tag=${firstByte}`, bytes.slice(0, 32));
            }
          } else {
            const str = String(data);
            console.log(`[Elementium] WS send #${idx}: text ${str.length} chars`, str.slice(0, 200));
          }

          // Reorder logic: buffer publisher offers while subscriber answer is pending
          if (firstByte === PROTO_TAG_REQ_OFFER && awaitingSubscriberAnswer) {
            console.log(`[Elementium] Buffering publisher offer (waiting for subscriber answer)`);
            bufferedOffers.push(data);
            return;
          }

          // Send the message
          origSend(data);

          // If this was the subscriber answer, flush buffered publisher offers after a delay
          // to give the SFU time to process the answer before receiving the offer
          if (firstByte === PROTO_TAG_REQ_ANSWER && bufferedOffers.length > 0) {
            console.log(`[Elementium] Subscriber answer sent — will flush ${bufferedOffers.length} buffered publisher offer(s) after 100ms`);
            awaitingSubscriberAnswer = false;
            if (reorderTimer !== null) { clearTimeout(reorderTimer); reorderTimer = null; }
            const toFlush = [...bufferedOffers];
            bufferedOffers = [];
            setTimeout(() => {
              console.log(`[Elementium] Flushing ${toFlush.length} buffered publisher offer(s) now`);
              for (const buf of toFlush) origSend(buf);
            }, 100);
          }
        };
      }
    }
  } as unknown as typeof WebSocket;

  console.log("[Elementium] RTCPeerConnection shim installed");
}
