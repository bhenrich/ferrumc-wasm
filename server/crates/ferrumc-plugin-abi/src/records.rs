//! C-layout ABI v1 declarations.
//!
//! Fields remain crate-private so Rust callers use constructors and accessors
//! instead of coupling ordinary code to the representation. Their order,
//! offsets, and widths are nevertheless part of the C ABI and are pinned by
//! the crate's layout tests.
//!
//! # Kind payload encoding
//!
//! Kind-specific payloads use a deterministic binary grammar. Multibyte
//! integers and IEEE-754 `f64` bit patterns are little-endian. A player id is
//! its 16 UUID bytes in network order. A block position is three consecutive
//! `i32` values (`x`, `y`, `z`). A vector is three consecutive `f64` values. A
//! text or byte field is a `u32` byte length followed by exactly that many
//! bytes; text must be UTF-8. Counts and lengths are validated against host
//! limits before allocation. No payload uses JSON.

use core::ffi::CStr;
use core::ptr;

use crate::{FcStatus, ABI_MAJOR, ABI_MINOR};

/// The nul-terminated bootstrap symbol exported by every ABI v1 plugin.
pub const ENTRYPOINT_V1: &CStr = c"ferrumc_plugin_entry_v1";

/// The common prefix of every extensible ABI v1 record.
///
/// Consumers must validate this header before reading any following field.
/// They may read only the prefix covered by both `struct_size` and the version
/// they implement.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FcAbiHeader {
    pub(crate) struct_size: u32,
    pub(crate) abi_major: u16,
    pub(crate) abi_minor: u16,
}

impl FcAbiHeader {
    /// The exact byte size of this header.
    pub const BYTE_SIZE: u32 = 8;

    /// Creates a header carrying an explicitly declared size and version.
    pub const fn new(struct_size: u32, abi_major: u16, abi_minor: u16) -> Self {
        Self {
            struct_size,
            abi_major,
            abi_minor,
        }
    }

    /// Creates a header for the current ABI version.
    pub const fn current(struct_size: u32) -> Self {
        Self::new(struct_size, ABI_MAJOR, ABI_MINOR)
    }

    /// Returns the producer-declared record size.
    pub const fn struct_size(self) -> u32 {
        self.struct_size
    }

    /// Returns the producer-declared ABI major.
    pub const fn abi_major(self) -> u16 {
        self.abi_major
    }

    /// Returns the producer-declared ABI minor.
    pub const fn abi_minor(self) -> u16 {
        self.abi_minor
    }

    /// Returns whether this header covers `required_size` bytes.
    pub const fn covers(self, required_size: u32) -> bool {
        self.struct_size >= required_size
    }
}

macro_rules! opaque_handle {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            /// The invalid handle value.
            pub const INVALID: Self = Self(0);

            /// Creates a handle from its uninterpreted ABI value.
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            /// Returns the uninterpreted ABI value.
            pub const fn raw(self) -> u64 {
                self.0
            }

            /// Returns whether this is a nonzero handle.
            pub const fn is_valid(self) -> bool {
                self.0 != 0
            }
        }
    };
}

opaque_handle!(
    /// Opaque host-instance token supplied to plugin callbacks.
    ///
    /// The host binds this token to one plugin instance. It is valid only in
    /// that instance's callbacks and cannot be substituted across instances.
    FcHostHandle
);
opaque_handle!(
    /// Opaque plugin-instance token returned by initialization.
    ///
    /// A successful initialization creates the association. The token remains
    /// valid through shutdown and is invalid afterward.
    FcPluginHandle
);
opaque_handle!(
    /// Opaque token identifying one callback and its bounded command buffer.
    ///
    /// The host binds the token to one plugin, hook, and invocation. It expires
    /// exactly when that callback returns; stale or foreign use is rejected.
    FcCallHandle
);
opaque_handle!(
    /// Opaque token for a host-owned world, player, entity, or shard resource.
    ///
    /// Each operation defines the permitted resource kind and lifetime. The
    /// host validates kind, plugin ownership, generation, and call association
    /// before use; a numeric collision does not make kinds interchangeable.
    FcResourceHandle
);

/// An event kind carried by [`FcEventV1`].
///
/// This is an integer wrapper rather than an enum so newer event kinds remain
/// representable to older hosts.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FcEventKind(u32);

impl FcEventKind {
    /// A player finished joining.
    ///
    /// Payload: one player id.
    pub const PLAYER_JOIN: Self = Self(1);

    /// A player left.
    ///
    /// Payload: one player id.
    pub const PLAYER_LEAVE: Self = Self(2);

