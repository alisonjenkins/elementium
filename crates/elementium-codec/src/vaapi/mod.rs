//! Hardware video encoding through VAAPI.
//!
//! Split so each layer can be checked on its own: [`status`] turns return codes into
//! errors that cannot be ignored, [`display`] owns the connection to the driver,
//! [`resource`] gives every libva handle a distinct type with its own destructor, and the
//! encoder above them is written against those rather than against raw integers.
//!
//! That split is not decoration. libva's handles are all `u32`, its resources must be
//! destroyed in the reverse order of creation, and every call returns a status that is
//! silent when ignored. Each of those is a segfault waiting to happen, and each is
//! answered by a type here.

pub mod display;
pub mod resource;
pub mod status;

pub use display::Display;
pub use status::Status;
