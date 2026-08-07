//! Getting a frame into a GPU surface, and reading one back out.
//!
//! This is the one place on the hardware path where the CPU still touches every pixel, so
//! how it is done matters more than the line count suggests.
//!
//! # Why uploads do not use `vaDeriveImage`
//!
//! There are two ways to fill a surface. `vaCreateImage` allocates an image in system
//! memory which `vaPutImage` then copies into the surface — two passes, the second inside
//! the driver. `vaDeriveImage` hands back an image *backed by the surface itself*, so
//! writing through its mapping should be one pass and no copy at all.
//!
//! It is not, on this driver. A derived image here is a scratch mapping: bytes written
//! through it read back correctly while it is still mapped and are gone once it is
//! unmapped. That was found by encoding a flat grey frame and decoding the result to a
//! uniformly black picture, then writing, unmapping, re-deriving and reading — 77 before,
//! 0 after.
//!
//! Reads through a derived image are no better here: they return zeroes whatever the
//! surface holds, which is worse than failing because a read-back built on one agrees with
//! itself and verifies nothing. So derived images are not used at all. Uploads go through
//! [`SurfaceUpload`], which creates one image and reuses it, and read-back goes through
//! `vaGetImage`.
//!
//! # Why the conversion happens during the write
//!
//! Frames arrive as I420 and encoders read NV12; the difference is only whether the chroma
//! samples are interleaved. Interleaving while writing into the staging image makes it free
//! — those bytes were being written regardless, just in a different order. Converting into
//! a separate buffer first would cost another whole pass for the same result.
//!
//! # The remaining copies
//!
//! Two, then: ours into the staging image, and the driver's into the surface. Removing both
//! means never putting the frame in ordinary memory at all — importing the camera's buffer
//! as a DMA-BUF so the GPU reads it where it lies. That is a different mechanism rather
//! than an optimisation of this one.

use std::sync::Arc;

use elementium_types::I420Frame;
use libva_sys::va_display_drm as va;

use super::display::Display;
use super::resource::SurfaceId;
use super::status::{Status, check};

/// NV12 as libva spells it.
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// A reusable staging image for getting frames into surfaces.
///
/// Created once and reused for the life of the encoder. Creating one per frame would
/// allocate and free GPU-visible memory thirty times a second to hold the same thing each
/// time.
pub struct SurfaceUpload {
    display: Arc<Display>,
    image: va::VAImage,
}

// SAFETY: a `VAImage` is a handle plus geometry, meaningful only against the display it was
// created on, which is held here and is itself `Send`. The encoder owns both and is not
// `Sync`, so no two threads reach one at once.
unsafe impl Send for SurfaceUpload {}

impl SurfaceUpload {
    /// Create a staging image of this size.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the driver will not create an NV12 image of this size.
    pub fn new(display: &Arc<Display>, width: u32, height: u32) -> Result<Self, Status> {
        let mut format = va::VAImageFormat {
            fourcc: FOURCC_NV12,
            byte_order: va::VA_LSB_FIRST,
            bits_per_pixel: 12,
            depth: 0,
            red_mask: 0,
            green_mask: 0,
            blue_mask: 0,
            alpha_mask: 0,
            va_reserved: [0; 4],
        };
        // SAFETY: `VAImage` is a plain C struct; libva fills every field it uses.
        let mut image: va::VAImage = unsafe { std::mem::zeroed() };
        // SAFETY: the display outlives the image, which holds it; libva writes only into
        // `image`, and reads the one format it is given.
        let status = unsafe {
            va::vaCreateImage(
                display.handle(),
                std::ptr::addr_of_mut!(format),
                i32::try_from(width).unwrap_or(0),
                i32::try_from(height).unwrap_or(0),
                std::ptr::addr_of_mut!(image),
            )
        };
        check(status, "vaCreateImage")?;
        Ok(Self {
            display: Arc::clone(display),
            image,
        })
    }

