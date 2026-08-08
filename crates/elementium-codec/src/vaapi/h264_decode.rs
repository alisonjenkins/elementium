//! Hardware H.264 decode through VAAPI.
//!
//! The software decoder is the one that must always work; this is the one that should be
//! used when it can be. Decoding on the GPU keeps a remote 720p stream off the CPU, which
//! matters most on exactly the machines where a software decode competes with capture,
//! encode and encryption for the same cores.
//!
//! **Scope is deliberately the same as [`super::h264_params`]:** Constrained Baseline and
//! Baseline, progressive, no scaling matrices, no B slices. That is what a WebRTC peer
//! sends. Anything else is refused here and falls back to software, rather than being
//! decoded approximately — a hardware decoder fed parameters it half-understands produces a
//! picture, and the picture is wrong.
//!
//! The reference-picture handling is a sliding window, which is all Baseline defines: each
//! reference frame displaces the oldest once the window is full. There are no long-term
//! references and no memory-management operations to honour, and the parser refuses any
//! stream that uses them rather than letting them be ignored here.

use std::sync::Arc;

use elementium_types::media::{I420Frame, PlaintextMedia};
use libva_sys::va_display_drm as va;

use super::display::Display;
use super::h264_params::{Pps, SliceHeader, Sps, nal_type, parse_pps, parse_slice_header, parse_sps, unescape_rbsp};
use super::image::SurfaceDownload;
use super::resource::{Buffer, Config, Context, SurfaceId, SurfacePool};
use super::status::{Status, check};
use crate::video::{VideoCodec, VideoDecoder};

/// Surfaces in the pool: the picture being decoded, plus the reference window, plus slack.
///
/// Baseline allows up to 16 references but a WebRTC encoder uses one. Sized for a handful
/// rather than the maximum, because every surface is a full picture of GPU memory.
const SURFACE_COUNT: usize = 8;

/// An unused entry in a reference list. `flags` of `INVALID` is how VAAPI reads "no picture
/// here", and `picture_id` must be `VA_INVALID_SURFACE` alongside it.
const fn empty_picture() -> va::VAPictureH264 {
    va::VAPictureH264 {
        picture_id: va::VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: va::VA_PICTURE_H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    }
}

/// A decoded picture being kept as a reference.
#[derive(Clone, Copy)]
struct Reference {
    surface: SurfaceId,
    frame_num: u32,
    poc: i32,
}

impl Reference {
    const fn as_va(&self) -> va::VAPictureH264 {
        va::VAPictureH264 {
            picture_id: self.surface.raw(),
            frame_idx: self.frame_num,
            flags: va::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            TopFieldOrderCnt: self.poc,
            BottomFieldOrderCnt: self.poc,
            va_reserved: [0; 4],
        }
    }
}

/// A VAAPI H.264 decoder.
pub struct H264Decoder {
    display: Arc<Display>,
    /// Built on the first keyframe, because the geometry comes from the stream rather than
    /// from the caller. A stream that changes resolution rebuilds it.
    session: Option<Session>,
    sps: Option<Sps>,
    pps: Option<Pps>,
}

/// Everything that depends on the picture size, and so cannot exist before the first SPS.
struct Session {
    context: Context,
    surfaces: SurfacePool,
    download: SurfaceDownload,
    width: u32,
    height: u32,
    /// Round-robin index into the pool, skipping surfaces still held as references.
    next: usize,
    references: Vec<Reference>,
    max_references: usize,
    /// `prev_poc_lsb` for the type-0 picture order count, which is only used to keep the
    /// reference list ordered.
    prev_poc_lsb: u32,
}

impl H264Decoder {
    /// Create a decoder, without yet knowing the picture size.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if no VAAPI display can be opened.
    pub fn new() -> Result<Self, Status> {
        let display = Arc::new(Display::open_any().ok_or(Status {
            operation: "no VAAPI render node could be opened",
            code: -1,
        })?);
        Ok(Self {
            display,
            session: None,
            sps: None,
            pps: None,
        })
    }

    /// Whether this machine can decode H.264 on the GPU at all.
    ///
    /// Probed by building a decoder rather than by reading a capability list: a driver that
    /// advertises the profile and then refuses the config is a real case, and the only
    /// honest answer to "can we" is having done it.
    #[must_use]
    pub fn is_available() -> bool {
        Self::new().is_ok_and(|d| d.ensure_profile().is_ok())
    }

