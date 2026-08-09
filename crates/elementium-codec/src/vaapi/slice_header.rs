//! Move `pic_parameter_set_id` into the first byte of an IDR slice, so that an encrypted
//! frame can still be depacketised.
//!
//! `LiveKit`'s end-to-end encryption leaves exactly two bytes of an H.264 frame in the
//! clear: the slice NAL unit's header byte and one more. Everything after that is
//! ciphertext. Chromium's receive path parses `first_mb_in_slice`, `slice_type` and
//! `pic_parameter_set_id` out of every IDR slice *before* any decryption happens, looks
//! up the PPS by that id, and drops the frame when it finds none:
//!
//! ```text
//! h264_sps_pps_tracker.cc] No PPS with id << 1 received
//! ```
//!
//! Then every delta frame is withheld behind the sequence gap the dropped keyframe left,
//! so the receiver counts packets, assembles nothing, and asks for a keyframe forever.
//!
//! Whether the id survives comes down to arithmetic. Our driver writes `slice_type` 7 --
//! "I, and every slice in this picture is I" -- whose `ue(v)` is seven bits, so with the
//! one bit of `first_mb_in_slice` the id begins at bit eight, the first ciphertext byte.
//! Chromium's own encoder writes `slice_type` 2, three bits, and its id fits with room to
//! spare. That is the whole of why browser-to-browser works under the same scheme and we
//! did not.
//!
//! 7 and 2 mean the same thing to a decoder, which reads `slice_type % 5`. Rewriting 7 as
//! 2 frees four bits and brings the id into the clear byte. The four bits are given back
//! to `idr_pic_id`, whose only requirement is that consecutive IDR pictures differ, so
//! the header keeps its exact length and every bit after it is copied across untouched --
//! including the entropy-coded slice data, which must not move.

use super::bitstream::{BitReader, BitWriter, nal_unit};
use super::h264_params::unescape_rbsp;

/// `slice_type` for an I slice, without the "all slices in the picture" claim.
const SLICE_TYPE_I: u32 = 2;
/// The value the driver writes, which is the same slice type with that claim attached.
const SLICE_TYPE_I_ALL: u32 = 7;
/// `nal_unit_type` 5.
const NAL_IDR_SLICE: u8 = 5;

/// The width in bits of `ue(value)`.
const fn ue_bits(value: u32) -> usize {
    let mut leading = 0_usize;
    let mut span = value.saturating_add(1);
    while span > 1 {
        span >>= 1_u32;
        leading = leading.saturating_add(1);
    }
    leading.saturating_mul(2).saturating_add(1)
}

/// The smallest value whose `ue(v)` is `bits` wide, if such a value exists.
///
/// `ue(v)` widths are always odd, so a caller asking for an even width is asking for
/// something that cannot be written and gets `None` rather than an approximation.
fn smallest_ue_of_width(bits: usize) -> Option<u32> {
    if bits.is_multiple_of(2) || bits == 0 {
        return None;
    }
    let leading = bits.saturating_sub(1) / 2;
    // The smallest value with `leading` leading zeros is 2^leading - 1.
    let span = 1_u32.checked_shl(u32::try_from(leading).ok()?)?;
    span.checked_sub(1)
}

