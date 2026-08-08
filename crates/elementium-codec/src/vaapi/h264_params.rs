//! Parse the H.264 headers a hardware decoder has to be told about.
//!
//! VAAPI does not parse the bitstream. The driver is handed a picture parameter buffer, a
//! slice parameter buffer and the slice bytes, and every field in the first two comes from
//! syntax the caller must read for itself. That is the work here.
//!
//! **Only what a WebRTC stream actually contains.** Constrained Baseline and Baseline, no
//! interlacing, no scaling matrices, no long-term references. Anything outside that is
//! refused rather than guessed at, because a field filled in with a plausible default
//! produces a picture that decodes and is subtly wrong — green blocks, or drift that only
//! appears seconds later — which is far harder to diagnose than a decoder that declines.

use super::bitstream::BitReader;

/// NAL unit types this decoder cares about.
pub mod nal_type {
    pub const NON_IDR_SLICE: u8 = 1;
    pub const IDR_SLICE: u8 = 5;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
}

/// A sequence parameter set, reduced to the fields a decode needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sps {
    pub id: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub log2_max_frame_num: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb: u32,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed: bool,
    pub width_mbs: u32,
    pub height_map_units: u32,
    pub frame_mbs_only: bool,
    pub direct_8x8_inference: bool,
}

impl Sps {
    /// Picture width in pixels, before cropping.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width_mbs.saturating_mul(16)
    }

    /// Picture height in pixels, before cropping.
    #[must_use]
    pub const fn height(&self) -> u32 {
        let factor = if self.frame_mbs_only { 1 } else { 2 };
        self.height_map_units.saturating_mul(16).saturating_mul(factor)
    }
}

/// A picture parameter set, reduced likewise.
///
/// The flags stay separate fields rather than being packed into a bitfield type: each one
/// is a distinct syntax element with its own name in the standard, and the VAAPI struct
/// this feeds has a field per flag too. Grouping them would only move the unpacking.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub second_chroma_qp_index_offset: i32,
}

/// The slice header fields the driver needs, plus where the slice data begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: u32,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: Option<u32>,
    pub pic_order_cnt_lsb: u32,
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    /// Bits consumed by the header, which is where the slice payload starts.
    pub header_bits: usize,
}

impl SliceHeader {
    /// Whether this slice is intra-coded (I or SI), and so needs no reference picture.
    #[must_use]
    pub const fn is_intra(&self) -> bool {
        matches!(self.slice_type % 5, 2 | 4)
    }
}

/// Strip the emulation-prevention bytes an encoder inserted.
///
/// The inverse of what [`super::bitstream::nal_unit`] does on the way out. Skipping it
/// works until a header happens to contain `00 00 03`, and then one particular frame size
/// or quantiser fails while everything else works.
#[must_use]
pub fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeroes = 0_usize;
    for &b in nal {
        if zeroes >= 2 && b == 0x03 {
            zeroes = 0;
            continue;
        }
        if b == 0 {
            zeroes = zeroes.saturating_add(1);
        } else {
            zeroes = 0;
        }
        out.push(b);
    }
    out
}

