//! Stable status values returned by ABI callbacks.

use core::fmt;

/// A status code returned across the C ABI.
///
/// This is an integer wrapper rather than an enum so an unknown code from a
/// newer peer remains representable and can be rejected without constructing
/// an invalid Rust value.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FcStatus(i32);

impl FcStatus {
    /// Creates a status from its exact C integer representation.
    pub const fn from_code(code: i32) -> Self {
        Self(code)
    }

    /// Returns the exact C integer representation.
    pub const fn code(self) -> i32 {
        self.0
    }

    /// Returns whether this status reports success.
    pub const fn is_ok(self) -> bool {
        self.0 == FC_OK.0
    }
}

impl fmt::Display for FcStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "plugin ABI status {}", self.0)
    }
}

/// The callback completed successfully.
pub const FC_OK: FcStatus = FcStatus::from_code(0);

/// The callback failed without a more specific status.
pub const FC_ERROR: FcStatus = FcStatus::from_code(1);

/// The plugin caught an unwinding panic before it crossed the C boundary.
pub const FC_PLUGIN_PANIC: FcStatus = FcStatus::from_code(2);

/// A host call requested a capability that was not granted.
pub const FC_CAPABILITY_DENIED: FcStatus = FcStatus::from_code(3);

/// A callback argument failed its documented validation.
pub const FC_INVALID_ARGUMENT: FcStatus = FcStatus::from_code(4);

/// A host-owned output buffer cannot hold the complete bounded result.
///
/// The buffer is fixed for one plugin callback, so this is a terminal result
/// for that query rather than a request to retry the callback.
pub const FC_BUFFER_TOO_SMALL: FcStatus = FcStatus::from_code(5);

/// The host-owned bounded command buffer rejected the newest command.
///
/// Commands already accepted for this callback remain ordered and owned unless
/// the plugin callback later returns a failure status, in which case the host
/// discards the entire callback buffer.
pub const FC_COMMAND_BUFFER_FULL: FcStatus = FcStatus::from_code(6);