/// Rewrite the first IDR slice in `frame`, if its `pic_parameter_set_id` would be
/// encrypted.
///
/// Returns `None` when there is nothing to do: no IDR slice, a `slice_type` that already
/// leaves room, or a header this cannot parse. `None` means "use the frame as it is" in
/// every case, because a frame we do not understand is one we must not rewrite.
#[must_use]
pub fn move_pps_id_into_the_clear(frame: &[u8], log2_max_frame_num: u8) -> Option<Vec<u8>> {
    let (start, payload_start, end) = first_idr_slice(frame)?;
    let payload = frame.get(payload_start..end)?;
    let rbsp = unescape_rbsp(payload);

    let mut r = BitReader::new(&rbsp);
    let first_mb_in_slice = r.ue()?;
    let slice_type = r.ue()?;
    if slice_type != SLICE_TYPE_I_ALL {
        return None;
    }
    let pps_id = r.ue()?;
    let frame_num = r.bits(log2_max_frame_num)?;
    let idr_pic_id = r.ue()?;
    let header_bits = r.position();

    // The four bits `slice_type` gives up have to go somewhere, or every bit after the
    // header shifts and the slice data lands at the wrong offset.
    let widened = ue_bits(idr_pic_id).checked_add(4)?;
    let new_idr_pic_id = smallest_ue_of_width(widened)?;

    let mut w = BitWriter::new();
    w.ue(first_mb_in_slice);
    w.ue(SLICE_TYPE_I);
    w.ue(pps_id);
    w.bits(frame_num, log2_max_frame_num);
    w.ue(new_idr_pic_id);
    if w.len_bits() != header_bits {
        // The arithmetic above should make these equal; if it ever does not, sending the
        // frame unchanged is a receiver that drops keyframes, and sending it changed is a
        // receiver that decodes garbage.
        return None;
    }

    // Everything from here on is the rest of the slice header and the entropy-coded slice
    // data. It is copied bit for bit: nothing in it is ours to interpret.
    while let Some(bit) = r.bit() {
        w.bit(bit);
    }
    let rewritten = w.finish();

    let mut out = Vec::with_capacity(frame.len().saturating_add(8));
    out.extend_from_slice(frame.get(..start)?);
    // `nal_ref_idc` 3: an IDR is always a reference picture.
    out.extend_from_slice(&nal_unit(3, NAL_IDR_SLICE, &rewritten));
    out.extend_from_slice(frame.get(end..)?);
    Some(out)
}

/// `(start code offset, payload offset, end offset)` of the first IDR slice NAL unit.
fn first_idr_slice(frame: &[u8]) -> Option<(usize, usize, usize)> {
    let starts = start_code_offsets(frame);
    for (n, &(code_at, header_at)) in starts.iter().enumerate() {
        let header = *frame.get(header_at)?;
        if header & 0x1F != NAL_IDR_SLICE {
            continue;
        }
        let end = starts
            .get(n.saturating_add(1))
            .map_or(frame.len(), |&(next_code, _)| next_code);
        return Some((code_at, header_at.saturating_add(1), end));
    }
    None
}

