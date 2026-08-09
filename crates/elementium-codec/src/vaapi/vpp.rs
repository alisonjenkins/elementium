//! Converting between pixel layouts on the GPU.
//!
//! A hardware JPEG decoder produces whatever the JPEG was: 4:2:2 for most UVC webcams,
//! 4:2:0 for most files. A hardware encoder reads NV12, which is 4:2:0. When those differ
//! something has to convert, and the post-processor is the only place that can do it
//! without the frame leaving the GPU.
//!
//! That is the whole reason this exists. Without it the accelerated path works only for
//! cameras that happen to emit 4:2:0, and every other camera falls back to decoding on the
//! CPU — which is the cost the accelerated path was built to remove.
//!
//! # Not a resize
//!
//! Scaling is deliberately absent even though the same call would do it. A call negotiates
//! one resolution and the encoder is built for it; a converter that quietly resized would
//! turn a geometry mismatch into a soft picture rather than an error, and geometry
//! mismatches are worth hearing about.

use std::sync::Arc;

use libva_sys::va_display_drm as va;

use super::display::Display;
use super::resource::{Buffer, Config, Context, SurfaceId, SurfacePool};
use super::status::{Status, check};

/// Output surfaces, alternating so the encoder can still be reading the previous one.
const SURFACE_COUNT: usize = 2;

/// A colour-space converter running on the GPU.
pub struct Converter {
    context: Context,
    outputs: SurfacePool,
    display: Arc<Display>,
    width: u32,
    height: u32,
    next: u64,
}

impl Converter {
    /// Build a converter producing NV12 surfaces of this size.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the device has no post-processor, or the resources cannot be
    /// created. Either means the caller must convert on the CPU instead.
    pub fn new_to_nv12(display: &Arc<Display>, width: u32, height: u32) -> Result<Self, Status> {
        // Post-processing hangs off a pseudo-profile rather than a codec, since it is not
        // tied to one.
        let mut attributes = [va::VAConfigAttrib {
            type_: va::VAConfigAttribType_VAConfigAttribRTFormat,
            value: va::VA_RT_FORMAT_YUV420,
        }];
        let config = Config::for_video_processing(display, &mut attributes)?;

        let mut outputs = SurfacePool::new_nv12(display, width, height, SURFACE_COUNT)?;
        let context = Context::new(config, width, height, outputs.raw())?;

        Ok(Self {
            context,
            outputs,
            display: Arc::clone(display),
            width,
            height,
            next: 0,
        })
    }

    /// Convert `source` into an NV12 surface and return it.
    ///
    /// The returned surface belongs to this converter and is reused, so a caller must be
    /// finished with it before the next call.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the driver refuses the conversion.
    pub fn to_nv12(&mut self, source: SurfaceId) -> Result<SurfaceId, Status> {
        let slot = usize::try_from(
            self.next
                .checked_rem(u64::try_from(SURFACE_COUNT).unwrap_or(1))
                .unwrap_or(0),
        )
        .unwrap_or(0);
        let output = self.outputs.surfaces().get(slot).copied().ok_or(Status::detected("no output surface available"))?;

        // Both regions cover the whole picture: this converts, it does not scale or crop.
        // Held in locals because the parameter buffer points at them and the driver reads
        // it when the picture ends, not when it is created.
        let region = va::VARectangle {
            x: 0,
            y: 0,
            width: u16::try_from(self.width).unwrap_or(u16::MAX),
            height: u16::try_from(self.height).unwrap_or(u16::MAX),
        };

        // SAFETY: a C struct of integers and pointers; all-zeroes is valid, and every field
        // the driver reads is set below.
        let mut pipeline: va::VAProcPipelineParameterBuffer = unsafe { std::mem::zeroed() };
        pipeline.surface = source.raw();
        pipeline.surface_region = std::ptr::addr_of!(region);
        pipeline.output_region = std::ptr::addr_of!(region);
        pipeline.output_background_color = 0xFF00_0000;
        // BT.601 both ways, which is what a webcam's JPEG carries and what a call's H.264
        // is interpreted as. Declaring one and meaning the other shifts every colour
        // slightly -- visible on skin tones and on nothing else, which makes it the kind of
        // fault that gets reported as "the picture looks a bit off".
        pipeline.surface_color_standard = va::_VAProcColorStandardType_VAProcColorStandardBT601;
        pipeline.output_color_standard = va::_VAProcColorStandardType_VAProcColorStandardBT601;
        pipeline.filter_flags = va::VA_FRAME_PICTURE;

        let buffer = Buffer::new(
            &self.context,
            va::VABufferType_VAProcPipelineParameterBufferType,
            &mut pipeline,
        )?;

        self.submit(output, &buffer)?;
        self.next = self.next.saturating_add(1);
        Ok(output)
    }