/// Parse a sequence parameter set from a NAL unit payload, header byte excluded.
///
/// Returns `None` for anything this decoder does not support, rather than a partly-filled
/// set. See the module comment.
#[must_use]
#[allow(clippy::similar_names, clippy::too_many_lines)]
pub fn parse_sps(rbsp: &[u8]) -> Option<Sps> {
    let mut r = BitReader::new(rbsp);
    let profile_idc = u8::try_from(r.bits(8)?).ok()?;
    // constraint_set flags and two reserved bits.
    let _constraints = r.bits(8)?;
    let level_idc = u8::try_from(r.bits(8)?).ok()?;
    let id = r.ue()?;

    let mut chroma_format_idc = 1;
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
    {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            let _separate_colour_plane = r.bit()?;
        }
        let _bit_depth_luma = r.ue()?;
        let _bit_depth_chroma = r.ue()?;
        let _qpprime_y_zero_transform_bypass = r.bit()?;
        if r.bit()? {
            // Scaling matrices. Refused rather than skipped: reading past them correctly is
            // easy to get wrong, and a WebRTC encoder does not send them.
            return None;
        }
    }

    let log2_max_frame_num = r.ue()?.checked_add(4)?;
    let pic_order_cnt_type = r.ue()?;
    let mut log2_max_pic_order_cnt_lsb = 0;
    match pic_order_cnt_type {
        0 => log2_max_pic_order_cnt_lsb = r.ue()?.checked_add(4)?,
        1 => {
            let _delta_pic_order_always_zero = r.bit()?;
            let _offset_for_non_ref_pic = r.se()?;
            let _offset_for_top_to_bottom_field = r.se()?;
            let cycle = r.ue()?;
            for _ in 0..cycle.min(256) {
                let _offset_for_ref_frame = r.se()?;
            }
        }
        // Type 2 needs no extra syntax. Anything else is not a value this standard defines.
        2 => {}
        _ => return None,
    }

    let max_num_ref_frames = r.ue()?;
    let gaps_in_frame_num_value_allowed = r.bit()?;
    let width_mbs = r.ue()?.checked_add(1)?;
    let height_map_units = r.ue()?.checked_add(1)?;
    let frame_mbs_only = r.bit()?;
    if !frame_mbs_only {
        // Interlaced. Not something a WebRTC encoder produces, and supporting it half-way
        // is worse than not at all.
        return None;
    }
    let direct_8x8_inference = r.bit()?;

    Some(Sps {
        id,
        profile_idc,
        level_idc,
        chroma_format_idc,
        log2_max_frame_num,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb,
        max_num_ref_frames,
        gaps_in_frame_num_value_allowed,
        width_mbs,
        height_map_units,
        frame_mbs_only,
        direct_8x8_inference,
    })
}

/// Parse a picture parameter set from a NAL unit payload, header byte excluded.
#[must_use]
pub fn parse_pps(rbsp: &[u8]) -> Option<Pps> {
    let mut r = BitReader::new(rbsp);
    let id = r.ue()?;
    let sps_id = r.ue()?;
    let entropy_coding_mode = r.bit()?;
    let bottom_field_pic_order_in_frame_present = r.bit()?;
    let num_slice_groups = r.ue()?.checked_add(1)?;
    if num_slice_groups > 1 {
        // Slice groups (FMO). Not produced by any WebRTC encoder, and the map syntax that
        // follows is long enough that skipping it wrongly would desynchronise everything.
        return None;
    }
    let num_ref_idx_l0_default_active_minus1 = r.ue()?;
    let num_ref_idx_l1_default_active_minus1 = r.ue()?;
    let weighted_pred = r.bit()?;
    let weighted_bipred_idc = r.bits(2)?;
    let pic_init_qp_minus26 = r.se()?;
    let _pic_init_qs_minus26 = r.se()?;
    let chroma_qp_index_offset = r.se()?;
    let deblocking_filter_control_present = r.bit()?;
    let constrained_intra_pred = r.bit()?;
    let redundant_pic_cnt_present = r.bit()?;

    // The trailing extension is optional. Its absence is normal, not an error, so what is
    // left is checked rather than assumed: `more_rbsp_data` in the standard's terms.
    let (transform_8x8_mode, second_chroma_qp_index_offset) = if r.remaining() > 8 {
        let transform = r.bit().unwrap_or(false);
        if r.bit().unwrap_or(false) {
            // pic_scaling_matrix_present. Refused, as in the SPS.
            return None;
        }
        (transform, r.se().unwrap_or(chroma_qp_index_offset))
    } else {
        (false, chroma_qp_index_offset)
    };

    Some(Pps {
        id,
        sps_id,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        chroma_qp_index_offset,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode,
        second_chroma_qp_index_offset,
    })
}

/// The deblocking-filter overrides at the end of a slice header, if the PPS enables them.
///
/// Split out only to keep `parse_slice_header` readable; it is the tail of that syntax and
/// has no meaning apart from it.
fn deblocking(r: &mut BitReader<'_>, pps: &Pps) -> Option<(u32, i32, i32)> {
    if !pps.deblocking_filter_control_present {
        return Some((0, 0, 0));
    }
    let disable = r.ue()?;
    if disable == 1 {
        return Some((disable, 0, 0));
    }
    Some((disable, r.se()?, r.se()?))
}

