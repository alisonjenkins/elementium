/**
 * WebRTC shim that intercepts RTCPeerConnection and routes to the Rust backend.
 *
 * This replaces the browser's WebRTC implementation with Tauri IPC calls
 * to the native str0m-based WebRTC engine.
 */

import { invoke } from "@tauri-apps/api/core";

interface PeerConnectionHandle {
  id: string;
}

/**
 * Shimmed RTCPeerConnection that delegates to the Rust backend.
 *
 * We don't implement the full RTCPeerConnection interface directly because
 * the legacy callback-based overloads create TypeScript compatibility issues.
 * Instead, we match the shape at runtime which is sufficient for Element Web.
 */
class ElementiumRTCPeerConnection extends EventTarget {
  private pcId: string | null = null;
  private _connectionState: RTCPeerConnectionState = "new";
  private _iceConnectionState: RTCIceConnectionState = "new";
  private _iceGatheringState: RTCIceGatheringState = "new";
  private _signalingState: RTCSignalingState = "stable";
  private _localDescription: RTCSessionDescription | null = null;
  private _remoteDescription: RTCSessionDescription | null = null;

  // Event handlers
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
    this.init(configuration);
  }

  private async init(configuration?: RTCConfiguration) {
    try {
      const handle = await invoke<PeerConnectionHandle>("create_peer_connection", {
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
    } catch (e) {
      console.error("[Elementium] Failed to create peer connection:", e);
    }
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
    if (!this.pcId) throw new Error("PeerConnection not initialized");
    const desc = await invoke<{ type: string; sdp: string }>("create_offer", { pcId: this.pcId });
    return { type: desc.type as RTCSdpType, sdp: desc.sdp };
  }

  async createAnswer(_options?: RTCAnswerOptions): Promise<RTCSessionDescriptionInit> {
    if (!this.pcId) throw new Error("PeerConnection not initialized");
    const desc = await invoke<{ type: string; sdp: string }>("create_answer", { pcId: this.pcId });
    return { type: desc.type as RTCSdpType, sdp: desc.sdp };
  }

  async setLocalDescription(description?: RTCSessionDescriptionInit): Promise<void> {
    if (!this.pcId || !description) return;
    await invoke("set_local_description", {
      pcId: this.pcId,
      description: { type: description.type, sdp: description.sdp },
    });
    this._localDescription = new RTCSessionDescription(description);
  }

  async setRemoteDescription(description: RTCSessionDescriptionInit): Promise<void> {
    if (!this.pcId) return;
    await invoke("set_remote_description", {
      pcId: this.pcId,
      description: { type: description.type, sdp: description.sdp },
    });
    this._remoteDescription = new RTCSessionDescription(description);
  }

  async addIceCandidate(candidate?: RTCIceCandidateInit | null): Promise<void> {
    if (!this.pcId || !candidate) return;
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
    }
  }

  // Stub implementations for API compliance
  addTrack(_track: MediaStreamTrack, ..._streams: MediaStream[]): RTCRtpSender {
    console.warn("[Elementium] addTrack: stub");
    return {} as RTCRtpSender;
  }

  removeTrack(_sender: RTCRtpSender): void {
    console.warn("[Elementium] removeTrack: stub");
  }

  addTransceiver(
    _trackOrKind: MediaStreamTrack | string,
    _init?: RTCRtpTransceiverInit,
  ): RTCRtpTransceiver {
    console.warn("[Elementium] addTransceiver: stub");
    return {} as RTCRtpTransceiver;
  }

  createDataChannel(_label: string, _dataChannelDict?: RTCDataChannelInit): RTCDataChannel {
    console.warn("[Elementium] createDataChannel: stub");
    return {} as RTCDataChannel;
  }

  getConfiguration(): RTCConfiguration { return {}; }
  setConfiguration(_configuration?: RTCConfiguration): void {}

  getSenders(): RTCRtpSender[] { return []; }
  getReceivers(): RTCRtpReceiver[] { return []; }
  getTransceivers(): RTCRtpTransceiver[] { return []; }

  async getStats(_selector?: MediaStreamTrack | null): Promise<RTCStatsReport> {
    return new Map() as unknown as RTCStatsReport;
  }

  restartIce(): void {
    console.warn("[Elementium] restartIce: stub");
  }
}

/**
 * Install the WebRTC shim, replacing the global RTCPeerConnection.
 */
export function setupWebRtcShim(): void {
  (window as unknown as Record<string, unknown>).RTCPeerConnection = ElementiumRTCPeerConnection;
  (window as unknown as Record<string, unknown>).webkitRTCPeerConnection = ElementiumRTCPeerConnection;
  console.log("[Elementium] RTCPeerConnection shim installed");
}
