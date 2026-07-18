//! Safe host-side invocation of raw-validated plugin callbacks.

use std::cell::Cell;
use std::fmt;
use std::mem::{align_of, size_of};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering};

use ferrumc_plugin_abi::{
    negotiate_current, FcAbiHeader, FcBytesView, FcCallHandle, FcCommandV1, FcEventV1,
    FcHostFunctionsV1, FcHostHandle, FcHostRequestV1, FcOutputBufferV1, FcPluginHandle, FcStatus,
    FcStrView, FC_BUFFER_TOO_SMALL, FC_ERROR, FC_INVALID_ARGUMENT, FC_OK,
};

use crate::loader::LoadedAbiPlugin;
use crate::values::{
    OwnedCommand, OwnedEvent, OwnedHostRequest, OwnedPluginMetadata, ValidatedCallbacks,
};

/// Default fixed host-query buffer offered during a plugin callback.
pub const DEFAULT_OUTPUT_CAPACITY: usize = 64 * 1024;

/// Default largest event, command, request, or diagnostic payload copied during
/// one callback.
pub const DEFAULT_PAYLOAD_LIMIT: u64 = 1024 * 1024;

/// Hard ceiling for a configured callback output buffer.
pub const MAX_OUTPUT_CAPACITY: usize = 1024 * 1024;

/// Hard ceiling for a configured call-scoped payload copy.
pub const MAX_PAYLOAD_LIMIT: u64 = 16 * 1024 * 1024;

/// Bounded memory policy for one native callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvocationLimits {
    payload_bytes: u64,
    output_bytes: usize,
}

impl InvocationLimits {
    /// The conservative default callback limits.
    pub const DEFAULT: Self = Self {
        payload_bytes: DEFAULT_PAYLOAD_LIMIT,
        output_bytes: DEFAULT_OUTPUT_CAPACITY,
    };

    /// Creates validated callback limits.
    pub const fn new(payload_bytes: u64, output_bytes: usize) -> Result<Self, CallbackError> {
        if payload_bytes > MAX_PAYLOAD_LIMIT || output_bytes > MAX_OUTPUT_CAPACITY {
            return Err(CallbackError::LimitsTooLarge {
                payload_bytes,
                output_bytes,
            });
        }
        Ok(Self {
            payload_bytes,
            output_bytes,
        })
    }

    /// Returns the maximum copied payload length.
    pub const fn payload_bytes(self) -> u64 {
        self.payload_bytes
    }

    /// Returns the fixed host-query buffer length.
    pub const fn output_bytes(self) -> usize {
        self.output_bytes
    }
}

impl Default for InvocationLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Result of one read-only host request issued by a plugin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostCallOutcome {
    /// The bounded response bytes to copy into the call's host-owned buffer.
    Response(Vec<u8>),
    /// A non-success ABI status with no response bytes.
    Status(FcStatus),
}

/// Safe host facades available to one native callback.
///
/// Implementations receive only owned, validated values. The caller of
/// [`PluginInstance::on_event`] retains ownership of any command buffer and can
/// discard it when the plugin returns a non-success status.
pub trait HostServices {
    /// Handles one validated read-only request.
    fn call(&mut self, request: OwnedHostRequest) -> HostCallOutcome;

    /// Admits one validated command into the host's bounded callback buffer.
    fn emit(&mut self, command: OwnedCommand) -> FcStatus;

    /// Records one validated diagnostic.
    fn diagnostic(&mut self, level: u32, message: String) -> FcStatus;
}

/// A failure before or around a validated plugin callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallbackError {
    /// A configured memory bound exceeds the crate's hard ceiling.
    LimitsTooLarge {
        /// Requested copied-payload ceiling.
        payload_bytes: u64,
        /// Requested output-buffer length.
        output_bytes: usize,
    },
    /// An owned event exceeds the configured call-scoped payload ceiling.
    EventPayloadTooLarge {
        /// Declared event payload length.
        declared: u64,
        /// Configured payload ceiling.
        maximum: u64,
    },
    /// The supported target cannot represent an internal opaque-handle address.
    HandleAddressUnsupported,
    /// A plugin callback attempted to nest another callback on the same thread.
    ReentrantInvocation,
    /// The per-instance call-handle sequence reached its permanent terminal value.
    CallHandleExhausted,
    /// Initialization returned success without a valid plugin handle.
    InvalidPluginHandle,
    /// A plugin lifecycle callback returned a non-success status.
    Status(FcStatus),
}

