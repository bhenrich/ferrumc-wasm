//! Plugin-side export and callback bridge.
//!
//! This module keeps the raw half of a dynamic plugin inside the audited ABI
//! boundary. A safe SDK supplies a [`PluginBridge`] implementation and immutable
//! descriptor/table statics, then invokes [`export_plugin_v1!`]. The generated
//! entrypoint contains no plugin-authored unsafe code.

use core::fmt;
use core::marker::PhantomData;
use core::mem::{align_of, size_of};
use core::ptr::{self, NonNull};

use ferrumc_plugin_abi::{
    FcAbiHeader, FcBytesView, FcCallHandle, FcCommandKind, FcCommandV1, FcEventKind, FcEventV1,
    FcHostFunctionsV1, FcHostHandle, FcHostRequestKind, FcHostRequestV1, FcOutputBufferV1,
    FcPluginDescriptorV1, FcPluginFunctionsV1, FcPluginHandle, FcResourceHandle, FcSemanticVersion,
    FcStatus, FcStrView, ABI_MAJOR, ABI_MINOR, FC_BUFFER_TOO_SMALL, FC_COMMAND_FLAGS_NONE,
    FC_ERROR, FC_EVENT_FLAGS_NONE, FC_HOST_REQUEST_FLAGS_NONE, FC_INVALID_ARGUMENT, FC_OK,
};

/// Hidden descriptor name used by [`export_plugin_v1!`].
///
/// Safe SDKs should use [`PluginBridge::descriptor`] rather than this alias.
#[doc(hidden)]
pub type ExportedPluginDescriptorV1 = FcPluginDescriptorV1;

/// A validated event borrowed for exactly one plugin callback.
///
/// The raw event envelope and payload pointer never reach safe plugin code. The
/// payload borrow cannot be stored in [`PluginBridge::Instance`], which must be
/// `'static`.
#[derive(Clone, Copy, Debug)]
pub struct PluginEvent<'call> {
    kind: FcEventKind,
    tick: u64,
    shard: FcResourceHandle,
    payload: &'call [u8],
}

impl<'call> PluginEvent<'call> {
    /// Returns the versioned event kind.
    pub const fn kind(&self) -> FcEventKind {
        self.kind
    }

    /// Returns the exact simulation tick, or `0` when unavailable off-tick.
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Returns the call-scoped live shard handle, or
    /// [`FcResourceHandle::INVALID`] when unavailable off-tick.
    pub const fn shard(&self) -> FcResourceHandle {
        self.shard
    }

    /// Returns the validated kind-specific payload.
    pub const fn payload(&self) -> &'call [u8] {
        self.payload
    }
}

/// Failure from a safe capability-scoped host operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginCallError {
    /// A safe caller supplied a byte slice whose length does not fit ABI v1.
    PayloadTooLong,
    /// The host returned this non-success ABI status.
    HostStatus(FcStatus),
    /// The host violated the ABI v1 output-buffer result protocol.
    InvalidHostOutput,
}

impl fmt::Display for PluginCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLong => formatter.write_str("plugin call payload exceeds ABI v1"),
            Self::HostStatus(status) => write!(formatter, "host call returned {status}"),
            Self::InvalidHostOutput => {
                formatter.write_str("host returned an invalid ABI v1 output buffer")
            }
        }
    }
}

impl std::error::Error for PluginCallError {}

/// Safe, call-scoped access to the ABI v1 host function table.
///
/// Raw handles, callback pointers, and the host-owned output record are private.
/// This value is created only after the trampoline validates the complete host
/// table and output prefix. Its lifetime prevents a safe plugin instance from
/// retaining it after the callback. It is deliberately neither `Send` nor
/// `Sync`: host call state belongs to the thread currently running the callback.
pub struct PluginCall<'call> {
    host: FcHostHandle,
    call: FcCallHandle,
    host_functions: FcHostFunctionsV1,
    output: NonNull<FcOutputBufferV1>,
    output_header: FcAbiHeader,
    output_data: *mut u8,
    output_capacity: usize,
    _call_scope: PhantomData<&'call mut FcOutputBufferV1>,
    _not_send_sync: PhantomData<std::rc::Rc<()>>,
}