    /// A block-break notification.
    ///
    /// Its payload is one [`FcBlockBreakEventPayloadV1`] record.
    pub const BLOCK_BREAK: Self = Self(3);

    /// A block placement accepted at the intent boundary and routed.
    ///
    /// The simulation may still reject the edit, so this is not a
    /// tick-confirmed mutation event.
    ///
    /// Payload: player id, block position, then a `u32` block-state id.
    pub const AFTER_BLOCK_PLACE: Self = Self(4);

    /// A block break accepted at the intent boundary and routed.
    ///
    /// The simulation may still reject the edit, so this is not a
    /// tick-confirmed mutation event.
    ///
    /// Its payload is one [`FcBlockBreakEventPayloadV1`] record.
    pub const AFTER_BLOCK_BREAK: Self = Self(5);

    /// A player crossed a block boundary.
    ///
    /// Payload: player id, previous block position, then new block position.
    pub const PLAYER_MOVE: Self = Self(6);

    /// A vetoable block-placement attempt.
    ///
    /// Payload: player id, block position, then a `u32` block-state id.
    pub const BLOCK_PLACE_ATTEMPT: Self = Self(7);

    /// A vetoable block-break attempt.
    ///
    /// Its payload is one [`FcBlockBreakEventPayloadV1`] record.
    pub const BLOCK_BREAK_ATTEMPT: Self = Self(8);

    /// A vetoable chat attempt.
    ///
    /// Payload: player id followed by one text field.
    pub const CHAT_ATTEMPT: Self = Self(9);

    /// A vetoable interaction attempt.
    ///
    /// Payload starts with player id, hand `u8` (`0` main, `1` off), target kind
    /// `u8` (`0` air, `1` block, `2` entity), and a zero `u16`. Air has no tail;
    /// block has a block position plus face `u8` and three zero bytes; entity has
    /// one `i32` protocol entity id.
    pub const INTERACT_ATTEMPT: Self = Self(10);

    /// A registered plugin command was invoked.
    ///
    /// Payload: handler id `u64`, player id, argument count `u32`, then each
    /// argument as a text name, kind `u8` (`0` text, `1` integer), three zero
    /// bytes, and either one text field or one `i64`.
    pub const COMMAND: Self = Self(11);

    /// A plugin timer became due.
    ///
    /// Payload: timer id `u64`.
    pub const TIMER: Self = Self(12);

    /// Creates an event kind from its exact ABI value.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the exact ABI value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A command kind carried by [`FcCommandV1`].
///
/// Unknown values remain representable so validation can classify them.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FcCommandKind(u32);

impl FcCommandKind {
    /// Requests one exact block-state mutation.
    ///
    /// Its payload is one [`FcSetBlockCommandPayloadV1`] record. Emission
    /// requires [`crate::FC_CAPABILITY_SUBMIT_INTENTS`].
    pub const SET_BLOCK: Self = Self(1);

    /// Requests a player teleport.
    ///
    /// Payload: player id followed by one vector.
    pub const TELEPORT: Self = Self(2);

    /// Requests a player message.
    ///
    /// Payload: player id followed by one UTF-8 text field.
    pub const MESSAGE: Self = Self(3);

    /// Subscribes the plugin to one event kind during initialization.
    ///
    /// Payload: event kind `u32`.
    pub const SUBSCRIBE_EVENT: Self = Self(4);

    /// Registers one bounded command tree during initialization.
    ///
    /// Payload starts with node count `u32`, followed by nodes in preorder. Each
    /// node contains parent index `u32` (`u32::MAX` for the root), node kind
    /// `u8` (`0` literal, `1` word, `2` greedy text, `3` bounded integer),
    /// executable `u8`, required level `u8` (`0xff` for none), zero `u8`,
    /// integer min/max `i64` (zero for non-integer nodes), one text name, one
    /// permission-node text field (empty for none), and handler id `u64` (zero
    /// for non-executable nodes). Parent indices must precede children.
    pub const REGISTER_COMMAND: Self = Self(5);

    /// Stores one value in the host-selected plugin namespace.
    ///
    /// Payload: one UTF-8 key field followed by one byte field.
    pub const STORAGE_PUT: Self = Self(6);

    /// Deletes one key from the host-selected plugin namespace.
    ///
    /// Payload: one UTF-8 key field.
    pub const STORAGE_DELETE: Self = Self(7);

    /// Schedules or replaces one deterministic plugin timer.
    ///
    /// Payload: timer id `u64` followed by delay ticks `u64`.
    pub const SCHEDULE_TIMER: Self = Self(8);