/// Parse a slice header, given the parameter sets it refers to.
///
/// `nal_unit_type` decides whether an IDR picture id is present; `nal_ref_idc` whether the
/// reference-marking syntax is.
#[must_use]
#[allow(clippy::similar_names)]
pub fn parse_slice_header(
    rbsp: &[u8],
    nal_unit_type: u8,
    nal_ref_idc: u8,
    sps: &Sps,
    pps: &Pps,
) -> Option<SliceHeader> {
    let mut r = BitReader::new(rbsp);
    let first_mb_in_slice = r.ue()?;
    let slice_type = r.ue()?;
    let pps_id = r.ue()?;
    let frame_num = r.bits(u8::try_from(sps.log2_max_frame_num).ok()?)?;

    let idr_pic_id = if nal_unit_type == nal_type::IDR_SLICE {
        Some(r.ue()?)
    } else {
        None
    };

    let mut pic_order_cnt_lsb = 0;
    if sps.pic_order_cnt_type == 0 {
        pic_order_cnt_lsb = r.bits(u8::try_from(sps.log2_max_pic_order_cnt_lsb).ok()?)?;
        if pps.bottom_field_pic_order_in_frame_present {
            let _delta_pic_order_cnt_bottom = r.se()?;
        }
    }
    if pps.redundant_pic_cnt_present {
        let _redundant_pic_cnt = r.ue()?;
    }

    // P slices only; B slices carry more syntax here and no WebRTC encoder sends them.
    let kind = slice_type % 5;
    if kind == 1 {
        return None;
    }
    let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    let num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    if kind == 0 {
        // P slice: num_ref_idx_active_override_flag.
        if r.bit()? {
            num_ref_idx_l0_active_minus1 = r.ue()?;
        }
        // ref_pic_list_modification for list 0.
        if r.bit()? {
            loop {
                let op = r.ue()?;
                if op == 3 {
                    break;
                }
                if op > 3 {
                    return None;
                }
                let _abs_diff_or_long_term = r.ue()?;
            }
        }
    } else if kind == 2 || kind == 4 {
        // I slice: no reference list syntax at all.
    } else {
        return None;
    }

    if nal_ref_idc != 0 {
        if nal_unit_type == nal_type::IDR_SLICE {
            let _no_output_of_prior_pics = r.bit()?;
            let _long_term_reference = r.bit()?;
        } else if r.bit()? {
            // adaptive_ref_pic_marking_mode_flag: a sequence of memory-management ops.
            loop {
                let op = r.ue()?;
                if op == 0 {
                    break;
                }
                if op > 6 {
                    return None;
                }
                if matches!(op, 1 | 3) {
                    let _difference_of_pic_nums = r.ue()?;
                }
                if matches!(op, 2) {
                    let _long_term_pic_num = r.ue()?;
                }
                if matches!(op, 3 | 6) {
                    let _long_term_frame_idx = r.ue()?;
                }
                if matches!(op, 4) {
                    let _max_long_term_frame_idx = r.ue()?;
                }
            }
        }
    }

    // CABAC and the slice QP delta follow. They are not needed in the parameter buffers,
    // but the *bit position* after them is: it is where the slice payload begins.
    let cabac_init_idc = if pps.entropy_coding_mode && kind != 2 && kind != 4 {
        r.ue()?
    } else {
        0
    };
    let slice_qp_delta = r.se()?;
    let (disable_deblocking_filter_idc, slice_alpha_c0_offset_div2, slice_beta_offset_div2) =
        deblocking(&mut r, pps)?;

    Some(SliceHeader {
        first_mb_in_slice,
        slice_type,
        pps_id,
        frame_num,
        idr_pic_id,
        pic_order_cnt_lsb,
        num_ref_idx_l0_active_minus1,
        num_ref_idx_l1_active_minus1,
        cabac_init_idc,
        slice_qp_delta,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        header_bits: rbsp.len().saturating_mul(8).saturating_sub(r.remaining()),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::{parse_pps, parse_sps, unescape_rbsp};

    /// The escape bytes an encoder inserts have to come back out, or every field after the
    /// first `00 00 03` reads from the wrong bit.
    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1]), vec![0, 0, 1]);
        assert_eq!(unescape_rbsp(&[0, 0, 3, 0, 0, 3, 2]), vec![0, 0, 0, 0, 2]);
        // A 3 that is not preceded by two zeroes is data, not an escape.
        assert_eq!(unescape_rbsp(&[1, 3, 3, 7]), vec![1, 3, 3, 7]);
        // And one that follows a *reset* zero run is data too.
        assert_eq!(unescape_rbsp(&[0, 1, 0, 3]), vec![0, 1, 0, 3]);
    }

    /// A truncated parameter set must be refused, not half-parsed.
    #[test]
    fn a_truncated_parameter_set_is_refused() {
        assert!(parse_sps(&[]).is_none());
        assert!(parse_sps(&[0x42]).is_none());
        assert!(parse_pps(&[]).is_none());
    }
}