    /// Begin, render and end one conversion, then wait for it.
    fn submit(&self, output: SurfaceId, buffer: &Buffer) -> Result<(), Status> {
        let handle = self.display.handle();
        let context = self.context.id().raw();

        // SAFETY: the output belongs to this converter's pool and the context is live.
        check(
            unsafe { va::vaBeginPicture(handle, context, output.raw()) },
            "vaBeginPicture",
        )?;

        let mut ids = [buffer.id().raw()];
        // SAFETY: a live buffer id created against this context.
        let rendered = unsafe {
            va::vaRenderPicture(
                handle,
                context,
                ids.as_mut_ptr(),
                i32::try_from(ids.len()).unwrap_or(0),
            )
        };
        if let Err(e) = check(rendered, "vaRenderPicture") {
            // The picture must be ended even on failure, or the context is left mid-picture
            // and every later frame fails too.
            // SAFETY: a picture is in progress on this context.
            unsafe {
                va::vaEndPicture(handle, context);
            }
            return Err(e);
        }

        // SAFETY: a picture is in progress on this context.
        check(unsafe { va::vaEndPicture(handle, context) }, "vaEndPicture")?;
        // Waited for here: the surface is handed straight to an encoder, and one that read
        // it before the conversion landed would encode the previous frame.
        // SAFETY: the surface was just written.
        check(
            unsafe { va::vaSyncSurface(handle, output.raw()) },
            "vaSyncSurface",
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::many_single_char_names
)]
mod tests {
    use super::super::display::Display;
    use super::super::image::{SurfaceDownload, SurfaceUpload};
    use super::super::resource::SurfacePool;
    use super::Converter;
    use elementium_types::I420Frame;

    const W: u32 = 640;
    const H: u32 = 480;
    const WIDTH: usize = 640;
    const HEIGHT: usize = 480;

    /// Converting NV12 to NV12 must leave the picture alone.
    ///
    /// A weaker check than converting from 4:2:2, and the one that can be built without a
    /// decoder: it still exercises the whole pipeline -- config, context, parameter buffer,
    /// begin, render, end, sync -- and a converter that dropped, shifted or blanked the
    /// picture fails it. What it cannot catch is a colour-space error, which needs a real
    /// 4:2:2 source and is checked where the decoder feeds this.
    #[test]
    fn a_conversion_preserves_the_picture() {
        let Some(display) = Display::open_any().map(std::sync::Arc::new) else {
            return; // No GPU on this machine.
        };
        let Ok(pool) = SurfacePool::new_nv12(&display, W, H, 1) else {
            return;
        };
        let source = pool.surfaces().first().copied().expect("one surface");
        let Ok(mut upload) = SurfaceUpload::new(&display, W, H) else {
            return;
        };

        // A gradient with a band, so a shifted or blanked picture is visible.
        let mut luma = vec![0_u8; WIDTH * HEIGHT];
        for row in 0..HEIGHT {
            for col in 0..WIDTH {
                luma[row * WIDTH + col] = if (100..140).contains(&row) {
                    200
                } else {
                    u8::try_from(col / 4).unwrap_or(0)
                };
            }
        }
        let frame = I420Frame::from_planes(
            W,
            H,
            &luma,
            &vec![90; (WIDTH / 2) * (HEIGHT / 2)],
            &vec![240; (WIDTH / 2) * (HEIGHT / 2)],
            0,
        )
        .expect("planes match the geometry");
        upload.upload(&frame, source).expect("upload");

        let Ok(mut converter) = Converter::new_to_nv12(&display, W, H) else {
            return; // No post-processor on this machine.
        };
        let output = converter.to_nv12(source).expect("convert");

        let mut download = SurfaceDownload::new(&display, W, H).expect("download image");
        let bytes = download.read(output).expect("read back");
        let layout = download.layout().expect("two planes");

        for row in [0, 50, 120, 300, HEIGHT - 1] {
            for col in [0, 200, WIDTH - 1] {
                let got = bytes[layout.luma_offset + row * layout.luma_pitch + col];
                let want = luma[row * WIDTH + col];
                assert!(
                    got.abs_diff(want) <= 2,
                    "luma at {row},{col} came back {got}, was {want}"
                );
            }
        }
        assert_eq!(bytes[layout.chroma_offset], 90, "U survived the conversion");
        assert_eq!(
            bytes[layout.chroma_offset + 1],
            240,
            "V survived the conversion"
        );
    }

    /// The converter must exist before anything relies on it, and a machine without a
    /// post-processor must say so rather than producing surfaces that are never written.
    #[test]
    fn a_converter_reports_whether_it_can_be_built() {
        let Some(display) = Display::open_any().map(std::sync::Arc::new) else {
            return;
        };
        match Converter::new_to_nv12(&display, W, H) {
            Ok(converter) => assert_eq!((converter.width, converter.height), (W, H)),
            Err(e) => {
                // Acceptable, and the caller falls back -- but it must be a real reason.
                assert!(!e.describe().is_empty());
            }
        }
    }
}