    /// Cancels one deterministic plugin timer.
    ///
    /// Payload: timer id `u64`.
    pub const CANCEL_TIMER: Self = Self(9);

    /// Records an allow decision for the current vetoable event.
    ///
    /// Payload is empty.
    pub const DECISION_ALLOW: Self = Self(10);

    /// Records a deny decision for the current vetoable event.
    ///
    /// Payload is zero or one UTF-8 text field for player feedback.
    pub const DECISION_DENY: Self = Self(11);

    /// Replaces the block-state id for the current placement attempt.
    ///
    /// Payload: one `u32` block-state id.
    pub const DECISION_REPLACE_BLOCK: Self = Self(12);

    /// Creates a command kind from its exact ABI value.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the exact ABI value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A read-only host-request kind carried by [`FcHostRequestV1`].
///
/// Unknown values remain representable so validation can classify them.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FcHostRequestKind(u32);

impl FcHostRequestKind {
    /// Queries the current dimension resource.
    ///
    /// Payload is empty. A successful response is one `u64`
    /// [`FcResourceHandle`] value for the current dimension.
    pub const DIMENSION: Self = Self(1);

    /// Queries whether one chunk is loaded.
    ///
    /// Payload: chunk x/z as two `i32` values. A successful response is one
    /// byte (`0` or `1`).
    pub const CHUNK_LOADED: Self = Self(2);

    /// Queries the block-state id at one position.
    ///
    /// Its payload is one [`FcBlockStateRequestPayloadV1`] record. A successful
    /// response is exactly four little-endian bytes containing a `u32` block
    /// state id.
    pub const BLOCK_STATE: Self = Self(3);

    /// Queries a player's current position.
    ///
    /// Payload: player id. A successful response is a presence byte followed,
    /// when present, by one vector.
    pub const PLAYER_POSITION: Self = Self(4);

    /// Resolves one permission node for a player.
    ///
    /// Payload: player id followed by one permission-node text field. A
    /// successful response is `u8`: `0` unset, `1` allowed, or `2` denied.
    pub const PERMISSION_RESOLVE: Self = Self(5);

    /// Reads one key from the host-selected plugin namespace.
    ///
    /// Payload: one UTF-8 key field. A successful response is a presence byte
    /// followed, when present, by one byte field.
    pub const STORAGE_GET: Self = Self(6);

    /// Lists keys in the host-selected plugin namespace.
    ///
    /// Payload is empty. A successful response is key count `u32` followed by
    /// that many UTF-8 text fields in deterministic byte order.
    pub const STORAGE_KEYS: Self = Self(7);

    /// Creates a host-request kind from its exact ABI value.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the exact ABI value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A fixed-width semantic version carried in a plugin descriptor.
///
/// The reserved word is always zero and remains reserved in ABI v1.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FcSemanticVersion {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) patch: u32,
    pub(crate) reserved: u32,
}

impl FcSemanticVersion {
    /// Creates a semantic version.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            reserved: 0,
        }
    }

    /// Returns the major component.
    pub const fn major(self) -> u32 {
        self.major
    }

    /// Returns the minor component.
    pub const fn minor(self) -> u32 {
        self.minor
    }

    /// Returns the patch component.
    pub const fn patch(self) -> u32 {
        self.patch
    }

    /// Returns the reserved word, which must be zero.
    pub const fn reserved(self) -> u32 {
        self.reserved
    }
}

/// A Java Edition player UUID in network byte order.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FcPlayerIdV1 {
    pub(crate) bytes: [u8; 16],
}

impl FcPlayerIdV1 {
    /// Creates an id from its 16 UUID bytes in network byte order.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self { bytes }
    }

    /// Returns the 16 UUID bytes in network byte order.
    pub const fn bytes(self) -> [u8; 16] {
        self.bytes
    }
}

/// A fixed-width block position used by ABI v1 payload records.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FcBlockPosV1 {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) z: i32,
}

impl FcBlockPosV1 {
    /// Creates a block position.
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// Returns the x coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the y coordinate.
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Returns the z coordinate.
    pub const fn z(self) -> i32 {
        self.z
    }
}

/// A call-scoped view of bytes owned by the producing side.
///
/// A receiver must validate that a nonzero length has a non-null pointer and
/// that the declared extent fits `usize`/`isize` and its configured limit before
/// reading. It must copy bytes it needs after the call returns.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcBytesView {
    pub(crate) data: *const u8,
    pub(crate) len: u64,
}

impl FcBytesView {
    /// Creates an inert view declaration from a pointer and length.
    ///
    /// This does not validate or dereference `data`.
    pub const fn new(data: *const u8, len: u64) -> Self {
        Self { data, len }
    }

