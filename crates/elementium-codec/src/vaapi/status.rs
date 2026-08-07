//! VAAPI return codes, as a type rather than an integer to remember to check.
//!
//! Every libva call returns `VAStatus`, and ignoring one is both easy and silent: the call
//! appears to succeed and the next one operates on a resource that was never created. The
//! `Result` here makes that a compile error via `#[must_use]`, and the operation name makes
//! a failure diagnosable without a debugger.

use std::fmt;

use libva_sys::va_display_drm as va;

/// A failed libva call: which one, and what the driver said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The libva function that failed, so a log line names it rather than a code alone.
    pub operation: &'static str,
    /// The driver's `VAStatus`.
    pub code: i32,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.operation, self.describe())
    }
}

impl std::error::Error for Status {}

impl Status {
    /// The driver's own description of the code.
    ///
    /// `vaErrorStr` returns a static C string, which is far more use than the number: a
    /// reader can act on "attribute not supported" and cannot act on `-6`.
    #[must_use]
    pub fn describe(&self) -> String {
        // SAFETY: `vaErrorStr` returns a pointer to a static string for any input, valid
        // for the program's lifetime.
        let raw = unsafe { va::vaErrorStr(self.code) };
        if raw.is_null() {
            return format!("status {}", self.code);
        }
        // SAFETY: non-null, NUL-terminated and static, as documented.
        unsafe { std::ffi::CStr::from_ptr(raw) }
            .to_str()
            .map_or_else(|_| format!("status {}", self.code), ToOwned::to_owned)
    }
}

/// Turn a `VAStatus` into a `Result`.
///
/// `#[must_use]` on `Result` is what makes an unchecked call a warning rather than a
/// silent corruption.
///
/// # Errors
///
/// Returns [`Status`] for any non-success code.
pub fn check(status: va::VAStatus, operation: &'static str) -> Result<(), Status> {
    if status == i32::try_from(va::VA_STATUS_SUCCESS).unwrap_or(0) {
        Ok(())
    } else {
        Err(Status {
            operation,
            code: status,
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{Status, check};

    /// Success must be success and everything else must be an error. A driver returning an
    /// unexpected code must not be read as working.
    #[test]
    fn only_success_is_ok() {
        assert!(check(0, "test").is_ok());
        assert!(check(-1, "test").is_err());
        assert!(check(1, "test").is_err());
    }

    /// A failure must name the call. "vaCreateContext failed" is actionable; a bare code
    /// sends the reader to the header files.
    #[test]
    fn a_failure_names_the_operation() {
        let err = check(-1, "vaCreateContext").expect_err("should fail");
        assert_eq!(err.operation, "vaCreateContext");
        assert!(err.to_string().contains("vaCreateContext"), "got {err}");
    }

    /// The driver's description is more use than the number.
    #[test]
    fn a_failure_describes_itself() {
        let status = Status {
            operation: "vaCreateConfig",
            code: -1,
        };
        assert!(!status.describe().is_empty());
    }
}
