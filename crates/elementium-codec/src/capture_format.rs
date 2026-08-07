//! Choosing what to ask the camera for.
//!
//! A webcam offers several formats and the pipeline picks one. The picking is usually
//! treated as a capture-side detail, but it is not: the cost of a format is entirely
//! determined by what happens to the frame *afterwards*, and that is decided by the encoder.
//!
//! The clearest case is MJPEG. Encoding in software, it is the worst format on offer — a
//! full JPEG decode on the CPU, which profiling put at 37.6% of the capture path. Encoding
//! in hardware on a GPU with a JPEG decode block, it is the *best* format on offer, because
//! the compressed bytes go straight to the GPU and the CPU never sees a pixel. Same camera,
//! same format, opposite conclusion.
//!
//! So this module does not rank formats. It ranks them *for a target*, where the target is
//! the encoder that was negotiated.
//!
//! # What is being minimised
//!
//! CPU time, deliberately, over bus bandwidth or GPU time. This runs on machines that are
//! also running games, and on laptops where it is battery — a cycle we do not spend is the
//! whole point of the application existing. Bus bandwidth breaks the tie, because it is
//! what limits the resolution and frame rate a camera can reach.
//!
//! # Why feasibility is not modelled here
//!
//! An uncompressed 720p60 stream is 110 MB/s and will not fit down USB 2.0. Nothing here
//! tries to work that out, because it does not have to: a camera advertises each format
//! only at the frame rates it can actually sustain, so a request constrained by size and
//! frame rate simply will not match a format the link cannot carry. Guessing at bus speeds
//! would be less accurate than the camera's own answer.
//!
//! The one thing that can defeat this is `PipeWire` inserting a converter to satisfy a
//! request the camera cannot meet natively, which moves the conversion cost into another
//! process where it is invisible to us rather than removing it. The negotiated format is
//! logged for exactly that reason; enumerating the node's native formats before choosing
//! would settle it properly, and is the obvious next refinement.

use crate::hardware::EncoderBackend;
use crate::video::PixelLayout;

/// A format a camera can deliver frames in.
///
/// Both compressed and raw, because the choice between them is the decision being made and
/// splitting them into two types would hide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureFormat {
    /// Motion JPEG: one baseline JPEG per frame.
    ///
    /// Roughly a tenth the bytes of raw, which is what lets a USB camera reach 720p60 at
    /// all. The decode is expensive on the CPU and nearly free on a GPU that has a JPEG
    /// block, which is why this format's rank swings so far between targets.
    Mjpeg,
    /// Planar YUV 4:2:0, three separate planes. What software encoders want.
    I420,
    /// Semi-planar YUV 4:2:0: a luma plane and one interleaved chroma plane. What hardware
    /// encoders want.
    Nv12,
    /// Packed YUV 4:2:2, two pixels per four bytes as `Y0 U Y1 V`. What UVC webcams
    /// produce natively when not compressing.
    Yuy2,
    /// 3 bytes per pixel, red or blue first.
    Rgb,
    Bgr,
    /// 4 bytes per pixel, red or blue first, 4th byte unused or alpha.
    Rgbx,
    Bgrx,
}

/// What has to happen to a captured frame before the encoder can read it.
///
/// Ordered cheapest first, so the derived `Ord` is the ranking. CPU cost dominates, because
/// that is the currency being saved; where two options cost the CPU the same, the one that
/// asks less of the GPU comes first. The variants are named for the work rather than for a
/// number of microseconds because the ratios hold across machines while the absolute
/// figures do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConversionCost {
    /// The captured buffer is handed to the encoder as it stands.
    None,
    /// Decoded by the GPU. The CPU moves only the compressed bytes — for 720p MJPEG,
    /// around a hundred kilobytes rather than a megabyte and a half.
    GpuDecode,
    /// Bytes are moved but nothing is decoded: an upload to a GPU surface, or a chroma
    /// de-interleave.
    Copy,
    /// An upload, and then a conversion the GPU's post-processor performs.
    ///
    /// Costs the CPU exactly what [`ConversionCost::Copy`] does, and is ranked below it
    /// because it additionally occupies the post-processor and adds a pass of latency.
    /// Reaching for it when a format needing neither is available would be waste.
    CopyAndGpuConvert,
    /// A conversion touching every pixel on the CPU, such as 4:2:2 to 4:2:0.
    Convert,
    /// A full decode on the CPU.
    Decode,
    /// A CPU decode and then a conversion. The worst case that still works.
    DecodeAndConvert,
    /// Cannot be used to reach this encoder at all.
    Unusable,
}