    /// Creates an empty view.
    pub const fn empty() -> Self {
        Self::new(ptr::null(), 0)
    }

    /// Returns the declared data pointer without dereferencing it.
    pub const fn data(self) -> *const u8 {
        self.data
    }

    /// Returns the declared byte length.
    pub const fn len(self) -> u64 {
        self.len
    }

    /// Returns whether the declared length is zero.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// A call-scoped view of UTF-8 bytes owned by the producing side.
///
/// The receiving unsafe boundary validates the pointer, extent, configured
/// length limit, `usize`/`isize` representability, and UTF-8 encoding before
/// exposing owned text to safe code.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcStrView {
    pub(crate) data: *const u8,
    pub(crate) len: u64,
}

impl FcStrView {
    /// Creates an inert UTF-8 view declaration from a pointer and length.
    ///
    /// This does not validate or dereference `data`.
    pub const fn new(data: *const u8, len: u64) -> Self {
        Self { data, len }
    }

    /// Creates an empty string view.
    pub const fn empty() -> Self {
        Self::new(ptr::null(), 0)
    }

    /// Returns the declared data pointer without dereferencing it.
    pub const fn data(self) -> *const u8 {
        self.data
    }

    /// Returns the declared byte length.
    pub const fn len(self) -> u64 {
        self.len
    }

