/**
 * LiveKit bridge — intercepts Element Call's `livekit-client` usage
 * and routes everything to the native Rust LiveKit backend.
 *
 * This module exports a shim `Room` class (and supporting types) that
 * mirrors the `livekit-client` API surface used by Element Call.
 * It is installed as a Vite alias so `import { Room } from "livekit-client"`
 * resolves to this module.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import { fetchFrame } from "../renderer/frame-fetcher";
import { createCanvasTrack } from "./canvas-track";

/**
 * Fixed geometry for canvas-backed remote video.
 *
 * `captureStream` binds a track's frame size when it is called, so this cannot follow the
 * incoming frames; frames of other sizes are letterboxed into it instead.
 */
const RENDER_WIDTH = 1280;
const RENDER_HEIGHT = 720;

// ─── Event types matching livekit-client ───

export enum RoomEvent {
  ParticipantConnected = "participantConnected",
  ParticipantDisconnected = "participantDisconnected",
  TrackSubscribed = "trackSubscribed",
  TrackUnsubscribed = "trackUnsubscribed",
  ConnectionStateChanged = "connectionStateChanged",
  ActiveSpeakersChanged = "activeSpeakersChanged",
  Disconnected = "disconnected",
  Connected = "connected",
}

export enum ConnectionState {
  Disconnected = "disconnected",
  Connecting = "connecting",
  Connected = "connected",
  Reconnecting = "reconnecting",
}

export enum TrackKind {
  Audio = "audio",
  Video = "video",
}

export enum TrackSource {
  Camera = "camera",
  Microphone = "microphone",
  ScreenShare = "screen_share",
  ScreenShareAudio = "screen_share_audio",
  Unknown = "unknown",
}

// ─── Track classes ───

export class Track {
  sid: string;
  kind: TrackKind;
  source: TrackSource;
  mediaStreamTrack: MediaStreamTrack | null = null;

  constructor(sid: string, kind: TrackKind, source: TrackSource = TrackSource.Unknown) {
    this.sid = sid;
    this.kind = kind;
    this.source = source;
  }
}

export class RemoteTrack extends Track {
  private _canvas: HTMLCanvasElement | null = null;
  private _scratch: HTMLCanvasElement | null = null;
  private _present: () => void = () => {};
  private _rendering = false;
  private _timerId: ReturnType<typeof setTimeout> | null = null;

  /** Attach video rendering to an element. */
  attach(element?: HTMLMediaElement): HTMLMediaElement {
    const el = element || document.createElement("video");

    if (this.kind === TrackKind.Video) {
      // Canvas-backed rendering, fed by the native decoder.
      //
      // The geometry is fixed here and never changed afterwards: `captureStream` binds
      // the track's frame size at the moment it is called, and resizing the canvas after
      // that leaves the track describing one size while the backing store holds another.
      // Frames are then read at the wrong stride, which tears the picture into horizontal
      // bands with rotated colour channels. Incoming frames of a different size are
      // scaled into this one instead -- see `_renderLoop`.
      this._canvas = document.createElement("canvas");
      this._canvas.width = RENDER_WIDTH;
      this._canvas.height = RENDER_HEIGHT;
      this._rendering = true;
      this._renderLoop();

      // Manually driven so a frame is only emitted once a draw has completed; the render
      // loop calls `present` -- see `createCanvasTrack`.
      const canvasTrack = createCanvasTrack(this._canvas);
      this._present = canvasTrack.present;
      el.srcObject = canvasTrack.stream;
      el.autoplay = true;
      (el as HTMLVideoElement).playsInline = true;
    }

    return el;
  }

  /** Detach video rendering. */
  detach(element?: HTMLMediaElement): HTMLMediaElement[] {
    this._rendering = false;
    if (this._timerId !== null) {
      clearTimeout(this._timerId);
      this._timerId = null;
    }
    return element ? [element] : [];
  }

