//! Decoding MJPEG on the GPU.
//!
//! This is the change that makes the hardware path worth having. Software encoding pays a
//! full JPEG decode per frame on the CPU — profiling put `decode_mcu` at 37.6% of the
//! capture path, the largest single cost in the application. Decoding on the GPU moves all
//! of it: the CPU copies the compressed bytes, around a hundred kilobytes, and never touches
//! a pixel.
//!
//! It only pays off when the *encoder* is also on the GPU. Decoding here to read the result
//! back for a software encoder would cost far more than it saves, because reading from GPU
//! memory is uncached and slow. So this is reached only when both ends of the pipeline are
//! hardware — see [`crate::capture_format`], where MJPEG's rank swings on exactly that.
//!
//! # What the GPU is given
//!
//! Not the file. A hardware decoder takes the quantisation tables, the Huffman tables, the
//! frame geometry, the scan's component mapping and a pointer to the entropy-coded bytes;
//! [`super::jpeg_headers`] extracts those. Everything the parser refuses — progressive,
//! 16-bit tables, 12-bit samples — is refused before a buffer is allocated.

use std::sync::Arc;

use libva_sys::va_display_drm as va;

use super::display::Display;
use super::jpeg_headers::{JpegHeaders, Subsampling, parse};
use super::resource::{Buffer, Config, Context, SurfaceId, SurfacePool};
use super::status::{Status, check};

/// Surfaces decoded frames land in.
///
/// Two, alternating: one being decoded into while the previous is still being read by the
/// encoder or the converter.
const SURFACE_COUNT: usize = 2;

/// A JPEG decoder running on the GPU.
pub struct JpegDecoder {
    context: Context,
    surfaces: SurfacePool,
    display: Arc<Display>,
    width: u32,
    height: u32,
    subsampling: Subsampling,
    next: u64,
}

impl JpegDecoder {
    /// Build a decoder for frames of this size and subsampling.
    ///
    /// The subsampling is fixed at construction because it decides the surface format, and
    /// a camera does not change it mid-stream. One that did would be caught by
    /// [`JpegDecoder::decode`] rather than silently decoded into the wrong layout.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if no device offers JPEG decoding, or if the resources cannot be
    /// created. Either means the caller should decode on the CPU instead.
    pub fn new(width: u32, height: u32, subsampling: Subsampling) -> Result<Self, Status> {
        let display = Arc::new(Display::open_any().ok_or(Status {
            operation: "no usable render node",
            code: -1,
        })?);
        Self::with_display(&display, width, height, subsampling)
    }

    /// Build a decoder sharing an existing display.
    ///
    /// Sharing matters: the decoded surface is handed to an encoder on the same display, and
    /// surfaces cannot cross displays. A decoder that opened its own would force a
    /// round trip through system memory, which is the entire cost being avoided.
    ///
    /// # Errors
    ///
    /// As [`JpegDecoder::new`].
    pub fn with_display(
        display: &Arc<Display>,
        width: u32,
        height: u32,
        subsampling: Subsampling,
    ) -> Result<Self, Status> {
        let format = match subsampling {
            Subsampling::Yuv420 => va::VA_RT_FORMAT_YUV420,
            Subsampling::Yuv422 => va::VA_RT_FORMAT_YUV422,
            Subsampling::Yuv444 => va::VA_RT_FORMAT_YUV444,
        };
        let mut attributes = [va::VAConfigAttrib {
            type_: va::VAConfigAttribType_VAConfigAttribRTFormat,
            value: format,
        }];
        let config = Config::for_decoding(
            display,
            va::VAProfile_VAProfileJPEGBaseline,
            &mut attributes,
        )?;

        let mut surfaces = SurfacePool::new_format(
            display,
            format,
            fourcc(subsampling),
            width,
            height,
            SURFACE_COUNT,
        )?;
        let context = Context::new(config, width, height, surfaces.raw())?;

        Ok(Self {
            context,
            surfaces,
            display: Arc::clone(display),
            width,
            height,
            subsampling,
            next: 0,
        })
    }

    /// The display these surfaces belong to, for whatever consumes them next.
    #[must_use]
    pub const fn display(&self) -> &Arc<Display> {
        &self.display
    }

    /// What the decoded surfaces hold.
    #[must_use]
    pub const fn subsampling(&self) -> Subsampling {
        self.subsampling
    }