    /// Returns whether the declared byte length is zero.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Returns a call-scoped UTF-8 view.
///
/// # Safety
///
/// The function pointer must come from a raw-validated, live plugin table and
/// must not unwind. Its result is inert until the unsafe boundary validates its
/// pointer, declared extent, configured bound, and UTF-8 encoding. The result
/// must be copied before this function returns again or the library unloads.
pub type FcStrFn = unsafe extern "C" fn() -> FcStrView;

/// Returns the plugin's versioned function table.
///
/// # Safety
///
/// The getter must come from a raw-validated descriptor in a resident library
/// and must not unwind. A non-null result must remain aligned, immutable, and
/// live while the library is resident. The unsafe boundary validates the table
/// header and every required raw slot before constructing a typed table value.
pub type FcPluginFunctionsFn = unsafe extern "C" fn() -> *const FcPluginFunctionsV1;

/// Initializes one plugin instance.
///
/// The host initializes `plugin_out` to [`FcPluginHandle::INVALID`]. Success
/// requires the callback to write a nonzero handle; every failure leaves it
/// invalid and causes the host to discard commands emitted during this call.
///
/// # Safety
///
/// The host must supply the plugin's valid `host` token, a fresh `call` token,
/// a raw-validated host table, the exact host-owned `output` buffer associated
/// with that call, and a non-null aligned `plugin_out`. All pointer arguments
/// are valid only until this callback returns and must not be retained. The
/// callback must not unwind. The host invokes initialization at most once and
/// never concurrently with another callback for this plugin instance.
pub type FcPluginInitFn = unsafe extern "C" fn(
    host: FcHostHandle,
    call: FcCallHandle,
    host_functions: *const FcHostFunctionsV1,
    granted_capabilities: u64,
    output: *mut FcOutputBufferV1,
    plugin_out: *mut FcPluginHandle,
) -> FcStatus;

/// Handles one versioned event.
///
/// Commands are submitted through the host table using `call`, which identifies
/// the host-owned bounded command buffer for this callback.
///
/// # Safety
///
/// The host must supply handles owned by this plugin instance, a raw-validated
/// host table, an aligned event whose validated prefix covers every field the
/// plugin reads, and the exact host-owned query buffer associated with `call`.
/// All pointers and the call token expire when this function returns and must
/// not be retained. The callback must not unwind. A host does not invoke two
/// callbacks concurrently for the same plugin handle; different plugin
/// instances may run concurrently. Any return other than [`crate::FC_OK`]
/// causes the host to discard every command buffered by this callback.
pub type FcPluginOnEventFn = unsafe extern "C" fn(
    host: FcHostHandle,
    plugin: FcPluginHandle,
    call: FcCallHandle,
    host_functions: *const FcHostFunctionsV1,
    event: *const FcEventV1,
    output: *mut FcOutputBufferV1,
) -> FcStatus;

/// Shuts down one initialized plugin instance.
///
/// # Safety
///
/// The host must supply the same valid host/plugin association established by a
/// successful initialization, a fresh call token, a raw-validated host table,
/// and that call's host-owned query buffer. All pointer arguments and `call`
/// expire on return. The callback must not unwind. Shutdown runs at most once,
/// after all event callbacks have returned, and is not concurrent with another
/// callback for this plugin handle. A failure status discards commands buffered
/// during shutdown.
pub type FcPluginShutdownFn = unsafe extern "C" fn(
    host: FcHostHandle,
    plugin: FcPluginHandle,
    call: FcCallHandle,
    host_functions: *const FcHostFunctionsV1,
    output: *mut FcOutputBufferV1,
) -> FcStatus;

/// Performs a capability-scoped host operation and writes any response into the
/// host-owned output buffer supplied to the current plugin callback.
///
/// The plugin may pass only that call's `output` pointer. Neither side transfers
/// allocation ownership across this call.
///
/// ABI v1 host calls are read-only queries. They commit no command or other
/// side effect. The host sizes the call buffer to its configured maximum before
/// entering the plugin; [`crate::FC_BUFFER_TOO_SMALL`] is a terminal bounded
/// failure for that query, not an instruction to retry the plugin callback.
///
/// # Safety
///
/// `host` and `call` must be the live associated tokens supplied to the current
/// plugin callback. `request` must be non-null, aligned, size/version validated,
/// and fully contained within the callback lifetime. `output` must be the exact
/// buffer pointer supplied for that call; the host checks its identity and uses
/// the original host-owned pointer/capacity rather than trusting plugin-mutated
/// fields. This callback must not unwind.
pub type FcHostCallFn = unsafe extern "C" fn(
    host: FcHostHandle,
    call: FcCallHandle,
    request: *const FcHostRequestV1,
    output: *mut FcOutputBufferV1,
) -> FcStatus;

/// Submits one command to the callback's host-owned bounded command buffer.
///
/// When the bound is full the host rejects this newest command with
/// [`crate::FC_COMMAND_BUFFER_FULL`]; it never creates an unbounded spill queue.
///
/// # Safety
///
/// `host` and `call` must be the live associated tokens supplied to the current
/// plugin callback. `command` must be non-null, aligned, size/version validated,
/// and call-scoped. The host validates the command kind, target-handle kind and
/// ownership, payload extent, and granted capability before copying it. This
/// callback must not unwind.
pub type FcHostEmitFn = unsafe extern "C" fn(
    host: FcHostHandle,
    call: FcCallHandle,
    command: *const FcCommandV1,
) -> FcStatus;

/// Records a call-scoped diagnostic message.
///
/// # Safety
///
/// The handles must identify the current callback. The host validates `level`
/// and the message view and copies accepted bytes before returning. The message
/// must not be retained, and this callback must not unwind.
pub type FcHostDiagnosticFn = unsafe extern "C" fn(
    host: FcHostHandle,
    call: FcCallHandle,
    level: u32,
    message: FcStrView,
) -> FcStatus;

/// The plugin bootstrap symbol's function signature.
///
/// The returned descriptor is plugin-owned and remains valid while its library
/// is resident. The unsafe boundary validates its raw prefix before creating a
/// typed value.
///
/// # Safety
///
/// The resolved symbol must have this exact signature and must not unwind. A
/// non-null result must remain aligned, immutable, and live while the library
/// is resident. The unsafe boundary reads only the fixed header and published
/// required-slot offsets until size/version/null validation succeeds.
pub type FcPluginEntryV1Fn = unsafe extern "C" fn() -> *const FcPluginDescriptorV1;

/// A host-owned output buffer offered for one ABI call.
///
/// Before a host query, the host restores the original pointer/capacity and sets
/// `result_len` to zero. On [`crate::FC_OK`], `result_len <= capacity` and
/// exactly that prefix is initialized. On [`crate::FC_BUFFER_TOO_SMALL`],
/// `result_len` is the required length, which is greater than `capacity`, and
/// the bytes are unchanged. Every other failure leaves bytes unchanged and
/// `result_len` zero. The plugin may inspect a successful result only during the
/// current callback. It must not modify, retain, or free the pointer or record.
/// A nonzero capacity always has a non-null pointer to that many writable bytes;
/// capacity is bounded by host policy and must fit both `usize` and `isize`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcOutputBufferV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) data: *mut u8,
    pub(crate) capacity: u64,
    pub(crate) result_len: u64,
}

impl FcOutputBufferV1 {
    /// Exact size of the ABI v1 prefix on the required 64-bit target.
    pub const STRUCT_SIZE: u32 = 32;