impl PluginCall<'_> {
    /// Runs one read-only capability-scoped host request.
    ///
    /// A successful result is copied into plugin-owned memory before this
    /// method returns. Buffer exhaustion and every other non-success status are
    /// terminal for this request; the wrapper never retries a callback.
    pub fn request(
        &mut self,
        kind: FcHostRequestKind,
        target: FcResourceHandle,
        payload: &[u8],
    ) -> Result<Vec<u8>, PluginCallError> {
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| PluginCallError::PayloadTooLong)?;
        let request = FcHostRequestV1::new(
            kind,
            FC_HOST_REQUEST_FLAGS_NONE,
            target,
            FcBytesView::new(payload.as_ptr(), payload_len),
        );

        if !self.output_identity_is_unchanged() {
            return Err(PluginCallError::InvalidHostOutput);
        }

        let call = self.host_functions.call();
        // SAFETY: construction validated the required host callback slot and
        // bound the host/call/output values to this live invocation. `request`
        // and its payload remain valid for the complete foreign call.
        let status = unsafe {
            call(
                self.host,
                self.call,
                ptr::from_ref(&request),
                self.output.as_ptr(),
            )
        };

        let output = self.output_snapshot();
        if output.header() != self.output_header
            || output.data() != self.output_data
            || usize::try_from(output.capacity()).ok() != Some(self.output_capacity)
        {
            return Err(PluginCallError::InvalidHostOutput);
        }

        let result_len =
            usize::try_from(output.result_len()).map_err(|_| PluginCallError::InvalidHostOutput)?;
        if status == FC_OK {
            if result_len > self.output_capacity {
                return Err(PluginCallError::InvalidHostOutput);
            }
            if result_len == 0 {
                return Ok(Vec::new());
            }

            // SAFETY: the original output pointer was validated for the full
            // capacity, and an FC_OK host callback promises that exactly the
            // `result_len` prefix is initialized before returning.
            let result = unsafe { core::slice::from_raw_parts(self.output_data, result_len) };
            return Ok(result.to_vec());
        }

        if status == FC_BUFFER_TOO_SMALL {
            if result_len <= self.output_capacity {
                return Err(PluginCallError::InvalidHostOutput);
            }
        } else if result_len != 0 {
            return Err(PluginCallError::InvalidHostOutput);
        }

        Err(PluginCallError::HostStatus(status))
    }

    /// Submits one command to the callback's host-owned bounded command buffer.
    pub fn emit(
        &mut self,
        kind: FcCommandKind,
        target: FcResourceHandle,
        payload: &[u8],
    ) -> Result<(), PluginCallError> {
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| PluginCallError::PayloadTooLong)?;
        let command = FcCommandV1::new(
            kind,
            FC_COMMAND_FLAGS_NONE,
            target,
            FcBytesView::new(payload.as_ptr(), payload_len),
        );
        let emit = self.host_functions.emit();
        // SAFETY: construction validated the required host callback slot and
        // live handle association. `command` and its borrowed payload remain
        // valid until the callback returns and are never retained by the host.
        let status = unsafe { emit(self.host, self.call, ptr::from_ref(&command)) };
        status_to_result(status)
    }

    /// Sends one call-scoped diagnostic to the host.
    pub fn diagnostic(&mut self, level: u32, message: &str) -> Result<(), PluginCallError> {
        let message_len =
            u64::try_from(message.len()).map_err(|_| PluginCallError::PayloadTooLong)?;
        let diagnostic = self.host_functions.diagnostic();
        // SAFETY: construction validated the required diagnostic slot and live
        // handles. The UTF-8 view borrows `message` only for this foreign call.
        let status = unsafe {
            diagnostic(
                self.host,
                self.call,
                level,
                FcStrView::new(message.as_ptr(), message_len),
            )
        };
        status_to_result(status)
    }

    fn output_snapshot(&self) -> FcOutputBufferV1 {
        // SAFETY: `output` was validated as a live, aligned callback-scoped
        // record and the host may access it only synchronously during calls
        // made through this exclusive `PluginCall`.
        unsafe { self.output.as_ptr().read() }
    }

    fn output_identity_is_unchanged(&self) -> bool {
        let output = self.output_snapshot();
        output.header() == self.output_header
            && output.data() == self.output_data
            && usize::try_from(output.capacity()).ok() == Some(self.output_capacity)
    }
}

