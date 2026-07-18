//! Stable scalar assignments shared by ABI v1 peers.

/// Grants read-only world-query host calls.
pub const FC_CAPABILITY_READ_WORLD: u64 = 1 << 0;

/// Grants submission to the host-owned bounded command buffer.
pub const FC_CAPABILITY_SUBMIT_INTENTS: u64 = 1 << 1;

/// Grants command registration during initialization.
pub const FC_CAPABILITY_REGISTER_COMMANDS: u64 = 1 << 2;

/// Grants subscription to and receipt of plugin events.
pub const FC_CAPABILITY_RECEIVE_EVENTS: u64 = 1 << 3;

/// Grants read-only permission-query host calls.
pub const FC_CAPABILITY_READ_PERMISSIONS: u64 = 1 << 4;

/// Grants access to the plugin's host-selected storage namespace.
pub const FC_CAPABILITY_STORAGE: u64 = 1 << 5;

/// Grants participation in block-edit decision hooks.
pub const FC_CAPABILITY_VETO_BLOCK_EDITS: u64 = 1 << 6;

/// Grants participation in vetoable non-block event hooks.
pub const FC_CAPABILITY_VETO_EVENTS: u64 = 1 << 7;

/// Every capability bit assigned by ABI v1 minor zero.
pub const FC_CAPABILITIES_V1: u64 = FC_CAPABILITY_READ_WORLD
    | FC_CAPABILITY_SUBMIT_INTENTS
    | FC_CAPABILITY_REGISTER_COMMANDS
    | FC_CAPABILITY_RECEIVE_EVENTS
    | FC_CAPABILITY_READ_PERMISSIONS
    | FC_CAPABILITY_STORAGE
    | FC_CAPABILITY_VETO_BLOCK_EDITS
    | FC_CAPABILITY_VETO_EVENTS;

/// The current event-envelope flags value.
///
/// ABI v1 minor zero defines no event flags; nonzero bits are rejected.
pub const FC_EVENT_FLAGS_NONE: u32 = 0;

/// The current command-envelope flags value.
///
/// ABI v1 minor zero defines no command flags; nonzero bits are rejected.
pub const FC_COMMAND_FLAGS_NONE: u32 = 0;

/// The current host-request-envelope flags value.
///
/// ABI v1 minor zero defines no request flags; nonzero bits are rejected.
pub const FC_HOST_REQUEST_FLAGS_NONE: u32 = 0;

/// Diagnostic severity for an error.
pub const FC_DIAGNOSTIC_ERROR: u32 = 1;

/// Diagnostic severity for a warning.
pub const FC_DIAGNOSTIC_WARN: u32 = 2;

/// Diagnostic severity for informational output.
pub const FC_DIAGNOSTIC_INFO: u32 = 3;

/// Diagnostic severity for debug output.
pub const FC_DIAGNOSTIC_DEBUG: u32 = 4;

/// Diagnostic severity for trace output.
pub const FC_DIAGNOSTIC_TRACE: u32 = 5;
