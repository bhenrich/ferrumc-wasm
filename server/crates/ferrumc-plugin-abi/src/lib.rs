#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod records;
mod semantics;
mod status;
mod version;

pub use records::{
    FcAbiHeader, FcBlockBreakEventPayloadV1, FcBlockPosV1, FcBlockStateRequestPayloadV1,
    FcBytesView, FcCallHandle, FcCommandKind, FcCommandV1, FcEventKind, FcEventV1, FcHostCallFn,
    FcHostDiagnosticFn, FcHostEmitFn, FcHostFunctionsV1, FcHostHandle, FcHostRequestKind,
    FcHostRequestV1, FcOutputBufferV1, FcPlayerIdV1, FcPluginDescriptorV1, FcPluginEntryV1Fn,
    FcPluginFunctionsFn, FcPluginFunctionsV1, FcPluginHandle, FcPluginInitFn, FcPluginOnEventFn,
    FcPluginShutdownFn, FcResourceHandle, FcSemanticVersion, FcSetBlockCommandPayloadV1, FcStrFn,
    FcStrView, ENTRYPOINT_V1,
};
pub use semantics::{
    FC_CAPABILITIES_V1, FC_CAPABILITY_READ_PERMISSIONS, FC_CAPABILITY_READ_WORLD,
    FC_CAPABILITY_RECEIVE_EVENTS, FC_CAPABILITY_REGISTER_COMMANDS, FC_CAPABILITY_STORAGE,
    FC_CAPABILITY_SUBMIT_INTENTS, FC_CAPABILITY_VETO_BLOCK_EDITS, FC_CAPABILITY_VETO_EVENTS,
    FC_COMMAND_FLAGS_NONE, FC_DIAGNOSTIC_DEBUG, FC_DIAGNOSTIC_ERROR, FC_DIAGNOSTIC_INFO,
    FC_DIAGNOSTIC_TRACE, FC_DIAGNOSTIC_WARN, FC_EVENT_FLAGS_NONE, FC_HOST_REQUEST_FLAGS_NONE,
};
pub use status::{
    FcStatus, FC_BUFFER_TOO_SMALL, FC_CAPABILITY_DENIED, FC_COMMAND_BUFFER_FULL, FC_ERROR,
    FC_INVALID_ARGUMENT, FC_OK, FC_PLUGIN_PANIC,
};
pub use version::{
    negotiate_current, AbiVersion, AbiVersionError, AbiVersionPolicy, ABI_MAJOR, ABI_MINOR,
    CURRENT_ABI,
};

#[cfg(test)]
mod layout_tests;