/// Safe provider implemented by the later dynamic SDK adapter.
///
/// A provider normally creates two immutable statics:
///
/// - a function table from [`plugin_functions_v1`];
/// - a descriptor from [`plugin_descriptor_v1`].
///
/// [`Self::functions`] and [`Self::descriptor`] return those statics. The
/// adapter owns panic conversion: Packet 55 wraps plugin logic with
/// `catch_unwind` and returns `FC_PLUGIN_PANIC`. This raw bridge deliberately
/// does not catch panics.
pub trait PluginBridge: Send + Sync + 'static {
    /// Per-initialization plugin state hidden behind [`FcPluginHandle`].
    ///
    /// The bridge allocates this value on initialization, lends it mutably to
    /// event callbacks, and consumes it exactly once during shutdown. `Send` is
    /// required because the host may move a non-concurrent instance between
    /// callback threads.
    type Instance: Send + 'static;

    /// Stable plugin id returned by the descriptor getter.
    const ID: &'static str;

    /// Human-readable plugin name returned by the descriptor getter.
    const NAME: &'static str;

    /// Exact target triple for this dynamic artifact.
    const TARGET: &'static str;

    /// Plugin semantic-version core.
    const VERSION: FcSemanticVersion;

    /// Requested host-facade capability mask.
    const REQUESTED_CAPABILITIES: u64;

    /// Returns the same process-lifetime immutable table built by
    /// [`plugin_functions_v1`] on every call.
    ///
    /// This getter is part of the raw bootstrap path and must not panic.
    #[doc(hidden)]
    fn functions() -> &'static FcPluginFunctionsV1;

    /// Returns the same process-lifetime immutable descriptor built by
    /// [`plugin_descriptor_v1`] on every call.
    ///
    /// This getter is the raw bootstrap entrypoint's complete body and must not
    /// panic.
    #[doc(hidden)]
    fn descriptor() -> &'static FcPluginDescriptorV1;

    /// Initializes one safe plugin instance.
    ///
    /// The returned error must be non-success. The bridge normalizes an
    /// accidental `Err(FC_OK)` to `FC_ERROR`.
    fn initialize(
        call: &mut PluginCall<'_>,
        granted_capabilities: u64,
    ) -> Result<Self::Instance, FcStatus>;

    /// Handles one validated call-scoped event.
    fn on_event(
        instance: &mut Self::Instance,
        call: &mut PluginCall<'_>,
        event: PluginEvent<'_>,
    ) -> FcStatus;

    /// Shuts down and consumes one initialized instance.
    fn shutdown(instance: Self::Instance, call: &mut PluginCall<'_>) -> FcStatus;
}

/// Builds the immutable ABI v1 function table for `P`.
#[doc(hidden)]
pub const fn plugin_functions_v1<P: PluginBridge>() -> FcPluginFunctionsV1 {
    FcPluginFunctionsV1::new(plugin_init::<P>, plugin_on_event::<P>, plugin_shutdown::<P>)
}

/// Builds the immutable ABI v1 descriptor for `P`.
///
/// The returned value must be stored in a process-lifetime immutable static
/// returned by [`PluginBridge::descriptor`].
#[doc(hidden)]
pub const fn plugin_descriptor_v1<P: PluginBridge>() -> FcPluginDescriptorV1 {
    FcPluginDescriptorV1::new(
        P::VERSION,
        P::REQUESTED_CAPABILITIES,
        plugin_id::<P>,
        plugin_name::<P>,
        plugin_target::<P>,
        plugin_functions::<P>,
    )
}

/// Exports the exact ABI v1 bootstrap symbol for one safe [`PluginBridge`].
///
/// The macro is defined in the sole unsafe-island crate. Consequently its
/// exported-symbol attribute retains this crate's lint context when expanded,
/// and a downstream SDK or plugin may keep `#![forbid(unsafe_code)]`.
///
/// Invoke this macro exactly once, at the dynamic plugin crate root.
#[allow(unsafe_code)]
#[macro_export]
macro_rules! export_plugin_v1 {
    ($bridge:ty) => {
        #[doc(hidden)]
        #[no_mangle]
        pub extern "C" fn ferrumc_plugin_entry_v1() -> *const $crate::ExportedPluginDescriptorV1 {
            ::core::ptr::from_ref(<$bridge as $crate::PluginBridge>::descriptor())
        }
    };
}

fn status_to_result(status: FcStatus) -> Result<(), PluginCallError> {
    if status == FC_OK {
        Ok(())
    } else {
        Err(PluginCallError::HostStatus(status))
    }
}

fn failure_status(status: FcStatus) -> FcStatus {
    if status == FC_OK {
        FC_ERROR
    } else {
        status
    }
}

fn static_str_view(value: &'static str) -> FcStrView {
    match u64::try_from(value.len()) {
        Ok(len) => FcStrView::new(value.as_ptr(), len),
        Err(_) => FcStrView::empty(),
    }
}

unsafe extern "C" fn plugin_id<P: PluginBridge>() -> FcStrView {
    static_str_view(P::ID)
}

unsafe extern "C" fn plugin_name<P: PluginBridge>() -> FcStrView {
    static_str_view(P::NAME)
}

unsafe extern "C" fn plugin_target<P: PluginBridge>() -> FcStrView {
    static_str_view(P::TARGET)
}

unsafe extern "C" fn plugin_functions<P: PluginBridge>() -> *const FcPluginFunctionsV1 {
    ptr::from_ref(P::functions())
}

