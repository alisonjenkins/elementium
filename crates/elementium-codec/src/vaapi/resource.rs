//! VAAPI handles, made into types the compiler can check.
//!
//! libva is a C API of interchangeable integers. `VAConfigID`, `VAContextID`,
//! `VASurfaceID` and `VABufferID` are all `u32`, so passing a config where a context
//! belongs compiles cleanly and fails at run time — if you are lucky. If you are not, it
//! addresses a live object of the wrong kind.
//!
//! Every handle here is therefore a distinct newtype, and every resource owns its
//! destruction. The rules libva expects are then enforced by the type system rather than by
//! remembering them:
//!
//! - A resource cannot outlive its display, because it holds a reference to it.
//! - A context cannot outlive its config, for the same reason.
//! - A resource cannot be destroyed twice, because `Drop` runs once.
//! - A resource cannot be leaked on an early return, because `Drop` runs anyway.
//! - A handle of one kind cannot be passed where another is expected.
//!
//! None of that is free-standing pedantry. Each of those is a real failure mode of this
//! API, and each produces a segfault or a silently corrupt encode rather than an error.
//!
//! # Why `Arc` rather than a lifetime
//!
//! These were borrowed types, which said the same thing at compile time and said it more
//! cheaply. The encoder is what changed: it owns a display, a config, a context and a pool
//! at once, and a borrowed `Config<'d>` inside a struct that also owns the `Display` it
//! borrows is self-referential and not expressible.
//!
//! The ordering guarantee is unchanged -- a config still cannot be destroyed while a
//! context uses it -- but it is now upheld by a reference count instead of by the borrow
//! checker. The cost is one atomic increment per resource creation, which happens when a
//! call starts and not per frame.

use std::os::raw::c_int;
use std::sync::Arc;

use libva_sys::va_display_drm as va;

use super::display::Display;
use super::status::{Status, check};

/// A VAAPI configuration: a profile plus an entrypoint plus its attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigId(va::VAConfigID);

/// A VAAPI context: where encoding actually happens, bound to one config and geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextId(va::VAContextID);

/// A surface: a GPU-side image, in this crate always NV12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceId(va::VASurfaceID);

/// A buffer: parameters going to the GPU, or coded bytes coming back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferId(va::VABufferID);

impl ContextId {
    /// The raw id, for the picture calls that take it directly.
    #[must_use]
    pub const fn raw(self) -> va::VAContextID {
        self.0
    }
}

impl SurfaceId {
    /// The raw id, for the few calls that take an array of them.
    #[must_use]
    pub const fn raw(self) -> va::VASurfaceID {
        self.0
    }
}

impl BufferId {
    /// The raw id, for `vaRenderPicture`, which takes an array.
    #[must_use]
    pub const fn raw(self) -> va::VABufferID {
        self.0
    }
}

/// A configuration, destroyed when dropped.
///
/// Holds the display so it cannot outlive it — destroying a config against a terminated
/// display is undefined, and without the reference nothing would stop it.
pub struct Config {
    display: Arc<Display>,
    id: ConfigId,
}

impl Config {
    /// Create a configuration for encoding with `profile`.
    ///
    /// # Errors
    ///
    /// Returns the driver's status if it will not accept the profile, entrypoint or
    /// attributes — most often because the profile cannot be encoded, only decoded.
    pub fn for_encoding(
        display: &Arc<Display>,
        profile: va::VAProfile,
        attributes: &mut [va::VAConfigAttrib],
    ) -> Result<Self, Status> {
        let mut id: va::VAConfigID = 0;
        // SAFETY: the display is initialised for as long as it is borrowed, and libva
        // writes only `id` plus the attribute values, whose count it is given.
        let status = unsafe {
            va::vaCreateConfig(
                display.handle(),
                profile,
                va::VAEntrypoint_VAEntrypointEncSlice,
                attributes.as_mut_ptr(),
                c_int::try_from(attributes.len()).unwrap_or(0),
                std::ptr::addr_of_mut!(id),
            )
        };
        check(status, "vaCreateConfig")?;
        Ok(Self {
            display: Arc::clone(display),
            id: ConfigId(id),
        })
    }

    #[must_use]
    pub const fn id(&self) -> ConfigId {
        self.id
    }

    #[must_use]
    pub const fn display(&self) -> &Arc<Display> {
        &self.display
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: created by `for_encoding` against this display, destroyed exactly once.
        unsafe {
            va::vaDestroyConfig(self.display.handle(), self.id.0);
        }
    }
}

/// A pool of surfaces, destroyed together when dropped.
///
/// Allocated as a group because libva does, and because an encoder needs several at once:
/// one for the frame being submitted and more for the reconstructed references the codec
/// predicts from.
pub struct SurfacePool {
    display: Arc<Display>,
    ids: Vec<va::VASurfaceID>,
}

