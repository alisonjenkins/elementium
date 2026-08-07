//! H.264 encoding on the GPU.
//!
//! The shape of a VAAPI encode is fixed and unforgiving. Per frame: begin a picture, render
//! a sequence parameter buffer (on keyframes), a picture parameter buffer, and a slice
//! parameter buffer, end the picture, wait for the surface, then map the coded buffer and
//! read the bytes out. Miss one and the driver either refuses or, worse, produces a
//! bitstream that decodes to something wrong.
//!
//! # What this deliberately does not do
//!
//! **No B-frames.** They compress better and they require reordering, which costs a frame
//! of latency in each direction. In a call that latency is the product; in a file it is
//! free. `ip_period` is 1 and stays there.
//!
//! **One slice per frame.** Multiple slices buy error resilience on a lossy link and cost
//! compression. RTP already fragments, and the SFU already handles loss with keyframe
//! requests, so the second slice would be paying twice for the same thing.
//!
//! # The stream is assembled here, not by the driver
//!
//! VAAPI lets a driver generate SPS and PPS, and lets an application supply them as packed
//! headers instead. This driver advertises support for every packed header type
//! (`VAConfigAttribEncPackedHeaders = 0x1f`) and then emits none of them: buffers sent that
//! way are accepted, the encode succeeds, and the coded buffer comes back without them.
//! Batching them differently, splitting them into their own `vaRenderPicture` calls, and
//! declaring them in the config all made no difference.
//!
//! What it does emit is a correct slice -- header and all -- with the NAL header byte left
//! as zero for the application to fill in. That was established by patching that one byte
//! in a dump and watching a reference decoder go from "missing picture in access unit" to
//! "non-existing PPS 0 referenced": it had parsed the slice header and wanted the parameter
//! sets it referred to.
//!
//! So the parameter sets are written here (see [`super::bitstream`]) and prepended to each
//! keyframe, and the driver's NAL header byte is filled in. The result is checked against a
//! real decoder in the tests rather than assumed.
//!
//! # Rate control
//!
//! Variable bitrate under a ceiling, not constant bitrate. CBR means exactly that: the
//! encoder pads every frame with filler data to hit the target, which was visible in the
//! first working version here as every frame coming out at precisely 8333 bytes -- 2000
//! kbps divided by 30 frames -- with a filler NAL making up the difference. On a call that
//! is bandwidth spent to transmit nothing whenever the picture is still.
//!
//! The ceiling still matters, because congestion control moves it while the call runs, so
//! the target is re-sent as a misc parameter whenever it changes.

use std::sync::Arc;

use elementium_types::{I420Frame, PlaintextMedia};
use libva_sys::va_display_drm as va;

use super::display::Display;
use super::image::SurfaceUpload;
use super::jpeg::JpegDecoder;
use super::jpeg_headers::Subsampling;
use super::resource::{Buffer, Config, Context, SurfaceId, SurfacePool};
use super::status::{Status, check};
use super::vpp::Converter;
use crate::video::{EncodedFrame, PixelLayout, VideoCodec, VideoEncoder};

/// Macroblocks are 16x16, always, in every H.264 profile.
const MACROBLOCK: u32 = 16;

/// Surfaces in each pool.
///
/// Two of each, alternating: while one frame is being encoded the previous frame's
/// reconstruction must still be intact, because that is what the next frame predicts from.
/// Without B-frames nothing else is in flight, so a third would sit idle.
const SURFACE_COUNT: usize = 2;

/// Frames between keyframes.
///
/// Matches the software encoder. A receiver joining mid-call cannot decode anything until
/// one arrives, but it can ask, and sending them unprompted every few seconds spends
/// bandwidth on a case that already has a mechanism.
const KEYFRAME_INTERVAL: u32 = 150;

/// H.264 NAL unit types used here.
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_SLICE: u8 = 1;
const NAL_IDR_SLICE: u8 = 5;

/// Level 4.0 covers 1080p30, comfortably above what a call negotiates.
///
/// Claiming a level below what the stream actually requires produces a bitstream that
/// conforming decoders may refuse outright.
const LEVEL_IDC: u8 = 40;

/// Sets `frame_num` to wrap at 256, and the picture order count at 512.
///
/// These appear in both the parameter buffer and the header we write, and the two must
/// agree: the encoder counts one way and the decoder reads the other.
const LOG2_MAX_FRAME_NUM_MINUS4: u8 = 4;
const LOG2_MAX_POC_LSB_MINUS4: u8 = 4;

/// Initial quantiser.
///
/// 26 is the midpoint of H.264's range and what rate control adjusts away from immediately;
/// it matters only for the first frame or two.
const INITIAL_QP: u8 = 26;

/// The coded buffer is sized from the frame, since a keyframe of noise is the worst case.
///
/// Half the uncompressed frame is far more than a keyframe ever needs and costs one
/// allocation for the life of the encoder.
fn coded_buffer_size(width: u32, height: u32) -> usize {
    let pixels = usize::try_from(width)
        .unwrap_or(0)
        .saturating_mul(usize::try_from(height).unwrap_or(0));
    pixels.saturating_mul(3).saturating_div(4).max(64 * 1024)
}

/// Where each NAL unit's header byte sits, by offset into `stream`.
///
/// A start code is `00 00 01`, optionally preceded by another zero; the byte after it is
/// the NAL header.
fn nal_header_offsets(stream: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut zeroes = 0_usize;
    for (index, &byte) in stream.iter().enumerate() {
        if byte == 1 && zeroes >= 2 {
            if let Some(header) = index.checked_add(1) {
                offsets.push(header);
            }
            zeroes = 0;
        } else if byte == 0 {
            zeroes = zeroes.saturating_add(1);
        } else {
            zeroes = 0;
        }
    }
    offsets
}