    /// Creates a current-version buffer declaration with no bytes written.
    pub const fn new(data: *mut u8, capacity: u64) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            data,
            capacity,
            result_len: 0,
        }
    }

    /// Returns the record header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the buffer pointer without dereferencing it.
    pub const fn data(self) -> *mut u8 {
        self.data
    }

    /// Returns the writable byte capacity.
    pub const fn capacity(self) -> u64 {
        self.capacity
    }

    /// Returns the complete or required result length.
    pub const fn result_len(self) -> u64 {
        self.result_len
    }

    /// Sets the complete or required result length.
    ///
    /// Host callback implementations set this according to the record's result
    /// protocol; plugins must not call this method on a host-owned buffer.
    pub fn set_result_len(&mut self, result_len: u64) {
        self.result_len = result_len;
    }
}

/// A versioned event envelope delivered to a plugin callback.
///
/// The payload has a kind-specific binary encoding; JSON is not part of the
/// per-event ABI. Simulation-owned dispatch carries its exact tick plus the
/// callback-scoped live shard resource that owns the event. Connection-side,
/// off-tick dispatch carries tick `0` and [`FcResourceHandle::INVALID`] as
/// documented "metadata unavailable" sentinels; consumers must not interpret
/// those values as authoritative tick or shard identity.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcEventV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) kind: FcEventKind,
    pub(crate) flags: u32,
    pub(crate) tick: u64,
    pub(crate) shard: FcResourceHandle,
    pub(crate) payload: FcBytesView,
}

impl FcEventV1 {
    /// Exact size of the ABI v1 prefix on the required 64-bit target.
    pub const STRUCT_SIZE: u32 = 48;

    /// Creates a current-version event envelope.
    pub const fn new(
        kind: FcEventKind,
        flags: u32,
        tick: u64,
        shard: FcResourceHandle,
        payload: FcBytesView,
    ) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            kind,
            flags,
            tick,
            shard,
            payload,
        }
    }

    /// Returns the record header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the event kind.
    pub const fn kind(self) -> FcEventKind {
        self.kind
    }

    /// Returns the versioned flag bits.
    pub const fn flags(self) -> u32 {
        self.flags
    }

    /// Returns the exact simulation tick, or `0` when unavailable off-tick.
    pub const fn tick(self) -> u64 {
        self.tick
    }

    /// Returns the opaque live shard resource, or
    /// [`FcResourceHandle::INVALID`] when unavailable.
    pub const fn shard(self) -> FcResourceHandle {
        self.shard
    }

    /// Returns the call-scoped payload declaration.
    pub const fn payload(self) -> FcBytesView {
        self.payload
    }
}

/// A versioned command or host-call request envelope.
///
/// The payload has a kind-specific binary encoding and is valid only for the
/// enclosing call. In ABI v1, [`FcCommandKind::SET_BLOCK`] requires `target` to
/// be a live dimension resource; every other assigned command kind requires
/// [`FcResourceHandle::INVALID`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcCommandV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) kind: FcCommandKind,
    pub(crate) flags: u32,
    pub(crate) target: FcResourceHandle,
    pub(crate) payload: FcBytesView,
}

impl FcCommandV1 {
    /// Exact size of the ABI v1 prefix on the required 64-bit target.
    pub const STRUCT_SIZE: u32 = 40;

    /// Creates a current-version command envelope.
    pub const fn new(
        kind: FcCommandKind,
        flags: u32,
        target: FcResourceHandle,
        payload: FcBytesView,
    ) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            kind,
            flags,
            target,
            payload,
        }
    }

    /// Returns the record header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the command kind.
    pub const fn kind(self) -> FcCommandKind {
        self.kind
    }

    /// Returns the versioned flag bits.
    pub const fn flags(self) -> u32 {
        self.flags
    }

    /// Returns the opaque target resource.
    pub const fn target(self) -> FcResourceHandle {
        self.target
    }

    /// Returns the call-scoped payload declaration.
    pub const fn payload(self) -> FcBytesView {
        self.payload
    }
}

/// Shared payload for block-break events.
///
/// Used by [`FcEventKind::BLOCK_BREAK`],
/// [`FcEventKind::AFTER_BLOCK_BREAK`], and
/// [`FcEventKind::BLOCK_BREAK_ATTEMPT`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcBlockBreakEventPayloadV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) player: FcPlayerIdV1,
    pub(crate) pos: FcBlockPosV1,
}

impl FcBlockBreakEventPayloadV1 {
    /// Exact size of this ABI v1 payload.
    pub const STRUCT_SIZE: u32 = 36;

    /// Creates a block-break event payload.
    pub const fn new(player: FcPlayerIdV1, pos: FcBlockPosV1) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            player,
            pos,
        }
    }

    /// Returns the payload header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the acting player.
    pub const fn player(self) -> FcPlayerIdV1 {
        self.player
    }

    /// Returns the block position.
    pub const fn pos(self) -> FcBlockPosV1 {
        self.pos
    }
}