impl fmt::Display for CallbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitsTooLarge {
                payload_bytes,
                output_bytes,
            } => write!(
                formatter,
                "native callback limits exceed hard ceilings: payload {payload_bytes}, output {output_bytes}"
            ),
            Self::EventPayloadTooLarge { declared, maximum } => write!(
                formatter,
                "native event payload length {declared} exceeds limit {maximum}"
            ),
            Self::HandleAddressUnsupported => {
                formatter.write_str("target cannot represent an internal native callback handle")
            }
            Self::ReentrantInvocation => {
                formatter.write_str("native plugin callbacks cannot be reentrant")
            }
            Self::CallHandleExhausted => {
                formatter.write_str("native plugin call-handle sequence is exhausted")
            }
            Self::InvalidPluginHandle => formatter.write_str(
                "native plugin initialization succeeded without a valid plugin handle",
            ),
            Self::Status(status) => write!(formatter, "native plugin callback returned {status}"),
        }
    }
}

impl std::error::Error for CallbackError {}

#[derive(Debug)]
struct HostIdentity {
    next_call: AtomicU64,
    _private: u8,
}

struct CallFrame<'a> {
    host: FcHostHandle,
    call: FcCallHandle,
    services: &'a mut dyn HostServices,
    output_record: *mut FcOutputBufferV1,
    output_data: *mut u8,
    output_capacity: usize,
    payload_limit: u64,
}

#[derive(Clone, Copy)]
struct ActiveCall {
    frame: *mut (),
    host: FcHostHandle,
    call: FcCallHandle,
}

#[derive(Clone, Copy)]
struct ThreadCallState {
    active: Option<ActiveCall>,
}

impl ThreadCallState {
    const INITIAL: Self = Self { active: None };
}

struct ActiveCallGuard<'state> {
    state: &'state Cell<ThreadCallState>,
}

impl Drop for ActiveCallGuard<'_> {
    fn drop(&mut self) {
        self.state.set(ThreadCallState { active: None });
    }
}

/// One initialized native plugin instance.
///
/// The validated callback pointers and internal host identity remain private.
/// Dropping this value does not unload its native library, which is permanently
/// resident after a successful platform load.
pub struct PluginInstance {
    metadata: OwnedPluginMetadata,
    callbacks: ValidatedCallbacks,
    plugin: FcPluginHandle,
    identity: &'static HostIdentity,
    limits: InvocationLimits,
}

impl fmt::Debug for PluginInstance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginInstance")
            .field("metadata", &self.metadata)
            .field("plugin", &self.plugin)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl LoadedAbiPlugin {
    /// Initializes a new plugin instance using conservative callback limits.
    ///
    /// The validated loaded plugin is a reusable factory. Each successful call
    /// creates an independent plugin handle and host identity; callers remain
    /// responsible for preventing concurrent callbacks on the returned
    /// [`PluginInstance`].
    pub fn initialize(
        &self,
        granted_capabilities: u64,
        services: &mut dyn HostServices,
    ) -> Result<PluginInstance, CallbackError> {
        self.initialize_with_limits(granted_capabilities, InvocationLimits::DEFAULT, services)
    }

    /// Initializes a new plugin instance using explicit bounded callback limits.
    ///
    /// A failed initialization does not consume this factory, so lifecycle
    /// policy may report the error and attempt a later enable without reopening
    /// the permanently resident library.
    pub fn initialize_with_limits(
        &self,
        granted_capabilities: u64,
        limits: InvocationLimits,
        services: &mut dyn HostServices,
    ) -> Result<PluginInstance, CallbackError> {
        let metadata = self.metadata().clone();
        let callbacks = self.callbacks();
        let identity = Box::leak(Box::new(HostIdentity {
            next_call: AtomicU64::new(1),
            _private: 0,
        }));
        let host = host_handle(identity)?;
        let call = next_call(identity)?;
        let mut plugin = FcPluginHandle::INVALID;
        let status = invoke_raw(
            host,
            call,
            limits,
            services,
            |call, host_functions, output| {
                let callback = callbacks.init();
                // SAFETY: raw validation proved this required slot non-null
                // before constructing `ValidatedCallbacks`. All arguments are
                // live, aligned, call-scoped records created by this module.
                unsafe {
                    callback(
                        host,
                        call,
                        host_functions,
                        granted_capabilities,
                        output,
                        ptr::from_mut(&mut plugin),
                    )
                }
            },
        )?;

        if !status.is_ok() {
            return Err(CallbackError::Status(status));
        }
        if !plugin.is_valid() {
            return Err(CallbackError::InvalidPluginHandle);
        }

        Ok(PluginInstance {
            metadata,
            callbacks,
            plugin,
            identity,
            limits,
        })
    }
}

