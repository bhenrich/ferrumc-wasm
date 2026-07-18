//! Capability facades and the adapter-facing host-services seam.

use core::num::NonZeroU64;

use crate::{
    BlockPos, CapabilityManifest, ChunkPos, CommandDefinition, EventKind, FacadeError,
    MessageOperation, PermissionNode, PlayerId, Resolution, SetBlockOperation, TeleportOperation,
    Vec3, WorldOperation,
};

/// Maximum storage-key byte length.
pub const MAX_STORAGE_KEY_BYTES: usize = 256;

/// Maximum storage-value byte length.
///
/// The limit leaves five bytes for the presence flag and byte-field length in
/// the current 65,536-byte host-owned output buffer.
pub const MAX_STORAGE_VALUE_BYTES: usize = 65_531;

/// Maximum number of keys returned by one storage listing.
pub const MAX_STORAGE_KEYS: usize = 256;

/// Maximum encoded bytes after the key-list count field.
///
/// Together with the four-byte count, this fits the current 65,536-byte
/// host-owned output buffer.
pub const MAX_STORAGE_KEY_LIST_BYTES: usize = 65_532;

/// Maximum byte length of one diagnostic message.
pub const MAX_DIAGNOSTIC_BYTES: usize = 4_096;

/// Stable nonzero identifier for a deterministic plugin timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(NonZeroU64);

impl TimerId {
    /// Creates a timer identifier, returning `None` for the reserved zero.
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the nonzero numeric identifier.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Severity of a plugin-authored diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DiagnosticLevel {
    /// An error that prevented intended plugin work.
    Error,
    /// A warning about degraded or suspicious behavior.
    Warn,
    /// Ordinary informational output.
    Info,
    /// Debugging detail.
    Debug,
    /// Fine-grained tracing detail.
    Trace,
}

/// Adapter-facing implementation of all shared plugin facades.
///
/// This trait is public only so packaging adapters and the deterministic
/// testhost can implement it. One callback context borrows exactly one
/// implementation. Even read operations take `&mut self`, matching the single
/// call handle owned by the trusted native plugin adapter and preventing simultaneous
/// aliases over that handle.
#[doc(hidden)]
pub trait HostServices {
    /// Returns the actual capabilities granted to this plugin instance.
    fn capabilities(&self) -> CapabilityManifest;

    /// Records one event subscription during plugin load.
    fn subscribe_event(&mut self, kind: EventKind) -> Result<(), FacadeError>;

    /// Records one pure-data command definition during plugin load.
    fn register_command(&mut self, command: &CommandDefinition) -> Result<(), FacadeError>;

    /// Queries whether the current world's chunk is loaded.
    fn is_chunk_loaded(&mut self, chunk: ChunkPos) -> Result<bool, FacadeError>;

    /// Queries a block state in the implicit current world.
    fn block_state_id(&mut self, pos: BlockPos) -> Result<Option<u32>, FacadeError>;

    /// Queries a player's current position.
    fn player_position(&mut self, player: PlayerId) -> Result<Option<Vec3>, FacadeError>;

    /// Submits one operation to the host-owned bounded command buffer.
    fn submit_world_operation(&mut self, operation: WorldOperation) -> Result<(), FacadeError>;

    /// Resolves one validated permission node for a player.
    fn resolve_permission(
        &mut self,
        player: PlayerId,
        node: &PermissionNode,
    ) -> Result<Resolution, FacadeError>;

    /// Reads one key from the host-selected plugin namespace.
    fn storage_get(&mut self, key: &str) -> Result<Option<Vec<u8>>, FacadeError>;

    /// Stores one value in the host-selected plugin namespace.
    fn storage_put(&mut self, key: &str, value: &[u8]) -> Result<(), FacadeError>;

    /// Deletes one key from the host-selected plugin namespace.
    ///
    /// The shared contract deliberately does not promise whether the key was
    /// present because the trusted native plugin `ABI` command has no presence result.
    fn storage_delete(&mut self, key: &str) -> Result<(), FacadeError>;

    /// Lists keys in the host-selected plugin namespace.
    fn storage_keys(&mut self) -> Result<Vec<String>, FacadeError>;

    /// Schedules or replaces a deterministic tick timer.
    fn schedule_timer(&mut self, id: TimerId, delay_ticks: u64) -> Result<(), FacadeError>;

    /// Cancels a deterministic tick timer.
    fn cancel_timer(&mut self, id: TimerId) -> Result<(), FacadeError>;