/// An H.264 encoder running on the GPU.
///
/// Fields are declared in the order they must be destroyed: the context owns its config, and
/// both go before the display that the `Arc` keeps alive underneath them.
pub struct H264Encoder {
    context: Context,
    /// Surfaces frames are uploaded into and read from.
    inputs: SurfacePool,
    /// Surfaces the encoder writes its reconstruction into, and predicts from.
    ///
    /// Separate from the inputs, and that separation is not an optimisation. VAAPI takes
    /// the source surface through `vaBeginPicture` and the reconstruction target through
    /// `CurrPic.picture_id`; naming the same surface for both has the encoder write its
    /// reconstruction over the picture it is reading. The first frames of every stream
    /// decoded to black until these were pulled apart.
    reconstructions: SurfacePool,
    /// One staging image, reused: see [`SurfaceUpload`].
    upload: SurfaceUpload,
    /// The accelerated MJPEG path, built on first use.
    ///
    /// Lazily, because it depends on the JPEG's own subsampling and nothing knows that
    /// until a frame arrives. Once built it is kept; a camera does not change subsampling
    /// mid-stream, and one that did is refused rather than silently re-negotiated.
    mjpeg: Option<MjpegPath>,
    /// Set once the accelerated path has been tried and found unavailable.
    ///
    /// Without this a machine with no JPEG decoder would attempt to build one on every
    /// single frame, which is a driver round trip thirty times a second to learn the same
    /// answer.
    mjpeg_unavailable: bool,
    display: Arc<Display>,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    /// Frames encoded, which drives `frame_num` and the keyframe cadence.
    frame_index: u64,
    /// Set by a receiver's keyframe request; cleared when one is produced.
    force_keyframe: bool,
}

impl H264Encoder {
    /// Build an encoder for frames of this size.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if no render node offers H.264 encoding, if the driver will not
    /// generate its own headers, or if any resource cannot be created. Every one of those
    /// means the caller should fall back to software rather than fail the call.
    pub fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self, Status> {
        let display = Arc::new(Display::open_any().ok_or(Status {
            operation: "no usable render node",
            code: -1,
        })?);

        let mut attributes = [
            va::VAConfigAttrib {
                type_: va::VAConfigAttribType_VAConfigAttribRTFormat,
                value: va::VA_RT_FORMAT_YUV420,
            },
            va::VAConfigAttrib {
                type_: va::VAConfigAttribType_VAConfigAttribRateControl,
                value: va::VA_RC_VBR,
            },
            // Declaring which headers we will supply. Leaving this out, or setting it to
            // NONE, has the driver ignore every packed header sent afterwards -- silently,
            // since the buffers are still accepted.
            va::VAConfigAttrib {
                type_: va::VAConfigAttribType_VAConfigAttribEncPackedHeaders,
                value: va::VA_ENC_PACKED_HEADER_SEQUENCE | va::VA_ENC_PACKED_HEADER_PICTURE,
            },
        ];
        let config = Config::for_encoding(
            &display,
            va::VAProfile_VAProfileH264ConstrainedBaseline,
            &mut attributes,
        )?;

        let mut inputs = SurfacePool::new_nv12(&display, width, height, SURFACE_COUNT)?;
        let reconstructions = SurfacePool::new_nv12(&display, width, height, SURFACE_COUNT)?;
        let context = Context::new(config, width, height, inputs.raw())?;
        let upload = SurfaceUpload::new(&display, width, height)?;

        Ok(Self {
            context,
            inputs,
            reconstructions,
            upload,
            mjpeg: None,
            mjpeg_unavailable: false,
            display,
            width,
            height,
            bitrate_kbps,
            frame_index: 0,
            force_keyframe: false,
        })
    }

    /// Whether the next frame will be a keyframe.
    ///
    /// The first frame always is -- nothing can decode a stream that opens with a
    /// difference against a picture that was never sent.
    fn wants_keyframe(&self) -> bool {
        self.force_keyframe
            || self.frame_index == 0
            || self
                .frame_index
                .checked_rem(u64::from(KEYFRAME_INTERVAL))
                .is_some_and(|r| r == 0)
    }

    /// Width and height in macroblocks, which is how H.264 counts them.
    ///
    /// Saturating rather than wrapping: a frame wide enough to overflow a `u16` of
    /// macroblocks is a million pixels across and no encoder would accept it, but silently
    /// wrapping to a small number would encode a sliver of the picture and report success.
    fn size_in_macroblocks(&self) -> (u16, u16) {
        (
            u16::try_from(self.width.div_ceil(MACROBLOCK)).unwrap_or(u16::MAX),
            u16::try_from(self.height.div_ceil(MACROBLOCK)).unwrap_or(u16::MAX),
        )
    }

    /// The surface frame `index` is uploaded into.
    fn input_for(&self, index: u64) -> Option<SurfaceId> {
        Self::slot(&self.inputs, index)
    }

    /// The surface frame `index`'s reconstruction is written into.
    fn reconstruction_for(&self, index: u64) -> Option<SurfaceId> {
        Self::slot(&self.reconstructions, index)
    }

    /// Alternate through a pool, so the previous frame's surface is still intact.
    fn slot(pool: &SurfacePool, index: u64) -> Option<SurfaceId> {
        let slot = usize::try_from(index.checked_rem(u64::try_from(SURFACE_COUNT).ok()?)?).ok()?;
        pool.surfaces().get(slot).copied()
    }
}

/// Decoding JPEG on the GPU and getting the result into a surface the encoder reads.
struct MjpegPath {
    decoder: JpegDecoder,
    /// Only present when the JPEG's subsampling is not already what the encoder reads.
    ///
    /// A 4:2:0 JPEG decodes straight into NV12 and needs nothing further; 4:2:2, which is
    /// what most UVC webcams emit, needs a pass through the post-processor.
    converter: Option<Converter>,
}