/// Parse what this project's own encoder writes, which is the test that matters.
///
/// The unit tests above check the parser against hand-made bytes. This checks it against a
/// real encoder's output, taken off the wire complete with whatever escaping it inserted --
/// so a disagreement is about the parser, not about my idea of what an encoder emits.
#[cfg(all(test, target_os = "linux", feature = "vaapi"))]
#[allow(clippy::expect_used, clippy::as_conversions, clippy::arithmetic_side_effects)]
mod encoder_agreement_tests {
    use super::{nal_type, parse_pps, parse_sps, unescape_rbsp};
    use crate::video::VideoEncoder;
    use elementium_types::media::I420Frame;

    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;

    #[test]
    fn the_parameter_sets_this_encoder_emits_read_back_correctly() {
        let Ok(mut encoder) = crate::vaapi::H264Encoder::new(WIDTH, HEIGHT, 1_000) else {
            eprintln!("skipping: no VAAPI H.264 encoder on this machine");
            return;
        };
        let planes = |n: usize, v: u8| vec![v; n];
        let frame = I420Frame::from_planes(
            WIDTH,
            HEIGHT,
            &planes((WIDTH * HEIGHT) as usize, 128),
            &planes((WIDTH * HEIGHT / 4) as usize, 128),
            &planes((WIDTH * HEIGHT / 4) as usize, 128),
            0,
        )
        .expect("fixture frame");

        let packets = VideoEncoder::encode(&mut encoder, &frame).expect("encode");
        let mut sps = None;
        let mut pps = None;
        for packet in &packets {
            for nal in openh264::nal_units(packet.data.as_bytes()) {
                // Skip the start code, then the one-byte NAL header.
                let start = nal
                    .windows(3)
                    .position(|w| w == [0, 0, 1])
                    .map_or(0, |i| i + 3);
                let Some((&header, payload)) = nal.get(start..).and_then(|r| r.split_first())
                else {
                    continue;
                };
                let rbsp = unescape_rbsp(payload);
                match header & 0x1f {
                    nal_type::SPS => sps = parse_sps(&rbsp),
                    nal_type::PPS => pps = parse_pps(&rbsp),
                    _ => {}
                }
            }
        }

        let sps = sps.expect("the encoder must emit an SPS this parser can read");
        let pps = pps.expect("the encoder must emit a PPS this parser can read");

        // The geometry is the field a wrong parse corrupts most visibly, and the one the
        // hardware decoder is configured from.
        assert_eq!(sps.width(), WIDTH, "parsed SPS width");
        assert_eq!(sps.height(), HEIGHT, "parsed SPS height");
        assert_eq!(sps.profile_idc, 66, "Constrained Baseline");
        assert!(sps.frame_mbs_only, "WebRTC streams are progressive");
        assert_eq!(sps.chroma_format_idc, 1, "4:2:0");
        assert_eq!(pps.sps_id, sps.id, "the PPS must refer to this SPS");
    }
}