    /// Confirm the driver will accept a decode config for the profile we need.
    fn ensure_profile(&self) -> Result<(), Status> {
        let mut attributes = [va::VAConfigAttrib {
            type_: va::VAConfigAttribType_VAConfigAttribRTFormat,
            value: va::VA_RT_FORMAT_YUV420,
        }];
        Config::for_decoding(
            &self.display,
            va::VAProfile_VAProfileH264ConstrainedBaseline,
            attributes.as_mut_slice(),
        )
        .map(|_| ())
    }

    /// Build the size-dependent state once the first SPS has been read.
    fn start_session(&mut self, sps: &Sps) -> Result<(), Status> {
        let (width, height) = (sps.width(), sps.height());
        let mut attributes = [va::VAConfigAttrib {
            type_: va::VAConfigAttribType_VAConfigAttribRTFormat,
            value: va::VA_RT_FORMAT_YUV420,
        }];
        let config = Config::for_decoding(
            &self.display,
            va::VAProfile_VAProfileH264ConstrainedBaseline,
            attributes.as_mut_slice(),
        )?;
        let mut surfaces = SurfacePool::new_nv12(&self.display, width, height, SURFACE_COUNT)?;
        let context = Context::new(config, width, height, surfaces.raw())?;
        let download = SurfaceDownload::new(&self.display, width, height)?;
        self.session = Some(Session {
            context,
            surfaces,
            download,
            width,
            height,
            next: 0,
            references: Vec::new(),
            max_references: usize::try_from(sps.max_num_ref_frames.max(1)).unwrap_or(1).min(SURFACE_COUNT.saturating_sub(2)),
            prev_poc_lsb: 0,
        });
        tracing::info!(width, height, "VAAPI H.264 decode session started");
        Ok(())
    }
}