impl VideoEncoder for H264Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn preferred_input(&self) -> PixelLayout {
        // NV12 is what the surface holds; the upload interleaves I420's chroma on its way
        // in, so a caller with either can be served without an extra pass.
        PixelLayout::Nv12
    }

    fn encode(&mut self, frame: &I420Frame) -> Result<Vec<EncodedFrame>, String> {
        if frame.width() != self.width || frame.height() != self.height {
            return Err(format!(
                "frame is {}x{}, encoder is {}x{}",
                frame.width(),
                frame.height(),
                self.width,
                self.height
            ));
        }
        self.encode_frame(frame).map_err(|e| e.to_string())
    }

    fn encode_mjpeg(&mut self, jpeg: &[u8]) -> Option<Result<Vec<EncodedFrame>, String>> {
        if self.mjpeg_unavailable {
            return None;
        }
        if self.mjpeg.is_none() {
            match self.build_mjpeg_path(jpeg) {
                Ok(path) => self.mjpeg = Some(path),
                Err(reason) => {
                    // Once, not per frame: the answer will not change, and the caller
                    // decoding in software from here on is a normal outcome rather than a
                    // fault.
                    tracing::info!(
                        reason = %reason,
                        "no accelerated MJPEG path; frames will be decoded on the CPU"
                    );
                    self.mjpeg_unavailable = true;
                    return None;
                }
            }
        }
        Some(self.encode_jpeg(jpeg).map_err(|e| e.to_string()))
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), String> {
        self.bitrate_kbps = kbps;
        Ok(())
    }
}

impl H264Encoder {
    /// Build the accelerated MJPEG path for the kind of JPEG this camera sends.
    ///
    /// The display is shared with the encoder's, which is not an optimisation but a
    /// requirement: surfaces belong to a display, and a decoder that opened its own would
    /// force every frame back through system memory -- the entire cost being avoided.
    fn build_mjpeg_path(&self, jpeg: &[u8]) -> Result<MjpegPath, Status> {
        let headers = super::jpeg_headers::parse(jpeg).map_err(|_| Status {
            operation: "not a baseline JPEG",
            code: -1,
        })?;
        if u32::from(headers.width) != self.width || u32::from(headers.height) != self.height {
            return Err(Status {
                operation: "JPEG is not the negotiated size",
                code: -1,
            });
        }
        let subsampling = headers.subsampling().ok_or(Status {
            operation: "unrecognised chroma subsampling",
            code: -1,
        })?;

        let display = self.context.display();
        let decoder = JpegDecoder::with_display(display, self.width, self.height, subsampling)?;
        // 4:2:0 decodes straight into NV12, which is what the encoder reads.
        let converter = match subsampling {
            Subsampling::Yuv420 => None,
            Subsampling::Yuv422 | Subsampling::Yuv444 => {
                Some(Converter::new_to_nv12(display, self.width, self.height)?)
            }
        };
        tracing::info!(
            width = self.width,
            height = self.height,
            ?subsampling,
            converting = converter.is_some(),
            "MJPEG will be decoded on the GPU"
        );
        Ok(MjpegPath { decoder, converter })
    }

    /// Decode a JPEG on the GPU and encode the result, without it leaving the GPU.
    fn encode_jpeg(&mut self, jpeg: &[u8]) -> Result<Vec<EncodedFrame>, Status> {
        let mut path = self.mjpeg.take().ok_or(Status {
            operation: "no MJPEG path",
            code: -1,
        })?;
        let result = (|| {
            let decoded = path.decoder.decode(jpeg)?;
            // 4:2:0 decodes straight into what the encoder reads; anything else needs
            // the post-processor.
            path.converter
                .as_mut()
                .map_or(Ok(decoded), |converter| converter.to_nv12(decoded))
        })();
        self.mjpeg = Some(path);

        let source = result?;
        self.encode_surface(source)
    }

    /// Upload one frame and encode it.
    fn encode_frame(&mut self, frame: &I420Frame) -> Result<Vec<EncodedFrame>, Status> {
        let source = self.input_for(self.frame_index).ok_or(Status {
            operation: "no input surface available",
            code: -1,
        })?;

        // Upload first: the surface must hold the picture before the encode is submitted.
        self.upload.upload(frame, source)?;

        self.submit_and_collect(source)
    }

    /// Encode a picture already sitting in a surface.
    ///
    /// The accelerated path's entry point: the frame was decoded on the GPU and never
    /// existed in ordinary memory, so there is nothing to upload.
    fn encode_surface(&mut self, source: SurfaceId) -> Result<Vec<EncodedFrame>, Status> {
        self.submit_and_collect(source)
    }