    /// Decode one JPEG, returning the surface it landed in.
    ///
    /// The surface belongs to this decoder and is reused, so a caller must finish with it
    /// before the next call but need not free it.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the data is not a baseline JPEG this decoder was built for, or
    /// if the driver refuses it.
    pub fn decode(&mut self, jpeg: &[u8]) -> Result<SurfaceId, Status> {
        let headers = parse(jpeg).map_err(|_| Status {
            operation: "the data is not a baseline JPEG",
            code: -1,
        })?;
        if u32::from(headers.width) != self.width || u32::from(headers.height) != self.height {
            return Err(Status {
                operation: "JPEG is not the negotiated size",
                code: -1,
            });
        }
        if headers.subsampling() != Some(self.subsampling) {
            return Err(Status {
                operation: "JPEG subsampling changed mid-stream",
                code: -1,
            });
        }

        let index = self.next;
        let slot = usize::try_from(
            index
                .checked_rem(u64::try_from(SURFACE_COUNT).unwrap_or(1))
                .unwrap_or(0),
        )
        .unwrap_or(0);
        let surface = self.surfaces.surfaces().get(slot).copied().ok_or(Status {
            operation: "no surface available",
            code: -1,
        })?;

        let scan = jpeg.get(headers.scan_data.clone()).ok_or(Status {
            operation: "scan data out of range",
            code: -1,
        })?;

        let mut picture = picture_parameters(&headers);
        let mut quantisers = quantisation_tables(&headers);
        let mut huffman = huffman_tables(&headers);
        let mut slice = slice_parameters(&headers, scan.len());
        // Held until after `vaEndPicture`: the driver reads them when the picture ends, not
        // when they are rendered.
        let buffers = [
            Buffer::new(
                &self.context,
                va::VABufferType_VAPictureParameterBufferType,
                &mut picture,
            )?,
            Buffer::new(
                &self.context,
                va::VABufferType_VAIQMatrixBufferType,
                &mut quantisers,
            )?,
            Buffer::new(
                &self.context,
                va::VABufferType_VAHuffmanTableBufferType,
                &mut huffman,
            )?,
            Buffer::new(
                &self.context,
                va::VABufferType_VASliceParameterBufferType,
                &mut slice,
            )?,
            // The entropy-coded bytes, and the only thing here proportional to the picture.
            Buffer::from_bytes(
                &self.context,
                va::VABufferType_VASliceDataBufferType,
                &mut scan.to_vec(),
            )?,
        ];

        self.submit(surface, &buffers)?;
        self.next = self.next.saturating_add(1);
        Ok(surface)
    }

    /// Begin, render and end one picture, then wait for it.
    fn submit(&self, surface: SurfaceId, buffers: &[Buffer]) -> Result<(), Status> {
        let handle = self.display.handle();
        let context = self.context.id().raw();

        // SAFETY: the surface belongs to this decoder's pool and the context is live.
        check(
            unsafe { va::vaBeginPicture(handle, context, surface.raw()) },
            "vaBeginPicture",
        )?;

        let mut ids: Vec<va::VABufferID> = buffers.iter().map(|b| b.id().raw()).collect();
        // SAFETY: live buffer ids created against this context; libva reads the count given.
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
        // Waited for here rather than by the caller: the surface is handed on immediately,
        // and a consumer reading it before the decode lands sees the previous frame.
        // SAFETY: the surface was just submitted.
        check(
            unsafe { va::vaSyncSurface(handle, surface.raw()) },
            "vaSyncSurface",
        )
    }
}

/// The fourcc a decoded surface holds, which follows the JPEG's own subsampling.
const fn fourcc(subsampling: Subsampling) -> u32 {
    match subsampling {
        // NV12 is what a 4:2:0 decode lands in and what an encoder reads, so that case
        // needs no conversion at all.
        Subsampling::Yuv420 => u32::from_le_bytes(*b"NV12"),
        Subsampling::Yuv422 => u32::from_le_bytes(*b"YUY2"),
        Subsampling::Yuv444 => u32::from_le_bytes(*b"444P"),
    }
}

