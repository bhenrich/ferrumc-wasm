//! Capability-masking forwarding backend.

use ferrumc_plugin_sdk::{
    BlockPos, CapabilityManifest, ChunkPos, CommandDefinition, DiagnosticLevel, EventKind,
    FacadeError, HostServices, PermissionNode, PlayerId, Resolution, TimerId, Vec3, WorldOperation,
};

pub(crate) struct MaskedServices<'call> {
    capabilities: CapabilityManifest,
    inner: &'call mut dyn HostServices,
}

impl<'call> MaskedServices<'call> {
    pub(crate) fn new(
        capabilities: CapabilityManifest,
        inner: &'call mut dyn HostServices,
    ) -> Self {
        Self {
            capabilities,
            inner,
        }
    }
}

impl HostServices for MaskedServices<'_> {
    fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    fn subscribe_event(&mut self, kind: EventKind) -> Result<(), FacadeError> {
        self.inner.subscribe_event(kind)
    }

    fn register_command(&mut self, command: &CommandDefinition) -> Result<(), FacadeError> {
        self.inner.register_command(command)
    }

    fn is_chunk_loaded(&mut self, chunk: ChunkPos) -> Result<bool, FacadeError> {
        self.inner.is_chunk_loaded(chunk)
    }

    fn block_state_id(&mut self, pos: BlockPos) -> Result<Option<u32>, FacadeError> {
        self.inner.block_state_id(pos)
    }

    fn player_position(&mut self, player: PlayerId) -> Result<Option<Vec3>, FacadeError> {
        self.inner.player_position(player)
    }

    fn submit_world_operation(&mut self, operation: WorldOperation) -> Result<(), FacadeError> {
        self.inner.submit_world_operation(operation)
    }

    fn resolve_permission(
        &mut self,
        player: PlayerId,
        node: &PermissionNode,
    ) -> Result<Resolution, FacadeError> {
        self.inner.resolve_permission(player, node)
    }

    fn storage_get(&mut self, key: &str) -> Result<Option<Vec<u8>>, FacadeError> {
        self.inner.storage_get(key)
    }

    fn storage_put(&mut self, key: &str, value: &[u8]) -> Result<(), FacadeError> {
        self.inner.storage_put(key, value)
    }

    fn storage_delete(&mut self, key: &str) -> Result<(), FacadeError> {
        self.inner.storage_delete(key)
    }

    fn storage_keys(&mut self) -> Result<Vec<String>, FacadeError> {
        self.inner.storage_keys()
    }

    fn schedule_timer(&mut self, id: TimerId, delay_ticks: u64) -> Result<(), FacadeError> {
        self.inner.schedule_timer(id, delay_ticks)
    }

    fn cancel_timer(&mut self, id: TimerId) -> Result<(), FacadeError> {
        self.inner.cancel_timer(id)
    }

    fn diagnostic(&mut self, level: DiagnosticLevel, message: &str) -> Result<(), FacadeError> {
        self.inner.diagnostic(level, message)
    }
}
