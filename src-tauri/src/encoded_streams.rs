//! Encoded remote video, streamed to the page for it to decode.
//!
//! ## Why this exists
//!
//! The established path decodes every remote frame in Rust and hands the page 1280x720
//! RGBA -- 3.7MB a frame. Measured on a real call it delivered about 14fps a track while the
//! backend sat idle at 97-123 media events a second with zero drops: the cost was entirely
//! in moving pixels. An encoded VP8 frame is 20-60kB, so forwarding the encoded frame
//! instead is roughly a hundredfold less data, and it moves decoding onto the webview's own
//! `GStreamer` pipeline rather than our software VP8 decoder.
//!
//! This webview reports `VideoDecoder` with `vp8` supported, which is the codec we
//! negotiate, so the page can do it.
//!
//! ## Why a stream and not the existing frame endpoint
//!
//! `/__elementium/frame/<id>` answers with the latest decoded frame, which is correct for
//! RGBA because every frame stands alone. Encoded frames do not: a VP8 interframe references
//! the one before it, so a reader that polls and misses frames feeds the decoder a broken
//! reference chain and gets nothing but errors -- the same frozen picture this project has
//! already spent a night on, arrived at from the other direction.
//!
//! So a track's frames go out in order, over one response that stays open, and the page
//! reads them off a `ReadableStream` rather than asking for each one.
//!
//! ## Backpressure
//!
//! The queue is bounded and the *oldest* frames are dropped when it fills, not the newest.
//! A consumer that has stalled wants to resume at the live edge, and dropping the newest
//! would hold it permanently behind. Drops are counted and reported, because a silently
//! shortened stream is indistinguishable from a sender that went quiet -- and because
//! dropping a frame breaks the reference chain until the next keyframe, which is a visible
//! consequence rather than an internal detail.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use elementium_types::PlaintextMedia;

/// How many encoded frames may wait for the page before the oldest are dropped.
///
/// Two seconds at 30fps. Long enough to absorb a stall from a busy main thread, short enough
/// that what the page eventually decodes is recent rather than a minute of history.
const QUEUE_LIMIT: usize = 60;

/// One encoded frame, as it goes on the wire to the page.
pub struct EncodedFrame {
    /// Decrypted, encoded bytes. Typed as [`PlaintextMedia`] all the way here so the frames
    /// cannot be the ciphertext ones by mistake -- the distinction this codebase has been
    /// bitten by before.
    pub data: PlaintextMedia,
    /// Whether this frame can be decoded without any earlier one.
    pub keyframe: bool,
    /// Microseconds since the track's first frame. `VideoDecoder` requires a timestamp per
    /// chunk and uses it to order output; it need only be monotonic, not wall-clock.
    pub timestamp_us: u64,
}

/// The queue of frames waiting for one track's reader.
struct TrackQueue {
    frames: std::collections::VecDeque<EncodedFrame>,
    /// Frames discarded because the reader was not keeping up.
    dropped: u64,
    /// Set when a reader is attached, so frames are not accumulated for nobody.
    subscribed: bool,
    /// Frames accepted for this reader, so an empty stream can be told apart from an
    /// unattached one. A reader that decodes nothing while this climbs is a decoder problem;
    /// one where this stays at zero never had any input, which is a routing problem.
    pushed: u64,
    /// When this reader's first frame arrived, so timestamps start near zero.
    ///
    /// `VideoDecoder` only requires timestamps to increase, but starting a fresh stream at
    /// some large elapsed value invites an overflow or a decoder that discards everything
    /// before its notion of the start.
    started: Option<std::time::Instant>,
}

/// Every track's encoded-frame queue, shared between the media path and the HTTP server.
#[derive(Clone)]
pub struct EncodedStreams(Arc<Mutex<HashMap<String, TrackQueue>>>);

impl Default for EncodedStreams {
    fn default() -> Self {
        Self::new()
    }
}

impl EncodedStreams {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Whether anything is reading this track, so the media path can skip the work entirely
    /// when nothing is.
    #[must_use]
    pub fn has_reader(&self, track_id: &str) -> bool {
        self.0
            .lock()
            .ok()
            .and_then(|queues| queues.get(track_id).map(|q| q.subscribed))
            .unwrap_or(false)
    }