/// Geometry and per-component sampling.
fn picture_parameters(headers: &JpegHeaders) -> va::VAPictureParameterBufferJPEGBaseline {
    // SAFETY: a C struct of integers; all-zeroes is valid and every used field is set.
    let mut picture: va::VAPictureParameterBufferJPEGBaseline = unsafe { std::mem::zeroed() };
    picture.picture_width = headers.width;
    picture.picture_height = headers.height;
    picture.num_components = u8::try_from(headers.components.len()).unwrap_or(0);
    // 0 is YUV. A JPEG with three components is YCbCr unless an Adobe marker says
    // otherwise, and cameras do not send one.
    picture.color_space = 0;
    picture.rotation = va::VA_ROTATION_NONE;
    for (slot, component) in picture.components.iter_mut().zip(&headers.components) {
        slot.component_id = component.id;
        slot.h_sampling_factor = component.horizontal_sampling;
        slot.v_sampling_factor = component.vertical_sampling;
        slot.quantiser_table_selector = component.quantiser;
    }
    picture
}

/// The quantisation tables, with a flag saying which are present.
fn quantisation_tables(headers: &JpegHeaders) -> va::VAIQMatrixBufferJPEGBaseline {
    // SAFETY: as `picture_parameters`.
    let mut matrix: va::VAIQMatrixBufferJPEGBaseline = unsafe { std::mem::zeroed() };
    for (index, table) in headers.quantisers.iter().enumerate() {
        let (Some(table), Some(load), Some(slot)) = (
            table.as_ref(),
            matrix.load_quantiser_table.get_mut(index),
            matrix.quantiser_table.get_mut(index),
        ) else {
            continue;
        };
        *load = 1;
        slot.copy_from_slice(table);
    }
    matrix
}

/// The Huffman tables, DC and AC paired as libva expects them.
fn huffman_tables(headers: &JpegHeaders) -> va::VAHuffmanTableBufferJPEGBaseline {
    // SAFETY: as `picture_parameters`.
    let mut tables: va::VAHuffmanTableBufferJPEGBaseline = unsafe { std::mem::zeroed() };
    for index in 0..2_usize {
        let (Some(load), Some(slot)) = (
            tables.load_huffman_table.get_mut(index),
            tables.huffman_table.get_mut(index),
        ) else {
            continue;
        };
        *load = 1;
        if let Some(dc) = headers.dc_tables.get(index).and_then(|t| t.as_ref()) {
            slot.num_dc_codes.copy_from_slice(&dc.counts);
            // A DC table has at most 12 values; the rest of the parsed buffer is padding.
            if let Some(values) = dc.values.get(..slot.dc_values.len()) {
                slot.dc_values.copy_from_slice(values);
            }
        }
        if let Some(ac) = headers.ac_tables.get(index).and_then(|t| t.as_ref()) {
            slot.num_ac_codes.copy_from_slice(&ac.counts);
            slot.ac_values.copy_from_slice(&ac.values);
        }
    }
    tables
}

/// The scan: where the data is, and which tables each component reads.
fn slice_parameters(
    headers: &JpegHeaders,
    scan_len: usize,
) -> va::VASliceParameterBufferJPEGBaseline {
    // SAFETY: as `picture_parameters`.
    let mut slice: va::VASliceParameterBufferJPEGBaseline = unsafe { std::mem::zeroed() };
    slice.slice_data_size = u32::try_from(scan_len).unwrap_or(0);
    // Zero because the buffer handed over holds the scan and nothing else.
    slice.slice_data_offset = 0;
    slice.slice_data_flag = va::VA_SLICE_DATA_FLAG_ALL;
    slice.num_components = u8::try_from(headers.scan.len()).unwrap_or(0);
    slice.restart_interval = headers.restart_interval;
    slice.num_mcus = headers.mcu_count();
    for (slot, component) in slice.components.iter_mut().zip(&headers.scan) {
        slot.component_selector = component.selector;
        slot.dc_table_selector = component.dc_table;
        slot.ac_table_selector = component.ac_table;
    }
    slice
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::many_single_char_names
)]
mod tests {
    use super::super::image::SurfaceDownload;
    use super::{JpegDecoder, Subsampling};

    // A realistic capture size. Hardware JPEG decoders have a minimum as well as a
    // maximum -- this one refuses 64x48 with "resolution not supported" -- so a fixture
    // small enough to be convenient is a fixture that skips the test entirely.
    const W: u32 = 640;
    const H: u32 = 480;
    const WIDTH: usize = 640;
    const HEIGHT: usize = 480;