  private async _renderLoop(): Promise<void> {
    if (!this._rendering || !this._canvas) return;

    const frame = await fetchFrame(this.sid);
    if (frame && this._canvas) {
      const ctx = this._canvas.getContext("2d");
      if (ctx) {
        // Copied out of the IPC buffer rather than viewed in place: the view starts at a
        // byte offset, and ImageData is specified over the whole of the array it is
        // given.
        const buf = new ArrayBuffer(frame.rgba.byteLength);
        new Uint8Array(buf).set(new Uint8Array(frame.rgba.buffer, frame.rgba.byteOffset, frame.rgba.byteLength));
        const imageData = new ImageData(new Uint8ClampedArray(buf), frame.width, frame.height);

        if (frame.width === this._canvas.width && frame.height === this._canvas.height) {
          ctx.putImageData(imageData, 0, 0);
        } else {
          // Scale through a scratch canvas, preserving aspect. Resizing this canvas is
          // not an option -- see `attach`.
          if (!this._scratch) this._scratch = document.createElement("canvas");
          if (this._scratch.width !== frame.width || this._scratch.height !== frame.height) {
            this._scratch.width = frame.width;
            this._scratch.height = frame.height;
          }
          const sctx = this._scratch.getContext("2d");
          if (sctx) {
            sctx.putImageData(imageData, 0, 0);
            const scale = Math.min(
              this._canvas.width / frame.width,
              this._canvas.height / frame.height,
            );
            const drawW = Math.round(frame.width * scale);
            const drawH = Math.round(frame.height * scale);
            ctx.fillStyle = "#000";
            ctx.fillRect(0, 0, this._canvas.width, this._canvas.height);
            ctx.drawImage(
              this._scratch,
              Math.round((this._canvas.width - drawW) / 2),
              Math.round((this._canvas.height - drawH) / 2),
              drawW,
              drawH,
            );
          }
        }
        // Draw complete: publish it as one frame.
        this._present();
      }
    }

    this._timerId = setTimeout(() => this._renderLoop(), 33);
  }
}

export class LocalTrack extends Track {
  async mute(): Promise<void> {
    // TODO: Mute track via Tauri command
  }

  async unmute(): Promise<void> {
    // TODO: Unmute track via Tauri command
  }
}

// ─── Participant classes ───

export class Participant {
  sid: string;
  identity: string;
  name: string;
  audioTracks = new Map<string, RemoteTrack>();
  videoTracks = new Map<string, RemoteTrack>();

  constructor(sid: string, identity: string, name: string = "") {
    this.sid = sid;
    this.identity = identity;
    this.name = name;
  }

  getTrackPublications(): Map<string, RemoteTrack> {
    const all = new Map<string, RemoteTrack>();
    for (const [k, v] of this.audioTracks) all.set(k, v);
    for (const [k, v] of this.videoTracks) all.set(k, v);
    return all;
  }
}

export class RemoteParticipant extends Participant {}

export class LocalParticipant extends Participant {
  private _roomId: string;

  constructor(roomId: string, sid: string, identity: string, name: string = "") {
    super(sid, identity, name);
    this._roomId = roomId;
  }

  /** Publish a local audio track (microphone). */
  async publishTrack(
    _track: MediaStreamTrack | LocalTrack,
    options?: { source?: string },
  ): Promise<void> {
    const kind = _track instanceof LocalTrack ? _track.kind : _track.kind as string;
    const source = options?.source || (_track instanceof LocalTrack ? _track.source : "microphone");

    await invoke("livekit_publish_track", {
      roomId: this._roomId,
      kind,
      source,
    });
  }

  /** Unpublish a local track. */
  async unpublishTrack(_track: MediaStreamTrack | LocalTrack): Promise<void> {
    // TODO: invoke livekit_unpublish_track
  }
}

// ─── Simple EventEmitter ───

type EventHandler = (...args: unknown[]) => void;

class EventEmitter {
  private _handlers: Map<string, Set<EventHandler>> = new Map();

  on(event: string, handler: EventHandler): this {
    if (!this._handlers.has(event)) {
      this._handlers.set(event, new Set());
    }
    this._handlers.get(event)!.add(handler);
    return this;
  }

  off(event: string, handler: EventHandler): this {
    this._handlers.get(event)?.delete(handler);
    return this;
  }

  protected emit(event: string, ...args: unknown[]): void {
    const handlers = this._handlers.get(event);
    if (handlers) {
      for (const h of handlers) {
        try {
          h(...args);
        } catch (e) {
          console.error(`[Elementium] Error in ${event} handler:`, e);
        }
      }
    }
  }

  removeAllListeners(): void {
    this._handlers.clear();
  }
}

// ─── Room class — main livekit-client shim ───

interface ConnectResult {
  roomId: string;
  roomName: string;
  localIdentity: string;
}

export class Room extends EventEmitter {
  private _roomId: string | null = null;
  private _unlisteners: UnlistenFn[] = [];
  private _state: ConnectionState = ConnectionState.Disconnected;
  private _participants = new Map<string, RemoteParticipant>();
  private _localParticipant: LocalParticipant | null = null;
  name: string = "";

  get state(): ConnectionState {
    return this._state;
  }

  get localParticipant(): LocalParticipant | null {
    return this._localParticipant;
  }

  get remoteParticipants(): Map<string, RemoteParticipant> {
    return this._participants;
  }