    /// Everything after the picture is in a surface: parameters, submit, read back.
    fn submit_and_collect(&mut self, source: SurfaceId) -> Result<Vec<EncodedFrame>, Status> {
        let index = self.frame_index;
        let keyframe = self.wants_keyframe();
        let reconstruction = self.reconstruction_for(index).ok_or(Status {
            operation: "no reconstruction surface available",
            code: -1,
        })?;

        let coded = Buffer::empty(
            &self.context,
            va::VABufferType_VAEncCodedBufferType,
            coded_buffer_size(self.width, self.height),
        )?;

        // Grouped, and each group rendered on its own. libva's reference encoder submits a
        // packed header's descriptor and its bytes as a pair in their own `vaRenderPicture`
        // call, and this driver ignores them when they arrive batched with everything else
        // -- silently, since the buffers are still accepted and the encode still succeeds.
        //
        // Held until after `vaEndPicture`: libva reads these buffers when the picture is
        // ended, not when they are rendered, so dropping one early frees memory the driver
        // is about to read.
        let mut groups: Vec<Vec<Buffer>> = Vec::with_capacity(6);
        let mut sequence = self.sequence_parameters();
        if keyframe {
            groups.push(vec![Buffer::new(
                &self.context,
                va::VABufferType_VAEncSequenceParameterBufferType,
                &mut sequence,
            )?]);
        }
        let mut rate_control = self.rate_control_parameters();
        groups.push(vec![Buffer::new(
            &self.context,
            va::VABufferType_VAEncMiscParameterBufferType,
            &mut rate_control,
        )?]);
        let mut picture = self.picture_parameters(reconstruction, coded.id(), index, keyframe);
        groups.push(vec![Buffer::new(
            &self.context,
            va::VABufferType_VAEncPictureParameterBufferType,
            &mut picture,
        )?]);

        let mut slice = self.slice_parameters(index, keyframe);
        groups.push(vec![Buffer::new(
            &self.context,
            va::VABufferType_VAEncSliceParameterBufferType,
            &mut slice,
        )?]);

        self.submit(source, &groups)?;
        drop(groups);

        let data = self.assemble(&self.read_coded(&coded)?, keyframe);
        self.frame_index = self.frame_index.saturating_add(1);
        if keyframe {
            self.force_keyframe = false;
        }

        if data.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![EncodedFrame {
            data: PlaintextMedia::from_encoder(data),
            is_keyframe: keyframe,
            pts: i64::try_from(index).unwrap_or(i64::MAX),
        }])
    }

    /// Begin, render and end one picture, submitting each group of buffers separately.
    fn submit(&self, surface: SurfaceId, groups: &[Vec<Buffer>]) -> Result<(), Status> {
        let handle = self.display.handle();
        let context = self.context.id().raw();

        // SAFETY: the surface belongs to this display's pool and the context is live.
        check(
            unsafe { va::vaBeginPicture(handle, context, surface.raw()) },
            "vaBeginPicture",
        )?;

        for group in groups {
            let mut ids: Vec<va::VABufferID> = group.iter().map(|b| b.id().raw()).collect();
            // SAFETY: `ids` holds live buffer ids created against this context, and libva
            // reads exactly the count it is given.
            let render = unsafe {
                va::vaRenderPicture(
                    handle,
                    context,
                    ids.as_mut_ptr(),
                    i32::try_from(ids.len()).unwrap_or(0),
                )
            };
            if let Err(e) = check(render, "vaRenderPicture") {
                // The picture was begun and must be ended even on failure, or the context
                // is left mid-picture and every later frame fails too.
                // SAFETY: a picture is in progress on this context.
                unsafe {
                    va::vaEndPicture(handle, context);
                }
                return Err(e);
            }
        }

        // SAFETY: a picture is in progress on this context.
        check(unsafe { va::vaEndPicture(handle, context) }, "vaEndPicture")?;
        // SAFETY: the surface was just submitted; this blocks until the GPU is done with it.
        check(
            unsafe { va::vaSyncSurface(handle, surface.raw()) },
            "vaSyncSurface",
        )
    }

    /// Map the coded buffer and copy the bitstream out.
    ///
    /// The driver returns a linked list of segments rather than one block, because a coded
    /// picture need not be contiguous. Reading only the first is a bitstream truncated at
    /// whatever boundary the driver happened to choose, which decodes for a while and then
    /// falls apart.
    fn read_coded(&self, coded: &Buffer) -> Result<Vec<u8>, Status> {
        let handle = self.display.handle();
        let mut mapped: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `coded` is a live buffer on this display; libva writes the address of the
        // first segment into `mapped`.
        check(
            unsafe { va::vaMapBuffer(handle, coded.id().raw(), std::ptr::addr_of_mut!(mapped)) },
            "vaMapBuffer",
        )?;

        let mut out = Vec::new();
        let mut segment = mapped.cast::<va::VACodedBufferSegment>();
        while !segment.is_null() {
            // SAFETY: non-null and produced by libva as a valid segment header.
            let current = unsafe { &*segment };
            let size = usize::try_from(current.size).unwrap_or(0);
            if !current.buf.is_null() && size > 0 {
                // SAFETY: the driver guarantees `buf` points to `size` readable bytes for
                // as long as the buffer is mapped, which is until the unmap below.
                out.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(current.buf.cast::<u8>(), size)
                });
            }
            segment = current.next.cast::<va::VACodedBufferSegment>();
        }

        // SAFETY: mapped immediately above, unmapped exactly once.
        check(
            unsafe { va::vaUnmapBuffer(handle, coded.id().raw()) },
            "vaUnmapBuffer",
        )?;
        Ok(out)
    }

    /// Turn what the driver produced into a stream a decoder will accept.
    ///
    /// Two things are missing from it. The parameter sets, which are prepended to every
    /// keyframe rather than sent once, because a receiver that joins mid-call has nothing
    /// to decode with until a keyframe arrives and must get them with it. And the NAL
    /// header byte of each slice, which the driver leaves as zero -- a value the standard
    /// reserves and no decoder accepts, so patching it cannot be confused with overwriting
    /// something meaningful.
    fn assemble(&self, coded: &[u8], keyframe: bool) -> Vec<u8> {
        // 3 for a picture others predict from, which without B-frames is every picture.
        let ref_idc = 3;
        let nal_type = if keyframe { NAL_IDR_SLICE } else { NAL_SLICE };

        let mut out = Vec::with_capacity(coded.len().saturating_add(64));
        if keyframe {
            out.extend_from_slice(&self.sequence_header());
            out.extend_from_slice(&Self::picture_header());
        }

        let start = out.len();
        out.extend_from_slice(coded);
        for offset in nal_header_offsets(coded) {
            if let Some(byte @ 0) = start.checked_add(offset).and_then(|i| out.get_mut(i)) {
                *byte = ((ref_idc & 0x3) << 5) | (nal_type & 0x1F);
            }
        }
        out
    }

    /// The sequence parameter set, as the bytes of a NAL unit.
    ///
    /// Every field here has to agree with [`H264Encoder::sequence_parameters`]: the driver
    /// encodes according to the parameter buffer and the decoder reads according to this,
    /// so a disagreement between them is a picture that decodes to the wrong size or not at
    /// all.
    fn sequence_header(&self) -> Vec<u8> {
        use super::bitstream::{BitWriter, nal_unit};
        let (mbs_wide, mbs_high) = self.size_in_macroblocks();
        let mut w = BitWriter::new();

        // Constrained Baseline is profile 66 with constraint_set1_flag set. The profile in
        // the header must match the one the config was created with, or a decoder applies
        // the wrong syntax to everything after it.
        w.bits(66, 8); // profile_idc
        w.bit(true); // constraint_set0_flag: Baseline
        w.bit(true); // constraint_set1_flag: Main -- both, hence "Constrained"
        w.bit(false); // constraint_set2_flag
        w.bit(false); // constraint_set3_flag
        w.bit(false); // constraint_set4_flag
        w.bit(false); // constraint_set5_flag
        w.bits(0, 2); // reserved_zero_2bits
        w.bits(u32::from(LEVEL_IDC), 8);
        w.ue(0); // seq_parameter_set_id
        w.ue(u32::from(LOG2_MAX_FRAME_NUM_MINUS4));
        w.ue(0); // pic_order_cnt_type
        w.ue(u32::from(LOG2_MAX_POC_LSB_MINUS4));
        w.ue(1); // max_num_ref_frames
        w.bit(false); // gaps_in_frame_num_value_allowed_flag
        w.ue(u32::from(mbs_wide).saturating_sub(1));
        w.ue(u32::from(mbs_high).saturating_sub(1));
        w.bit(true); // frame_mbs_only_flag: progressive
        w.bit(true); // direct_8x8_inference_flag

        // Macroblocks are 16 pixels, so a frame whose size is not a multiple of 16 is coded
        // larger and cropped. 720 is fine; 1080 is not, and without this a decoder shows
        // eight rows of whatever the encoder padded with.
        let coded_width = u32::from(mbs_wide).saturating_mul(MACROBLOCK);
        let coded_height = u32::from(mbs_high).saturating_mul(MACROBLOCK);
        let cropping = coded_width != self.width || coded_height != self.height;
        w.bit(cropping);
        if cropping {
            w.ue(0); // frame_crop_left_offset
            // In units of two luma samples horizontally for 4:2:0, and two more vertically
            // again because frame_mbs_only_flag is set.
            w.ue(coded_width.saturating_sub(self.width).saturating_div(2));
            w.ue(0); // frame_crop_top_offset
            w.ue(coded_height.saturating_sub(self.height).saturating_div(2));
        }
        w.bit(false); // vui_parameters_present_flag
        w.trailing_bits();

        // nal_ref_idc 3: a parameter set is needed to decode everything that follows.
        nal_unit(3, NAL_SPS, &w.finish())
    }

    /// The picture parameter set, as the bytes of a NAL unit.
    ///
    /// Nothing here varies with the frame size, so it is the same for every encoder.
    fn picture_header() -> Vec<u8> {
        use super::bitstream::{BitWriter, nal_unit};
        let mut w = BitWriter::new();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        // CAVLC: Constrained Baseline does not permit CABAC.
        w.bit(false); // entropy_coding_mode_flag
        w.bit(false); // bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1: one slice group
        w.ue(0); // num_ref_idx_l0_default_active_minus1: one reference
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.bit(false); // weighted_pred_flag
        w.bits(0, 2); // weighted_bipred_idc
        w.se(i32::from(INITIAL_QP).saturating_sub(26)); // pic_init_qp_minus26
        w.se(0); // pic_init_qs_minus26
        w.se(0); // chroma_qp_index_offset
        w.bit(true); // deblocking_filter_control_present_flag
        w.bit(false); // constrained_intra_pred_flag
        w.bit(false); // redundant_pic_cnt_present_flag
        w.trailing_bits();
        nal_unit(3, NAL_PPS, &w.finish())
    }

    /// The sequence parameters, sent with every keyframe.
    fn sequence_parameters(&self) -> va::VAEncSequenceParameterBufferH264 {
        let (mbs_wide, mbs_high) = self.size_in_macroblocks();
        // SAFETY: a C struct of integers and bitfields, for which all-zeroes is valid; every
        // field that matters is set below.
        let mut seq: va::VAEncSequenceParameterBufferH264 = unsafe { std::mem::zeroed() };
        seq.seq_parameter_set_id = 0;
        seq.level_idc = LEVEL_IDC;
        seq.intra_period = KEYFRAME_INTERVAL;
        seq.intra_idr_period = KEYFRAME_INTERVAL;
        // 1 means no B-frames: every picture is either I or P, which is what a real-time
        // call wants since B-frames trade latency for compression.
        seq.ip_period = 1;
        seq.bits_per_second = self.bitrate_kbps.saturating_mul(1000);
        seq.max_num_ref_frames = 1;
        seq.picture_width_in_mbs = mbs_wide;
        seq.picture_height_in_mbs = mbs_high;
        // SAFETY: `seq_fields` is a union of a bitfield struct and the `u32` covering it.
        // Both variants are plain data with no invalid bit patterns, and the whole struct
        // was zeroed above, so reading or writing either is defined.
        unsafe {
            seq.seq_fields.bits.set_chroma_format_idc(1); // 4:2:0
            seq.seq_fields.bits.set_frame_mbs_only_flag(1); // progressive
            seq.seq_fields.bits.set_direct_8x8_inference_flag(1);
            seq.seq_fields
                .bits
                .set_log2_max_frame_num_minus4(u32::from(LOG2_MAX_FRAME_NUM_MINUS4));
            seq.seq_fields.bits.set_pic_order_cnt_type(0);
            seq.seq_fields
                .bits
                .set_log2_max_pic_order_cnt_lsb_minus4(u32::from(LOG2_MAX_POC_LSB_MINUS4));
        }

        // Macroblocks are 16 pixels, so a frame whose height is not a multiple of 16 is
        // coded taller and cropped. 720 is fine; 1080 is not, and without this the decoder
        // shows eight rows of whatever the encoder padded with.
        let coded_height = u32::from(mbs_high).saturating_mul(MACROBLOCK);
        let coded_width = u32::from(mbs_wide).saturating_mul(MACROBLOCK);
        if coded_width != self.width || coded_height != self.height {
            seq.frame_cropping_flag = 1;
            // In units of two luma samples for 4:2:0, which is why these are halved.
            seq.frame_crop_right_offset = coded_width.saturating_sub(self.width).saturating_div(2);
            seq.frame_crop_bottom_offset =
                coded_height.saturating_sub(self.height).saturating_div(2);
        }
        seq
    }

    /// The picture parameters, sent with every frame.
    fn picture_parameters(
        &self,
        reconstruction: SurfaceId,
        coded: super::resource::BufferId,
        index: u64,
        keyframe: bool,
    ) -> va::VAEncPictureParameterBufferH264 {
        // SAFETY: as `sequence_parameters`.
        let mut pic: va::VAEncPictureParameterBufferH264 = unsafe { std::mem::zeroed() };
        let frame_num = u16::try_from(index.checked_rem(65_536).unwrap_or(0)).unwrap_or(0);

        // Where the encoder writes its reconstruction, which is *not* the source surface.
        pic.CurrPic.picture_id = reconstruction.raw();
        pic.CurrPic.frame_idx = u32::from(frame_num);
        pic.CurrPic.flags = 0;
        pic.CurrPic.TopFieldOrderCnt = i32::from(frame_num).saturating_mul(2);
        pic.CurrPic.BottomFieldOrderCnt = pic.CurrPic.TopFieldOrderCnt;

        // Unused entries must say so explicitly. A zeroed entry is surface 0, which is a
        // real surface, and the encoder would predict from whatever it happens to hold.
        for reference in &mut pic.ReferenceFrames {
            reference.picture_id = va::VA_INVALID_ID;
            reference.flags = va::VA_PICTURE_H264_INVALID;
        }
        if let (false, Some(previous), Some(slot)) = (
            keyframe,
            index
                .checked_sub(1)
                .and_then(|i| self.reconstruction_for(i)),
            pic.ReferenceFrames.first_mut(),
        ) {
            slot.picture_id = previous.raw();
            slot.frame_idx = u32::from(frame_num.saturating_sub(1));
            slot.flags = va::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
        }

        pic.coded_buf = coded.raw();
        pic.pic_parameter_set_id = 0;
        pic.seq_parameter_set_id = 0;
        pic.frame_num = frame_num;
        pic.pic_init_qp = INITIAL_QP;
        pic.last_picture = 0;
        // SAFETY: as in `sequence_parameters` -- a union of plain data, already zeroed.
        unsafe {
            pic.pic_fields.bits.set_idr_pic_flag(u32::from(keyframe));
            // Every frame is a reference: with no B-frames, each predicts from the last.
            pic.pic_fields.bits.set_reference_pic_flag(1);
            // CAVLC, not CABAC: Constrained Baseline does not permit CABAC, and asking for
            // it is refused by the driver rather than silently downgraded.
            pic.pic_fields.bits.set_entropy_coding_mode_flag(0);
            pic.pic_fields
                .bits
                .set_deblocking_filter_control_present_flag(1);
        }
        pic
    }

    /// The slice parameters: one slice covering the whole frame.
    fn slice_parameters(&self, index: u64, keyframe: bool) -> va::VAEncSliceParameterBufferH264 {
        let (mbs_wide, mbs_high) = self.size_in_macroblocks();
        // SAFETY: as `sequence_parameters`.
        let mut slice: va::VAEncSliceParameterBufferH264 = unsafe { std::mem::zeroed() };
        slice.macroblock_address = 0;
        slice.num_macroblocks = u32::from(mbs_wide).saturating_mul(u32::from(mbs_high));
        slice.macroblock_info = va::VA_INVALID_ID;
        // 2 is I, 0 is P, in the slice_type values H.264 defines.
        slice.slice_type = if keyframe { 2 } else { 0 };
        slice.pic_parameter_set_id = 0;
        slice.idr_pic_id = 0;
        let frame_num = u16::try_from(index.checked_rem(65_536).unwrap_or(0)).unwrap_or(0);
        slice.pic_order_cnt_lsb = frame_num.saturating_mul(2);
        slice.num_ref_idx_active_override_flag = 1;
        slice.num_ref_idx_l0_active_minus1 = 0;
        slice.slice_qp_delta = 0;
        slice.disable_deblocking_filter_idc = 0;

        for entry in slice
            .RefPicList0
            .iter_mut()
            .chain(slice.RefPicList1.iter_mut())
        {
            entry.picture_id = va::VA_INVALID_ID;
            entry.flags = va::VA_PICTURE_H264_INVALID;
        }
        if let (false, Some(previous), Some(slot)) = (
            keyframe,
            index
                .checked_sub(1)
                .and_then(|i| self.reconstruction_for(i)),
            slice.RefPicList0.first_mut(),
        ) {
            slot.picture_id = previous.raw();
            slot.frame_idx = u32::from(frame_num.saturating_sub(1));
            slot.flags = va::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
        }
        slice
    }

    /// Constant-bitrate rate control, re-sent every frame so a changed target takes effect.
    fn rate_control_parameters(&self) -> RateControlBuffer {
        // SAFETY: a C struct of integers and bitfields; all-zeroes is valid.
        let mut rc: va::VAEncMiscParameterRateControl = unsafe { std::mem::zeroed() };
        rc.bits_per_second = self.bitrate_kbps.saturating_mul(1000);
        // The target as a percentage of that ceiling. Below 100 the encoder is allowed to
        // undershoot rather than pad, which is the whole point of choosing VBR: a still
        // picture should cost almost nothing instead of a full frame of filler.
        rc.target_percentage = 90;
        rc.window_size = 1000;
        rc.initial_qp = u32::from(INITIAL_QP);
        rc.min_qp = 0;
        RateControlBuffer {
            header: va::VAEncMiscParameterBuffer {
                type_: va::VAEncMiscParameterType_VAEncMiscParameterTypeRateControl,
                data: __IncompleteArrayField::new(),
            },
            payload: rc,
        }
    }
}