    /// Write `frame` into the staging image and copy it into `surface`.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the image cannot be mapped, if the frame does not fit, or if
    /// the driver refuses the copy.
    pub fn upload(&mut self, frame: &I420Frame, surface: SurfaceId) -> Result<(), Status> {
        let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `image.buf` belongs to an image created in `new` on this display.
        check(
            unsafe {
                va::vaMapBuffer(self.display.handle(), self.image.buf, std::ptr::addr_of_mut!(data))
            },
            "vaMapBuffer",
        )?;

        let written = {
            let mut view = ImageView {
                image: self.image,
                data: data.cast::<u8>(),
            };
            view.write_i420(frame)
        };

        // SAFETY: mapped immediately above. Unmapped whether or not the write succeeded --
        // leaving it mapped would fail every later frame.
        let unmapped = check(
            unsafe { va::vaUnmapBuffer(self.display.handle(), self.image.buf) },
            "vaUnmapBuffer",
        );
        written?;
        unmapped?;

        // SAFETY: the image and surface belong to this display, and the rectangle is the
        // whole image, whose geometry libva itself reported.
        check(
            unsafe {
                va::vaPutImage(
                    self.display.handle(),
                    surface.raw(),
                    self.image.image_id,
                    0,
                    0,
                    u32::from(self.image.width),
                    u32::from(self.image.height),
                    0,
                    0,
                    u32::from(self.image.width),
                    u32::from(self.image.height),
                )
            },
            "vaPutImage",
        )?;

        // Wait for the copy before the caller submits an encode reading this surface.
        //
        // `vaPutImage` is allowed to complete asynchronously, and without this the encoder
        // reads a surface the copy has not reached yet. It showed up as the first two
        // frames of every stream decoding to black while every later frame was exact --
        // the pipeline being cold at the start and the copy winning the race thereafter,
        // which is the shape of a bug that survives casual testing and appears under load.
        //
        // SAFETY: the surface belongs to this display and has just been written to.
        check(
            unsafe { va::vaSyncSurface(self.display.handle(), surface.raw()) },
            "vaSyncSurface",
        )
    }

    /// Read a surface back into ordinary memory, as NV12.
    ///
    /// `vaGetImage` rather than a derived image. A derived image on this driver is a
    /// scratch mapping in both directions: writes to it are discarded on unmap, and reads
    /// from it return zeroes regardless of what the surface holds. A read-back built on it
    /// therefore agrees with itself and says nothing about the surface, which is exactly
    /// what a verification must not do.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the driver will not read the surface or map the result.
    pub fn download(&mut self, surface: SurfaceId) -> Result<Vec<u8>, Status> {
        // SAFETY: image and surface belong to this display; the rectangle is the whole
        // image, whose geometry libva reported.
        check(
            unsafe {
                va::vaGetImage(
                    self.display.handle(),
                    surface.raw(),
                    0,
                    0,
                    u32::from(self.image.width),
                    u32::from(self.image.height),
                    self.image.image_id,
                )
            },
            "vaGetImage",
        )?;

        let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `image.buf` belongs to an image created in `new` on this display.
        check(
            unsafe {
                va::vaMapBuffer(self.display.handle(), self.image.buf, std::ptr::addr_of_mut!(data))
            },
            "vaMapBuffer",
        )?;
        let len = usize::try_from(self.image.data_size).unwrap_or(0);
        let copied = if data.is_null() || len == 0 {
            Vec::new()
        } else {
            // SAFETY: the driver's mapping of `data_size` bytes, valid until unmapped just
            // below.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) }.to_vec()
        };
        // SAFETY: mapped immediately above, unmapped exactly once.
        check(
            unsafe { va::vaUnmapBuffer(self.display.handle(), self.image.buf) },
            "vaUnmapBuffer",
        )?;
        Ok(copied)
    }

    /// Where each plane sits in what [`SurfaceUpload::download`] returns.
    #[must_use]
    pub fn layout(&self) -> Option<PlaneLayout> {
        PlaneLayout::of(&self.image)
    }
}

impl Drop for SurfaceUpload {
    fn drop(&mut self) {
        // SAFETY: created in `new` and destroyed exactly once.
        unsafe {
            va::vaDestroyImage(self.display.handle(), self.image.image_id);
        }
    }
}

/// A mapped image's bytes, borrowed for the length of one write.
struct ImageView {
    image: va::VAImage,
    data: *mut u8,
}

impl ImageView {
    /// The whole mapping, as bytes.
    ///
    /// One slice rather than one per plane because the planes are offsets into a single
    /// allocation, and handing out several overlapping mutable slices would be unsound.
    fn bytes(&mut self) -> Option<&mut [u8]> {
        let len = usize::try_from(self.image.data_size).ok()?;
        if self.data.is_null() || len == 0 {
            return None;
        }
        // SAFETY: `data` is the driver's mapping of `data_size` bytes, valid until this
        // image is unmapped, which happens in `Drop` and therefore not before this borrow
        // ends. Nothing else holds a reference to it: the mapping is reachable only through
        // `self`, and this takes `&mut self`.
        Some(unsafe { std::slice::from_raw_parts_mut(self.data, len) })
    }