/// Every `(start code offset, NAL header offset)` pair in an Annex B stream.
fn start_code_offsets(frame: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0_usize;
    while i.saturating_add(3) <= frame.len() {
        let three = frame.get(i..i.saturating_add(3));
        if three == Some(&[0, 0, 1][..]) {
            // A four-byte start code is a three-byte one with a zero in front, and the
            // NAL begins after the three-byte form either way.
            let code_at = if i > 0 && frame.get(i.saturating_sub(1)) == Some(&0) {
                i.saturating_sub(1)
            } else {
                i
            };
            out.push((code_at, i.saturating_add(3)));
            i = i.saturating_add(3);
        } else {
            i = i.saturating_add(1);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::{
        BitReader, move_pps_id_into_the_clear, smallest_ue_of_width, ue_bits, unescape_rbsp,
    };

    /// `log2_max_frame_num` for the encoder this exists for.
    const FRAME_NUM_BITS: u8 = 8;

    /// An IDR slice as our driver writes it: `first_mb_in_slice` 0, `slice_type` 7,
    /// `pic_parameter_set_id` 0, then a frame number, an IDR id, and opaque slice data.
    fn driver_idr(slice_data: &[u8]) -> Vec<u8> {
        use super::BitWriter;
        let mut w = BitWriter::new();
        w.ue(0); // first_mb_in_slice
        w.ue(7); // slice_type: I, all slices
        w.ue(0); // pic_parameter_set_id
        w.bits(0x2a, FRAME_NUM_BITS);
        w.ue(0); // idr_pic_id
        w.bits(0, 4); // pic_order_cnt_lsb, part of the header this does not interpret
        for &b in slice_data {
            w.bits(u32::from(b), 8);
        }
        w.trailing_bits();
        let rbsp = w.finish();
        super::nal_unit(3, 5, &rbsp)
    }

    #[test]
    fn ue_widths_are_the_ones_the_standard_defines() {
        assert_eq!(ue_bits(0), 1);
        assert_eq!(ue_bits(1), 3);
        assert_eq!(ue_bits(2), 3);
        assert_eq!(ue_bits(6), 5);
        assert_eq!(ue_bits(7), 7);
    }

    #[test]
    fn a_replacement_id_of_the_required_width_exists() {
        assert_eq!(smallest_ue_of_width(5), Some(3));
        assert_eq!(smallest_ue_of_width(1), Some(0));
        // Even widths are unwritable, and saying so beats rounding to one that is.
        assert_eq!(smallest_ue_of_width(4), None);
    }

    /// The point of the whole module: the id must be readable from the first byte alone.
    #[test]
    fn the_pps_id_ends_up_inside_the_first_payload_byte() {
        let frame = driver_idr(&[0x9c; 64]);
        let rewritten = move_pps_id_into_the_clear(&frame, FRAME_NUM_BITS).unwrap();

        // Two bytes into the slice NAL is all livekit leaves in the clear: the header byte
        // and one more. Everything the receiver needs must be inside that one byte.
        let payload_start = 5_usize; // four-byte start code, then the NAL header byte
        let clear = &rewritten[payload_start..payload_start.saturating_add(1)];
        let mut r = BitReader::new(clear);
        assert_eq!(r.ue().unwrap(), 0, "first_mb_in_slice");
        assert_eq!(r.ue().unwrap(), 2, "slice_type, no longer the seven-bit form");
        assert_eq!(r.ue().unwrap(), 0, "pic_parameter_set_id, now readable");
    }

    /// The bits after the header must not move: they are entropy-coded slice data, and a
    /// decoder reading them one bit out of place produces nothing recognisable.
    #[test]
    fn the_slice_data_is_left_exactly_where_it_was() {
        let frame = driver_idr(&[0x5a, 0xa5, 0x33, 0xcc, 0x0f]);
        let rewritten = move_pps_id_into_the_clear(&frame, FRAME_NUM_BITS).unwrap();

        let before = unescape_rbsp(&frame[5..]);
        let after = unescape_rbsp(&rewritten[5..]);
        assert_eq!(before.len(), after.len(), "the frame changed length");

        // Past the header, which is 1 + 7 + 1 + 8 + 1 = 18 bits either way.
        let mut a = BitReader::new(&before);
        let mut b = BitReader::new(&after);
        for _ in 0..18 {
            let _ = a.bit();
            let _ = b.bit();
        }
        while let (Some(x), Some(y)) = (a.bit(), b.bit()) {
            assert_eq!(x, y, "a bit of slice data moved");
        }
    }

    /// A slice that already leaves room is left alone, rather than rewritten for the sake
    /// of it: every rewrite is a chance to corrupt a frame.
    #[test]
    fn a_slice_type_that_already_fits_is_untouched() {
        use super::BitWriter;
        let mut w = BitWriter::new();
        w.ue(0); // first_mb_in_slice
        w.ue(2); // slice_type: I, the short form
        w.ue(0); // pic_parameter_set_id
        w.bits(1, FRAME_NUM_BITS);
        w.ue(0);
        w.trailing_bits();
        let frame = super::nal_unit(3, 5, &w.finish());
        assert!(move_pps_id_into_the_clear(&frame, FRAME_NUM_BITS).is_none());
    }

    #[test]
    fn a_frame_with_no_idr_slice_is_untouched() {
        let frame = super::nal_unit(2, 1, &[0x9a, 0x00, 0x11, 0x80]);
        assert!(move_pps_id_into_the_clear(&frame, FRAME_NUM_BITS).is_none());
    }
}