use libva_sys::va_display_drm::__IncompleteArrayField;

/// A misc parameter buffer with its payload immediately after it.
///
/// libva declares these as a header with a flexible array member, which Rust cannot express
/// -- the payload is simply expected to follow the header in memory. `repr(C)` on a struct
/// of exactly those two parts produces that layout, and keeps the size and the contents in
/// step with each other, which passing a hand-computed byte count would not.
#[repr(C)]
struct RateControlBuffer {
    header: va::VAEncMiscParameterBuffer,
    payload: va::VAEncMiscParameterRateControl,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic
)]
mod tests {
    use super::{H264Encoder, coded_buffer_size};
    use crate::video::{VideoCodec, VideoEncoder};
    use elementium_types::I420Frame;

    const W: u32 = 640;
    const H: u32 = 480;
    const WIDTH: usize = 640;
    const HEIGHT: usize = 480;

    fn frame(value: u8) -> I420Frame {
        let (w, h) = (WIDTH, HEIGHT);
        I420Frame::from_planes(
            W,
            H,
            &vec![value; w * h],
            &vec![128; (w / 2) * (h / 2)],
            &vec![128; (w / 2) * (h / 2)],
            0,
        )
        .expect("planes match the geometry")
    }

    /// Skip where there is no H.264 encoder: CI has no GPU, and a test that fails there
    /// teaches people to ignore failures.
    fn encoder() -> Option<H264Encoder> {
        H264Encoder::new(W, H, 2000).ok()
    }

