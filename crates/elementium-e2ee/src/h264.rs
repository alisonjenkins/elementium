//! H.264 frame framing for `LiveKit`'s end-to-end encryption.
//!
//! VP8 tells you how much of a frame stays in the clear by looking at one byte. H.264 does
//! not, and the difference is the whole reason this module exists: the clear header ends two
//! bytes into the first *slice* NAL unit, wherever that happens to be, and the ciphertext
//! that follows has to be escaped so it cannot be mistaken for a start code.
//!
//! Everything here follows livekit-client 2.21.0 exactly -- `e2ee/worker/naluUtils.ts` and
//! `e2ee/utils.ts` -- because the far end is livekit's own worker and a difference of one
//! byte is a frame nobody can decrypt. Where livekit's implementation has a quirk, this has
//! the same quirk on purpose; the places that matter are commented.
//!
//! Nothing here is about H.264 in general. It is about agreeing with one specific peer.

/// The low five bits of a NAL header byte carry the unit's type.
const NALU_TYPE_MASK: u8 = 0x1f;
/// Coded slice of a non-IDR picture.
const SLICE_NON_IDR: u8 = 1;
/// Coded slice of an IDR picture.
const SLICE_IDR: u8 = 5;

/// Zeros needed before a byte has to be escaped.
const ZEROS_IN_START_SEQUENCE: usize = 2;
/// The byte inserted to break up an accidental start code.
const EMULATION_BYTE: u8 = 3;

/// How many bytes at the front of an H.264 frame `LiveKit` leaves unencrypted.
///
/// Two bytes past the start of the first slice NAL unit -- its header byte plus one. Slice
/// data partitions and every non-slice unit (SPS, PPS, SEI) are skipped over, so on a
/// keyframe the parameter sets travel in the clear along with everything before the slice.
///
/// Returns `None` when the frame holds no slice NAL unit, or does not begin with a start
/// code. livekit throws in both cases and its caller falls back to the VP8 sizes; `None`
/// says the same thing without deciding what the caller should do about it.
#[must_use]
pub fn unencrypted_bytes(frame: &[u8]) -> Option<usize> {
    for index in nalu_indices(frame)? {
        let nalu_type = frame.get(index)? & NALU_TYPE_MASK;
        if nalu_type == SLICE_IDR || nalu_type == SLICE_NON_IDR {
            return index
                .checked_add(2)?
                .checked_add(extra_clear_bytes())
                .filter(|end| *end <= frame.len());
        }
    }
    None
}

/// The most extra clear bytes `ELEMENTIUM_H264_CLEAR_EXTRA` may ask for.
///
/// The knob exists to find where a receiver stops reading the slice header, not to hand a
/// diagnostic run an unbounded amount of plaintext. Nothing upstream capped it before; this
/// is a guess at "far more than a slice header could plausibly need", not a measured limit.
const MAX_CLEAR_EXTRA_BYTES: usize = 64;

/// The outcome of reading `ELEMENTIUM_H264_CLEAR_EXTRA`: the value to use, and a line to
/// log once if the environment needs explaining -- unset or valid-and-zero warrant nothing.
struct ClearExtraOutcome {
    bytes: usize,
    warning: Option<String>,
}