    /// Emits one bounded diagnostic.
    fn diagnostic(&mut self, level: DiagnosticLevel, message: &str) -> Result<(), FacadeError>;
}

/// Load-phase event-subscription facade.
pub struct EventSubscriptions<'call> {
    services: &'call mut dyn HostServices,
}

impl<'call> EventSubscriptions<'call> {
    pub(crate) fn new(services: &'call mut dyn HostServices) -> Self {
        Self { services }
    }

    /// Subscribes to one event kind.
    pub fn subscribe(&mut self, kind: EventKind) -> Result<(), FacadeError> {
        self.services.subscribe_event(kind)
    }
}

/// Load-phase pure-data command-registration facade.
pub struct CommandRegistrations<'call> {
    services: &'call mut dyn HostServices,
}

impl<'call> CommandRegistrations<'call> {
    pub(crate) fn new(services: &'call mut dyn HostServices) -> Self {
        Self { services }
    }

    /// Registers one validated command tree.
    pub fn register(&mut self, command: &CommandDefinition) -> Result<(), FacadeError> {
        self.services.register_command(command)
    }
}

/// Read-only view of the implicit current world for one callback.
///
/// No dimension identifier is exposed: the trusted native plugin adapter keeps its
/// opaque current-dimension resource handle internal.
pub struct WorldView<'call> {
    services: &'call mut dyn HostServices,
}

impl<'call> WorldView<'call> {
    pub(crate) fn new(services: &'call mut dyn HostServices) -> Self {
        Self { services }
    }

    /// Returns whether one typed chunk coordinate is loaded.
    pub fn is_chunk_loaded(&mut self, chunk: ChunkPos) -> Result<bool, FacadeError> {
        self.services.is_chunk_loaded(chunk)
    }

    /// Returns the block-state identifier, or `None` when unavailable because
    /// its chunk is not loaded.
    pub fn block_state_id(&mut self, pos: BlockPos) -> Result<Option<u32>, FacadeError> {
        self.services.block_state_id(pos)
    }

    /// Returns a player's position when present in this callback's view.
    pub fn player_position(&mut self, player: PlayerId) -> Result<Option<Vec3>, FacadeError> {
        self.services.player_position(player)
    }
}

/// Bounded world-operation facade.
pub struct WorldOperations<'call> {
    services: &'call mut dyn HostServices,
}

impl<'call> WorldOperations<'call> {
    pub(crate) fn new(services: &'call mut dyn HostServices) -> Self {
        Self { services }
    }

    /// Requests one exact block-state write.
    pub fn set_block(&mut self, pos: BlockPos, block_state_id: u32) -> Result<(), FacadeError> {
        self.services
            .submit_world_operation(WorldOperation::SetBlock(SetBlockOperation::new(
                pos,
                block_state_id,
            )))
    }

    /// Requests a player teleport after rejecting non-finite coordinates.
    pub fn teleport(&mut self, player: PlayerId, position: Vec3) -> Result<(), FacadeError> {
        let operation = TeleportOperation::new(player, position)?;
        self.services
            .submit_world_operation(WorldOperation::Teleport(operation))
    }

    /// Requests a bounded plain-text player message.
    pub fn message(
        &mut self,
        player: PlayerId,
        message: impl Into<String>,
    ) -> Result<(), FacadeError> {
        let operation = MessageOperation::new(player, message)?;
        self.services
            .submit_world_operation(WorldOperation::Message(operation))
    }
}

/// Read-only permission-query facade.
pub struct PermissionQueries<'call> {
    services: &'call mut dyn HostServices,
}

impl<'call> PermissionQueries<'call> {
    pub(crate) fn new(services: &'call mut dyn HostServices) -> Self {
        Self { services }
    }

    /// Resolves a validated permission node to allowed, denied, or unset.
    pub fn resolve(
        &mut self,
        player: PlayerId,
        node: &PermissionNode,
    ) -> Result<Resolution, FacadeError> {
        self.services.resolve_permission(player, node)
    }

    /// Returns whether the node is explicitly allowed, closing unset by
    /// default.
    pub fn has_permission(
        &mut self,
        player: PlayerId,
        node: &PermissionNode,
    ) -> Result<bool, FacadeError> {
        self.resolve(player, node).map(Resolution::is_allowed)
    }
}

/// Host-namespaced bounded storage facade.
///
/// No method accepts a plugin identifier or namespace.
pub struct NamespacedStorage<'call> {
    services: &'call mut dyn HostServices,
}