    /// Split an Annex-B stream into NAL units and report their types.
    ///
    /// The bitstream is the product, and "the call returned Ok" says nothing about it. A
    /// wrong parameter buffer produces bytes that look like a bitstream and are not one.
    fn nal_types(stream: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 <= stream.len() {
            let is_start = stream[i] == 0
                && stream[i + 1] == 0
                && ((stream[i + 2] == 1) || (stream[i + 2] == 0 && stream[i + 3] == 1));
            if is_start {
                let header = if stream[i + 2] == 1 { i + 3 } else { i + 4 };
                if let Some(&byte) = stream.get(header) {
                    types.push(byte & 0x1F);
                }
                i = header + 1;
            } else {
                i += 1;
            }
        }
        types
    }

    /// The first frame must be a keyframe carrying its own parameter sets, because nothing
    /// downstream can decode a stream that opens with anything else.
    #[test]
    fn the_first_frame_is_a_decodable_keyframe() {
        let Some(mut encoder) = encoder() else {
            return;
        };
        assert_eq!(encoder.codec(), VideoCodec::H264);

        let packets = VideoEncoder::encode(&mut encoder, &frame(80)).expect("encode");
        let first = packets.first().expect("a packet");
        assert!(
            first.is_keyframe,
            "the first frame must be independently decodable"
        );

        let types = nal_types(first.data.as_bytes());
        assert!(types.contains(&7), "no SPS in the first frame: {types:?}");
        assert!(types.contains(&8), "no PPS in the first frame: {types:?}");
        assert!(
            types.contains(&5),
            "no IDR slice in the first frame: {types:?}"
        );
    }