/// Parse and bound `ELEMENTIUM_H264_CLEAR_EXTRA`'s raw value.
///
/// Split out from the process-environment read so the parsing and bounding can be tested
/// directly: mutating `std::env` from a test races every other test in the same binary, and
/// this is the part actually worth testing.
///
/// A value that fails to parse is reported and ignored (falls back to zero) rather than
/// silently defaulted, so a typo is visible instead of just quietly encrypting normally. A
/// value that parses but exceeds [`MAX_CLEAR_EXTRA_BYTES`] is capped there and reported,
/// rather than rejected outright, so an overshoot still says something about where the
/// receiver stops being confused.
fn parse_extra_clear_bytes(raw: Option<&str>) -> ClearExtraOutcome {
    let no_change = || ClearExtraOutcome {
        bytes: 0,
        warning: None,
    };
    let Some(raw) = raw else {
        return no_change();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return no_change();
    }
    match trimmed.parse::<usize>() {
        Ok(0) => no_change(),
        Ok(requested) if requested <= MAX_CLEAR_EXTRA_BYTES => ClearExtraOutcome {
            bytes: requested,
            warning: Some(format!(
                "ELEMENTIUM_H264_CLEAR_EXTRA={requested} is widening every outgoing H.264 \
                 frame's plaintext header by {requested} bytes beyond livekit's framing; a \
                 real livekit peer will not be able to decrypt this stream"
            )),
        },
        Ok(requested) => ClearExtraOutcome {
            bytes: MAX_CLEAR_EXTRA_BYTES,
            warning: Some(format!(
                "ELEMENTIUM_H264_CLEAR_EXTRA={requested} exceeds the {MAX_CLEAR_EXTRA_BYTES}-\
                 byte cap; using {MAX_CLEAR_EXTRA_BYTES} instead of the requested value"
            )),
        },
        Err(_) => ClearExtraOutcome {
            bytes: 0,
            warning: Some(format!(
                "ELEMENTIUM_H264_CLEAR_EXTRA={trimmed:?} is not a byte count; ignoring it and \
                 leaving the clear header at its normal size"
            )),
        },
    }
}

/// Extra slice bytes to leave in the clear, for diagnosis only.
///
/// Chromium assembles no frames at all from our encrypted H.264 -- not even with its
/// decryption transform removed, so the packets themselves are what it will not accept.
/// The one thing those packets have that a working plain stream does not is a slice whose
/// bytes are opaque after the second. Widening the clear header says whether the receiver
/// is reading further into the slice header than the two bytes livekit leaves it.
///
/// Never set in normal operation: any value but zero puts plaintext on the wire and makes
/// us disagree with livekit's own framing, so a peer could not decrypt us. Read once and
/// cached rather than parsed on every frame -- this sits on the per-frame hot path, and an
/// environment lookup has no business there when the answer cannot change mid-process.
fn extra_clear_bytes() -> usize {
    static RESOLVED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        let raw = std::env::var("ELEMENTIUM_H264_CLEAR_EXTRA").ok();
        let outcome = parse_extra_clear_bytes(raw.as_deref());
        if let Some(warning) = outcome.warning {
            // Said once, because get_or_init runs the closure at most once per process --
            // a machine running with this set should say so once, loudly, not stay quiet.
            tracing::warn!("{warning}");
        }
        outcome.bytes
    })
}

/// Offsets of every NAL header byte in an Annex B stream.
///
/// Returns `None` if the stream does not start with a start code, which is livekit's
/// "byte stream contains leading data".
fn nalu_indices(stream: &[u8]) -> Option<Vec<usize>> {
    // A transliteration of livekit's `findNALUIndices`, quirks included, because the far end
    // is that function and agreeing with it is the entire requirement.
    //
    // The quirk that matters: the scan stops three bytes from the end, and the outer loop
    // re-tests that bound *before* recording the NAL unit it just walked past. So the last
    // unit in a frame is dropped whenever its start code falls inside that final window. A
    // differential run against livekit's own code found this -- the obvious reading, which
    // records it, disagreed on 16 of 596 frames.
    let search_len = stream.len().saturating_sub(3);

    let mut indices = Vec::new();
    let mut pos = 0usize;
    let mut start = 0usize;

    while pos < search_len {
        while pos < search_len && start_code_len(stream, pos).is_none() {
            pos = pos.saturating_add(1);
        }
        if pos >= search_len {
            pos = stream.len();
        }

        if start == 0 {
            // Trailing zeros before the first start code do not count as leading data.
            let mut end = pos;
            while end > start && stream.get(end.saturating_sub(1)) == Some(&0) {
                end = end.saturating_sub(1);
            }
            // The stream must open with a start code; anything before it is data livekit
            // refuses to interpret, and guessing at an offset would corrupt the frame.
            if end != start {
                return None;
            }
        } else {
            indices.push(start);
        }

        let code = if pos < search_len {
            start_code_len(stream, pos).unwrap_or(3)
        } else {
            3
        };
        pos = pos.saturating_add(code);
        start = pos;
    }
    Some(indices)
}