unsafe extern "C" fn plugin_init<P: PluginBridge>(
    host: FcHostHandle,
    call: FcCallHandle,
    host_functions: *const FcHostFunctionsV1,
    granted_capabilities: u64,
    output: *mut FcOutputBufferV1,
    plugin_out: *mut FcPluginHandle,
) -> FcStatus {
    let Some(plugin_out) = NonNull::new(plugin_out) else {
        return FC_INVALID_ARGUMENT;
    };
    if !plugin_out.as_ptr().is_aligned() {
        return FC_INVALID_ARGUMENT;
    }

    // SAFETY: the ABI caller promises that a non-null aligned `plugin_out`
    // points to writable storage for one handle for the complete callback.
    unsafe { plugin_out.as_ptr().write(FcPluginHandle::INVALID) };

    // SAFETY: the ABI caller promises that the raw host table and output
    // pointers denote live callback-scoped records. `make_call` validates every
    // representable prefix and required slot before constructing safe access.
    let Some(mut safe_call) = (unsafe { make_call(host, call, host_functions, output) }) else {
        return FC_INVALID_ARGUMENT;
    };

    let instance = match P::initialize(&mut safe_call, granted_capabilities) {
        Ok(instance) => instance,
        Err(status) => return failure_status(status),
    };
    let Some(handle) = instance_into_handle(instance) else {
        return FC_ERROR;
    };

    // SAFETY: `plugin_out` remains the same valid exclusive output location
    // established above, and the newly allocated handle is nonzero.
    unsafe { plugin_out.as_ptr().write(handle) };
    FC_OK
}

unsafe extern "C" fn plugin_on_event<P: PluginBridge>(
    host: FcHostHandle,
    plugin: FcPluginHandle,
    call: FcCallHandle,
    host_functions: *const FcHostFunctionsV1,
    event: *const FcEventV1,
    output: *mut FcOutputBufferV1,
) -> FcStatus {
    let Some(mut instance) = handle_pointer::<P::Instance>(plugin) else {
        return FC_INVALID_ARGUMENT;
    };

    // SAFETY: the ABI caller promises that the raw host table and output
    // pointers denote this live callback's records; `make_call` validates their
    // complete required prefixes before exposing safe operations.
    let Some(mut safe_call) = (unsafe { make_call(host, call, host_functions, output) }) else {
        return FC_INVALID_ARGUMENT;
    };
    // SAFETY: the ABI caller promises that `event` remains readable for this
    // callback. `event_from_raw` validates its prefix, flags, handles, and
    // declared payload extent before constructing a scoped slice.
    let Some(safe_event) = (unsafe { event_from_raw(event) }) else {
        return FC_INVALID_ARGUMENT;
    };

    // SAFETY: only a handle issued by `plugin_init::<P>` is valid for this
    // function. The ABI contract prohibits stale, foreign, or concurrent use,
    // so this callback owns the sole mutable borrow for its duration.
    let instance = unsafe { instance.as_mut() };
    P::on_event(instance, &mut safe_call, safe_event)
}

unsafe extern "C" fn plugin_shutdown<P: PluginBridge>(
    host: FcHostHandle,
    plugin: FcPluginHandle,
    call: FcCallHandle,
    host_functions: *const FcHostFunctionsV1,
    output: *mut FcOutputBufferV1,
) -> FcStatus {
    let Some(instance) = handle_pointer::<P::Instance>(plugin) else {
        return FC_INVALID_ARGUMENT;
    };

    // SAFETY: the ABI caller promises that the raw host table and output
    // pointers denote this live callback's records; `make_call` validates their
    // complete required prefixes before exposing safe operations.
    let Some(mut safe_call) = (unsafe { make_call(host, call, host_functions, output) }) else {
        return FC_INVALID_ARGUMENT;
    };

    // SAFETY: the ABI caller promises this is the one shutdown for a handle
    // issued by `plugin_init::<P>`, after all event callbacks returned. Rebuilding
    // the Box transfers the allocation back exactly once and makes the handle
    // stale before plugin shutdown logic runs.
    let instance = unsafe { Box::from_raw(instance.as_ptr()) };
    P::shutdown(*instance, &mut safe_call)
}

fn instance_into_handle<T>(instance: T) -> Option<FcPluginHandle> {
    let raw = Box::into_raw(Box::new(instance));
    let address = raw as usize;
    let Ok(encoded) = u64::try_from(address) else {
        // SAFETY: `raw` came from `Box::into_raw` immediately above and has not
        // been aliased or reconstructed, so reclaiming it here is exact.
        drop(unsafe { Box::from_raw(raw) });
        return None;
    };
    if encoded == 0 {
        // SAFETY: as above, this is the sole pointer produced by Box and no
        // handle has been published, so exact reclamation is required.
        drop(unsafe { Box::from_raw(raw) });
        return None;
    }
    Some(FcPluginHandle::from_raw(encoded))
}

fn handle_pointer<T>(handle: FcPluginHandle) -> Option<NonNull<T>> {
    if !handle.is_valid() {
        return None;
    }
    let address = usize::try_from(handle.raw()).ok()?;
    NonNull::new(address as *mut T).filter(|pointer| pointer.as_ptr().is_aligned())
}