    /// Copy an I420 frame in, interleaving its chroma into NV12 as it goes.
    ///
    /// One pass over the frame. The luma plane is copied row by row because the image's
    /// pitch is the driver's choice and rarely equals the frame's width, and the chroma
    /// planes are interleaved during the same pass rather than converted beforehand.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the image is not NV12, or if its geometry does not admit the
    /// frame. Both mean the image was not the one the caller thought it was, which is worth
    /// refusing rather than writing past a plane.
    fn write_i420(&mut self, frame: &I420Frame) -> Result<(), Status> {
        let fail = |what: &'static str| Status { operation: what, code: -1 };

        if self.image.format.fourcc != FOURCC_NV12 {
            return Err(fail("derived image is not NV12"));
        }
        let (width, height) = (
            usize::try_from(frame.width()).map_err(|_| fail("frame width"))?,
            usize::try_from(frame.height()).map_err(|_| fail("frame height"))?,
        );
        if usize::from(self.image.width) < width || usize::from(self.image.height) < height {
            return Err(fail("surface smaller than the frame"));
        }

        let layout = PlaneLayout::of(&self.image).ok_or_else(|| fail("image plane layout"))?;
        let (y_stride, uv_stride) = (frame.y_stride(), frame.uv_stride());
        let (chroma_width, chroma_height) = (
            width.checked_div(2).ok_or_else(|| fail("chroma width"))?,
            height.checked_div(2).ok_or_else(|| fail("chroma height"))?,
        );

        // Read before the mapping is borrowed mutably: the frame and the surface are
        // separate allocations, but the borrow checker cannot see that through a raw
        // pointer, so the planes are captured first.
        let (y_plane, u_plane, v_plane) = (frame.y(), frame.u(), frame.v());
        let bytes = self.bytes().ok_or_else(|| fail("empty mapping"))?;

        for row in 0..height {
            let src = y_plane
                .get(row.checked_mul(y_stride).ok_or_else(|| fail("luma row"))?..)
                .and_then(|r| r.get(..width))
                .ok_or_else(|| fail("luma plane too small"))?;
            let start = layout
                .luma_offset
                .checked_add(row.checked_mul(layout.luma_pitch).ok_or_else(|| fail("luma row"))?)
                .ok_or_else(|| fail("luma row"))?;
            bytes
                .get_mut(start..start.checked_add(width).ok_or_else(|| fail("luma row"))?)
                .ok_or_else(|| fail("surface luma plane too small"))?
                .copy_from_slice(src);
        }

        for row in 0..chroma_height {
            let offset = row.checked_mul(uv_stride).ok_or_else(|| fail("chroma row"))?;
            let u_row = u_plane
                .get(offset..)
                .and_then(|r| r.get(..chroma_width))
                .ok_or_else(|| fail("u plane too small"))?;
            let v_row = v_plane
                .get(offset..)
                .and_then(|r| r.get(..chroma_width))
                .ok_or_else(|| fail("v plane too small"))?;
            let start = layout
                .chroma_offset
                .checked_add(
                    row.checked_mul(layout.chroma_pitch).ok_or_else(|| fail("chroma row"))?,
                )
                .ok_or_else(|| fail("chroma row"))?;
            let interleaved = bytes
                .get_mut(
                    start
                        ..start
                            .checked_add(chroma_width.checked_mul(2).ok_or_else(|| fail("uv"))?)
                            .ok_or_else(|| fail("chroma row"))?,
                )
                .ok_or_else(|| fail("surface chroma plane too small"))?;
            for (pair, (&u, &v)) in interleaved.chunks_exact_mut(2).zip(u_row.iter().zip(v_row)) {
                // `chunks_exact_mut(2)` yields exactly two elements, so neither write can
                // be out of bounds.
                if let [first, second] = pair {
                    *first = u;
                    *second = v;
                }
            }
        }

        Ok(())
    }

}

/// Reading a surface back into ordinary memory.
///
/// Separate from [`SurfaceUpload`] because it is not part of the capture path: nothing in a
/// call reads a surface back, since doing so would undo the point of putting the frame on
/// the GPU. It exists so a decode or an upload can be checked against what it was supposed
/// to produce, which is otherwise unobservable.
pub struct SurfaceDownload {
    display: Arc<Display>,
    image: va::VAImage,
}

// SAFETY: as `SurfaceUpload`.
unsafe impl Send for SurfaceDownload {}

