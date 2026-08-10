//! Enforce that the capture path does not copy frames.
//!
//! "Zero copy" was true when it was measured and nothing kept it true. `from_planes` sits
//! beside `from_padded` and copies three planes; any `to_vec()` added to the hot path
//! restores a megabyte of copying per frame, and every other test still passes. The
//! benchmark would show it as a few hundred microseconds — real, but easy to attribute to
//! noise or to the machine being busy.
//!
//! So this counts bytes allocated during each step and fails if a frame-sized allocation
//! appears where none should. It is a separate test binary because a global allocator can
//! only be installed once per binary.

#![allow(
    clippy::expect_used,
    clippy::as_conversions,
    clippy::arithmetic_side_effects
)]

use elementium_codec::Vp8Encoder;
use elementium_types::I420Frame;
use std::alloc::{GlobalAlloc, Layout, System};

// Bytes allocated on *this thread* since its counter was last reset.
//
// Thread-local, and that is the whole point. A global counter attributes every thread's
// allocations to whichever measurement happens to be running, so the harness printing a
// result on one thread lands inside another's measurement -- which showed up as
// `adopting_a_decoded_buffer_allocates_nothing` failing only in a full workspace run and
// passing every time the file was run alone. Serialising the measurements did not fix it,
// because the problem was never concurrent resets; it was concurrent allocation.
thread_local! {
    static ALLOCATED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The system allocator, counting what it hands out.
struct Counting;

// SAFETY: every method delegates to the system allocator unchanged; the counter is
// incidental and cannot affect the returned pointers.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with`, not `with`: a thread-local is unavailable during thread teardown, and
        // allocating there must not panic inside the allocator.
        let _ = ALLOCATED.try_with(|n| n.set(n.get().saturating_add(layout.size())));
        // SAFETY: forwarding the caller's own contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarding the caller's own contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let grown = new_size.saturating_sub(layout.size());
        let _ = ALLOCATED.try_with(|n| n.set(n.get().saturating_add(grown)));
        // SAFETY: forwarding the caller's own contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Bytes allocated on this thread while running `f`.
fn allocated_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    ALLOCATED.with(|n| n.set(0));
    let value = f();
    (value, ALLOCATED.with(std::cell::Cell::get))
}

/// 720p, the resolution the camera negotiates.
const W: u32 = 1280;
const H: u32 = 720;

/// One frame's worth of I420, which is what a copy would cost.
const FRAME_BYTES: usize = (W as usize) * (H as usize) * 3 / 2;

fn padded_buffer() -> Vec<u8> {
    // Strides as libjpeg-turbo produces them: padded, not tight.
    let y_stride = (W as usize).next_multiple_of(32);
    let uv_stride = ((W as usize) / 2).next_multiple_of(32);
    vec![0x80_u8; y_stride * (H as usize) + uv_stride * (H as usize / 2) * 2]
}

/// Adopting a decoder's buffer must copy nothing at all.
///
/// This is the whole claim. `from_padded` takes the `Vec` by value, so it should move it;
/// if someone changes it to take a slice and copy, or normalises the strides by repacking,
/// this fails immediately rather than showing up as a benchmark that drifted.
#[test]
fn adopting_a_decoded_buffer_allocates_nothing() {
    let y_stride = (W as usize).next_multiple_of(32);
    let uv_stride = ((W as usize) / 2).next_multiple_of(32);
    let buffer = padded_buffer();

    let (frame, bytes) = allocated_during(|| {
        I420Frame::from_padded(W, H, buffer, y_stride, uv_stride, 0).expect("valid frame")
    });

    assert_eq!(
        bytes, 0,
        "adopting a decoder buffer allocated {bytes} bytes; it must move, not copy"
    );
    assert_eq!(frame.width(), W);
    assert_eq!(
        frame.y_stride(),
        y_stride,
        "padding must be preserved, not repacked"
    );
}