unsafe fn make_call<'call>(
    host: FcHostHandle,
    call: FcCallHandle,
    host_functions: *const FcHostFunctionsV1,
    output: *mut FcOutputBufferV1,
) -> Option<PluginCall<'call>> {
    if !host.is_valid() || !call.is_valid() {
        return None;
    }

    // SAFETY: this function's caller carries the ABI callback contract that
    // `host_functions` denotes readable call-scoped memory. The validator reads
    // only the header and covered raw slots before constructing a typed table.
    let host_functions = unsafe { validate_host_functions(host_functions) }?;
    // SAFETY: this function's caller carries the ABI callback contract that
    // `output` denotes writable call-scoped memory. The validator checks its
    // header, pointer/length pair, and initial result state before retaining it.
    let output = unsafe { validate_output(output) }?;

    Some(PluginCall {
        host,
        call,
        host_functions,
        output: output.record,
        output_header: output.header,
        output_data: output.data,
        output_capacity: output.capacity,
        _call_scope: PhantomData,
        _not_send_sync: PhantomData,
    })
}

struct ValidatedOutput {
    record: NonNull<FcOutputBufferV1>,
    header: FcAbiHeader,
    data: *mut u8,
    capacity: usize,
}

unsafe fn validate_output(output: *mut FcOutputBufferV1) -> Option<ValidatedOutput> {
    let record = NonNull::new(output)?;
    if !record.as_ptr().is_aligned() {
        return None;
    }

    // SAFETY: the ABI caller promises `output` points to readable storage for
    // at least its leading size word. The helper proves that word covers the
    // common header before reading version fields.
    let header = unsafe { read_compatible_header(record.as_ptr(), FcOutputBufferV1::STRUCT_SIZE) }?;

    // SAFETY: the validated header covers the full ABI v1 output prefix, whose
    // remaining fields are fixed-width integers and raw pointers with no
    // invalid bit patterns.
    let snapshot = unsafe { record.as_ptr().read() };
    let capacity = usize::try_from(snapshot.capacity()).ok()?;
    let max_slice_len = usize::try_from(isize::MAX).ok()?;
    if capacity > max_slice_len || snapshot.result_len() != 0 {
        return None;
    }
    if !valid_byte_extent(snapshot.data().cast_const(), capacity) {
        return None;
    }

    Some(ValidatedOutput {
        record,
        header,
        data: snapshot.data(),
        capacity,
    })
}

unsafe fn validate_host_functions(
    functions: *const FcHostFunctionsV1,
) -> Option<FcHostFunctionsV1> {
    if functions.is_null() || !functions.is_aligned() {
        return None;
    }

    // SAFETY: the ABI caller promises `functions` points to readable storage
    // for at least its leading size word. The helper proves that word covers
    // the common header before reading version fields.
    let _header = unsafe { read_compatible_header(functions, FcHostFunctionsV1::STRUCT_SIZE) }?;
    if size_of::<usize>() != size_of::<ferrumc_plugin_abi::FcHostCallFn>()
        || size_of::<usize>() != size_of::<ferrumc_plugin_abi::FcHostEmitFn>()
        || size_of::<usize>() != size_of::<ferrumc_plugin_abi::FcHostDiagnosticFn>()
    {
        return None;
    }

    for offset in [
        FcHostFunctionsV1::CALL_OFFSET,
        FcHostFunctionsV1::EMIT_OFFSET,
        FcHostFunctionsV1::DIAGNOSTIC_OFFSET,
    ] {
        let slot_end = offset.checked_add(size_of::<usize>())?;
        if offset % align_of::<usize>() != 0 || slot_end > FcHostFunctionsV1::STRUCT_SIZE as usize {
            return None;
        }

        // SAFETY: the validated record covers every byte in this pointer-sized
        // required slot, and the published offsets preserve pointer alignment.
        let word = unsafe {
            functions
                .cast::<usize>()
                .add(offset / size_of::<usize>())
                .read()
        };
        if word == 0 {
            return None;
        }
    }

    // SAFETY: the header covers the complete current prefix and every required
    // function-pointer word is nonzero. Callable-address validity and no-unwind
    // behavior remain explicit obligations of the trusted ABI peer.
    Some(unsafe { functions.read() })
}