impl SurfaceDownload {
    /// Create an NV12 image to read surfaces into.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the driver will not create an image of this size.
    pub fn new(display: &Arc<Display>, width: u32, height: u32) -> Result<Self, Status> {
        let upload = SurfaceUpload::new(display, width, height)?;
        let image = upload.image;
        // The image is now owned here, so the upload must not destroy it too.
        std::mem::forget(upload);
        Ok(Self {
            display: Arc::clone(display),
            image,
        })
    }

    /// Read `surface` into ordinary memory, as NV12.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the driver will not read the surface or map the result.
    pub fn read(&mut self, surface: SurfaceId) -> Result<Vec<u8>, Status> {
        // SAFETY: image and surface belong to this display; the rectangle is the whole
        // image, whose geometry libva reported.
        check(
            unsafe {
                va::vaGetImage(
                    self.display.handle(),
                    surface.raw(),
                    0,
                    0,
                    u32::from(self.image.width),
                    u32::from(self.image.height),
                    self.image.image_id,
                )
            },
            "vaGetImage",
        )?;

        let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `image.buf` belongs to an image created on this display.
        check(
            unsafe {
                va::vaMapBuffer(self.display.handle(), self.image.buf, std::ptr::addr_of_mut!(data))
            },
            "vaMapBuffer",
        )?;
        let len = usize::try_from(self.image.data_size).unwrap_or(0);
        let copied = if data.is_null() || len == 0 {
            Vec::new()
        } else {
            // SAFETY: the driver's mapping of `data_size` bytes, valid until unmapped below.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) }.to_vec()
        };
        // SAFETY: mapped immediately above, unmapped exactly once.
        check(
            unsafe { va::vaUnmapBuffer(self.display.handle(), self.image.buf) },
            "vaUnmapBuffer",
        )?;
        Ok(copied)
    }

    /// Where each plane sits in what [`SurfaceDownload::read`] returns.
    #[must_use]
    pub fn layout(&self) -> Option<PlaneLayout> {
        PlaneLayout::of(&self.image)
    }
}

impl Drop for SurfaceDownload {
    fn drop(&mut self) {
        // SAFETY: created in `new` and destroyed exactly once.
        unsafe {
            va::vaDestroyImage(self.display.handle(), self.image.image_id);
        }
    }
}

/// Where NV12's two planes sit inside an image.
///
/// The driver chooses both the offsets and the pitches, and they are routinely not what the
/// geometry would suggest — assuming `width` for the pitch, or that chroma follows luma
/// immediately, writes a sheared or misaligned picture that still encodes successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    pub luma_offset: usize,
    pub luma_pitch: usize,
    pub chroma_offset: usize,
    pub chroma_pitch: usize,
}