  /** Connect to a LiveKit room via the native backend. */
  async connect(url: string, token: string): Promise<void> {
    console.log(`[Elementium] Connecting to LiveKit SFU: ${url}`);
    this._state = ConnectionState.Connecting;

    try {
      const result = await invoke<ConnectResult>("livekit_connect", {
        sfuUrl: url,
        token,
      });

      this._roomId = result.roomId;
      this.name = result.roomName;
      this._localParticipant = new LocalParticipant(
        result.roomId,
        "", // sid filled in later if needed
        result.localIdentity,
      );
      this._state = ConnectionState.Connected;

      await this._setupListeners();
      this.emit(RoomEvent.Connected);
    } catch (e) {
      this._state = ConnectionState.Disconnected;
      throw e;
    }
  }

  /** Disconnect from the room. */
  async disconnect(): Promise<void> {
    if (!this._roomId) return;

    console.log(`[Elementium] Disconnecting from LiveKit room: ${this._roomId}`);

    try {
      await invoke("livekit_disconnect", { roomId: this._roomId });
    } catch (e) {
      console.error("[Elementium] Disconnect error:", e);
    }

    this._cleanup();
    this._state = ConnectionState.Disconnected;
    this.emit(RoomEvent.Disconnected);
  }

  private async _setupListeners(): Promise<void> {
    this._unlisteners.push(
      await listen<{ roomId: string; identity: string; sid: string; name: string }>(
        "livekit-participant-joined",
        (event) => {
          if (event.payload.roomId !== this._roomId) return;
          const p = new RemoteParticipant(
            event.payload.sid,
            event.payload.identity,
            event.payload.name,
          );
          this._participants.set(event.payload.sid, p);
          this.emit(RoomEvent.ParticipantConnected, p);
        },
      ),
    );

    this._unlisteners.push(
      await listen<{ roomId: string; identity: string; sid: string }>(
        "livekit-participant-left",
        (event) => {
          if (event.payload.roomId !== this._roomId) return;
          const p = this._participants.get(event.payload.sid);
          if (p) {
            this._participants.delete(event.payload.sid);
            this.emit(RoomEvent.ParticipantDisconnected, p);
          }
        },
      ),
    );

    this._unlisteners.push(
      await listen<{
        roomId: string;
        participantSid: string;
        trackSid: string;
        kind: string;
      }>(
        "livekit-track-subscribed",
        (event) => {
          if (event.payload.roomId !== this._roomId) return;
          const participant = this._participants.get(event.payload.participantSid);
          const kind =
            event.payload.kind === "audio" ? TrackKind.Audio : TrackKind.Video;
          const track = new RemoteTrack(event.payload.trackSid, kind);

          if (participant) {
            if (kind === TrackKind.Audio) {
              participant.audioTracks.set(track.sid, track);
            } else {
              participant.videoTracks.set(track.sid, track);
            }
          }

          // Element Call expects: (track, publication, participant)
          this.emit(RoomEvent.TrackSubscribed, track, null, participant);
        },
      ),
    );

    this._unlisteners.push(
      await listen<{
        roomId: string;
        participantSid: string;
        trackSid: string;
      }>(
        "livekit-track-unsubscribed",
        (event) => {
          if (event.payload.roomId !== this._roomId) return;
          const participant = this._participants.get(event.payload.participantSid);
          const track =
            participant?.audioTracks.get(event.payload.trackSid) ||
            participant?.videoTracks.get(event.payload.trackSid);

          if (track) {
            participant?.audioTracks.delete(event.payload.trackSid);
            participant?.videoTracks.delete(event.payload.trackSid);
            this.emit(RoomEvent.TrackUnsubscribed, track, null, participant);
          }
        },
      ),
    );

    this._unlisteners.push(
      await listen<{ roomId: string; state: string }>(
        "livekit-connection-state",
        (event) => {
          if (event.payload.roomId !== this._roomId) return;
          this._state = event.payload.state as ConnectionState;
          this.emit(RoomEvent.ConnectionStateChanged, this._state);

          if (this._state === ConnectionState.Disconnected) {
            this.emit(RoomEvent.Disconnected);
          }
        },
      ),
    );

    this._unlisteners.push(
      await listen<{ roomId: string; speakers: string[] }>(
        "livekit-active-speakers",
        (event) => {
          if (event.payload.roomId !== this._roomId) return;
          const speakers = event.payload.speakers
            .map((sid) => this._participants.get(sid))
            .filter((p): p is RemoteParticipant => p !== undefined);
          this.emit(RoomEvent.ActiveSpeakersChanged, speakers);
        },
      ),
    );
  }

  private _cleanup(): void {
    for (const unlisten of this._unlisteners) {
      unlisten();
    }
    this._unlisteners = [];
    this._participants.clear();
    this._roomId = null;
    this._localParticipant = null;
    this.removeAllListeners();
  }
}

// ─── Re-exports matching livekit-client API ───

export default {
  Room,
  RoomEvent,
  ConnectionState,
  Track,
  RemoteTrack,
  LocalTrack,
  Participant,
  RemoteParticipant,
  LocalParticipant,
  TrackKind,
  TrackSource,
};
