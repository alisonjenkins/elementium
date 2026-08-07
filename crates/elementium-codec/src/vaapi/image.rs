//! Getting a frame into a GPU surface.
//!
//! This is the one place on the hardware path where the CPU still touches every pixel, so
//! how it is done matters more than the line count suggests.
//!
//! There are two ways to fill a surface. `vaCreateImage` allocates a separate image in
//! system memory, which is then copied into the surface by `vaPutImage` — two copies, one
//! of them inside the driver where it cannot be measured. `vaDeriveImage` instead hands
//! back an image *backed by the surface itself*, so mapping it gives a pointer to the
//! memory the encoder will read. Writing through that pointer is one pass, and there is no
//! second copy at all.
//!
//! Derivation is not guaranteed. Some drivers refuse it for some formats, so the caller is
//! told plainly when it is unavailable rather than being silently given the slower path
//! with the same signature.
//!
//! # Why the conversion happens here
//!
//! Frames arrive as I420 and encoders read NV12; the difference is only whether the chroma
//! samples are interleaved. Doing that during the upload means the interleave is free — the
//! bytes were being written to GPU memory regardless, and they are simply written in a
//! different order. Converting first, into a staging buffer, would cost a whole extra pass
//! over the frame to achieve exactly the same result.
//!
//! # The remaining copy
//!
//! One pass over the frame is what this costs, and it cannot go lower while the frame lives
//! in ordinary memory. Removing it entirely means never putting it there: importing the
//! camera's buffer as a DMA-BUF so the GPU reads it where it already lies. That is a
//! different mechanism, not an optimisation of this one.

use elementium_types::I420Frame;
use libva_sys::va_display_drm as va;

use super::display::Display;
use super::resource::SurfaceId;
use super::status::{Status, check};

/// NV12 as libva spells it.
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// A surface's own memory, mapped for writing.
///
/// Unmapped and destroyed on drop, in that order, which is the order libva requires and
/// does not check.
pub struct MappedImage<'d> {
    display: &'d Display,
    image: va::VAImage,
    data: *mut u8,
}

impl<'d> MappedImage<'d> {
    /// Map the memory behind `surface`.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the driver will not derive an image from this surface, or will
    /// not map it. A driver that refuses derivation is not broken — it is telling us the
    /// surface is in a layout it cannot expose — and the caller's answer is to fall back
    /// rather than to fail the call.
    pub fn derive(display: &'d Display, surface: SurfaceId) -> Result<Self, Status> {
        // Zeroed rather than `Default`: the binding does not derive it, and libva fills
        // every field it uses.
        // SAFETY: `VAImage` is a plain C struct of integers and a nested format struct, for
        // which all-zeroes is a valid bit pattern.
        let mut image: va::VAImage = unsafe { std::mem::zeroed() };
        // SAFETY: the display outlives this image by borrow, the surface belongs to it, and
        // libva writes only into `image`.
        let status = unsafe {
            va::vaDeriveImage(display.handle(), surface.raw(), std::ptr::addr_of_mut!(image))
        };
        check(status, "vaDeriveImage")?;

        let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `image.buf` was just produced by `vaDeriveImage`; libva writes the
        // mapping address into `data`.
        let status = unsafe {
            va::vaMapBuffer(display.handle(), image.buf, std::ptr::addr_of_mut!(data))
        };
        if let Err(e) = check(status, "vaMapBuffer") {
            // The image exists whether or not it could be mapped, and leaking it would
            // hold GPU memory for the life of the process.
            // SAFETY: `image` was created above and is destroyed exactly once.
            unsafe {
                va::vaDestroyImage(display.handle(), image.image_id);
            }
            return Err(e);
        }

        Ok(Self {
            display,
            image,
            data: data.cast::<u8>(),
        })
    }

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
    /// One pass over the frame. The luma plane is copied row by row because the surface's
    /// pitch is the driver's choice and rarely equals the frame's width, and the chroma
    /// planes are interleaved during the same pass rather than converted beforehand.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the derived image is not NV12, or if its geometry does not
    /// admit the frame. Both mean the surface was not the one the caller thought it was,
    /// which is worth refusing rather than writing past a plane.
    pub fn write_i420(&mut self, frame: &I420Frame) -> Result<(), Status> {
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

    /// The mapped bytes, for reading back what was written.
    ///
    /// Exists for tests: an upload that silently wrote to the wrong offsets produces a
    /// picture that is merely wrong rather than an error, so it has to be read back to be
    /// believed.
    #[must_use]
    pub fn as_bytes(&mut self) -> Option<&[u8]> {
        self.bytes().map(|b| &*b)
    }

    /// Where each plane starts and how wide its rows are.
    #[must_use]
    pub fn layout(&self) -> Option<PlaneLayout> {
        PlaneLayout::of(&self.image)
    }
}

/// Where NV12's two planes sit inside a mapped image.
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

impl Drop for MappedImage<'_> {
    fn drop(&mut self) {
        // SAFETY: mapped and created in `derive`; released exactly once, and in the order
        // libva requires -- unmapping after destroying the image is undefined.
        unsafe {
            va::vaUnmapBuffer(self.display.handle(), self.image.buf);
            va::vaDestroyImage(self.display.handle(), self.image.image_id);
        }
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
    use super::{MappedImage, PlaneLayout};
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
        let u: Vec<u8> = (0..(w / 2) * (h / 2)).map(|i| u8::try_from(100 + i % 50).unwrap_or(0)).collect();
        let v: Vec<u8> = (0..(w / 2) * (h / 2)).map(|i| u8::try_from(200 + i % 50).unwrap_or(0)).collect();
        I420Frame::from_planes(W, H, &y, &u, &v, 0).expect("planes match the geometry")
    }