impl<'call> NamespacedStorage<'call> {
    pub(crate) fn new(services: &'call mut dyn HostServices) -> Self {
        Self { services }
    }

    /// Reads one key from this plugin's namespace.
    pub fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, FacadeError> {
        validate_storage_key(key)?;
        let value = self.services.storage_get(key)?;
        if value
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_STORAGE_VALUE_BYTES)
        {
            return Err(FacadeError::InvalidHostResponse {
                resource: "storage value",
                reason: "value exceeds the shared SDK limit",
            });
        }
        Ok(value)
    }

    /// Stores one bounded value in this plugin's namespace.
    pub fn put(&mut self, key: &str, value: &[u8]) -> Result<(), FacadeError> {
        validate_storage_key(key)?;
        if value.len() > MAX_STORAGE_VALUE_BYTES {
            return Err(FacadeError::LimitExceeded {
                resource: "storage value",
                len: value.len(),
                max: MAX_STORAGE_VALUE_BYTES,
            });
        }
        self.services.storage_put(key, value)
    }

    /// Deletes one key from this plugin's namespace.
    pub fn delete(&mut self, key: &str) -> Result<(), FacadeError> {
        validate_storage_key(key)?;
        self.services.storage_delete(key)
    }

    /// Lists bounded keys in deterministic byte order.
    pub fn keys(&mut self) -> Result<Vec<String>, FacadeError> {
        let mut keys = self.services.storage_keys()?;
        if keys.len() > MAX_STORAGE_KEYS {
            return Err(FacadeError::InvalidHostResponse {
                resource: "storage key list",
                reason: "key count exceeds the shared SDK limit",
            });
        }
        let mut encoded_bytes = 0usize;
        for key in &keys {
            validate_storage_key(key).map_err(|_| FacadeError::InvalidHostResponse {
                resource: "storage key list",
                reason: "host returned an invalid key",
            })?;
            encoded_bytes = encoded_bytes
                .checked_add(4)
                .and_then(|value| value.checked_add(key.len()))
                .ok_or(FacadeError::InvalidHostResponse {
                    resource: "storage key list",
                    reason: "encoded length overflowed",
                })?;
        }
        if encoded_bytes > MAX_STORAGE_KEY_LIST_BYTES {
            return Err(FacadeError::InvalidHostResponse {
                resource: "storage key list",
                reason: "encoded response exceeds the shared SDK limit",
            });
        }
        keys.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        Ok(keys)
    }
}

/// Deterministic tick-timer facade.
pub struct Timers<'call> {
    services: &'call mut dyn HostServices,
}

impl<'call> Timers<'call> {
    pub(crate) fn new(services: &'call mut dyn HostServices) -> Self {
        Self { services }
    }

    /// Schedules or replaces a timer after a positive tick delay.
    pub fn schedule(&mut self, id: TimerId, delay_ticks: u64) -> Result<(), FacadeError> {
        if delay_ticks == 0 {
            return Err(FacadeError::InvalidInput {
                resource: "timer delay",
                reason: "delay must be at least one tick",
            });
        }
        self.services.schedule_timer(id, delay_ticks)
    }

    /// Cancels a timer. Cancelling an absent timer is idempotent.
    pub fn cancel(&mut self, id: TimerId) -> Result<(), FacadeError> {
        self.services.cancel_timer(id)
    }
}

/// Bounded diagnostic facade available in every callback phase.
pub struct Diagnostics<'call> {
    services: &'call mut dyn HostServices,
}

impl<'call> Diagnostics<'call> {
    pub(crate) fn new(services: &'call mut dyn HostServices) -> Self {
        Self { services }
    }

    /// Emits one bounded plain-text diagnostic.
    pub fn emit(&mut self, level: DiagnosticLevel, message: &str) -> Result<(), FacadeError> {
        if message.len() > MAX_DIAGNOSTIC_BYTES {
            return Err(FacadeError::LimitExceeded {
                resource: "diagnostic",
                len: message.len(),
                max: MAX_DIAGNOSTIC_BYTES,
            });
        }
        self.services.diagnostic(level, message)
    }
}

fn validate_storage_key(key: &str) -> Result<(), FacadeError> {
    if key.is_empty() {
        return Err(FacadeError::InvalidInput {
            resource: "storage key",
            reason: "key must not be empty",
        });
    }
    if key.len() > MAX_STORAGE_KEY_BYTES {
        return Err(FacadeError::LimitExceeded {
            resource: "storage key",
            len: key.len(),
            max: MAX_STORAGE_KEY_BYTES,
        });
    }
    Ok(())
}