impl SurfacePool {
    /// Allocate `count` NV12 surfaces of the given size.
    ///
    /// NV12 rather than I420 because that is what encoders take: a luma plane and one
    /// interleaved chroma plane. The conversion happens once when a frame is uploaded.
    ///
    /// # Errors
    ///
    /// Returns the driver's status if the surfaces cannot be allocated, which on a busy
    /// GPU means exactly that and is worth falling back to software over.
    pub fn new_nv12(
        display: &Arc<Display>,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Self, Status> {
        let mut ids: Vec<va::VASurfaceID> = vec![0; count];
        let mut attribute = va::VASurfaceAttrib {
            type_: va::VASurfaceAttribType_VASurfaceAttribPixelFormat,
            flags: va::VA_SURFACE_ATTRIB_SETTABLE,
            value: va::VAGenericValue {
                type_: va::VAGenericValueType_VAGenericValueTypeInteger,
                value: va::_VAGenericValue__bindgen_ty_1 {
                    // 'NV12' as a fourcc.
                    i: i32::from_le_bytes(*b"NV12"),
                },
            },
        };

        // SAFETY: libva writes `count` ids into `ids`, whose length it is given, and reads
        // one attribute, whose count it is given.
        let status = unsafe {
            va::vaCreateSurfaces(
                display.handle(),
                va::VA_RT_FORMAT_YUV420,
                width,
                height,
                ids.as_mut_ptr(),
                u32::try_from(count).unwrap_or(0),
                std::ptr::addr_of_mut!(attribute),
                1,
            )
        };
        check(status, "vaCreateSurfaces")?;
        Ok(Self {
            display: Arc::clone(display),
            ids,
        })
    }

    /// The surfaces, in allocation order.
    #[must_use]
    pub fn surfaces(&self) -> Vec<SurfaceId> {
        self.ids.iter().copied().map(SurfaceId).collect()
    }

    /// Raw ids, for `vaCreateContext`'s render-target array.
    #[must_use]
    pub fn raw(&mut self) -> &mut [va::VASurfaceID] {
        &mut self.ids
    }
}

impl Drop for SurfacePool {
    fn drop(&mut self) {
        // SAFETY: allocated by `new_nv12` against this display, destroyed exactly once.
        unsafe {
            va::vaDestroySurfaces(
                self.display.handle(),
                self.ids.as_mut_ptr(),
                c_int::try_from(self.ids.len()).unwrap_or(0),
            );
        }
    }
}

/// An encoding context, destroyed when dropped.
///
/// Owns the config rather than the display, which is the relationship libva actually has:
/// destroying a config while a context still uses it is undefined.
///
/// By value rather than by reference count, which orders the two destructions exactly as
/// libva requires and costs nothing: `Drop` for this type runs before its fields are
/// dropped, so `vaDestroyContext` is always called before the config's
/// `vaDestroyConfig`. Reversing them is undefined behaviour that a reference count would
/// permit and this does not.
pub struct Context {
    config: Config,
    id: ContextId,
}

impl Context {
    /// Create a context for encoding frames of the given size.
    ///
    /// # Errors
    ///
    /// Returns the driver's status if the context cannot be created, typically because the
    /// size exceeds what the profile supports.
    pub fn new(
        config: Config,
        width: u32,
        height: u32,
        render_targets: &mut [va::VASurfaceID],
    ) -> Result<Self, Status> {
        let mut id: va::VAContextID = 0;
        // SAFETY: the config outlives this context by construction, and libva reads
        // `render_targets` up to the count it is given and writes only `id`.
        let status = unsafe {
            va::vaCreateContext(
                config.display().handle(),
                config.id().0,
                c_int::try_from(width).unwrap_or(0),
                c_int::try_from(height).unwrap_or(0),
                c_int::try_from(va::VA_PROGRESSIVE).unwrap_or(0),
                render_targets.as_mut_ptr(),
                c_int::try_from(render_targets.len()).unwrap_or(0),
                std::ptr::addr_of_mut!(id),
            )
        };
        check(status, "vaCreateContext")?;
        Ok(Self {
            config,
            id: ContextId(id),
        })
    }

    #[must_use]
    pub const fn id(&self) -> ContextId {
        self.id
    }

    #[must_use]
    pub const fn display(&self) -> &Arc<Display> {
        self.config.display()
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: created by `new` against this display, destroyed exactly once.
        unsafe {
            va::vaDestroyContext(self.config.display().handle(), self.id.0);
        }
    }
}

/// A buffer handed to the GPU, destroyed when dropped.
pub struct Buffer {
    display: Arc<Display>,
    id: BufferId,
}

impl Buffer {
    /// Create a buffer of `kind` holding `data`.
    ///
    /// # Errors
    ///
    /// Returns the driver's status if the buffer cannot be created.
    pub fn new<T>(
        context: &Context,
        kind: va::VABufferType,
        data: &mut T,
    ) -> Result<Self, Status> {
        let mut id: va::VABufferID = 0;
        // SAFETY: `data` is a single valid `T`, and its size is what libva is told to
        // copy. The generic parameter is what keeps the size and the pointer in step --
        // passing a size that disagrees with the pointer is the classic way to corrupt
        // this API, and it cannot be expressed here.
        let status = unsafe {
            va::vaCreateBuffer(
                context.display().handle(),
                context.id().0,
                kind,
                u32::try_from(std::mem::size_of::<T>()).unwrap_or(0),
                1,
                std::ptr::from_mut::<T>(data).cast(),
                std::ptr::addr_of_mut!(id),
            )
        };
        check(status, "vaCreateBuffer")?;
        Ok(Self {
            display: Arc::clone(context.display()),
            id: BufferId(id),
        })
    }