/// Length of the start code at `pos`, if one begins there.
fn start_code_len(stream: &[u8], pos: usize) -> Option<usize> {
    let rest = stream.get(pos..)?;
    match rest {
        [0, 0, 0, 1, ..] => Some(4),
        [0, 0, 1, ..] => Some(3),
        _ => None,
    }
}

/// Insert emulation-prevention bytes, as livekit's `writeRbsp` does.
///
/// Ciphertext is uniformly random, so a `00 00 00` or `00 00 01` sequence turns up by
/// chance every few thousand frames. Anything parsing the stream would read it as the start
/// of a new NAL unit. Escaping is therefore not an optimisation or a nicety: without it a
/// small fraction of frames corrupt, which is the hardest failure rate there is to find.
#[must_use]
pub fn write_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &byte in data {
        if byte <= EMULATION_BYTE && zeros >= ZEROS_IN_START_SEQUENCE {
            out.push(EMULATION_BYTE);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 {
            zeros.saturating_add(1)
        } else {
            0
        };
    }
    out
}

/// Whether `data` contains a `00 00 03` sequence, and so may have been escaped.
///
/// livekit checks this before unescaping rather than tracking whether it escaped, so a
/// frame from a sender that did not escape is left alone.
///
/// Its loop stops at `length - 3`, so a `00 00 03` sequence occupying the last three bytes
/// is not seen. Matched deliberately: disagreeing means one side unescapes a frame the other
/// did not escape, which is worse than either behaviour on its own.
#[must_use]
pub fn needs_rbsp_unescaping(data: &[u8]) -> bool {
    let limit = data.len().saturating_sub(3);
    data.windows(3)
        .take(limit)
        .any(|w| w == [0, 0, EMULATION_BYTE])
}