/// The encoder a captured frame is destined for.
///
/// Carried as a struct rather than three arguments because these three facts are only ever
/// meaningful together: the layout without the backend cannot distinguish a software
/// encoder wanting NV12 from a hardware one, and the JPEG flag matters only for hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeTarget {
    /// The layout the encoder reads.
    pub layout: PixelLayout,
    /// Where encoding happens.
    pub backend: EncoderBackend,
    /// Whether the GPU can decode MJPEG into a surface itself.
    ///
    /// Only consulted for hardware backends: decoding on the GPU to then read the result
    /// back for a software encoder would cost far more than decoding on the CPU, because
    /// reading from GPU memory is uncached and slow.
    pub gpu_jpeg_decode: bool,
}

impl EncodeTarget {
    /// A target for software encoding, which always reads I420.
    #[must_use]
    pub const fn software() -> Self {
        Self {
            layout: PixelLayout::I420,
            backend: EncoderBackend::Software,
            gpu_jpeg_decode: false,
        }
    }

    /// The target for encoding `codec` at this size on this machine.
    ///
    /// Resolved from what this build can actually use rather than from what the hardware
    /// reports, so a GPU whose encoder is not implemented yet still yields a software
    /// target — and with it the software format preference, which is the correct one while
    /// the CPU is doing the encoding.
    ///
    /// The layout is inferred from the backend rather than asked of the encoder, because
    /// the format has to be negotiated with the camera before an encoder exists. Every
    /// hardware encoder on every platform reads NV12 and every software one reads I420, so
    /// the inference holds; an encoder that broke it would report so through
    /// [`VideoEncoder::preferred_input`](crate::video::VideoEncoder::preferred_input) and
    /// the frame would be converted rather than mis-negotiated.
    #[must_use]
    pub fn negotiated(codec: crate::video::VideoCodec, width: u32, height: u32) -> Self {
        let caps = crate::hardware::capabilities();
        let backend =
            crate::hardware::selectable_backend_from(&caps.encoders, codec, width, height);
        Self {
            layout: if backend.is_hardware() {
                PixelLayout::Nv12
            } else {
                PixelLayout::I420
            },
            backend,
            // Only meaningful for a hardware target: decoding on the GPU to read the
            // result back for a CPU encoder would cost far more than decoding on the CPU.
            gpu_jpeg_decode: backend.is_hardware() && caps.jpeg_decode,
        }
    }
}