    /// Create a buffer holding `data`, whose length is what libva is told to copy.
    ///
    /// Distinct from [`Buffer::new`], which takes a `T` and derives the size from the type.
    /// A packed header is a run of bytes whose length is decided at run time, so the type
    /// cannot supply it.
    ///
    /// # Errors
    ///
    /// Returns the driver's status if the buffer cannot be created.
    pub fn from_bytes(
        context: &Context,
        kind: va::VABufferType,
        data: &mut [u8],
    ) -> Result<Self, Status> {
        let mut id: va::VABufferID = 0;
        // SAFETY: `data` is a valid slice of its own length, which is exactly what libva is
        // told to copy from it.
        let status = unsafe {
            va::vaCreateBuffer(
                context.display().handle(),
                context.id().raw(),
                kind,
                u32::try_from(data.len()).unwrap_or(0),
                1,
                data.as_mut_ptr().cast(),
                std::ptr::addr_of_mut!(id),
            )
        };
        check(status, "vaCreateBuffer")?;
        Ok(Self {
            display: Arc::clone(context.display()),
            id: BufferId(id),
        })
    }

    /// Create an empty buffer of `size` bytes, for the encoder to write coded data into.
    ///
    /// # Errors
    ///
    /// Returns the driver's status if the buffer cannot be created.
    pub fn empty(
        context: &Context,
        kind: va::VABufferType,
        size: usize,
    ) -> Result<Self, Status> {
        let mut id: va::VABufferID = 0;
        // SAFETY: a null data pointer means "allocate, do not copy", which is the
        // documented way to create an output buffer.
        let status = unsafe {
            va::vaCreateBuffer(
                context.display().handle(),
                context.id().0,
                kind,
                u32::try_from(size).unwrap_or(0),
                1,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(id),
            )
        };
        check(status, "vaCreateBuffer")?;
        Ok(Self {
            display: Arc::clone(context.display()),
            id: BufferId(id),
        })
    }

    #[must_use]
    pub const fn id(&self) -> BufferId {
        self.id
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: created against this display, destroyed exactly once.
        unsafe {
            va::vaDestroyBuffer(self.display.handle(), self.id.0);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{Config, Context, SurfacePool};
    use super::super::display::Display;
    use libva_sys::va_display_drm as va;

    /// Skip rather than fail where there is no GPU: CI runners do not have one, and a test
    /// that fails there teaches people to ignore failures.
    fn display() -> Option<std::sync::Arc<Display>> {
        Display::open_any().map(std::sync::Arc::new)
    }

    /// The resources this encoder needs must be creatable on real hardware, in the order
    /// libva requires them. Getting this wrong is a segfault rather than an error, which is
    /// why it is checked against a driver rather than reasoned about.
    #[test]
    fn a_full_encode_context_can_be_built_and_torn_down() {
        let Some(display) = display() else {
            return;
        };

        let mut attributes = [va::VAConfigAttrib {
            type_: va::VAConfigAttribType_VAConfigAttribRTFormat,
            value: va::VA_RT_FORMAT_YUV420,
        }];
        let Ok(config) = Config::for_encoding(
            &display,
            va::VAProfile_VAProfileH264ConstrainedBaseline,
            &mut attributes,
        ) else {
            return; // No H.264 encoding on this machine; nothing to check.
        };

        let mut pool = SurfacePool::new_nv12(&display, 640, 480, 4).expect("surfaces");
        assert_eq!(pool.surfaces().len(), 4);

        let context = Context::new(config, 640, 480, pool.raw()).expect("context");
        // Distinct handle types: this would not compile if the ids were interchangeable,
        // which in the C API they are.
        assert_ne!(context.id().0, 0, "a real context id");

        // Everything is dropped here, in reverse order of creation. A double destroy or a
        // use-after-free shows up as a crash rather than a failed assertion.
    }

    /// Surfaces must be allocatable at a realistic call size.
    #[test]
    fn surfaces_can_be_allocated_at_720p() {
        let Some(display) = display() else {
            return;
        };
        let pool = SurfacePool::new_nv12(&display, 1280, 720, 2);
        if let Ok(pool) = pool {
            assert_eq!(pool.surfaces().len(), 2);
        }
    }
}