impl PlaneLayout {
    /// Read the layout out of a `VAImage`, or `None` if it does not describe two planes.
    fn of(image: &va::VAImage) -> Option<Self> {
        if image.num_planes < 2 {
            return None;
        }
        Some(Self {
            luma_offset: usize::try_from(*image.offsets.first()?).ok()?,
            luma_pitch: usize::try_from(*image.pitches.first()?).ok()?,
            chroma_offset: usize::try_from(*image.offsets.get(1)?).ok()?,
            chroma_pitch: usize::try_from(*image.pitches.get(1)?).ok()?,
        })
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
    use super::{PlaneLayout, SurfaceUpload};
    use super::super::display::Display;
    use super::super::resource::SurfacePool;
    use elementium_types::I420Frame;

    const W: u32 = 64;
    const H: u32 = 32;
    const WIDTH: usize = 64;
    const HEIGHT: usize = 32;

    /// A frame whose every sample says where it came from, so a plane written to the wrong
    /// offset is visible rather than merely plausible.
    fn test_frame() -> I420Frame {
        let (w, h) = (WIDTH, HEIGHT);
        let y: Vec<u8> = (0..w * h).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        let u: Vec<u8> = (0..(w / 2) * (h / 2))
            .map(|i| u8::try_from(100 + i % 50).unwrap_or(0))
            .collect();
        let v: Vec<u8> = (0..(w / 2) * (h / 2))
            .map(|i| u8::try_from(200 + i % 50).unwrap_or(0))
            .collect();
        I420Frame::from_planes(W, H, &y, &u, &v, 0).expect("planes match the geometry")
    }

    /// Everything needed for an upload, or `None` where there is no GPU.
    fn fixture() -> Option<(SurfaceUpload, super::super::resource::SurfaceId, SurfacePool)> {
        let display = std::sync::Arc::new(Display::open_any()?);
        let pool = SurfacePool::new_nv12(&display, W, H, 1).ok()?;
        let surface = pool.surfaces().first().copied()?;
        let upload = SurfaceUpload::new(&display, W, H).ok()?;
        Some((upload, surface, pool))
    }

    /// The upload must land in the surface the encoder reads, which is not something the
    /// call succeeding tells you: on this driver a write through a derived image is
    /// discarded on unmap and every status is still success.
    ///
    /// So the surface is read back with `vaGetImage` -- which reads the surface rather
    /// than a mapping of our own making -- and compared sample by sample, chroma
    /// interleave included. Swapping U and V is the classic fault here and changes only
    /// the colour, which no size or status check can catch.
    #[test]
    fn an_uploaded_frame_reads_back_out_of_the_surface() {
        let Some((mut upload, surface, _pool)) = fixture() else {
            return; // No GPU on this machine; nothing to check.
        };
        let frame = test_frame();
        upload.upload(&frame, surface).expect("upload");

        let bytes = upload.download(surface).expect("read back");
        let layout = upload.layout().expect("two planes");
        assert!(!bytes.is_empty(), "the surface read back empty");

        for row in 0..HEIGHT {
            let start = layout.luma_offset + row * layout.luma_pitch;
            assert_eq!(
                &bytes[start..start + WIDTH],
                &frame.y()[row * frame.y_stride()..row * frame.y_stride() + WIDTH],
                "luma row {row} is not what was uploaded"
            );
        }
        for row in 0..HEIGHT / 2 {
            let start = layout.chroma_offset + row * layout.chroma_pitch;
            for x in 0..WIDTH / 2 {
                let u = frame.u()[row * frame.uv_stride() + x];
                let v = frame.v()[row * frame.uv_stride() + x];
                assert_eq!(bytes[start + x * 2], u, "U at {row},{x}");
                assert_eq!(
                    bytes[start + x * 2 + 1],
                    v,
                    "V at {row},{x}: the chroma planes are interleaved the wrong way round"
                );
            }
        }
    }

    /// Uploading twice must replace the picture, not leave the first one. A staging image
    /// that is reused has to be fully overwritten each time.
    #[test]
    fn a_second_upload_replaces_the_first() {
        let Some((mut upload, surface, _pool)) = fixture() else {
            return;
        };
        upload.upload(&test_frame(), surface).expect("first upload");

        let flat = I420Frame::from_planes(
            W,
            H,
            &vec![17; WIDTH * HEIGHT],
            &vec![33; (WIDTH / 2) * (HEIGHT / 2)],
            &vec![44; (WIDTH / 2) * (HEIGHT / 2)],
            0,
        )
        .expect("planes match the geometry");
        upload.upload(&flat, surface).expect("second upload");

        let bytes = upload.download(surface).expect("read back");
        let layout = upload.layout().expect("two planes");
        assert_eq!(bytes[layout.luma_offset], 17, "the first frame is still there");
        assert_eq!(bytes[layout.chroma_offset], 33);
        assert_eq!(bytes[layout.chroma_offset + 1], 44);
    }

    /// A frame larger than the staging image must be refused rather than written past the
    /// end of a plane, which corrupts whatever the driver put next in the mapping.
    #[test]
    fn a_frame_too_large_for_the_surface_is_refused() {
        let Some((mut upload, surface, _pool)) = fixture() else {
            return;
        };
        let (w, h) = (256_usize, 128_usize);
        let big = I420Frame::from_planes(
            u32::try_from(w).expect("width"),
            u32::try_from(h).expect("height"),
            &vec![0; w * h],
            &vec![0; (w / 2) * (h / 2)],
            &vec![0; (w / 2) * (h / 2)],
            0,
        )
        .expect("planes match the geometry");

        assert!(
            upload.upload(&big, surface).is_err(),
            "a frame larger than the surface must not be written"
        );
    }

    /// The plane layout comes from the driver, and both planes must be described. A layout
    /// read as zeroes would stack chroma on top of luma and encode a grey picture.
    #[test]
    fn the_driver_describes_both_planes() {
        let Some((upload, _surface, _pool)) = fixture() else {
            return;
        };
        let PlaneLayout { luma_pitch, chroma_offset, chroma_pitch, .. } =
            upload.layout().expect("the driver described fewer than two planes");
        assert!(luma_pitch >= WIDTH, "luma pitch {luma_pitch} is narrower than the frame");
        assert!(
            chroma_pitch >= WIDTH,
            "chroma pitch {chroma_pitch} cannot hold interleaved UV"
        );
        assert!(chroma_offset > 0, "chroma cannot start at the same place as luma");
    }
}