    /// Later frames must predict rather than repeat: a P slice, and materially smaller
    /// than the keyframe. An encoder that emitted keyframes throughout would "work" and
    /// spend several times the bandwidth.
    #[test]
    fn later_frames_are_predicted_and_smaller() {
        let Some(mut encoder) = encoder() else {
            return;
        };
        let key = VideoEncoder::encode(&mut encoder, &frame(80)).expect("encode");
        let key_len = key.first().expect("a packet").data.as_bytes().len();

        let next = VideoEncoder::encode(&mut encoder, &frame(80)).expect("encode");
        let packet = next.first().expect("a packet");
        assert!(!packet.is_keyframe);

        let types = nal_types(packet.data.as_bytes());
        assert!(
            types.contains(&1),
            "expected a non-IDR slice, got {types:?}"
        );
        assert!(
            packet.data.as_bytes().len() < key_len,
            "a predicted frame of identical content ({} bytes) should be far smaller than \
             the keyframe ({key_len} bytes)",
            packet.data.as_bytes().len()
        );
    }

    /// A receiver's keyframe request must actually produce one, since that is the only
    /// mechanism by which a participant joining mid-call ever sees a picture.
    #[test]
    fn a_requested_keyframe_arrives() {
        let Some(mut encoder) = encoder() else {
            return;
        };
        let _ = VideoEncoder::encode(&mut encoder, &frame(80)).expect("encode");
        let _ = VideoEncoder::encode(&mut encoder, &frame(90)).expect("encode");

        encoder.request_keyframe();
        let packets = VideoEncoder::encode(&mut encoder, &frame(100)).expect("encode");
        let packet = packets.first().expect("a packet");
        assert!(packet.is_keyframe);
        assert!(
            nal_types(packet.data.as_bytes()).contains(&5),
            "a requested keyframe must contain an IDR slice"
        );
    }

    /// A frame of the wrong size must be refused rather than encoded into a corrupt
    /// picture or written past the end of a surface.
    #[test]
    fn a_frame_of_the_wrong_size_is_refused() {
        let Some(mut encoder) = encoder() else {
            return;
        };
        let (w, h) = (320_usize, 240_usize);
        let small = I420Frame::from_planes(
            320,
            240,
            &vec![0; w * h],
            &vec![0; (w / 2) * (h / 2)],
            &vec![0; (w / 2) * (h / 2)],
            0,
        )
        .expect("planes match the geometry");
        assert!(VideoEncoder::encode(&mut encoder, &small).is_err());
    }

    /// The only test that proves the encoder works: a reference decoder reconstructs the
    /// pictures we put in.
    ///
    /// Everything else here checks the stream is *shaped* like H.264 -- the right NAL
    /// types in the right order -- and a stream can be perfectly shaped and decode to
    /// black. This one did, for a while: the encoder was writing its reconstruction over
    /// the surface it was reading, and every structural assertion still passed.
    ///
    /// A moving bar rather than a flat picture, because a flat one decodes identically
    /// whether or not motion, prediction and plane offsets are right.
    #[test]
    fn a_reference_decoder_reconstructs_what_was_encoded() {
        const FRAMES: usize = 8;
        const BAR: usize = 20;

        let Some(mut encoder) = encoder() else {
            return;
        };
        let Some(dir) = scratch_dir() else {
            return; // No ffmpeg on this machine; the structural tests still ran.
        };

        let mut stream = Vec::new();
        for n in 0..FRAMES {
            let packets = VideoEncoder::encode(&mut encoder, &bar_frame(n * BAR)).expect("encode");
            for packet in &packets {
                stream.extend_from_slice(packet.data.as_bytes());
            }
        }

        let bitstream_path = dir.join("encoded.h264");
        let decoded = dir.join("decoded.yuv");
        std::fs::write(&bitstream_path, &stream).expect("write the stream");
        let run = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&bitstream_path)
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-y"])
            .arg(&decoded)
            .output()
            .expect("run ffmpeg");
        assert!(
            run.status.success(),
            "the decoder rejected the stream: {}",
            String::from_utf8_lossy(&run.stderr)
        );

        let raw = std::fs::read(&decoded).expect("read the decoded frames");
        let frame_bytes = WIDTH * HEIGHT * 3 / 2;
        assert_eq!(
            raw.len() / frame_bytes,
            FRAMES,
            "decoded {} frames, encoded {FRAMES}",
            raw.len() / frame_bytes
        );

