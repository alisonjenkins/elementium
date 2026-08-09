//! VAAPI return codes, as a type rather than an integer to remember to check.
//!
//! Every libva call returns `VAStatus`, and ignoring one is both easy and silent: the call
//! appears to succeed and the next one operates on a resource that was never created. The
//! `Result` here makes that a compile error via `#[must_use]`, and the operation name makes
//! a failure diagnosable without a debugger.

use std::fmt;

use libva_sys::va_display_drm as va;

/// A failure on the VAAPI path: which operation, and why.
///
/// Most of these are a driver call's own `VAStatus` (see [`Status::driver`]), but not all --
/// some are detected by this crate before any driver call is made (a render node that will
/// not open, a JPEG that fails to parse), and those keep the real cause via
/// [`Status::caused_by`] rather than losing it inside a fabricated code. An audit of this
/// crate found four sites doing exactly that: `code: -1` standing in for a `ParseError`, a
/// `TryFromIntError` or an `io::Error` that was simply thrown away.
#[derive(Debug)]
pub struct Status {
    /// The operation that failed, so a log line names it rather than a code alone.
    operation: &'static str,
    /// The driver's `VAStatus`, or `-1` when this `Status` was not a driver call at all.
    code: i32,
    /// The real cause, when there is one beyond the code -- see the type-level docs.
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Status {
    /// A `Status` for a driver call's own return code.
    #[must_use]
    pub const fn driver(operation: &'static str, code: i32) -> Self {
        Self {
            operation,
            code,
            source: None,
        }
    }

    /// A `Status` for a failure this crate detected itself, with nothing beyond the fact --
    /// "no usable render node" is the whole story, not a summary of some other error.
    #[must_use]
    pub const fn detected(operation: &'static str) -> Self {
        Self {
            operation,
            code: -1,
            source: None,
        }
    }

    /// As [`Status::detected`], but keeping the real cause instead of discarding it.
    ///
    /// `code` stays `-1`: there is no `VAStatus` here, and `describe()` prefers `source`'s
    /// own `Display` over asking the driver to describe a code it never returned.
    pub fn caused_by(operation: &'static str, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            operation,
            code: -1,
            source: Some(Box::new(source)),
        }
    }

    /// The operation that failed.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// The driver's `VAStatus`, or `-1` for a `Status` with no driver call behind it.
    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }

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

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            // The real cause is more use than a driver description of a code that was never
            // a `VAStatus` in the first place.
            Some(source) => write!(f, "{} failed: {source}", self.operation),
            None => write!(f, "{} failed: {}", self.operation, self.describe()),
        }
    }
}

impl std::error::Error for Status {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // An implicit unsize coercion (dropping `Send + Sync`), not an `as` cast: the
        // workspace denies `as_conversions`, and `source.as_ref()`'s type is pinned by this
        // let binding's annotation rather than by a cast.
        self.source.as_ref().map(|source| {
            let source: &(dyn std::error::Error + 'static) = source.as_ref();
            source
        })
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
        Err(Status::driver(operation, status))
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
        assert_eq!(err.operation(), "vaCreateContext");
        assert!(err.to_string().contains("vaCreateContext"), "got {err}");
    }

    /// The driver's description is more use than the number.
    #[test]
    fn a_failure_describes_itself() {
        let status = Status::driver("vaCreateConfig", -1);
        assert!(!status.describe().is_empty());
    }

    /// A `Status` built from a real cause must keep it reachable through the error chain,
    /// and must show it rather than a driver description of a code that was never real.
    #[test]
    fn a_detected_failure_keeps_its_cause() {
        use std::error::Error as _;

        let cause = "not a JPEG"
            .parse::<i32>()
            .expect_err("deliberately not a number");
        let status = Status::caused_by("parse something", cause);

        assert!(status.to_string().contains("invalid digit"), "got {status}");
        assert!(
            status.source().is_some(),
            "the real cause must be reachable via Error::source"
        );
    }
}