unsafe fn event_from_raw<'call>(event: *const FcEventV1) -> Option<PluginEvent<'call>> {
    if event.is_null() || !event.is_aligned() {
        return None;
    }

    // SAFETY: the ABI caller promises `event` points to readable storage for at
    // least its leading size word. The helper proves that word covers the
    // common header before reading version fields.
    let _header = unsafe { read_compatible_header(event, FcEventV1::STRUCT_SIZE) }?;

    // SAFETY: the validated header covers the complete event prefix. Its
    // remaining fields are fixed-width scalars, opaque integers, and a raw view
    // whose bit patterns are inert until validated below.
    let event = unsafe { event.read() };
    if event.flags() != FC_EVENT_FLAGS_NONE || (!event.shard().is_valid() && event.tick() != 0) {
        return None;
    }

    let view = event.payload();
    let len = usize::try_from(view.len()).ok()?;
    let max_slice_len = usize::try_from(isize::MAX).ok()?;
    if len > max_slice_len {
        return None;
    }
    let payload = if len == 0 {
        &[]
    } else {
        if !valid_byte_extent(view.data(), len) {
            return None;
        }
        // SAFETY: the ABI caller promises the non-null declared payload extent
        // is readable for this callback. Length representability and the slice
        // size bound were validated above.
        unsafe { core::slice::from_raw_parts(view.data(), len) }
    };

    Some(PluginEvent {
        kind: event.kind(),
        tick: event.tick(),
        shard: event.shard(),
        payload,
    })
}

fn header_is_compatible(header: FcAbiHeader, required_size: u32) -> bool {
    producer_header_is_compatible(header, required_size, ABI_MAJOR, ABI_MINOR)
}

fn producer_header_is_compatible(
    header: FcAbiHeader,
    required_size: u32,
    consumer_major: u16,
    consumer_minor: u16,
) -> bool {
    header.covers(required_size)
        && header.abi_major() == consumer_major
        && header.abi_minor() >= consumer_minor
}

unsafe fn read_compatible_header<T>(record: *const T, required_size: u32) -> Option<FcAbiHeader> {
    // SAFETY: the caller promises `record` is readable for the leading u32 and
    // has already checked the record's alignment.
    let declared_size = unsafe { record.cast::<u32>().read() };
    if declared_size < FcAbiHeader::BYTE_SIZE {
        return None;
    }

    // SAFETY: the just-read size declares coverage of the complete common
    // header, so reading its fixed-width scalar fields does not cross the
    // producer-declared prefix.
    let header = unsafe { record.cast::<FcAbiHeader>().read() };
    if header.struct_size() != declared_size || !header_is_compatible(header, required_size) {
        return None;
    }
    Some(header)
}

fn valid_byte_extent(data: *const u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if data.is_null() {
        return false;
    }

    let start = data as usize;
    start.checked_add(len).is_some()
}

#[cfg(test)]
mod tests {
    use core::ptr;

    use ferrumc_plugin_abi::{
        FcAbiHeader, FcBytesView, FcCallHandle, FcCommandKind, FcCommandV1, FcEventKind, FcEventV1,
        FcHostFunctionsV1, FcHostHandle, FcHostRequestKind, FcHostRequestV1, FcOutputBufferV1,
        FcPluginDescriptorV1, FcPluginFunctionsV1, FcPluginHandle, FcResourceHandle,
        FcSemanticVersion, FcStatus, FcStrView, ABI_MAJOR, ABI_MINOR, FC_DIAGNOSTIC_INFO, FC_ERROR,
        FC_EVENT_FLAGS_NONE, FC_INVALID_ARGUMENT, FC_OK,
    };

    use super::{
        event_from_raw, plugin_descriptor_v1, plugin_functions_v1, plugin_init, plugin_on_event,
        plugin_shutdown, producer_header_is_compatible, valid_byte_extent, PluginBridge,
        PluginCall, PluginEvent,
    };

    struct Bridge;

    struct Instance {
        granted_capabilities: u64,
        events: u64,
    }

    static FUNCTIONS: FcPluginFunctionsV1 = plugin_functions_v1::<Bridge>();
    static DESCRIPTOR: FcPluginDescriptorV1 = plugin_descriptor_v1::<Bridge>();

    impl PluginBridge for Bridge {
        type Instance = Instance;

        const ID: &'static str = "bridge.test";
        const NAME: &'static str = "Bridge Test";
        const TARGET: &'static str = "aarch64-unknown-linux-gnu";
        const VERSION: FcSemanticVersion = FcSemanticVersion::new(1, 2, 3);
        const REQUESTED_CAPABILITIES: u64 = 5;

        fn functions() -> &'static FcPluginFunctionsV1 {
            &FUNCTIONS
        }