impl CaptureFormat {
    /// Every format worth asking a camera for.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Mjpeg,
            Self::I420,
            Self::Nv12,
            Self::Yuy2,
            Self::Rgb,
            Self::Bgr,
            Self::Rgbx,
            Self::Bgrx,
        ]
    }

    /// Bytes this format spends on 16 pixels, as a proxy for what it costs the link.
    ///
    /// Exact for the raw layouts. For MJPEG it is an estimate from a 10:1 ratio, which is
    /// what UVC cameras achieve at the quality they encode at; it is used only to rank
    /// against the raw formats, and any ratio better than about 2:1 gives the same order.
    #[must_use]
    pub const fn wire_cost_per_16_pixels(self) -> u32 {
        match self {
            Self::Mjpeg => 3,
            Self::I420 | Self::Nv12 => 24,
            Self::Yuy2 => 32,
            Self::Rgb | Self::Bgr => 48,
            Self::Rgbx | Self::Bgrx => 64,
        }
    }

    /// What it costs to get a frame from this format to `target`.
    #[must_use]
    pub const fn conversion_cost(self, target: EncodeTarget) -> ConversionCost {
        if target.backend.is_hardware() {
            self.hardware_cost(target.gpu_jpeg_decode)
        } else {
            self.software_cost(target.layout)
        }
    }

    /// Cost of reaching a CPU encoder reading `layout`.
    const fn software_cost(self, layout: PixelLayout) -> ConversionCost {
        match (layout, self) {
            // Already what the encoder reads: the decoder's buffer is adopted whole.
            (PixelLayout::I420, Self::I420) | (PixelLayout::Nv12, Self::Nv12) => {
                ConversionCost::None
            }
            // Same subsampling, different interleaving: half the frame is touched.
            (PixelLayout::I420, Self::Nv12) | (PixelLayout::Nv12, Self::I420) => {
                ConversionCost::Copy
            }
            // MJPEG decodes to 4:2:0 planes, which is I420. Reaching NV12 interleaves after.
            (PixelLayout::I420, Self::Mjpeg) => ConversionCost::Decode,
            (PixelLayout::Nv12, Self::Mjpeg) => ConversionCost::DecodeAndConvert,
            // Every remaining layout is a per-pixel conversion, whichever way round.
            (_, Self::Yuy2 | Self::Rgb | Self::Bgr | Self::Rgbx | Self::Bgrx) => {
                ConversionCost::Convert
            }
        }
    }

    /// Cost of reaching a GPU encoder, which always reads an NV12 surface.
    ///
    /// Every raw layout costs the CPU the same — an upload — but only NV12 is what the
    /// encoder reads. The rest additionally occupy the post-processor for a conversion
    /// pass, which is not free even though the CPU does not pay for it.
    const fn hardware_cost(self, gpu_jpeg_decode: bool) -> ConversionCost {
        match self {
            Self::Mjpeg if gpu_jpeg_decode => ConversionCost::GpuDecode,
            // The CPU decodes it and then uploads the result: the worst of both.
            Self::Mjpeg => ConversionCost::DecodeAndConvert,
            // Uploaded and encoded as it stands.
            Self::Nv12 => ConversionCost::Copy,
            Self::I420 | Self::Yuy2 | Self::Rgb | Self::Bgr | Self::Rgbx | Self::Bgrx => {
                ConversionCost::CopyAndGpuConvert
            }
        }
    }
}

/// The formats worth asking for, best first, for a given encoder.
///
/// Every usable format is included rather than only the best one. A camera that cannot
/// produce our first choice at the requested size and frame rate would otherwise fail to
/// negotiate at all, and a working call in a worse format beats no camera.
#[must_use]
pub fn preference(target: EncodeTarget) -> Vec<CaptureFormat> {
    let mut formats: Vec<CaptureFormat> = CaptureFormat::all()
        .iter()
        .copied()
        .filter(|f| f.conversion_cost(target) != ConversionCost::Unusable)
        .collect();
    formats.sort_by_key(|f| (f.conversion_cost(target), f.wire_cost_per_16_pixels()));
    formats
}

#[cfg(test)]
mod tests {
    use super::{CaptureFormat, ConversionCost, EncodeTarget, preference};
    use crate::hardware::EncoderBackend;
    use crate::video::PixelLayout;

    fn hardware(gpu_jpeg_decode: bool) -> EncodeTarget {
        EncodeTarget {
            layout: PixelLayout::Nv12,
            backend: EncoderBackend::Vaapi,
            gpu_jpeg_decode,
        }
    }

    /// The claim the module exists to make: the same format is ranked differently
    /// depending on where encoding happens, so capture cannot choose alone.
    #[test]
    fn mjpeg_is_worst_in_software_and_best_on_a_gpu_that_decodes_it() {
        let software = preference(EncodeTarget::software());
        let accelerated = preference(hardware(true));

        assert_eq!(
            software.last(),
            Some(&CaptureFormat::Mjpeg),
            "a CPU encoder pays a full JPEG decode, which is the most expensive option"
        );
        assert_eq!(
            accelerated.first(),
            Some(&CaptureFormat::Mjpeg),
            "a GPU that decodes JPEG never shows the CPU a pixel"
        );
    }