/// Encoding must read the frame where it lies.
///
/// libvpx takes a stride per plane, so a padded frame needs no repacking. An encoder that
/// copied the frame first would allocate at least `FRAME_BYTES` here.
#[test]
fn encoding_does_not_copy_the_frame() {
    let mut encoder = Vp8Encoder::new(W, H, 2764, 30).expect("encoder");
    let y_stride = (W as usize).next_multiple_of(32);
    let uv_stride = ((W as usize) / 2).next_multiple_of(32);
    let frame =
        I420Frame::from_padded(W, H, padded_buffer(), y_stride, uv_stride, 0).expect("valid frame");

    // Prime it: the first encode allocates the codec's internal buffers, which is a
    // one-off and not what this is measuring.
    let _ = encoder.encode(&frame);

    let (packets, bytes) = allocated_during(|| encoder.encode(&frame).expect("encode"));

    let coded: usize = packets.iter().map(|p| p.data.as_bytes().len()).sum();
    assert!(
        bytes < FRAME_BYTES,
        "encoding allocated {bytes} bytes for a {FRAME_BYTES}-byte frame; \
         it should allocate only the coded output ({coded} bytes), not a copy of the input"
    );
}

/// The tightly-packed constructor does copy, and must: it is for callers that produce
/// planes themselves.
///
/// Asserted so the difference between the two constructors stays deliberate. If this ever
/// allocates nothing, the two have been conflated and the zero-copy path is no longer
/// distinguishable from the copying one.
#[test]
fn building_from_separate_planes_is_understood_to_copy() {
    let y = vec![0x80_u8; (W as usize) * (H as usize)];
    let u = vec![0x80_u8; (W as usize) / 2 * (H as usize) / 2];
    let v = u.clone();

    let (_frame, bytes) =
        allocated_during(|| I420Frame::from_planes(W, H, &y, &u, &v, 0).expect("valid frame"));

    assert!(
        bytes >= FRAME_BYTES,
        "from_planes allocated {bytes} bytes; it is expected to copy {FRAME_BYTES}"
    );
}

/// Uploading a frame to a GPU surface must write into the driver's mapping, not through a
/// staging copy of its own.
///
/// This is the step the benchmark attributes least reliably. `SurfaceUpload::upload` maps
/// the image, writes the planes row by row into the driver's memory, and unmaps -- so a
/// `to_vec()` or a repack added anywhere in that path costs a frame-sized allocation per
/// frame and shows up only as the accelerated path quietly losing its advantage.
///
/// Skips where there is no VAAPI device rather than failing: a machine without a GPU
/// driver cannot answer this question, and pretending otherwise would make the suite fail
/// for a reason that has nothing to do with the code.
#[cfg(all(target_os = "linux", feature = "vaapi"))]
#[test]
fn uploading_to_a_surface_does_not_copy_the_frame() {
    use elementium_codec::vaapi::{Display, image::SurfaceUpload, resource::SurfacePool};
    use std::sync::Arc;

    let Some(display) = Display::open_any() else {
        eprintln!("skipping: no VAAPI render node on this machine");
        return;
    };
    let display = Arc::new(display);
    let Ok(pool) = SurfacePool::new_nv12(&display, W, H, 2) else {
        eprintln!("skipping: the driver would not allocate NV12 surfaces");
        return;
    };
    let Some(surface) = pool.surfaces().first().copied() else {
        eprintln!("skipping: the pool produced no surface");
        return;
    };
    let Ok(mut upload) = SurfaceUpload::new(&display, W, H) else {
        eprintln!("skipping: the driver would not create an upload image");
        return;
    };

    let y_stride = (W as usize).next_multiple_of(32);
    let uv_stride = ((W as usize) / 2).next_multiple_of(32);
    let frame =
        I420Frame::from_padded(W, H, padded_buffer(), y_stride, uv_stride, 0).expect("valid frame");

    // Prime it: the first upload can fault in the driver's mapping, which is a one-off.
    let _ = upload.upload(&frame, surface);

    let (result, bytes) = allocated_during(|| upload.upload(&frame, surface));
    if result.is_err() {
        eprintln!("skipping: the driver refused the upload");
        return;
    }

    // Zero, not "less than a frame". The upload writes into the driver's mapping and has
    // nothing to allocate, so any allocation at all is a copy that was not there before --
    // and a threshold of one frame would miss a single plane, which is how the first
    // version of this test passed while a whole luma plane was being copied.
    assert_eq!(
        bytes, 0,
        "uploading allocated {bytes} bytes; it must write into the driver's mapping and \
         allocate nothing (a {FRAME_BYTES}-byte frame)"
    );
}