/// Payload for [`FcCommandKind::SET_BLOCK`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcSetBlockCommandPayloadV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) pos: FcBlockPosV1,
    pub(crate) block_state_id: u32,
}

impl FcSetBlockCommandPayloadV1 {
    /// Exact size of this ABI v1 payload.
    pub const STRUCT_SIZE: u32 = 24;

    /// Creates an exact block-state mutation payload.
    pub const fn new(pos: FcBlockPosV1, block_state_id: u32) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            pos,
            block_state_id,
        }
    }

    /// Returns the payload header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the target block position.
    pub const fn pos(self) -> FcBlockPosV1 {
        self.pos
    }

    /// Returns the opaque registry block-state id.
    pub const fn block_state_id(self) -> u32 {
        self.block_state_id
    }
}

/// Payload for [`FcHostRequestKind::BLOCK_STATE`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcBlockStateRequestPayloadV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) pos: FcBlockPosV1,
}

impl FcBlockStateRequestPayloadV1 {
    /// Exact size of this ABI v1 payload.
    pub const STRUCT_SIZE: u32 = 20;

    /// Creates a block-state query payload.
    pub const fn new(pos: FcBlockPosV1) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            pos,
        }
    }

    /// Returns the payload header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the queried block position.
    pub const fn pos(self) -> FcBlockPosV1 {
        self.pos
    }
}

/// A versioned read-only host-call request envelope.
///
/// Host request kinds and command kinds are disjoint typed namespaces. ABI v1
/// host requests are query-only so a buffer-size retry cannot duplicate a
/// mutation. [`FcHostRequestKind::CHUNK_LOADED`] and
/// [`FcHostRequestKind::BLOCK_STATE`] require `target` to be a live dimension
/// resource; every other assigned request kind requires
/// [`FcResourceHandle::INVALID`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcHostRequestV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) kind: FcHostRequestKind,
    pub(crate) flags: u32,
    pub(crate) target: FcResourceHandle,
    pub(crate) payload: FcBytesView,
}

impl FcHostRequestV1 {
    /// Exact size of the ABI v1 prefix on the required 64-bit target.
    pub const STRUCT_SIZE: u32 = 40;

    /// Creates a current-version read-only host request.
    pub const fn new(
        kind: FcHostRequestKind,
        flags: u32,
        target: FcResourceHandle,
        payload: FcBytesView,
    ) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            kind,
            flags,
            target,
            payload,
        }
    }

    /// Returns the request header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the request kind.
    pub const fn kind(self) -> FcHostRequestKind {
        self.kind
    }

    /// Returns the versioned flag bits.
    pub const fn flags(self) -> u32 {
        self.flags
    }

    /// Returns the opaque target resource.
    pub const fn target(self) -> FcResourceHandle {
        self.target
    }

    /// Returns the call-scoped payload declaration.
    pub const fn payload(self) -> FcBytesView {
        self.payload
    }
}

/// The required plugin callback table for ABI v1.
///
/// Every v1 slot is required. The unsafe boundary checks the raw slot words for
/// null before constructing this typed table. Later optional callbacks append
/// after this prefix and require a higher ABI minor.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcPluginFunctionsV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) init: FcPluginInitFn,
    pub(crate) on_event: FcPluginOnEventFn,
    pub(crate) shutdown: FcPluginShutdownFn,
}

impl FcPluginFunctionsV1 {
    /// Exact size of the required ABI v1 callback prefix on a 64-bit target.
    pub const STRUCT_SIZE: u32 = 32;

    /// Byte offset of the required initialization callback slot.
    pub const INIT_OFFSET: usize = 8;

    /// Byte offset of the required event callback slot.
    pub const ON_EVENT_OFFSET: usize = 16;

    /// Byte offset of the required shutdown callback slot.
    pub const SHUTDOWN_OFFSET: usize = 24;

    /// Creates the current required callback table.
    pub const fn new(
        init: FcPluginInitFn,
        on_event: FcPluginOnEventFn,
        shutdown: FcPluginShutdownFn,
    ) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            init,
            on_event,
            shutdown,
        }
    }

    /// Returns the table header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the initialization callback.
    pub const fn init(self) -> FcPluginInitFn {
        self.init
    }

    /// Returns the event callback.
    pub const fn on_event(self) -> FcPluginOnEventFn {
        self.on_event
    }

    /// Returns the shutdown callback.
    pub const fn shutdown(self) -> FcPluginShutdownFn {
        self.shutdown
    }
}