impl PluginInstance {
    /// Returns the plugin metadata copied during raw validation.
    pub const fn metadata(&self) -> &OwnedPluginMetadata {
        &self.metadata
    }

    /// Returns the opaque ABI plugin handle for diagnostics and host bookkeeping.
    pub const fn plugin_handle(&self) -> FcPluginHandle {
        self.plugin
    }

    /// Invokes the plugin's event callback with an owned event.
    ///
    /// Every non-success status is returned to the caller so the caller can
    /// discard commands admitted through `services` for this callback.
    pub fn on_event(
        &mut self,
        event: &OwnedEvent,
        services: &mut dyn HostServices,
    ) -> Result<FcStatus, CallbackError> {
        let declared = u64::try_from(event.payload().len()).map_err(|_| {
            CallbackError::EventPayloadTooLarge {
                declared: u64::MAX,
                maximum: self.limits.payload_bytes,
            }
        })?;
        if declared > self.limits.payload_bytes {
            return Err(CallbackError::EventPayloadTooLarge {
                declared,
                maximum: self.limits.payload_bytes,
            });
        }

        let payload = FcBytesView::new(event.payload().as_ptr(), declared);
        let envelope = FcEventV1::new(
            event.kind(),
            event.flags(),
            event.tick(),
            event.shard(),
            payload,
        );
        let host = host_handle(self.identity)?;
        let call = next_call(self.identity)?;
        let callback = self.callbacks.on_event();
        let plugin = self.plugin;

        invoke_raw(
            host,
            call,
            self.limits,
            services,
            |call, host_functions, output| {
                // SAFETY: the callback came from a fully raw-validated table.
                // The envelope and payload remain alive for this synchronous
                // call, and the host/output records are owned by `invoke_raw`.
                unsafe {
                    callback(
                        host,
                        plugin,
                        call,
                        host_functions,
                        ptr::from_ref(&envelope),
                        output,
                    )
                }
            },
        )
    }

    /// Runs the plugin's shutdown callback exactly once by consuming the instance.
    pub fn shutdown(self, services: &mut dyn HostServices) -> Result<FcStatus, CallbackError> {
        let host = host_handle(self.identity)?;
        let call = next_call(self.identity)?;
        let callback = self.callbacks.shutdown();
        let plugin = self.plugin;
        invoke_raw(
            host,
            call,
            self.limits,
            services,
            |call, host_functions, output| {
                // SAFETY: the callback came from a fully raw-validated table.
                // The instance is consumed, no other callback is concurrent,
                // and all pointer arguments are live for this call.
                unsafe { callback(host, plugin, call, host_functions, output) }
            },
        )
    }
}

fn host_handle(identity: &HostIdentity) -> Result<FcHostHandle, CallbackError> {
    let address = ptr::from_ref(identity) as usize;
    let raw = u64::try_from(address).map_err(|_| CallbackError::HandleAddressUnsupported)?;
    if raw == 0 {
        return Err(CallbackError::HandleAddressUnsupported);
    }
    Ok(FcHostHandle::from_raw(raw))
}

fn next_call(identity: &HostIdentity) -> Result<FcCallHandle, CallbackError> {
    let raw = identity
        .next_call
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| CallbackError::CallHandleExhausted)?;
    if raw == 0 {
        return Err(CallbackError::CallHandleExhausted);
    }
    Ok(FcCallHandle::from_raw(raw))
}

fn invoke_raw(
    host: FcHostHandle,
    call: FcCallHandle,
    limits: InvocationLimits,
    services: &mut dyn HostServices,
    invoke: impl FnOnce(FcCallHandle, *const FcHostFunctionsV1, *mut FcOutputBufferV1) -> FcStatus,
) -> Result<FcStatus, CallbackError> {
    let mut output_bytes = vec![0; limits.output_bytes];
    let output_data = output_bytes.as_mut_ptr();
    let output_capacity =
        u64::try_from(limits.output_bytes).map_err(|_| CallbackError::HandleAddressUnsupported)?;
    let mut output_record = FcOutputBufferV1::new(output_data, output_capacity);
    let mut frame = CallFrame {
        host,
        call: FcCallHandle::INVALID,
        services,
        output_record: ptr::from_mut(&mut output_record),
        output_data,
        output_capacity: limits.output_bytes,
        payload_limit: limits.payload_bytes,
    };
    activate_call(&mut frame, call, invoke)
}

static HOST_FUNCTIONS: FcHostFunctionsV1 =
    FcHostFunctionsV1::new(host_call, host_emit, host_diagnostic);

