//! An initialised VA display, and the render node it came from.
//!
//! Everything else in this module borrows a `Display`, which is what stops a config,
//! context or surface outliving the display it was created against — an ordering libva
//! requires and does not enforce.

use std::os::raw::c_int;

use libva_sys::va_display_drm as va;

use super::status::{Status, check};

/// Render nodes to try, in order.
///
/// A machine can have several GPUs, and they do not have the same capabilities. The
/// discrete card enumerates first, which is the one worth using.
pub const RENDER_NODES: [&str; 4] = [
    "/dev/dri/renderD128",
    "/dev/dri/renderD129",
    "/dev/dri/renderD130",
    "/dev/dri/renderD131",
];

/// An initialised VA display. Terminated on drop.
pub struct Display {
    handle: va::VADisplay,
    /// Kept open for the display's lifetime: libva uses this descriptor, and closing it
    /// early leaves the driver mapping a file nobody holds.
    _node: std::fs::File,
}

// SAFETY: `VADisplay` is an opaque driver handle, not a pointer into this process's own
// state, and libva's drivers serialise access to it internally -- which is what makes
// `vaInitialize`/`vaTerminate` reference-counted per display rather than per thread.
//
// `Sync` is needed because the resources below share one display through an `Arc`, and an
// `Arc<T>` is only `Send` when `T` is both. In practice concurrent access does not arise:
// an encoder owns its display and is `Send` but not `Sync`, so no two threads hold one at
// the same time. The driver's own locking is what makes the weaker guarantee sound anyway.
unsafe impl Send for Display {}
// SAFETY: as above.
unsafe impl Sync for Display {}

impl Display {
    /// Open the first render node that initialises.
    #[must_use]
    pub fn open_any() -> Option<Self> {
        RENDER_NODES.iter().find_map(|node| Self::open(node).ok())
    }

    /// Open a specific render node.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] if the node cannot be opened or libva will not initialise on it.
    pub fn open(path: &str) -> Result<Self, Status> {
        use std::os::fd::AsRawFd as _;

        // Read-write: the driver maps buffers through this descriptor, and a read-only
        // handle initialises successfully and then fails inside the driver with EACCES --
        // which arrives as a segfault rather than an error.
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| Status {
                operation: "open render node",
                code: -1,
            })?;

        // SAFETY: the descriptor is valid and outlives the display, which owns the file.
        let handle = unsafe { va::vaGetDisplayDRM(node.as_raw_fd()) };
        if handle.is_null() {
            return Err(Status {
                operation: "vaGetDisplayDRM",
                code: -1,
            });
        }

        let (mut major, mut minor): (c_int, c_int) = (0, 0);
        // SAFETY: `handle` is non-null and libva writes only the two version out-params.
        let status = unsafe {
            va::vaInitialize(
                handle,
                std::ptr::addr_of_mut!(major),
                std::ptr::addr_of_mut!(minor),
            )
        };
        check(status, "vaInitialize")?;

        Ok(Self {
            handle,
            _node: node,
        })
    }

    /// The raw display, for calls that need it.
    #[must_use]
    pub const fn handle(&self) -> va::VADisplay {
        self.handle
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: initialised in `open` and terminated exactly once.
        unsafe {
            va::vaTerminate(self.handle);
        }
    }
}