    /// Without a JPEG block on the GPU, MJPEG loses its advantage entirely: the CPU
    /// decodes it and then uploads the result.
    #[test]
    fn mjpeg_is_not_preferred_on_hardware_that_cannot_decode_it() {
        let order = preference(hardware(false));
        assert_eq!(
            order.first(),
            Some(&CaptureFormat::Nv12),
            "an upload of what the encoder already wants is the cheapest path"
        );
        assert_eq!(order.last(), Some(&CaptureFormat::Mjpeg));
    }

    /// On a GPU encoder, NV12 and I420 carry identical bytes and cost the CPU the same
    /// upload — but only NV12 is what the encoder reads, so I420 additionally occupies the
    /// post-processor. Without that distinction the two tie and declaration order decides,
    /// which is not a decision at all.
    #[test]
    fn the_layout_the_gpu_encoder_reads_beats_an_equally_sized_one_it_does_not() {
        let target = hardware(false);
        assert_eq!(
            CaptureFormat::Nv12.conversion_cost(target),
            ConversionCost::Copy
        );
        assert_eq!(
            CaptureFormat::I420.conversion_cost(target),
            ConversionCost::CopyAndGpuConvert
        );
        assert_eq!(
            CaptureFormat::Nv12.wire_cost_per_16_pixels(),
            CaptureFormat::I420.wire_cost_per_16_pixels(),
            "the link cannot be what separates them"
        );
    }

    /// A software encoder reading I420 given I420 must do nothing at all. This is the
    /// zero-copy path, stated as a cost so it cannot regress silently.
    #[test]
    fn matching_layouts_cost_nothing() {
        assert_eq!(
            CaptureFormat::I420.conversion_cost(EncodeTarget::software()),
            ConversionCost::None
        );
        assert_eq!(
            preference(EncodeTarget::software()).first(),
            Some(&CaptureFormat::I420)
        );
    }

    /// Ranking is by CPU cost first. NV12 needs a chroma de-interleave and YUY2 needs a
    /// per-pixel conversion, so NV12 comes first despite carrying the same bytes as I420.
    #[test]
    fn cheaper_cpu_work_outranks_a_cheaper_link() {
        let order = preference(EncodeTarget::software());
        let position = |f: CaptureFormat| order.iter().position(|&x| x == f);
        assert!(position(CaptureFormat::Nv12) < position(CaptureFormat::Yuy2));
        assert!(
            position(CaptureFormat::Yuy2) < position(CaptureFormat::Rgbx),
            "equal CPU cost is broken by the link, and YUY2 carries half the bytes"
        );
    }

    /// Every format has to be offered, not just the best: a camera that cannot produce our
    /// first choice at the requested size and rate must still negotiate something.
    #[test]
    fn every_format_is_offered() {
        for target in [EncodeTarget::software(), hardware(true), hardware(false)] {
            assert_eq!(
                preference(target).len(),
                CaptureFormat::all().len(),
                "a format was dropped from the offer for {target:?}"
            );
        }
    }

    /// The link cost has to reflect reality, because it is what breaks ties between
    /// formats that cost the CPU the same. Checked against the definitions rather than
    /// trusted: 4:2:0 is 1.5 bytes per pixel, 4:2:2 is 2, RGB is 3, RGBX is 4.
    #[test]
    fn link_costs_match_the_formats() {
        assert_eq!(CaptureFormat::I420.wire_cost_per_16_pixels(), 24);
        assert_eq!(CaptureFormat::Yuy2.wire_cost_per_16_pixels(), 32);
        assert_eq!(CaptureFormat::Rgb.wire_cost_per_16_pixels(), 48);
        assert_eq!(CaptureFormat::Rgbx.wire_cost_per_16_pixels(), 64);
        assert!(
            CaptureFormat::Mjpeg.wire_cost_per_16_pixels()
                < CaptureFormat::I420.wire_cost_per_16_pixels(),
            "compressed frames are what let a USB camera reach 720p60"
        );
    }
}