unsafe extern "C" fn host_call(
    host: FcHostHandle,
    call: FcCallHandle,
    request: *const FcHostRequestV1,
    output: *mut FcOutputBufferV1,
) -> FcStatus {
    match with_resolved_frame(host, call, |frame| {
        if output != frame.output_record {
            return FC_INVALID_ARGUMENT;
        }

        restore_output(frame);
        let Ok(request) = copy_request(request, frame.payload_limit) else {
            return FC_INVALID_ARGUMENT;
        };
        let Ok(outcome) = catch_unwind(AssertUnwindSafe(|| frame.services.call(request))) else {
            return FC_ERROR;
        };

        match outcome {
            HostCallOutcome::Response(response) => write_response(frame, &response),
            HostCallOutcome::Status(status) => {
                if status.is_ok() {
                    FC_ERROR
                } else {
                    status
                }
            }
        }
    }) {
        Some(status) => status,
        None => FC_INVALID_ARGUMENT,
    }
}

unsafe extern "C" fn host_emit(
    host: FcHostHandle,
    call: FcCallHandle,
    command: *const FcCommandV1,
) -> FcStatus {
    match with_resolved_frame(host, call, |frame| {
        let Ok(command) = copy_command(command, frame.payload_limit) else {
            return FC_INVALID_ARGUMENT;
        };
        match catch_unwind(AssertUnwindSafe(|| frame.services.emit(command))) {
            Ok(status) => status,
            Err(_) => FC_ERROR,
        }
    }) {
        Some(status) => status,
        None => FC_INVALID_ARGUMENT,
    }
}

unsafe extern "C" fn host_diagnostic(
    host: FcHostHandle,
    call: FcCallHandle,
    level: u32,
    message: FcStrView,
) -> FcStatus {
    match with_resolved_frame(host, call, |frame| {
        let Ok(bytes) = copy_view(
            FcBytesView::new(message.data(), message.len()),
            frame.payload_limit,
        ) else {
            return FC_INVALID_ARGUMENT;
        };
        let Ok(message) = String::from_utf8(bytes) else {
            return FC_INVALID_ARGUMENT;
        };
        match catch_unwind(AssertUnwindSafe(|| {
            frame.services.diagnostic(level, message)
        })) {
            Ok(status) => status,
            Err(_) => FC_ERROR,
        }
    }) {
        Some(status) => status,
        None => FC_INVALID_ARGUMENT,
    }
}

fn activate_call(
    frame: &mut CallFrame<'_>,
    call: FcCallHandle,
    invoke: impl FnOnce(FcCallHandle, *const FcHostFunctionsV1, *mut FcOutputBufferV1) -> FcStatus,
) -> Result<FcStatus, CallbackError> {
    with_thread_call_state(|state| {
        let current = state.get();
        if current.active.is_some() {
            return Err(CallbackError::ReentrantInvocation);
        }

        if !call.is_valid() {
            return Err(CallbackError::HandleAddressUnsupported);
        }
        frame.call = call;
        state.set(ThreadCallState {
            active: Some(ActiveCall {
                frame: ptr::from_mut(frame).cast(),
                host: frame.host,
                call,
            }),
        });
        let _guard = ActiveCallGuard { state };

        Ok(invoke(
            call,
            ptr::from_ref(&HOST_FUNCTIONS),
            frame.output_record,
        ))
    })
}

fn with_thread_call_state<R>(use_state: impl FnOnce(&Cell<ThreadCallState>) -> R) -> R {
    thread_local! {
        // One bounded call slot per callback thread lets the raw host callbacks
        // validate opaque tokens without a shared process-global registry.
        static STATE: Cell<ThreadCallState> = const { Cell::new(ThreadCallState::INITIAL) };
    }
    STATE.with(use_state)
}