        for n in 0..FRAMES {
            let luma = &raw[n * frame_bytes..n * frame_bytes + WIDTH * HEIGHT];
            let bright: Vec<usize> = (0..HEIGHT)
                .filter(|row| luma[row * WIDTH + WIDTH / 2] > 150)
                .collect();
            let expected: Vec<usize> = (n * BAR..(n * BAR + BAR).min(HEIGHT)).collect();
            assert_eq!(
                bright, expected,
                "frame {n}: the bar is not where it was drawn, so the picture is not the \
                 one that was encoded"
            );
            // Quantisation moves samples by a step or two; a wrong plane moves them by a
            // hundred.
            let background = luma[(HEIGHT - 4) * WIDTH];
            assert!(
                background.abs_diff(80) < 10,
                "frame {n}: background decoded as {background}, encoded as 80"
            );
        }
    }

    /// The accelerated path end to end: a JPEG goes in, H.264 comes out, and the picture
    /// survives.
    ///
    /// This is the one that proves the whole chain -- GPU JPEG decode, colour conversion
    /// where the subsampling needs it, and encode -- without the frame ever existing in
    /// ordinary memory. Every stage reports success independently while producing a black
    /// or shifted picture, so the only check worth making is what a decoder reconstructs.
    ///
    /// Run for both 4:2:0 and 4:2:2 because they take different routes: the first decodes
    /// straight into NV12, the second needs the post-processor. A camera emits one or the
    /// other and we do not get to choose.
    #[test]
    fn a_jpeg_encodes_without_being_decoded_on_the_cpu() {
        const FRAMES: usize = 6;
        const BAR: usize = 40;

        let Some(dir) = scratch_dir() else {
            return; // No ffmpeg to check against.
        };
        for (name, sampling) in [
            ("4:2:0", jpeg_encoder::SamplingFactor::F_2_2),
            ("4:2:2", jpeg_encoder::SamplingFactor::F_2_1),
        ] {
            let Some(mut encoder) = encoder() else {
                return;
            };
            let mut stream = Vec::new();
            let mut accelerated = true;
            for n in 0..FRAMES {
                let jpeg = jpeg_of_bar(n * BAR, sampling);
                match encoder.encode_mjpeg(&jpeg) {
                    Some(Ok(packets)) => {
                        for packet in &packets {
                            stream.extend_from_slice(packet.data.as_bytes());
                        }
                    }
                    Some(Err(e)) => panic!("{name}: the accelerated path failed: {e}"),
                    None => {
                        accelerated = false;
                        break;
                    }
                }
            }
            if !accelerated {
                continue; // No JPEG decoding here; the software path covers it.
            }

            let path = dir.join(format!("mjpeg-{}.h264", name.replace(':', "")));
            let decoded = dir.join(format!("mjpeg-{}.yuv", name.replace(':', "")));
            std::fs::write(&path, &stream).expect("write");
            let run = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-y"])
                .arg(&decoded)
                .output()
                .expect("run ffmpeg");
            assert!(
                run.status.success(),
                "{name}: the decoder rejected the stream: {}",
                String::from_utf8_lossy(&run.stderr)
            );

            let raw = std::fs::read(&decoded).expect("read");
            let frame_bytes = WIDTH * HEIGHT * 3 / 2;
            assert_eq!(raw.len() / frame_bytes, FRAMES, "{name}: wrong frame count");
            for n in 0..FRAMES {
                let luma = &raw[n * frame_bytes..n * frame_bytes + WIDTH * HEIGHT];
                let bright: Vec<usize> = (0..HEIGHT)
                    .filter(|row| luma[row * WIDTH + WIDTH / 2] > 150)
                    .collect();
                let expected: Vec<usize> = (n * BAR..(n * BAR + BAR).min(HEIGHT)).collect();
                assert_eq!(
                    bright, expected,
                    "{name}: frame {n}: the bar is not where it was drawn, so the picture \
                     that came out is not the one that went in"
                );
            }
        }
    }

    /// A JPEG of the standard test picture, at a chosen subsampling.
    fn jpeg_of_bar(row: usize, sampling: jpeg_encoder::SamplingFactor) -> Vec<u8> {
        let mut rgb = vec![0_u8; WIDTH * HEIGHT * 3];
        for r in 0..HEIGHT {
            let value = if (row..row + 40).contains(&r) {
                230
            } else {
                60
            };
            for c in 0..WIDTH {
                let at = (r * WIDTH + c) * 3;
                rgb[at] = value;
                rgb[at + 1] = value;
                rgb[at + 2] = value;
            }
        }
        let mut jpeg = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, 92);
        encoder.set_sampling_factor(sampling);
        encoder
            .encode(
                &rgb,
                u16::try_from(WIDTH).expect("width"),
                u16::try_from(HEIGHT).expect("height"),
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("encode");
        jpeg
    }

    /// A frame with a horizontal bar starting at `row`.
    fn bar_frame(row: usize) -> I420Frame {
        let mut luma = vec![80_u8; WIDTH * HEIGHT];
        for r in row..(row + 20).min(HEIGHT) {
            for c in 0..WIDTH {
                luma[r * WIDTH + c] = 200;
            }
        }
        I420Frame::from_planes(
            W,
            H,
            &luma,
            &vec![90; (WIDTH / 2) * (HEIGHT / 2)],
            &vec![240; (WIDTH / 2) * (HEIGHT / 2)],
            0,
        )
        .expect("planes match the geometry")
    }

    /// A directory to work in, or `None` where there is no `ffmpeg` to check against.
    fn scratch_dir() -> Option<std::path::PathBuf> {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        let dir = std::env::temp_dir().join("elementium-h264-test");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    /// The coded buffer has to hold a keyframe of the worst-case content, and never be so
    /// small that a frame is truncated.
    #[test]
    fn the_coded_buffer_is_large_enough_for_a_keyframe() {
        assert!(coded_buffer_size(1280, 720) >= 1280 * 720 / 2);
        assert!(
            coded_buffer_size(16, 16) >= 64 * 1024,
            "a floor for tiny frames"
        );
    }
}