    /// The upload must land where the encoder will read, which is not something the call
    /// succeeding tells you: writing to the wrong offsets or with the wrong pitch produces
    /// a surface that encodes perfectly into a wrong picture.
    ///
    /// So the surface is read back and compared sample by sample, including the chroma
    /// interleave -- swapping U and V is the classic fault here and changes only the
    /// colour, which no size or status check can catch.
    #[test]
    fn an_uploaded_frame_reads_back_as_nv12() {
        let Some(display) = Display::open_any() else {
            return; // No GPU on this machine; nothing to check.
        };
        let Ok(pool) = SurfacePool::new_nv12(&display, W, H, 1) else {
            return;
        };
        let surface = pool.surfaces().first().copied().expect("one surface");

        let Ok(mut image) = MappedImage::derive(&display, surface) else {
            return; // This driver will not derive; the caller falls back.
        };
        let frame = test_frame();
        image.write_i420(&frame).expect("upload");

        let layout = image.layout().expect("two planes");
        let bytes = image.as_bytes().expect("mapping").to_vec();
        drop(image);

        let (w, h) = (WIDTH, HEIGHT);
        for row in 0..h {
            let start = layout.luma_offset + row * layout.luma_pitch;
            assert_eq!(
                &bytes[start..start + w],
                &frame.y()[row * frame.y_stride()..row * frame.y_stride() + w],
                "luma row {row} did not land where the encoder will read it"
            );
        }
        for row in 0..h / 2 {
            let start = layout.chroma_offset + row * layout.chroma_pitch;
            for x in 0..w / 2 {
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

    /// A frame larger than the surface must be refused rather than written past the end of
    /// a plane, which corrupts whatever the driver put next in the mapping.
    #[test]
    fn a_frame_too_large_for_the_surface_is_refused() {
        let Some(display) = Display::open_any() else {
            return;
        };
        let Ok(pool) = SurfacePool::new_nv12(&display, W, H, 1) else {
            return;
        };
        let surface = pool.surfaces().first().copied().expect("one surface");
        let Ok(mut image) = MappedImage::derive(&display, surface) else {
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
            image.write_i420(&big).is_err(),
            "a frame larger than the surface must not be written"
        );
    }

    /// The plane layout comes from the driver, and both planes must be described. A layout
    /// read as zeroes would stack chroma on top of luma and encode a grey picture.
    #[test]
    fn the_driver_describes_both_planes() {
        let Some(display) = Display::open_any() else {
            return;
        };
        let Ok(pool) = SurfacePool::new_nv12(&display, W, H, 1) else {
            return;
        };
        let surface = pool.surfaces().first().copied().expect("one surface");
        let Ok(image) = MappedImage::derive(&display, surface) else {
            return;
        };
        let PlaneLayout { luma_pitch, chroma_offset, chroma_pitch, .. } = image
            .layout()
            .expect("the driver described fewer than two planes");
        assert!(luma_pitch >= WIDTH, "luma pitch {luma_pitch} is narrower than the frame");
        assert!(chroma_pitch >= WIDTH, "chroma pitch {chroma_pitch} cannot hold interleaved UV");
        assert!(chroma_offset > 0, "chroma cannot start at the same place as luma");
    }
}
