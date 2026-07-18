use ferrumc_plugin_abi::{
    AbiVersion, FcCommandKind, FcEventKind, FcHostRequestKind, FcPluginInitFn, FcPluginOnEventFn,
    FcPluginShutdownFn, FcResourceHandle,
};

/// A plugin semantic version copied into host-owned storage.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PluginSemanticVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl PluginSemanticVersion {
    /// Creates a semantic version from its numeric core.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
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
}

/// Validated plugin metadata whose variable-size fields are host owned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedPluginMetadata {
    abi_version: AbiVersion,
    version: PluginSemanticVersion,
    requested_capabilities: u64,
    id: String,
    name: String,
    target: String,
}

impl OwnedPluginMetadata {
    pub(crate) fn new(
        abi_version: AbiVersion,
        version: PluginSemanticVersion,
        requested_capabilities: u64,
        id: String,
        name: String,
        target: String,
    ) -> Self {
        Self {
            abi_version,
            version,
            requested_capabilities,
            id,
            name,
            target,
        }
    }

    /// Returns the negotiated ABI version.
    pub const fn abi_version(&self) -> AbiVersion {
        self.abi_version
    }

    /// Returns the plugin's numeric semantic version.
    pub const fn version(&self) -> PluginSemanticVersion {
        self.version
    }

    /// Returns the requested host-facade capability bits.
    pub const fn requested_capabilities(&self) -> u64 {
        self.requested_capabilities
    }

    /// Returns the plugin identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the plugin display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the plugin target triple.
    pub fn target(&self) -> &str {
        &self.target
    }
}

/// An event envelope with a host-owned payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedEvent {
    kind: FcEventKind,
    flags: u32,
    tick: u64,
    shard: FcResourceHandle,
    payload: Vec<u8>,
}

impl OwnedEvent {
    /// Creates an owned event envelope.
    pub fn new(
        kind: FcEventKind,
        flags: u32,
        tick: u64,
        shard: FcResourceHandle,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            kind,
            flags,
            tick,
            shard,
            payload,
        }
    }

    /// Returns the event kind.
    pub const fn kind(&self) -> FcEventKind {
        self.kind
    }

    /// Returns the versioned event flags.
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    /// Returns the exact simulation tick, or `0` when unavailable off-tick.
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Returns the live shard resource handle, or
    /// [`FcResourceHandle::INVALID`] when unavailable off-tick.
    pub const fn shard(&self) -> FcResourceHandle {
        self.shard
    }

    /// Returns the owned payload bytes.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the event and returns its payload.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

/// A command envelope with a host-owned payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedCommand {
    kind: FcCommandKind,
    flags: u32,
    target: FcResourceHandle,
    payload: Vec<u8>,
}

impl OwnedCommand {
    /// Creates an owned command envelope.
    pub fn new(
        kind: FcCommandKind,
        flags: u32,
        target: FcResourceHandle,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            kind,
            flags,
            target,
            payload,
        }
    }

    /// Returns the command kind.
    pub const fn kind(&self) -> FcCommandKind {
        self.kind
    }

    /// Returns the versioned command flags.
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    /// Returns the target resource handle.
    pub const fn target(&self) -> FcResourceHandle {
        self.target
    }

    /// Returns the owned payload bytes.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the command and returns its payload.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

/// A host-request envelope with a host-owned payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedHostRequest {
    kind: FcHostRequestKind,
    flags: u32,
    target: FcResourceHandle,
    payload: Vec<u8>,
}

impl OwnedHostRequest {
    /// Creates an owned host-request envelope.
    pub fn new(
        kind: FcHostRequestKind,
        flags: u32,
        target: FcResourceHandle,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            kind,
            flags,
            target,
            payload,
        }
    }

    /// Returns the host-request kind.
    pub const fn kind(&self) -> FcHostRequestKind {
        self.kind
    }

    /// Returns the versioned host-request flags.
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    /// Returns the target resource handle.
    pub const fn target(&self) -> FcResourceHandle {
        self.target
    }

    /// Returns the owned payload bytes.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the request and returns its payload.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ValidatedCallbacks {
    init: FcPluginInitFn,
    on_event: FcPluginOnEventFn,
    shutdown: FcPluginShutdownFn,
}

impl ValidatedCallbacks {
    pub(crate) const fn new(
        init: FcPluginInitFn,
        on_event: FcPluginOnEventFn,
        shutdown: FcPluginShutdownFn,
    ) -> Self {
        Self {
            init,
            on_event,
            shutdown,
        }
    }

    pub(crate) const fn init(self) -> FcPluginInitFn {
        self.init
    }

    pub(crate) const fn on_event(self) -> FcPluginOnEventFn {
        self.on_event
    }

    pub(crate) const fn shutdown(self) -> FcPluginShutdownFn {
        self.shutdown
    }
}