    /// Attach a reader, discarding whatever a previous one left behind.
    ///
    /// A fresh reader must start at a keyframe, and the frames queued for a reader that has
    /// gone are by definition ones it never acknowledged -- keeping them would hand the new
    /// decoder a chain starting mid-way.
    pub fn subscribe(&self, track_id: &str) {
        if let Ok(mut queues) = self.0.lock() {
            let queue = queues.entry(track_id.to_owned()).or_insert_with(|| TrackQueue {
                frames: std::collections::VecDeque::new(),
                dropped: 0,
                subscribed: false,
                pushed: 0,
                started: None,
            });
            queue.frames.clear();
            queue.subscribed = true;
            queue.pushed = 0;
            queue.started = None;
        }
    }

    /// Detach the reader for a track, reporting how many frames it was given and how many
    /// were dropped before it could take them.
    pub fn unsubscribe(&self, track_id: &str) -> (u64, u64) {
        let mut counts = (0, 0);
        if let Ok(mut queues) = self.0.lock()
            && let Some(queue) = queues.get_mut(track_id)
        {
            queue.subscribed = false;
            queue.frames.clear();
            counts = (queue.pushed, queue.dropped);
            queue.pushed = 0;
            queue.dropped = 0;
        }
        counts
    }

    /// Offer a frame to whoever is reading this track. A no-op when nobody is.
    ///
    /// The timestamp is assigned here, from the arrival of the reader's first frame, because
    /// it must be continuous for the reader that will consume it -- not for whatever earlier
    /// reader happened to have this track open.
    pub fn push_encoded(&self, track_id: &str, data: PlaintextMedia) {
        let keyframe = is_vp8_keyframe(data.as_bytes());
        let now = std::time::Instant::now();
        let Ok(mut queues) = self.0.lock() else {
            return;
        };
        let Some(queue) = queues.get_mut(track_id) else {
            return;
        };
        if !queue.subscribed {
            return;
        }
        queue.pushed = queue.pushed.saturating_add(1);
        let started = *queue.started.get_or_insert(now);
        let timestamp_us = u64::try_from(now.duration_since(started).as_micros()).unwrap_or(0);
        Self::enqueue(
            queue,
            EncodedFrame {
                data,
                keyframe,
                timestamp_us,
            },
        );
    }

    /// Queue a ready-made frame. Split out so the eviction rule has one implementation and
    /// the tests can exercise it without inventing arrival times.
    fn enqueue(queue: &mut TrackQueue, frame: EncodedFrame) {
        while queue.frames.len() >= QUEUE_LIMIT {
            queue.frames.pop_front();
            queue.dropped = queue.dropped.saturating_add(1);
        }
        queue.frames.push_back(frame);
    }

    /// Queue a frame directly, for tests that need control over its timestamp.
    #[cfg(test)]
    fn push(&self, track_id: &str, frame: EncodedFrame) {
        if let Ok(mut queues) = self.0.lock()
            && let Some(queue) = queues.get_mut(track_id)
            && queue.subscribed
        {
            Self::enqueue(queue, frame);
        }
    }

    /// Take the next frame for a track, if one is waiting.
    #[must_use]
    pub fn pop(&self, track_id: &str) -> Option<EncodedFrame> {
        self.0
            .lock()
            .ok()
            .and_then(|mut queues| queues.get_mut(track_id).and_then(|q| q.frames.pop_front()))
    }

    /// How many frames this track's reader has missed so far.
    #[must_use]
    #[cfg(test)]
    fn dropped(&self, track_id: &str) -> u64 {
        self.0
            .lock()
            .ok()
            .and_then(|queues| queues.get(track_id).map(|q| q.dropped))
            .unwrap_or(0)
    }
}

/// Whether a VP8 frame is a keyframe.
///
/// Bit 0 of the frame tag is the frame type, and it is inverted from the obvious reading:
/// `0` means key frame, `1` means interframe (RFC 6386 section 9.1). The same bit is read by
/// the E2EE framing to decide how many bytes stay in the clear, and getting it backwards
/// there produced frames only the far end could tell were wrong.
#[must_use]
pub fn is_vp8_keyframe(frame: &[u8]) -> bool {
    frame.first().is_some_and(|b| b & 0x01 == 0)
}