fn with_resolved_frame<R>(
    host: FcHostHandle,
    call: FcCallHandle,
    use_frame: impl FnOnce(&mut CallFrame<'_>) -> R,
) -> Option<R> {
    if !host.is_valid() || !call.is_valid() {
        return None;
    }
    with_thread_call_state(|state| {
        let active = state.get().active?;
        if active.host != host || active.call != call || active.frame.is_null() {
            return None;
        }

        // SAFETY: `active.frame` was stored only by `activate_call` from its
        // live stack frame and is removed by the guard before that frame dies.
        // Crucially, this pointer comes from internal thread-local state after
        // the plugin-supplied scalar tokens match; no plugin-provided integer
        // is ever converted into or dereferenced as a pointer.
        let frame = unsafe { &mut *active.frame.cast::<CallFrame<'_>>() };
        Some(use_frame(frame))
    })
}

fn restore_output(frame: &mut CallFrame<'_>) {
    let capacity = u64::try_from(frame.output_capacity).map_or(u64::MAX, |value| value);
    // SAFETY: `CallFrame` stores the address of the live, aligned output record
    // created by `invoke_raw`; pointer identity was checked before this call.
    unsafe {
        frame
            .output_record
            .write(FcOutputBufferV1::new(frame.output_data, capacity));
    }
}

fn write_response(frame: &mut CallFrame<'_>, response: &[u8]) -> FcStatus {
    let Ok(result_len) = u64::try_from(response.len()) else {
        return FC_ERROR;
    };
    if response.len() > frame.output_capacity {
        // SAFETY: the output record is the live host-owned record restored by
        // `host_call`; only its scalar result length is changed.
        unsafe {
            (*frame.output_record).set_result_len(result_len);
        }
        return FC_BUFFER_TOO_SMALL;
    }

    if !response.is_empty() {
        // SAFETY: `response` and the host-owned output allocation are distinct,
        // live allocations. The preceding capacity check proves the complete
        // response fits the writable destination.
        unsafe {
            ptr::copy_nonoverlapping(response.as_ptr(), frame.output_data, response.len());
        }
    }
    // SAFETY: the output record remains live and aligned for this call.
    unsafe {
        (*frame.output_record).set_result_len(result_len);
    }
    FC_OK
}

fn copy_request(
    pointer: *const FcHostRequestV1,
    payload_limit: u64,
) -> Result<OwnedHostRequest, ()> {
    validate_record_pointer(pointer, FcHostRequestV1::STRUCT_SIZE)?;
    // SAFETY: the pointer passed the null/alignment/header/version/size checks.
    // The callback contract promises an initialized immutable record for this
    // call, and this C record has no invalid scalar bit patterns.
    let request = unsafe { ptr::read(pointer) };
    let payload = copy_view(request.payload(), payload_limit)?;
    Ok(OwnedHostRequest::new(
        request.kind(),
        request.flags(),
        request.target(),
        payload,
    ))
}

fn copy_command(pointer: *const FcCommandV1, payload_limit: u64) -> Result<OwnedCommand, ()> {
    validate_record_pointer(pointer, FcCommandV1::STRUCT_SIZE)?;
    // SAFETY: the pointer passed the null/alignment/header/version/size checks.
    // The callback contract promises an initialized immutable record for this
    // call, and this C record has no invalid scalar bit patterns.
    let command = unsafe { ptr::read(pointer) };
    let payload = copy_view(command.payload(), payload_limit)?;
    Ok(OwnedCommand::new(
        command.kind(),
        command.flags(),
        command.target(),
        payload,
    ))
}

fn validate_record_pointer<T>(pointer: *const T, required_size: u32) -> Result<(), ()> {
    let address = pointer as usize;
    if pointer.is_null() || address.checked_rem(align_of::<T>()) != Some(0) {
        return Err(());
    }
    if size_of::<FcAbiHeader>() != usize::try_from(FcAbiHeader::BYTE_SIZE).map_err(|_| ())? {
        return Err(());
    }
    // SAFETY: null and record alignment were checked. The unsafe callback
    // contract supplies a live leading size word; every `u32` bit pattern is
    // valid and unaligned reading avoids a stronger cast-alignment claim.
    let declared_size = unsafe { ptr::read_unaligned(pointer.cast::<u32>()) };
    if declared_size < FcAbiHeader::BYTE_SIZE {
        return Err(());
    }
    let bytes = pointer.cast::<u8>();
    // SAFETY: the declared size now covers the complete common header, so its
    // fixed-width major field at byte offset four is readable.
    let abi_major = unsafe { ptr::read_unaligned(bytes.add(4).cast::<u16>()) };
    // SAFETY: the declared size covers the minor field at byte offset six.
    let abi_minor = unsafe { ptr::read_unaligned(bytes.add(6).cast::<u16>()) };
    if negotiate_current(ferrumc_plugin_abi::AbiVersion::new(abi_major, abi_minor)).is_err()
        || declared_size < required_size
    {
        return Err(());
    }
    Ok(())
}

fn copy_view(view: FcBytesView, maximum: u64) -> Result<Vec<u8>, ()> {
    let declared = view.len();
    if declared == 0 {
        return Ok(Vec::new());
    }
    if declared > maximum {
        return Err(());
    }
    let length = usize::try_from(declared).map_err(|_| ())?;
    if length > isize::MAX.unsigned_abs() || view.data().is_null() {
        return Err(());
    }
    let start = view.data() as usize;
    if start.checked_add(length).is_none() {
        return Err(());
    }

    // SAFETY: the explicit-length view is non-null; its length was bounded,
    // converted without truncation, checked against `isize::MAX`, and checked
    // for address wrap. The unsafe callback contract supplies live readable
    // bytes for this call. The bytes are copied before returning.
    let bytes = unsafe { slice::from_raw_parts(view.data(), length) };
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::{
        copy_view, invoke_raw, next_call, CallbackError, HostCallOutcome, HostIdentity,
        HostServices, InvocationLimits, DEFAULT_OUTPUT_CAPACITY, DEFAULT_PAYLOAD_LIMIT,
        MAX_OUTPUT_CAPACITY, MAX_PAYLOAD_LIMIT,
    };
    use crate::values::{OwnedPluginMetadata, PluginSemanticVersion, ValidatedCallbacks};
    use crate::{LoadedAbiPlugin, OwnedCommand, OwnedHostRequest};
    use ferrumc_plugin_abi::{
        FcBytesView, FcCallHandle, FcCommandKind, FcCommandV1, FcEventV1, FcHostFunctionsV1,
        FcHostHandle, FcHostRequestKind, FcHostRequestV1, FcOutputBufferV1, FcPluginHandle,
        FcResourceHandle, FcStatus, FcStrView, CURRENT_ABI, FC_BUFFER_TOO_SMALL,
        FC_COMMAND_FLAGS_NONE, FC_DIAGNOSTIC_INFO, FC_ERROR, FC_HOST_REQUEST_FLAGS_NONE,
        FC_INVALID_ARGUMENT, FC_OK,
    };

    #[derive(Default)]
    struct RecordingHost {
        requests: Vec<OwnedHostRequest>,
        commands: Vec<OwnedCommand>,
        diagnostics: Vec<(u32, String)>,
        response: Vec<u8>,
        diagnostic_status: Option<FcStatus>,
    }

    impl HostServices for RecordingHost {
        fn call(&mut self, request: OwnedHostRequest) -> HostCallOutcome {
            self.requests.push(request);
            HostCallOutcome::Response(self.response.clone())
        }

        fn emit(&mut self, command: OwnedCommand) -> FcStatus {
            self.commands.push(command);
            FC_OK
        }

        fn diagnostic(&mut self, level: u32, message: String) -> FcStatus {
            self.diagnostics.push((level, message));
            self.diagnostic_status.unwrap_or(FC_OK)
        }
    }

    unsafe extern "C" fn lifecycle_init(
        host: FcHostHandle,
        call: FcCallHandle,
        host_functions: *const FcHostFunctionsV1,
        _granted_capabilities: u64,
        _output: *mut FcOutputBufferV1,
        plugin_out: *mut FcPluginHandle,
    ) -> FcStatus {
        // SAFETY: this synthetic callback is invoked only through the boundary
        // with its live, raw-validated host table.
        let functions = unsafe { host_functions.read() };
        let message = "init";
        // SAFETY: the host/call pair and table are live for this callback, and
        // the UTF-8 message remains readable until the call returns.
        let status = unsafe {
            (functions.diagnostic())(
                host,
                call,
                FC_DIAGNOSTIC_INFO,
                FcStrView::new(message.as_ptr(), 4),
            )
        };
        if !status.is_ok() {
            return status;
        }

        // SAFETY: the boundary supplies a non-null aligned output pointer. The
        // process-resident host identity is nonzero and unique to this instance.
        unsafe { plugin_out.write(FcPluginHandle::from_raw(host.raw())) };
        FC_OK
    }

    unsafe extern "C" fn lifecycle_event(
        _host: FcHostHandle,
        _plugin: FcPluginHandle,
        _call: FcCallHandle,
        _host_functions: *const FcHostFunctionsV1,
        _event: *const FcEventV1,
        _output: *mut FcOutputBufferV1,
    ) -> FcStatus {
        FC_OK
    }

    unsafe extern "C" fn lifecycle_shutdown(
        host: FcHostHandle,
        _plugin: FcPluginHandle,
        call: FcCallHandle,
        host_functions: *const FcHostFunctionsV1,
        _output: *mut FcOutputBufferV1,
    ) -> FcStatus {
        // SAFETY: this synthetic callback is invoked only through the boundary
        // with its live, raw-validated host table.
        let functions = unsafe { host_functions.read() };
        let message = "shutdown";
        // SAFETY: the host/call pair and table are live for this callback, and
        // the UTF-8 message remains readable until the call returns.
        unsafe {
            (functions.diagnostic())(
                host,
                call,
                FC_DIAGNOSTIC_INFO,
                FcStrView::new(message.as_ptr(), 8),
            )
        }
    }

    fn lifecycle_factory() -> LoadedAbiPlugin {
        LoadedAbiPlugin::from_validated(
            OwnedPluginMetadata::new(
                CURRENT_ABI,
                PluginSemanticVersion::new(1, 2, 3),
                0,
                "lifecycle".to_owned(),
                "Lifecycle".to_owned(),
                "test-target".to_owned(),
            ),
            ValidatedCallbacks::new(lifecycle_init, lifecycle_event, lifecycle_shutdown),
        )
    }

    #[test]
    fn invocation_limits_enforce_bounded_memory_ceilings() {
        assert_eq!(
            InvocationLimits::DEFAULT.output_bytes(),
            DEFAULT_OUTPUT_CAPACITY
        );
        assert_eq!(
            InvocationLimits::DEFAULT.payload_bytes(),
            DEFAULT_PAYLOAD_LIMIT
        );
        assert!(InvocationLimits::new(MAX_PAYLOAD_LIMIT, MAX_OUTPUT_CAPACITY).is_ok());
        assert!(InvocationLimits::new(MAX_PAYLOAD_LIMIT + 1, 0).is_err());
        assert!(InvocationLimits::new(0, MAX_OUTPUT_CAPACITY + 1).is_err());
    }

    #[test]
    fn loaded_factory_survives_shutdown_for_a_fresh_instance() {
        let factory = lifecycle_factory();
        let mut services = RecordingHost::default();

        let first = factory
            .initialize(0, &mut services)
            .expect("first instance initializes");
        let first_handle = first.plugin_handle();
        assert_eq!(factory.metadata().id(), "lifecycle");
        assert_eq!(first.shutdown(&mut services), Ok(FC_OK));

        let second = factory
            .initialize(0, &mut services)
            .expect("second instance initializes");
        let second_handle = second.plugin_handle();
        assert_ne!(first_handle, second_handle);
        assert_eq!(second.shutdown(&mut services), Ok(FC_OK));
        assert_eq!(
            services
                .diagnostics
                .iter()
                .map(|(_, message)| message.as_str())
                .collect::<Vec<_>>(),
            ["init", "shutdown", "init", "shutdown"]
        );
    }

    #[test]
    fn failed_initialization_does_not_consume_loaded_factory() {
        let factory = lifecycle_factory();
        let mut services = RecordingHost {
            diagnostic_status: Some(FC_ERROR),
            ..RecordingHost::default()
        };

        assert!(matches!(
            factory.initialize(0, &mut services),
            Err(CallbackError::Status(status)) if status == FC_ERROR
        ));
        services.diagnostic_status = None;

        let instance = factory
            .initialize(0, &mut services)
            .expect("retry initializes from the same factory");
        assert_eq!(instance.shutdown(&mut services), Ok(FC_OK));
    }

    #[test]
    fn call_handle_exhaustion_is_permanent_without_reuse() {
        let identity = HostIdentity {
            next_call: std::sync::atomic::AtomicU64::new(u64::MAX),
            _private: 0,
        };
        assert_eq!(
            next_call(&identity),
            Err(CallbackError::CallHandleExhausted)
        );
        assert_eq!(
            next_call(&identity),
            Err(CallbackError::CallHandleExhausted)
        );
    }

    #[test]
    fn explicit_length_is_bounded_before_pointer_access() {
        static SENTINEL: u8 = 0;
        let invalid = FcBytesView::new(std::ptr::from_ref(&SENTINEL), u64::MAX);
        assert!(copy_view(invalid, 16).is_err());
        assert_eq!(copy_view(FcBytesView::empty(), 0), Ok(Vec::<u8>::new()));
    }

    #[test]
    fn host_call_outcome_retains_owned_response() {
        assert_eq!(
            HostCallOutcome::Response(vec![1, 2, 3]),
            HostCallOutcome::Response(vec![1, 2, 3])
        );
    }

    #[test]
    fn private_host_table_copies_every_variable_input_before_returning() {
        let mut services = RecordingHost {
            response: vec![9, 8, 7],
            ..RecordingHost::default()
        };
        let host = FcHostHandle::from_raw(17);
        let limits = InvocationLimits::new(32, 8).expect("bounded test limits");
        let request_payload = [1, 2];
        let command_payload = [3, 4, 5];
        let message = "diagnostic";

        let status = invoke_raw(
            host,
            FcCallHandle::from_raw(23),
            limits,
            &mut services,
            |call, host_functions, output| {
                // SAFETY: `invoke_raw` supplies its private immutable table for
                // this synchronous test callback.
                let host_functions = unsafe { host_functions.read() };
                let stale_call = FcCallHandle::from_raw(call.raw() + 1);
                // SAFETY: the deliberately foreign token must be rejected
                // before the null command pointer is inspected.
                assert_eq!(
                    unsafe { (host_functions.emit())(host, stale_call, std::ptr::null()) },
                    FC_INVALID_ARGUMENT
                );
                std::thread::scope(|scope| {
                    let cross_thread = scope
                        .spawn(|| {
                            // SAFETY: this deliberately calls from a thread
                            // with no active call slot; rejection must precede
                            // inspection of the null command pointer.
                            unsafe { (host_functions.emit())(host, call, std::ptr::null()) }
                        })
                        .join()
                        .expect("scoped test thread returns");
                    assert_eq!(cross_thread, FC_INVALID_ARGUMENT);
                });
                let request = FcHostRequestV1::new(
                    FcHostRequestKind::DIMENSION,
                    FC_HOST_REQUEST_FLAGS_NONE,
                    FcResourceHandle::INVALID,
                    FcBytesView::new(request_payload.as_ptr(), 2),
                );
                // SAFETY: all arguments were produced by `invoke_raw`; the
                // request and payload remain live for the complete call.
                let request_status = unsafe {
                    (host_functions.call())(host, call, std::ptr::from_ref(&request), output)
                };
                assert_eq!(request_status, FC_OK);
                // SAFETY: the host-owned output record remains live until this
                // test callback returns.
                let output_snapshot = unsafe { output.read() };
                assert_eq!(output_snapshot.result_len(), 3);
                // SAFETY: FC_OK initialized exactly the declared three-byte
                // prefix in the live host-owned output allocation.
                let response = unsafe { std::slice::from_raw_parts(output_snapshot.data(), 3) };
                assert_eq!(response, [9, 8, 7]);

                let command = FcCommandV1::new(
                    FcCommandKind::MESSAGE,
                    FC_COMMAND_FLAGS_NONE,
                    FcResourceHandle::INVALID,
                    FcBytesView::new(command_payload.as_ptr(), 3),
                );
                // SAFETY: the private table and handles are call-scoped, and
                // the command payload remains live for this call.
                assert_eq!(
                    unsafe { (host_functions.emit())(host, call, std::ptr::from_ref(&command),) },
                    FC_OK
                );
                // SAFETY: the UTF-8 message and private table remain live for
                // this call-scoped diagnostic.
                assert_eq!(
                    unsafe {
                        (host_functions.diagnostic())(
                            host,
                            call,
                            FC_DIAGNOSTIC_INFO,
                            FcStrView::new(message.as_ptr(), 10),
                        )
                    },
                    FC_OK
                );
                FC_OK
            },
        )
        .expect("internal handles are representable");

        assert_eq!(status, FC_OK);
        assert_eq!(services.requests.len(), 1);
        assert_eq!(services.requests[0].payload(), request_payload);
        assert_eq!(services.commands.len(), 1);
        assert_eq!(services.commands[0].payload(), command_payload);
        assert_eq!(
            services.diagnostics,
            vec![(FC_DIAGNOSTIC_INFO, message.to_owned())]
        );
    }

    #[test]
    fn fixed_output_buffer_reports_terminal_exhaustion_without_retry() {
        let mut services = RecordingHost {
            response: vec![1, 2, 3],
            ..RecordingHost::default()
        };
        let host = FcHostHandle::from_raw(19);
        let limits = InvocationLimits::new(8, 2).expect("bounded test limits");
        let status = invoke_raw(
            host,
            FcCallHandle::from_raw(29),
            limits,
            &mut services,
            |call, host_functions, output| {
                // SAFETY: `invoke_raw` supplies its private immutable table for
                // this synchronous test callback.
                let host_functions = unsafe { host_functions.read() };
                let request = FcHostRequestV1::new(
                    FcHostRequestKind::DIMENSION,
                    FC_HOST_REQUEST_FLAGS_NONE,
                    FcResourceHandle::INVALID,
                    FcBytesView::empty(),
                );
                // SAFETY: the call-scoped records remain live for this one
                // query and are never retained.
                let request_status = unsafe {
                    (host_functions.call())(host, call, std::ptr::from_ref(&request), output)
                };
                assert_eq!(request_status, FC_BUFFER_TOO_SMALL);
                // SAFETY: the output record is live until this closure returns.
                let output_snapshot = unsafe { output.read() };
                assert_eq!(output_snapshot.result_len(), 3);
                FC_OK
            },
        )
        .expect("internal handles are representable");

        assert_eq!(status, FC_OK);
        assert_eq!(services.requests.len(), 1);
    }
}