        fn descriptor() -> &'static FcPluginDescriptorV1 {
            &DESCRIPTOR
        }

        fn initialize(
            call: &mut PluginCall<'_>,
            granted_capabilities: u64,
        ) -> Result<Self::Instance, FcStatus> {
            if call
                .diagnostic(FC_DIAGNOSTIC_INFO, "bridge initialized")
                .is_err()
            {
                return Err(FC_ERROR);
            }
            Ok(Instance {
                granted_capabilities,
                events: 0,
            })
        }

        fn on_event(
            instance: &mut Self::Instance,
            call: &mut PluginCall<'_>,
            event: PluginEvent<'_>,
        ) -> FcStatus {
            if event.kind() != FcEventKind::BLOCK_BREAK
                || event.tick() != 46
                || event.shard() != FcResourceHandle::from_raw(7)
                || event.payload() != [1, 2, 3]
                || call
                    .request(FcHostRequestKind::DIMENSION, FcResourceHandle::INVALID, &[])
                    .as_deref()
                    != Ok([9, 8].as_slice())
                || call
                    .emit(
                        FcCommandKind::MESSAGE,
                        FcResourceHandle::INVALID,
                        b"message",
                    )
                    .is_err()
                || call.diagnostic(FC_DIAGNOSTIC_INFO, "bridge event").is_err()
            {
                return FC_ERROR;
            }
            instance.events += 1;
            FC_OK
        }

        fn shutdown(instance: Self::Instance, call: &mut PluginCall<'_>) -> FcStatus {
            if instance.granted_capabilities == 5
                && instance.events == 1
                && call
                    .diagnostic(FC_DIAGNOSTIC_INFO, "bridge stopped")
                    .is_ok()
            {
                FC_OK
            } else {
                FC_ERROR
            }
        }
    }

    unsafe extern "C" fn host_call(
        _host: FcHostHandle,
        _call: FcCallHandle,
        _request: *const FcHostRequestV1,
        output: *mut FcOutputBufferV1,
    ) -> FcStatus {
        if output.is_null() || !output.is_aligned() {
            return FC_INVALID_ARGUMENT;
        }

        // SAFETY: this test host receives the live aligned output record built
        // by the trampoline test and accesses it only during this callback.
        let output = unsafe { &mut *output };
        if output.capacity() < 2 || output.data().is_null() {
            return FC_ERROR;
        }
        let response = [9, 8];
        // SAFETY: the checked capacity covers both response bytes, source and
        // destination do not overlap, and the destination remains writable for
        // this synchronous callback.
        unsafe { ptr::copy_nonoverlapping(response.as_ptr(), output.data(), response.len()) };
        output.set_result_len(2);
        FC_OK
    }

    unsafe extern "C" fn host_emit(
        _host: FcHostHandle,
        _call: FcCallHandle,
        _command: *const FcCommandV1,
    ) -> FcStatus {
        FC_OK
    }

    unsafe extern "C" fn host_diagnostic(
        _host: FcHostHandle,
        _call: FcCallHandle,
        _level: u32,
        _message: FcStrView,
    ) -> FcStatus {
        FC_OK
    }

    static HOST_FUNCTIONS: FcHostFunctionsV1 =
        FcHostFunctionsV1::new(host_call, host_emit, host_diagnostic);

    fn make_output(bytes: &mut [u8]) -> FcOutputBufferV1 {
        let capacity = u64::try_from(bytes.len()).expect("test output length fits ABI v1");
        FcOutputBufferV1::new(bytes.as_mut_ptr(), capacity)
    }

    #[test]
    fn safe_bridge_builders_publish_current_records() {
        assert_eq!(FUNCTIONS.header().abi_major(), ABI_MAJOR);
        assert_eq!(FUNCTIONS.header().abi_minor(), ABI_MINOR);
        assert_eq!(
            FUNCTIONS.header().struct_size(),
            FcPluginFunctionsV1::STRUCT_SIZE
        );
        assert_eq!(DESCRIPTOR.header().abi_major(), ABI_MAJOR);
        assert_eq!(DESCRIPTOR.header().abi_minor(), ABI_MINOR);
        assert_eq!(
            DESCRIPTOR.header().struct_size(),
            FcPluginDescriptorV1::STRUCT_SIZE
        );
        assert_eq!(DESCRIPTOR.version(), Bridge::VERSION);
        assert_eq!(
            DESCRIPTOR.requested_capabilities(),
            Bridge::REQUESTED_CAPABILITIES
        );
    }

    #[test]
    fn plugin_consumer_accepts_additive_newer_host_minor_only() {
        let required = FcHostFunctionsV1::STRUCT_SIZE;
        assert!(producer_header_is_compatible(
            FcAbiHeader::new(required, 1, 1),
            required,
            1,
            0,
        ));
        assert!(producer_header_is_compatible(
            FcAbiHeader::new(required + 8, 1, 3),
            required,
            1,
            2,
        ));
        assert!(!producer_header_is_compatible(
            FcAbiHeader::new(required, 1, 1),
            required,
            1,
            2,
        ));
        assert!(!producer_header_is_compatible(
            FcAbiHeader::new(required, 2, 3),
            required,
            1,
            0,
        ));
    }

    #[test]
    fn event_decoder_preserves_the_unavailable_shard_sentinel() {
        let event = FcEventV1::new(
            FcEventKind::BLOCK_PLACE_ATTEMPT,
            FC_EVENT_FLAGS_NONE,
            0,
            FcResourceHandle::INVALID,
            FcBytesView::empty(),
        );

        // SAFETY: `event` is an aligned, complete ABI v1 record that remains
        // live for the returned call-scoped view; its empty payload is inert.
        let decoded = unsafe { event_from_raw(ptr::from_ref(&event)) }
            .expect("connection-side event sentinel is a valid envelope");
        assert_eq!(decoded.tick(), 0);
        assert_eq!(decoded.shard(), FcResourceHandle::INVALID);
        assert!(decoded.payload().is_empty());

        let mismatched = FcEventV1::new(
            FcEventKind::BLOCK_PLACE_ATTEMPT,
            FC_EVENT_FLAGS_NONE,
            1,
            FcResourceHandle::INVALID,
            FcBytesView::empty(),
        );
        // SAFETY: `mismatched` is a complete, live record; the deliberately
        // inconsistent sentinel pair must be rejected before a view is exposed.
        assert!(unsafe { event_from_raw(ptr::from_ref(&mismatched)) }.is_none());
    }

    #[test]
    fn generic_trampolines_keep_state_behind_the_opaque_handle() {
        let host = FcHostHandle::from_raw(1);
        let call = FcCallHandle::from_raw(2);
        let mut output_bytes = [0; 16];
        let mut output = make_output(&mut output_bytes);
        let mut plugin = FcPluginHandle::INVALID;

        // SAFETY: all records and output locations are aligned, live for this
        // synchronous call, and use the exact ABI v1 callback signatures.
        let status = unsafe {
            plugin_init::<Bridge>(
                host,
                call,
                ptr::from_ref(&HOST_FUNCTIONS),
                5,
                ptr::from_mut(&mut output),
                ptr::from_mut(&mut plugin),
            )
        };
        assert_eq!(status, FC_OK);
        assert!(plugin.is_valid());

        let payload = [1, 2, 3];
        let event = FcEventV1::new(
            FcEventKind::BLOCK_BREAK,
            FC_EVENT_FLAGS_NONE,
            46,
            FcResourceHandle::from_raw(7),
            ferrumc_plugin_abi::FcBytesView::new(
                payload.as_ptr(),
                u64::try_from(payload.len()).expect("test payload fits ABI v1"),
            ),
        );
        let mut event_output_bytes = [0; 16];
        let mut event_output = make_output(&mut event_output_bytes);
        // SAFETY: `plugin` was issued by the matching generic initialization
        // trampoline and all call-scoped records remain live and non-aliased.
        let status = unsafe {
            plugin_on_event::<Bridge>(
                host,
                plugin,
                call,
                ptr::from_ref(&HOST_FUNCTIONS),
                ptr::from_ref(&event),
                ptr::from_mut(&mut event_output),
            )
        };
        assert_eq!(status, FC_OK);

        let mut shutdown_output_bytes = [0; 16];
        let mut shutdown_output = make_output(&mut shutdown_output_bytes);
        // SAFETY: this is the sole shutdown for the live handle after the event
        // callback returned; every raw argument remains valid for the call.
        let status = unsafe {
            plugin_shutdown::<Bridge>(
                host,
                plugin,
                call,
                ptr::from_ref(&HOST_FUNCTIONS),
                ptr::from_mut(&mut shutdown_output),
            )
        };
        assert_eq!(status, FC_OK);
    }

    #[repr(C, align(8))]
    struct RawHostFunctions {
        bytes: [u8; FcHostFunctionsV1::STRUCT_SIZE as usize],
    }

    #[test]
    fn plugin_init_rejects_null_host_slots_without_constructing_a_table() {
        let mut raw = RawHostFunctions {
            bytes: [0; FcHostFunctionsV1::STRUCT_SIZE as usize],
        };
        raw.bytes[0..4].copy_from_slice(&FcHostFunctionsV1::STRUCT_SIZE.to_ne_bytes());
        raw.bytes[4..6].copy_from_slice(&ABI_MAJOR.to_ne_bytes());
        raw.bytes[6..8].copy_from_slice(&ABI_MINOR.to_ne_bytes());

        let mut output_bytes = [0; 8];
        let mut output = make_output(&mut output_bytes);
        let mut plugin = FcPluginHandle::from_raw(99);
        // SAFETY: the aligned synthetic table is readable for its declared
        // prefix. Its zero raw slots are intentionally malformed test input.
        let status = unsafe {
            plugin_init::<Bridge>(
                FcHostHandle::from_raw(1),
                FcCallHandle::from_raw(2),
                raw.bytes.as_ptr().cast(),
                5,
                ptr::from_mut(&mut output),
                ptr::from_mut(&mut plugin),
            )
        };
        assert_eq!(status, FC_INVALID_ARGUMENT);
        assert_eq!(plugin, FcPluginHandle::INVALID);
    }

    #[test]
    fn byte_extent_rejects_address_wrap_before_slice_construction() {
        let wrapping = usize::MAX as *const u8;
        assert!(!valid_byte_extent(wrapping, 2));
        assert!(valid_byte_extent(ptr::null(), 0));
    }
}