/// Serialise one frame for the wire: `[u32 len][u8 keyframe][u64 timestamp_us][bytes]`.
///
/// Length-prefixed because the transport is a byte stream with no record boundaries of its
/// own, and the reader must be able to find the start of the next frame without scanning for
/// a marker that could occur inside compressed video.
#[must_use]
pub fn encode_wire_frame(frame: &EncodedFrame) -> Vec<u8> {
    let payload = frame.data.as_bytes();
    let mut out = Vec::with_capacity(payload.len().saturating_add(13));
    out.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.push(u8::from(frame.keyframe));
    out.extend_from_slice(&frame.timestamp_us.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::{EncodedFrame, EncodedStreams, encode_wire_frame, is_vp8_keyframe};
    use elementium_types::PlaintextMedia;

    fn frame(byte: u8, timestamp_us: u64) -> EncodedFrame {
        EncodedFrame {
            data: PlaintextMedia::from_decrypted(vec![byte; 4]),
            keyframe: byte == 0,
            timestamp_us,
        }
    }

    /// Bit 0 of the VP8 frame tag is inverted from the obvious reading: 0 is the key frame.
    #[test]
    fn a_vp8_keyframe_is_the_one_with_the_bit_clear() {
        assert!(is_vp8_keyframe(&[0x00, 0x01, 0x02]));
        assert!(!is_vp8_keyframe(&[0x01, 0x02, 0x03]));
        // An empty frame is not a keyframe, and asking must not panic.
        assert!(!is_vp8_keyframe(&[]));
    }

    /// Frames offered with nobody reading must not accumulate: a call with no page-side
    /// decoder attached would otherwise grow a queue per remote track forever.
    #[test]
    fn frames_are_discarded_when_nothing_is_reading() {
        let streams = EncodedStreams::new();
        streams.push("t", frame(1, 0));
        assert!(streams.pop("t").is_none());
        assert!(!streams.has_reader("t"));
    }

    #[test]
    fn frames_arrive_in_order_once_a_reader_is_attached() {
        let streams = EncodedStreams::new();
        streams.subscribe("t");
        streams.push("t", frame(0, 100));
        streams.push("t", frame(1, 200));

        assert_eq!(streams.pop("t").map(|f| f.timestamp_us), Some(100));
        assert_eq!(streams.pop("t").map(|f| f.timestamp_us), Some(200));
        assert!(streams.pop("t").is_none());
    }

    /// The *oldest* frames go when the queue fills. A stalled reader wants to resume at the
    /// live edge; dropping the newest would hold it permanently behind.
    #[test]
    fn a_full_queue_drops_the_oldest_and_counts_it() {
        let streams = EncodedStreams::new();
        streams.subscribe("t");
        for i in 0..70u64 {
            streams.push("t", frame(1, i));
        }
        assert_eq!(streams.dropped("t"), 10);
        // The survivor at the head is the eleventh pushed, not the first.
        assert_eq!(streams.pop("t").map(|f| f.timestamp_us), Some(10));
    }

    /// A new reader must not inherit a chain that starts mid-way.
    #[test]
    fn subscribing_again_discards_what_the_previous_reader_left() {
        let streams = EncodedStreams::new();
        streams.subscribe("t");
        streams.push("t", frame(1, 1));
        streams.subscribe("t");
        assert!(streams.pop("t").is_none());
    }

    #[test]
    fn unsubscribing_reports_the_drops_and_stops_accepting_frames() {
        let streams = EncodedStreams::new();
        streams.subscribe("t");
        for i in 0..70u64 {
            streams.push("t", frame(1, i));
        }
        assert_eq!(streams.unsubscribe("t"), (0, 10));
        streams.push("t", frame(1, 999));
        assert!(streams.pop("t").is_none());
    }

    #[test]
    #[allow(clippy::indexing_slicing)]
    fn the_wire_frame_carries_length_flag_timestamp_then_payload() {
        let wire = encode_wire_frame(&EncodedFrame {
            data: PlaintextMedia::from_decrypted(vec![0xAA, 0xBB]),
            keyframe: true,
            timestamp_us: 0x0102,
        });
        assert_eq!(&wire[0..4], &2u32.to_le_bytes());
        assert_eq!(wire[4], 1);
        assert_eq!(&wire[5..13], &0x0102u64.to_le_bytes());
        assert_eq!(&wire[13..], &[0xAA, 0xBB]);
    }
}