/// The required host callback table for ABI v1.
///
/// Capability checks remain host-owned. A denied request returns
/// [`crate::FC_CAPABILITY_DENIED`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcHostFunctionsV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) call: FcHostCallFn,
    pub(crate) emit: FcHostEmitFn,
    pub(crate) diagnostic: FcHostDiagnosticFn,
}

impl FcHostFunctionsV1 {
    /// Exact size of the required ABI v1 host-callback prefix on a 64-bit target.
    pub const STRUCT_SIZE: u32 = 32;

    /// Byte offset of the required host-call slot.
    pub const CALL_OFFSET: usize = 8;

    /// Byte offset of the required command-emission slot.
    pub const EMIT_OFFSET: usize = 16;

    /// Byte offset of the required diagnostic slot.
    pub const DIAGNOSTIC_OFFSET: usize = 24;

    /// Creates the current required host table.
    pub const fn new(
        call: FcHostCallFn,
        emit: FcHostEmitFn,
        diagnostic: FcHostDiagnosticFn,
    ) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            call,
            emit,
            diagnostic,
        }
    }

    /// Returns the table header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the capability-scoped host-call callback.
    pub const fn call(self) -> FcHostCallFn {
        self.call
    }

    /// Returns the command-emission callback.
    pub const fn emit(self) -> FcHostEmitFn {
        self.emit
    }

    /// Returns the diagnostic callback.
    pub const fn diagnostic(self) -> FcHostDiagnosticFn {
        self.diagnostic
    }
}

/// The size-prefixed ABI v1 plugin descriptor.
///
/// Metadata is returned through required getters so this descriptor can be a
/// safe immutable static in a plugin. The unsafe boundary calls each getter and
/// copies its call-scoped result into host-owned storage. The numeric version is
/// the manifest version's major/minor/patch core. The manifest remains
/// authoritative for optional semantic-version prerelease and build metadata;
/// the loader requires the three numeric components to match.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FcPluginDescriptorV1 {
    pub(crate) header: FcAbiHeader,
    pub(crate) version: FcSemanticVersion,
    pub(crate) requested_capabilities: u64,
    pub(crate) id: FcStrFn,
    pub(crate) name: FcStrFn,
    pub(crate) target: FcStrFn,
    pub(crate) functions: FcPluginFunctionsFn,
}

impl FcPluginDescriptorV1 {
    /// Exact size of the required ABI v1 descriptor prefix on a 64-bit target.
    pub const STRUCT_SIZE: u32 = 64;

    /// Byte offset of the required plugin-id getter slot.
    pub const ID_OFFSET: usize = 32;

    /// Byte offset of the required plugin-name getter slot.
    pub const NAME_OFFSET: usize = 40;

    /// Byte offset of the required target getter slot.
    pub const TARGET_OFFSET: usize = 48;

    /// Byte offset of the required function-table getter slot.
    pub const FUNCTIONS_OFFSET: usize = 56;

    /// Creates a current-version descriptor.
    pub const fn new(
        version: FcSemanticVersion,
        requested_capabilities: u64,
        id: FcStrFn,
        name: FcStrFn,
        target: FcStrFn,
        functions: FcPluginFunctionsFn,
    ) -> Self {
        Self {
            header: FcAbiHeader::current(Self::STRUCT_SIZE),
            version,
            requested_capabilities,
            id,
            name,
            target,
            functions,
        }
    }

    /// Returns the descriptor header.
    pub const fn header(self) -> FcAbiHeader {
        self.header
    }

    /// Returns the plugin semantic version.
    pub const fn version(self) -> FcSemanticVersion {
        self.version
    }

    /// Returns the plugin semantic-version major.
    pub const fn version_major(self) -> u32 {
        self.version.major()
    }

    /// Returns the plugin semantic-version minor.
    pub const fn version_minor(self) -> u32 {
        self.version.minor()
    }

    /// Returns the plugin semantic-version patch.
    pub const fn version_patch(self) -> u32 {
        self.version.patch()
    }

    /// Returns the requested host-facade capability bits.
    pub const fn requested_capabilities(self) -> u64 {
        self.requested_capabilities
    }

    /// Returns the plugin-id getter.
    pub const fn id(self) -> FcStrFn {
        self.id
    }

    /// Returns the display-name getter.
    pub const fn name(self) -> FcStrFn {
        self.name
    }

    /// Returns the target-triple getter.
    pub const fn target(self) -> FcStrFn {
        self.target
    }

    /// Returns the plugin-function-table getter.
    pub const fn functions(self) -> FcPluginFunctionsFn {
        self.functions
    }
}