impl Session {
    /// A surface not currently held as a reference.
    fn free_surface(&mut self) -> Option<SurfaceId> {
        let all = self.surfaces.surfaces();
        for _ in 0..all.len() {
            let slot = self.next.checked_rem(all.len().max(1)).unwrap_or(0);
            self.next = self.next.wrapping_add(1);
            let candidate = *all.get(slot)?;
            if !self.references.iter().any(|r| r.surface == candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Record a decoded picture as a reference, evicting the oldest once the window is full.
    fn remember(&mut self, reference: Reference) {
        self.references.push(reference);
        while self.references.len() > self.max_references {
            self.references.remove(0);
        }
    }
}

impl H264Decoder {
    /// Decode one packet, returning whatever pictures it completed.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the driver refuses a picture. A NAL unit this decoder does not
    /// support is skipped rather than failing the packet -- the caller falls back to
    /// software for the stream, and a single odd unit must not lose the ones around it.
    pub fn decode_packet(&mut self, data: &[u8]) -> Result<Vec<I420Frame>, Status> {
        let mut frames = Vec::new();
        for nal in nal_units(data) {
            let Some((&header, payload)) = nal.split_first() else {
                continue;
            };
            let nal_unit_type = header & 0x1f;
            let nal_ref_idc = (header >> 5) & 0x03;
            let rbsp = unescape_rbsp(payload);

            match nal_unit_type {
                nal_type::SPS => {
                    let Some(sps) = parse_sps(&rbsp) else {
                        return Err(Status {
                            operation: "SPS uses features this decoder does not support",
                            code: -1,
                        });
                    };
                    // A resolution change invalidates every surface and the context. Torn
                    // down rather than reused: the old surfaces are the wrong size, and the
                    // references in them describe a different picture.
                    let changed = self
                        .session
                        .as_ref()
                        .is_some_and(|s| s.width != sps.width() || s.height != sps.height());
                    if changed {
                        self.session = None;
                    }
                    self.sps = Some(sps);
                }
                nal_type::PPS => {
                    let Some(pps) = parse_pps(&rbsp) else {
                        return Err(Status {
                            operation: "PPS uses features this decoder does not support",
                            code: -1,
                        });
                    };
                    self.pps = Some(pps);
                }
                nal_type::IDR_SLICE | nal_type::NON_IDR_SLICE => {
                    if let Some(frame) =
                        self.decode_slice(nal, &rbsp, nal_unit_type, nal_ref_idc)?
                    {
                        frames.push(frame);
                    }
                }
                // SEI, access unit delimiters, filler. Nothing here needs them.
                _ => {}
            }
        }
        Ok(frames)
    }

    /// Decode one slice, which for a WebRTC stream is one whole picture.
    ///
    /// Returns the decoded frame. Multi-slice pictures would need the slices accumulated
    /// before `vaEndPicture`; no WebRTC encoder emits them at these sizes, and a stream
    /// that did would be decoded wrongly here rather than slowly, so it is refused.
    fn decode_slice(
        &mut self,
        nal: &[u8],
        rbsp: &[u8],
        nal_unit_type: u8,
        nal_ref_idc: u8,
    ) -> Result<Option<I420Frame>, Status> {
        let (Some(sps), Some(pps)) = (self.sps, self.pps) else {
            // Slices before the parameter sets. Normal at the start of a stream joined
            // mid-flight, and not something to fail on.
            return Ok(None);
        };
        let Some(header) = parse_slice_header(rbsp, nal_unit_type, nal_ref_idc, &sps, &pps) else {
            return Err(Status {
                operation: "slice header uses features this decoder does not support",
                code: -1,
            });
        };
        if header.first_mb_in_slice != 0 {
            return Err(Status {
                operation: "multi-slice pictures are not supported",
                code: -1,
            });
        }
        if self.session.is_none() {
            self.start_session(&sps)?;
        }
        let Some(session) = self.session.as_mut() else {
            return Err(Status {
                operation: "no decode session",
                code: -1,
            });
        };

        let is_idr = nal_unit_type == nal_type::IDR_SLICE;
        if is_idr {
            // An IDR resets everything: no earlier picture may be referenced across it.
            session.references.clear();
            session.prev_poc_lsb = 0;
        }
        let poc = i32::try_from(header.pic_order_cnt_lsb).unwrap_or(0);
        session.prev_poc_lsb = header.pic_order_cnt_lsb;

        let surface = session.free_surface().ok_or(Status {
            operation: "no free surface for the picture",
            code: -1,
        })?;

        let mut picture = picture_parameters(&sps, &pps, &header, session, surface, poc);
        let mut matrix = flat_iq_matrix();
        // The slice data buffer is the NAL unit exactly as it arrived: header byte
        // included, emulation-prevention bytes still in place. The driver skips those
        // itself, and `slice_data_bit_offset` is counted in that same escaped domain --
        // which is why the offset the parser reports, measured over the unescaped payload,
        // has to be translated before it is used.
        let bit_offset = escaped_bit_offset(nal, header.header_bits);
        let mut slice = slice_parameters(&header, session, nal.len(), bit_offset);
        let buffers = [
            Buffer::new(
                &session.context,
                va::VABufferType_VAPictureParameterBufferType,
                &mut picture,
            )?,
            Buffer::new(
                &session.context,
                va::VABufferType_VAIQMatrixBufferType,
                &mut matrix,
            )?,
            Buffer::new(
                &session.context,
                va::VABufferType_VASliceParameterBufferType,
                &mut slice,
            )?,
            Buffer::from_bytes(
                &session.context,
                va::VABufferType_VASliceDataBufferType,
                &mut nal.to_vec(),
            )?,
        ];

        submit(&self.display, &session.context, surface, &buffers)?;

        if nal_ref_idc != 0 {
            session.remember(Reference {
                surface,
                frame_num: header.frame_num,
                poc,
            });
        }

        let nv12 = session.download.read(surface)?;
        let layout = session.download.layout().ok_or(Status {
            operation: "the decoded image does not describe two planes",
            code: -1,
        })?;
        nv12_to_i420(&nv12, &layout, session.width, session.height)
            .map(Some)
            .ok_or(Status {
                operation: "the decoded image is smaller than its own geometry",
                code: -1,
            })
    }
}

/// Split a byte stream into NAL units, payload only, start codes removed.
///
/// Written here rather than reusing openh264's splitter: the hardware path must not depend
/// on the software decoder being compiled in, and the rule is three lines of scanning.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0_usize;
    while i.saturating_add(3) <= data.len() {
        let three = data.get(i..i.saturating_add(3));
        if three == Some(&[0, 0, 1]) {
            starts.push(i.saturating_add(3));
            i = i.saturating_add(3);
        } else {
            i = i.saturating_add(1);
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        // Each unit runs to the next start code, minus the zero bytes belonging to it.
        let end = starts.get(n.saturating_add(1)).map_or(data.len(), |&next| {
            let mut e = next.saturating_sub(3);
            while e > start && data.get(e.saturating_sub(1)) == Some(&0) {
                e = e.saturating_sub(1);
            }
            e
        });
        if let Some(unit) = data.get(start..end)
            && !unit.is_empty()
        {
            units.push(unit);
        }
    }
    units
}

/// The picture parameter buffer, filled from the parameter sets and the reference window.
fn picture_parameters(
    sps: &Sps,
    pps: &Pps,
    header: &SliceHeader,
    session: &Session,
    surface: SurfaceId,
    poc: i32,
) -> va::VAPictureParameterBufferH264 {
    let mut reference_frames = [empty_picture(); 16];
    for (slot, reference) in reference_frames.iter_mut().zip(session.references.iter()) {
        *slot = reference.as_va();
    }

    let mut seq_fields = va::_VAPictureParameterBufferH264__bindgen_ty_1 { value: 0 };
    // SAFETY: the union's two members are a `u32` and a bitfield struct of the same size;
    // writing through the bitfield and reading the value is how libva expects it to be set.
    unsafe {
        seq_fields.bits.set_chroma_format_idc(sps.chroma_format_idc);
        seq_fields.bits.set_residual_colour_transform_flag(0);
        seq_fields.bits.set_gaps_in_frame_num_value_allowed_flag(u32::from(sps.gaps_in_frame_num_value_allowed));
        seq_fields.bits.set_frame_mbs_only_flag(u32::from(sps.frame_mbs_only));
        seq_fields.bits.set_mb_adaptive_frame_field_flag(0);
        seq_fields.bits.set_direct_8x8_inference_flag(u32::from(sps.direct_8x8_inference));
        seq_fields.bits.set_MinLumaBiPredSize8x8(0);
        seq_fields.bits.set_log2_max_frame_num_minus4(sps.log2_max_frame_num.saturating_sub(4));
        seq_fields.bits.set_pic_order_cnt_type(sps.pic_order_cnt_type);
        seq_fields.bits.set_log2_max_pic_order_cnt_lsb_minus4(sps.log2_max_pic_order_cnt_lsb.saturating_sub(4));
        seq_fields.bits.set_delta_pic_order_always_zero_flag(0);
    }

    let mut pic_fields = va::_VAPictureParameterBufferH264__bindgen_ty_2 { value: 0 };
    // SAFETY: as above.
    unsafe {
        pic_fields.bits.set_entropy_coding_mode_flag(u32::from(pps.entropy_coding_mode));
        pic_fields.bits.set_weighted_pred_flag(u32::from(pps.weighted_pred));
        pic_fields.bits.set_weighted_bipred_idc(pps.weighted_bipred_idc);
        pic_fields.bits.set_transform_8x8_mode_flag(u32::from(pps.transform_8x8_mode));
        pic_fields.bits.set_field_pic_flag(0);
        pic_fields.bits.set_constrained_intra_pred_flag(u32::from(pps.constrained_intra_pred));
        pic_fields.bits.set_pic_order_present_flag(u32::from(pps.bottom_field_pic_order_in_frame_present));
        pic_fields.bits.set_deblocking_filter_control_present_flag(u32::from(pps.deblocking_filter_control_present));
        pic_fields.bits.set_redundant_pic_cnt_present_flag(u32::from(pps.redundant_pic_cnt_present));
        pic_fields.bits.set_reference_pic_flag(u32::from(!header.is_intra() || header.idr_pic_id.is_some()));
    }

    va::VAPictureParameterBufferH264 {
        CurrPic: va::VAPictureH264 {
            picture_id: surface.raw(),
            frame_idx: header.frame_num,
            flags: 0,
            TopFieldOrderCnt: poc,
            BottomFieldOrderCnt: poc,
            va_reserved: [0; 4],
        },
        ReferenceFrames: reference_frames,
        picture_width_in_mbs_minus1: u16::try_from(sps.width_mbs.saturating_sub(1)).unwrap_or(0),
        picture_height_in_mbs_minus1: u16::try_from(sps.height_map_units.saturating_sub(1)).unwrap_or(0),
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        num_ref_frames: u8::try_from(sps.max_num_ref_frames).unwrap_or(1),
        seq_fields,
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        slice_group_change_rate_minus1: 0,
        pic_init_qp_minus26: i8::try_from(pps.pic_init_qp_minus26).unwrap_or(0),
        pic_init_qs_minus26: 0,
        chroma_qp_index_offset: i8::try_from(pps.chroma_qp_index_offset).unwrap_or(0),
        second_chroma_qp_index_offset: i8::try_from(pps.second_chroma_qp_index_offset).unwrap_or(0),
        pic_fields,
        frame_num: u16::try_from(header.frame_num).unwrap_or(0),
        va_reserved: [0; 8],
    }
}

/// Translate a bit offset measured over the unescaped payload into one over the NAL unit.
///
/// The parser works on the RBSP, with the emulation-prevention bytes already removed and
/// the NAL header byte stripped. VAAPI is handed the NAL unit as it arrived and counts from
/// the start of *that*, so the offset has to gain the header byte and one byte for every
/// escape that occurs before the point in question.
///
/// Getting this wrong does not fail: the driver starts reading macroblocks a few bits early
/// or late and decodes noise, which looks like a broken picture rather than a broken number.
fn escaped_bit_offset(nal: &[u8], rbsp_bits: usize) -> usize {
    let target_byte = rbsp_bits / 8;
    let mut removed = 0_usize;
    let mut rbsp_index = 0_usize;
    let mut zeroes = 0_usize;
    // Skip the NAL header byte: the payload the parser saw begins after it.
    for &b in nal.iter().skip(1) {
        if rbsp_index >= target_byte {
            break;
        }
        if zeroes >= 2 && b == 0x03 {
            removed = removed.saturating_add(1);
            zeroes = 0;
            continue;
        }
        if b == 0 {
            zeroes = zeroes.saturating_add(1);
        } else {
            zeroes = 0;
        }
        rbsp_index = rbsp_index.saturating_add(1);
    }
    // Eight bits for the NAL header, plus eight for each escape byte before this point.
    rbsp_bits
        .saturating_add(8)
        .saturating_add(removed.saturating_mul(8))
}

/// The default flat quantisation matrices.
///
/// Sent explicitly rather than omitted: a driver handed no matrix buffer at all is entitled
/// to reuse whatever the previous picture left, and the parser has already refused any
/// stream that carries scaling lists of its own.
const fn flat_iq_matrix() -> va::VAIQMatrixBufferH264 {
    va::VAIQMatrixBufferH264 {
        ScalingList4x4: [[16; 16]; 6],
        ScalingList8x8: [[16; 64]; 2],
        va_reserved: [0; 4],
    }
}

/// The slice parameter buffer, including where in the buffer the payload starts.
fn slice_parameters(
    header: &SliceHeader,
    session: &Session,
    nal_len: usize,
    slice_data_bit_offset: usize,
) -> va::VASliceParameterBufferH264 {
    let mut list0 = [empty_picture(); 32];
    for (slot, reference) in list0.iter_mut().zip(session.references.iter().rev()) {
        *slot = reference.as_va();
    }

    va::VASliceParameterBufferH264 {
        slice_data_size: u32::try_from(nal_len).unwrap_or(0),
        slice_data_offset: 0,
        slice_data_flag: va::VA_SLICE_DATA_FLAG_ALL,
        // Where the macroblock data begins, counted in bits from the start of the buffer.
        // This is the single field most likely to be wrong and least likely to say so: an
        // error here decodes to noise rather than to an error.
        slice_data_bit_offset: u16::try_from(slice_data_bit_offset).unwrap_or(0),
        first_mb_in_slice: u16::try_from(header.first_mb_in_slice).unwrap_or(0),
        slice_type: u8::try_from(header.slice_type % 5).unwrap_or(0),
        direct_spatial_mv_pred_flag: 0,
        num_ref_idx_l0_active_minus1: u8::try_from(header.num_ref_idx_l0_active_minus1).unwrap_or(0),
        num_ref_idx_l1_active_minus1: u8::try_from(header.num_ref_idx_l1_active_minus1).unwrap_or(0),
        cabac_init_idc: u8::try_from(header.cabac_init_idc).unwrap_or(0),
        slice_qp_delta: i8::try_from(header.slice_qp_delta).unwrap_or(0),
        disable_deblocking_filter_idc: u8::try_from(header.disable_deblocking_filter_idc).unwrap_or(0),
        slice_alpha_c0_offset_div2: i8::try_from(header.slice_alpha_c0_offset_div2).unwrap_or(0),
        slice_beta_offset_div2: i8::try_from(header.slice_beta_offset_div2).unwrap_or(0),
        RefPicList0: list0,
        RefPicList1: [empty_picture(); 32],
        luma_log2_weight_denom: 0,
        chroma_log2_weight_denom: 0,
        luma_weight_l0_flag: 0,
        luma_weight_l0: [0; 32],
        luma_offset_l0: [0; 32],
        chroma_weight_l0_flag: 0,
        chroma_weight_l0: [[0; 2]; 32],
        chroma_offset_l0: [[0; 2]; 32],
        luma_weight_l1_flag: 0,
        luma_weight_l1: [0; 32],
        luma_offset_l1: [0; 32],
        chroma_weight_l1_flag: 0,
        chroma_weight_l1: [[0; 2]; 32],
        chroma_offset_l1: [[0; 2]; 32],
        va_reserved: [0; 4],
    }
}

/// Begin, render and end one picture, then wait for it.
fn submit(
    display: &Arc<Display>,
    context: &Context,
    surface: SurfaceId,
    buffers: &[Buffer],
) -> Result<(), Status> {
    let handle = display.handle();
    let ctx = context.id().raw();

    // SAFETY: the surface belongs to this decoder's pool and the context is live.
    check(
        unsafe { va::vaBeginPicture(handle, ctx, surface.raw()) },
        "vaBeginPicture",
    )?;

    let mut ids: Vec<va::VABufferID> = buffers.iter().map(|b| b.id().raw()).collect();
    // SAFETY: live buffer ids created against this context; libva reads the count given.
    let rendered = unsafe {
        va::vaRenderPicture(
            handle,
            ctx,
            ids.as_mut_ptr(),
            i32::try_from(ids.len()).unwrap_or(0),
        )
    };
    if let Err(e) = check(rendered, "vaRenderPicture") {
        // The picture has to be ended even on failure, or the context is left mid-picture
        // and every later frame fails too.
        // SAFETY: a picture is in progress on this context.
        unsafe {
            va::vaEndPicture(handle, ctx);
        }
        return Err(e);
    }

    // SAFETY: a picture is in progress on this context.
    check(unsafe { va::vaEndPicture(handle, ctx) }, "vaEndPicture")?;
    // SAFETY: the surface was just submitted.
    check(
        unsafe { va::vaSyncSurface(handle, surface.raw()) },
        "vaSyncSurface",
    )
}

/// Turn what the driver hands back into the packed I420 the rest of the pipeline uses.
///
/// The surface is NV12 -- one luma plane and one interleaved chroma plane, each with a
/// pitch the driver chose, which is usually wider than the picture.
fn nv12_to_i420(
    nv12: &[u8],
    layout: &super::image::PlaneLayout,
    width: u32,
    height: u32,
) -> Option<I420Frame> {
    let cols = usize::try_from(width).ok()?;
    let rows = usize::try_from(height).ok()?;
    let (chroma_cols, chroma_rows) = (cols.div_ceil(2), rows.div_ceil(2));

    let mut luma = Vec::with_capacity(cols.checked_mul(rows)?);
    for row in 0..rows {
        let start = layout.luma_offset.checked_add(row.checked_mul(layout.luma_pitch)?)?;
        luma.extend_from_slice(nv12.get(start..start.checked_add(cols)?)?);
    }

    let mut chroma_b = Vec::with_capacity(chroma_cols.checked_mul(chroma_rows)?);
    let mut chroma_r = Vec::with_capacity(chroma_cols.checked_mul(chroma_rows)?);
    for row in 0..chroma_rows {
        let start = layout
            .chroma_offset
            .checked_add(row.checked_mul(layout.chroma_pitch)?)?;
        let line = nv12.get(start..start.checked_add(chroma_cols.checked_mul(2)?)?)?;
        for pair in line.chunks_exact(2) {
            chroma_b.push(*pair.first()?);
            chroma_r.push(*pair.get(1)?);
        }
    }

    I420Frame::from_planes(width, height, &luma, &chroma_b, &chroma_r, 0)
}

impl VideoDecoder for H264Decoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn decode(&mut self, data: &PlaintextMedia) -> Result<Vec<I420Frame>, String> {
        self.decode_packet(data.as_bytes())
            .map_err(|e| format!("VAAPI H.264 decode: {e}"))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::as_conversions, clippy::arithmetic_side_effects)]
mod tests {
    use super::{H264Decoder, nal_units};
    use crate::video::{VideoDecoder, VideoEncoder};
    use elementium_types::media::I420Frame;

    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;

    #[test]
    fn nal_units_are_split_on_both_start_code_lengths() {
        // Three-byte and four-byte start codes, and the trailing zero of the longer form
        // must not be left on the end of the unit before it.
        let stream = [0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB];
        let units = nal_units(&stream);
        assert_eq!(units.len(), 2, "two units");
        assert_eq!(units[0], &[0x67, 0xAA], "the first must not keep the next unit's zero");
        assert_eq!(units[1], &[0x68, 0xBB]);
    }

    #[test]
    fn a_stream_with_no_start_code_yields_nothing() {
        assert!(nal_units(&[1, 2, 3, 4]).is_empty());
    }

    /// A moving picture, so a reference frame that is ignored or stale shows up as a
    /// difference rather than decoding to the same thing either way.
    fn moving_frame(step: usize) -> I420Frame {
        let (cols, rows) = (WIDTH as usize, HEIGHT as usize);
        let mut luma = vec![0u8; cols * rows];
        for (i, px) in luma.iter_mut().enumerate() {
            let (col, row) = (i % cols, i / cols);
            *px = if ((col + step * 8) / 16 + row / 16).is_multiple_of(2) { 210 } else { 30 };
        }
        let chroma_b = vec![100u8; (cols / 2) * (rows / 2)];
        let chroma_r = vec![150u8; (cols / 2) * (rows / 2)];
        I420Frame::from_planes(WIDTH, HEIGHT, &luma, &chroma_b, &chroma_r, 0)
            .expect("fixture frame")
    }

    /// Hardware and software must agree, frame for frame.
    ///
    /// This is the test the hardware decoder exists to pass, and the only one that can
    /// catch what it is likely to get wrong. A picture parameter filled in incorrectly --
    /// the wrong reference list, the wrong slice bit offset -- still produces a picture,
    /// and it will still be roughly the right size and roughly the right colour. Comparing
    /// it against a decoder that is known to be right is what turns "it produced a frame"
    /// into "it produced the frame".
    ///
    /// P frames matter more than the keyframe here: a keyframe decodes correctly even with
    /// the reference handling entirely broken.
    #[test]
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    fn hardware_decodes_the_same_pictures_as_software() {
        let Ok(mut encoder) = crate::vaapi::H264Encoder::new(WIDTH, HEIGHT, 2_000) else {
            eprintln!("skipping: no VAAPI H.264 encoder on this machine");
            return;
        };
        let Ok(mut hardware) = H264Decoder::new() else {
            eprintln!("skipping: no VAAPI display");
            return;
        };
        let mut software = crate::h264_codec::H264Decoder::new().expect("software decoder");

        let mut compared = 0;
        for step in 0..6 {
            let packets = VideoEncoder::encode(&mut encoder, &moving_frame(step)).expect("encode");
            for packet in packets {
                let hw = match VideoDecoder::decode(&mut hardware, &packet.data) {
                    Ok(frames) => frames,
                    Err(e) => {
                        eprintln!("skipping: hardware decode unavailable ({e})");
                        return;
                    }
                };
                let sw = VideoDecoder::decode(&mut software, &packet.data).expect("software decode");
                assert_eq!(hw.len(), sw.len(), "both decoders must complete the same pictures");

                for (h, s) in hw.iter().zip(sw.iter()) {
                    assert_eq!((h.width(), h.height()), (s.width(), s.height()), "geometry");
                    let (hy, sy) = (h.y(), s.y());
                    assert_eq!(hy.len(), sy.len(), "luma plane length");
                    // Exact equality is too strong across two implementations of an inexact
                    // reconstruction, but the difference must be small everywhere: a wrong
                    // reference or bit offset produces blocks that are wrong by far more
                    // than rounding.
                    let worst = hy
                        .iter()
                        .zip(sy.iter())
                        .map(|(a, b)| u32::from(a.abs_diff(*b)))
                        .max()
                        .unwrap_or(0);
                    assert!(
                        worst <= 8,
                        "picture {compared}: hardware and software differ by {worst} at worst"
                    );
                    compared += 1;
                }
            }
        }
        assert!(compared >= 2, "expected several pictures, compared {compared}");
    }
}