    /// A JPEG of a picture with structure, so a decode that lands in the wrong place is
    /// visible rather than merely plausible.
    fn fixture(sampling: jpeg_encoder::SamplingFactor) -> (Vec<u8>, Vec<u8>) {
        let (w, h) = (WIDTH, HEIGHT);
        let mut rgb = vec![0_u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let at = (y * w + x) * 3;
                // A gradient with a bright band, which survives compression and moves if
                // the geometry is read wrongly.
                rgb[at] = u8::try_from(x / 3).unwrap_or(255);
                rgb[at + 1] = if (160..320).contains(&y) { 220 } else { 60 };
                rgb[at + 2] = u8::try_from(y / 2).unwrap_or(255);
            }
        }
        let mut jpeg = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, 90);
        encoder.set_sampling_factor(sampling);
        encoder
            .encode(
                &rgb,
                u16::try_from(w).expect("w"),
                u16::try_from(h).expect("h"),
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("encode");
        (jpeg, rgb)
    }

    /// The GPU must decode to the same picture libjpeg-turbo does.
    ///
    /// Compared against a software decode of the same file rather than against itself: a
    /// hardware decode that succeeds and produces the wrong picture is the failure worth
    /// catching, and it cannot be caught by checking a status.
    ///
    /// The two will not be identical -- different IDCT implementations differ by a step or
    /// two per sample -- so the comparison is a mean absolute difference, which a wrong
    /// picture blows past by an order of magnitude.
    #[test]
    fn a_decoded_frame_matches_a_software_decode() {
        let (jpeg, _) = fixture(jpeg_encoder::SamplingFactor::F_2_2);
        let Ok(mut decoder) = JpegDecoder::new(W, H, Subsampling::Yuv420) else {
            return; // No JPEG decoding on this machine.
        };
        let surface = decoder.decode(&jpeg).expect("decode");

        let mut download = SurfaceDownload::new(decoder.display(), W, H).expect("a download image");
        let bytes = download.read(surface).expect("read the surface back");
        let layout = download.layout().expect("two planes");

        let reference = turbojpeg::decompress_to_yuv(&jpeg).expect("software decode");
        let (w, h) = (WIDTH, HEIGHT);
        let mut total = 0_u64;
        for row in 0..h {
            for col in 0..w {
                let gpu = u32::from(bytes[layout.luma_offset + row * layout.luma_pitch + col]);
                let cpu = u32::from(reference.pixels[row * w + col]);
                total += u64::from(gpu.abs_diff(cpu));
            }
        }
        let mean = total / u64::try_from(w * h).unwrap_or(1);
        assert!(
            mean < 8,
            "the GPU decode differs from the software decode by {mean} per sample on \
             average, which is a different picture rather than a different IDCT"
        );
    }

    /// A JPEG of the wrong size must be refused rather than decoded into a surface that
    /// cannot hold it.
    #[test]
    fn a_jpeg_of_the_wrong_size_is_refused() {
        let (jpeg, _) = fixture(jpeg_encoder::SamplingFactor::F_2_2);
        let Ok(mut decoder) = JpegDecoder::new(W * 2, H * 2, Subsampling::Yuv420) else {
            return;
        };
        assert!(decoder.decode(&jpeg).is_err());
    }

    /// A camera that changed subsampling mid-stream would decode into a surface of the
    /// wrong layout, putting chroma where luma belongs. Refused instead.
    #[test]
    fn a_change_of_subsampling_is_refused() {
        let (jpeg, _) = fixture(jpeg_encoder::SamplingFactor::F_2_1);
        let Ok(mut decoder) = JpegDecoder::new(W, H, Subsampling::Yuv420) else {
            return;
        };
        assert!(decoder.decode(&jpeg).is_err(), "4:2:2 into a 4:2:0 decoder");
    }

    /// Rubbish must be refused before any GPU resource is touched.
    #[test]
    fn rubbish_is_refused() {
        let Ok(mut decoder) = JpegDecoder::new(W, H, Subsampling::Yuv420) else {
            return;
        };
        assert!(decoder.decode(b"not a jpeg").is_err());
        assert!(decoder.decode(&[]).is_err());
    }
}