/// Remove emulation-prevention bytes, as livekit's `parseRbsp` does.
#[must_use]
pub fn parse_rbsp(stream: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(stream.len());
    let mut i = 0usize;
    while let Some(&byte) = stream.get(i) {
        out.push(byte);
        i = i.saturating_add(1);
        let escaped = byte == 0
            && matches!(
                (stream.get(i), stream.get(i.saturating_add(1))),
                (Some(&0), Some(&EMULATION_BYTE))
            );
        if escaped {
            out.push(0);
            // Skip the zero we just wrote and the emulation byte after it.
            i = i.saturating_add(2);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// An Annex B frame: SPS, PPS, then an IDR slice.
    fn keyframe() -> Vec<u8> {
        let mut f = vec![0, 0, 0, 1, 0x67, 0xaa, 0xbb]; // SPS (type 7)
        f.extend_from_slice(&[0, 0, 1, 0x68, 0xcc]); // PPS (type 8)
        f.extend_from_slice(&[0, 0, 1, 0x65, 0x11, 0x22, 0x33]); // IDR slice (type 5)
        f
    }

    #[test]
    fn clear_header_ends_two_bytes_into_the_first_slice() {
        let frame = keyframe();
        // SPS header at 4, PPS at 10, IDR at 15. Two bytes past the IDR header is 17, so the
        // parameter sets travel in the clear and the slice payload does not.
        assert_eq!(frame[15] & NALU_TYPE_MASK, SLICE_IDR);
        assert_eq!(unencrypted_bytes(&frame), Some(17));
    }

    #[test]
    fn parameter_sets_are_not_mistaken_for_slices() {
        // Only SPS and PPS: nothing to encrypt against, so nothing to report.
        let frame = vec![0, 0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb, 0xcc, 0xdd];
        assert_eq!(unencrypted_bytes(&frame), None);
    }

    #[test]
    fn a_non_idr_slice_counts_too() {
        let frame = vec![0, 0, 0, 1, 0x41, 0x11, 0x22, 0x33];
        assert_eq!(frame[4] & NALU_TYPE_MASK, SLICE_NON_IDR);
        assert_eq!(unencrypted_bytes(&frame), Some(6));
    }

    #[test]
    fn leading_data_is_refused_rather_than_guessed_at() {
        // livekit throws here; a frame that does not open with a start code is not a stream
        // this code understands, and picking an offset anyway would corrupt it silently.
        let frame = vec![0xde, 0xad, 0, 0, 0, 1, 0x65, 0x11, 0x22];
        assert_eq!(unencrypted_bytes(&frame), None);
    }

    /// Both of these are livekit bugs, and both must be reproduced exactly.
    ///
    /// They were found by running 596 frames through livekit's own code and this crate's and
    /// comparing, not by reading. The obvious implementation disagrees on 16 frames, and a
    /// disagreement here is a frame the peer slices in a different place and cannot
    /// authenticate.
    mod quirks_of_the_reference_implementation {
        use super::{SLICE_IDR, needs_rbsp_unescaping, unencrypted_bytes};

        #[test]
        fn the_last_nal_unit_is_dropped_when_it_starts_near_the_end() {
            // An access unit delimiter, then an IDR slice whose start code falls inside the
            // last three bytes of the scan. livekit never records that slice, so the frame
            // has no slice to find and falls back to the VP8 sizes.
            let frame = vec![
                0, 0, 0, 1, 0x69, 0x31, 0xe5, 0xfe, 0, 0, 1, 0x65, 0xc5, 0x62,
            ];
            assert_eq!(frame[11] & 0x1f, SLICE_IDR, "there really is a slice there");
            assert_eq!(unencrypted_bytes(&frame), None);
        }

        #[test]
        fn an_escape_in_the_last_three_bytes_is_not_seen() {
            assert!(!needs_rbsp_unescaping(&[0, 0, 0, 3]));
            assert!(!needs_rbsp_unescaping(&[0x8d, 0, 0, 3]));
            // One byte further from the end and it is seen.
            assert!(needs_rbsp_unescaping(&[0x8d, 0, 0, 3, 0x11]));
        }
    }

    #[test]
    fn escaping_breaks_up_accidental_start_codes() {
        assert_eq!(write_rbsp(&[0, 0, 0]), vec![0, 0, 3, 0]);
        assert_eq!(write_rbsp(&[0, 0, 1]), vec![0, 0, 3, 1]);
        assert_eq!(write_rbsp(&[0, 0, 3]), vec![0, 0, 3, 3]);
        // Four or more values above the emulation byte need nothing done to them.
        assert_eq!(write_rbsp(&[0, 0, 4]), vec![0, 0, 4]);
    }

    #[test]
    fn escaping_round_trips() {
        for data in [
            vec![0, 0, 0, 0, 0],
            vec![0, 0, 1, 0, 0, 2, 0, 0, 3],
            vec![0xff; 8],
            vec![],
            vec![0],
        ] {
            let escaped = write_rbsp(&data);
            assert_eq!(parse_rbsp(&escaped), data, "round trip failed for {data:?}");
        }
    }

    #[test]
    fn unescaping_is_only_attempted_when_there_is_something_to_undo() {
        assert!(!needs_rbsp_unescaping(&[0, 0, 1, 0, 0, 2]));
        assert!(needs_rbsp_unescaping(&[0xaa, 0, 0, 3, 0xbb]));
        assert!(!needs_rbsp_unescaping(&[0, 3]));
    }

    /// Random-looking payloads are the case that matters: escaping exists because ciphertext
    /// produces start-code sequences by accident, so the round trip has to hold for data
    /// nobody chose.
    #[test]
    fn escaping_round_trips_over_pseudo_random_payloads() {
        let mut state: u32 = 0x1234_5678;
        for _ in 0..200 {
            let payload: Vec<u8> = (0..64)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    // Biased hard towards zero, so start-code sequences actually occur.
                    u8::try_from((state >> 24) % 4).unwrap_or(0)
                })
                .collect();
            let escaped = write_rbsp(&payload);
            assert!(!contains_start_code(&escaped), "escaping left a start code");
            assert_eq!(parse_rbsp(&escaped), payload);
        }
    }

    fn contains_start_code(data: &[u8]) -> bool {
        data.windows(3).any(|w| w == [0, 0, 0] || w == [0, 0, 1])
    }

    /// [`parse_extra_clear_bytes`] rather than [`extra_clear_bytes`] itself: the latter reads
    /// `std::env`, and mutating process environment from a test races every other test in
    /// this binary under a parallel runner.
    mod clear_extra_parsing {
        use super::{ClearExtraOutcome, MAX_CLEAR_EXTRA_BYTES, parse_extra_clear_bytes};

        #[test]
        fn unset_is_zero_and_quiet() {
            let ClearExtraOutcome { bytes, warning } = parse_extra_clear_bytes(None);
            assert_eq!(bytes, 0);
            assert!(warning.is_none());
        }

        #[test]
        fn empty_string_is_zero_and_quiet() {
            // An exported-but-empty variable, e.g. `FOO= cmd`, should behave like unset.
            let ClearExtraOutcome { bytes, warning } = parse_extra_clear_bytes(Some(""));
            assert_eq!(bytes, 0);
            assert!(warning.is_none());
        }

        #[test]
        fn explicit_zero_is_zero_and_quiet() {
            // Zero changes nothing, so it does not warrant the "plaintext is wider" warning.
            let ClearExtraOutcome { bytes, warning } = parse_extra_clear_bytes(Some("0"));
            assert_eq!(bytes, 0);
            assert!(warning.is_none());
        }

        #[test]
        fn an_in_range_value_is_used_and_warned_about() {
            let ClearExtraOutcome { bytes, warning } = parse_extra_clear_bytes(Some("5"));
            assert_eq!(bytes, 5);
            // A nonzero value must warn; module-level `unwrap_used` allow covers this.
            let warning = warning.unwrap();
            assert!(warning.contains("ELEMENTIUM_H264_CLEAR_EXTRA=5"));
            assert!(warning.contains('5'));
        }

        #[test]
        fn the_upper_bound_itself_is_accepted_without_capping() {
            let expected = format!("{MAX_CLEAR_EXTRA_BYTES}");
            let ClearExtraOutcome { bytes, warning } =
                parse_extra_clear_bytes(Some(expected.as_str()));
            assert_eq!(bytes, MAX_CLEAR_EXTRA_BYTES);
            assert!(warning.is_some());
        }

        #[test]
        fn an_oversized_value_is_capped_and_reports_the_cap() {
            let requested = MAX_CLEAR_EXTRA_BYTES.saturating_add(1000);
            let ClearExtraOutcome { bytes, warning } =
                parse_extra_clear_bytes(Some(&requested.to_string()));
            assert_eq!(bytes, MAX_CLEAR_EXTRA_BYTES);
            // An oversized value must warn; module-level `unwrap_used` allow covers this.
            let warning = warning.unwrap();
            assert!(warning.contains(&requested.to_string()));
            assert!(warning.contains(&MAX_CLEAR_EXTRA_BYTES.to_string()));
        }

        #[test]
        fn unparsable_values_are_ignored_and_reported() {
            for junk in ["not-a-number", "-1", "3.5", "0x10"] {
                let ClearExtraOutcome { bytes, warning } = parse_extra_clear_bytes(Some(junk));
                assert_eq!(bytes, 0, "junk input {junk:?} must fall back to zero");
                assert!(warning.is_some(), "junk input {junk:?} must be reported");
            }
        }

        #[test]
        fn surrounding_whitespace_is_tolerated() {
            let ClearExtraOutcome { bytes, .. } = parse_extra_clear_bytes(Some("  7  "));
            assert_eq!(bytes, 7);
        }
    }
}
