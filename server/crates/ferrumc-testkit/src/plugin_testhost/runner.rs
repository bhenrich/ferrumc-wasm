//! Public testhost configuration and packaging-mode replay drivers.

use std::path::Path;

use ferrumc_plugin_abi::{
    FC_CAPABILITIES_V1, FC_CAPABILITY_DENIED, FC_COMMAND_BUFFER_FULL, FC_ERROR, FC_PLUGIN_PANIC,
};
use ferrumc_plugin_abi_sys::{CallbackError, PluginInstance};
use ferrumc_plugin_sdk::{
    CapabilityManifest, ChunkPos, Event, EventKind, PermissionNode, PlayerId, Resolution, Tick,
    Vec3,
};
use ferrumc_plugin_sdk_builtin::{
    BuiltinCallbackError, BuiltinPluginFactory, BuiltinPluginInstance,
};

use super::backend::{
    validate_seed_storage_set, CallbackFrame, FramePhase, RuntimeState, SeedState,
    DEFAULT_DIMENSION_HANDLE,
};
use super::codec;
use super::{
    PermissionSetting, PluginCallbackPhase, PluginFailureKind, PluginRun, PluginTestHostError,
};

/// Hard ceiling for semantic effects admitted by one callback.
///
/// The caller chooses a smaller capacity to exercise backpressure. The hard
/// ceiling prevents a malicious trusted native plugin callback from turning a test
/// configuration such as `usize::MAX` into a queue without a practical bound.
pub const MAX_CALLBACK_EFFECTS: usize = 4_096;
/// Hard ceiling for one caller-provided synthetic event log.
pub const MAX_SCHEDULED_EVENTS: usize = 65_536;
const DEFAULT_SHARD_HANDLE: u64 = 0x5348_5244;

/// One synthetic event scheduled at an explicit deterministic tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledPluginEvent {
    tick: Tick,
    event: Event,
}

impl ScheduledPluginEvent {
    /// Creates one scheduled event.
    pub const fn new(tick: Tick, event: Event) -> Self {
        Self { tick, event }
    }

    /// Returns its deterministic tick.
    pub const fn tick(&self) -> Tick {
        self.tick
    }

    /// Returns the shared-SDK event.
    pub const fn event(&self) -> &Event {
        &self.event
    }
}

/// Deterministic local host blueprint for shared-SDK plugin replay.
///
/// A blueprint contains only initial semantic state, offered capabilities,
/// opaque dynamic handles, and bounded callback policy. Each replay starts from
/// a fresh clone and runs load, ordered events, then unload, so built-in and
/// trusted native plugin packaging can consume the same log independently. Callback
/// effects commit atomically only on success; the crate-level replay contract
/// documents bounds, rollback, diagnostics, and digest normalization.
#[derive(Clone, Debug)]
pub struct PluginTestHost {
    granted: CapabilityManifest,
    callback_capacity: usize,
    dimension_handle: u64,
    shard_handle: u64,
    seed: SeedState,
}

impl PluginTestHost {
    /// Creates a host with explicit grants and per-callback effect capacity.
    pub fn new(
        granted: CapabilityManifest,
        callback_capacity: usize,
    ) -> Result<Self, PluginTestHostError> {
        if callback_capacity == 0 || callback_capacity > MAX_CALLBACK_EFFECTS {
            return Err(PluginTestHostError::InvalidCapacity {
                requested: callback_capacity,
                maximum: MAX_CALLBACK_EFFECTS,
            });
        }
        Ok(Self {
            granted,
            callback_capacity,
            dimension_handle: DEFAULT_DIMENSION_HANDLE,
            shard_handle: DEFAULT_SHARD_HANDLE,
            seed: SeedState::empty(),
        })
    }

    /// Returns capabilities offered to a plugin before request intersection.
    pub const fn granted_capabilities(&self) -> CapabilityManifest {
        self.granted
    }

    /// Returns the per-callback semantic-effect capacity.
    pub const fn callback_capacity(&self) -> usize {
        self.callback_capacity
    }

    /// Changes the raw nonzero dimension handle used only by dynamic ABI calls.
    ///
    /// Resource handles are plumbing and never enter snapshots or digests. This
    /// setter exists so conformance tests can prove that invariant.
    pub fn set_dynamic_dimension_handle(&mut self, handle: u64) -> Result<(), PluginTestHostError> {
        if handle == 0 || handle == self.shard_handle {
            return Err(PluginTestHostError::InvalidResourceHandles);
        }
        self.dimension_handle = handle;
        Ok(())
    }

    /// Changes the raw nonzero shard handle carried only by dynamic events.
    ///
    /// Shard and dimension resources are distinct types and may not reuse a
    /// token even though neither token contributes to semantic output.
    pub fn set_dynamic_shard_handle(&mut self, handle: u64) -> Result<(), PluginTestHostError> {
        if handle == 0 || handle == self.dimension_handle {
            return Err(PluginTestHostError::InvalidResourceHandles);
        }
        self.shard_handle = handle;
        Ok(())
    }

    /// Marks or unmarks one typed chunk as loaded in the initial world view.
    pub fn set_chunk_loaded(&mut self, chunk: ChunkPos, loaded: bool) {
        if loaded {
            self.seed.loaded_chunks.insert(chunk);
        } else {
            self.seed.loaded_chunks.remove(&chunk);
        }
    }

    /// Seeds one block state and marks its containing chunk loaded.
    pub fn set_block(&mut self, pos: ferrumc_plugin_sdk::BlockPos, block_state_id: u32) {
        self.seed.loaded_chunks.insert(pos.to_chunk_pos());
        self.seed.blocks.insert(pos, block_state_id);
    }

    /// Seeds one finite player position.
    pub fn set_player_position(
        &mut self,
        player: PlayerId,
        position: Vec3,
    ) -> Result<(), PluginTestHostError> {
        if !position.x.is_finite() || !position.y.is_finite() || !position.z.is_finite() {
            return Err(PluginTestHostError::NonFinitePlayerPosition);
        }
        self.seed.player_positions.insert(player, position);
        Ok(())
    }

    /// Seeds one permission resolution, replacing an earlier identical key.
    pub fn set_permission(
        &mut self,
        player: PlayerId,
        node: PermissionNode,
        resolution: Resolution,
    ) {
        if let Some(setting) = self
            .seed
            .permissions
            .iter_mut()
            .find(|setting| setting.player() == player && setting.node() == &node)
        {
            *setting = PermissionSetting::new(player, node, resolution);
        } else {
            self.seed
                .permissions
                .push(PermissionSetting::new(player, node, resolution));
            self.seed.permissions.sort_unstable_by(|left, right| {
                left.player()
                    .cmp(&right.player())
                    .then_with(|| left.node().as_str().cmp(right.node().as_str()))
            });
        }
    }

    /// Seeds one bounded namespaced storage value.
    pub fn set_storage(
        &mut self,
        key: impl Into<String>,
        value: Vec<u8>,
    ) -> Result<(), PluginTestHostError> {
        let key = key.into();
        validate_seed_storage_set(&self.seed.storage, &key, &value)
            .map_err(PluginTestHostError::InvalidStorage)?;
        self.seed.storage.insert(key, value);
        Ok(())
    }

    /// Replays load, ordered events, and unload through a compiled-in factory.
    ///
    /// The instance receives the intersection of its requested capabilities
    /// and this host's offered grants.
    pub fn replay_builtin(
        &self,
        factory: BuiltinPluginFactory,
        events: &[ScheduledPluginEvent],
    ) -> Result<PluginRun, PluginTestHostError> {
        validate_schedule(events)?;
        let effective = intersect(factory.requested_capabilities(), self.granted);
        let mut state = RuntimeState::from_seed(&self.seed);
        let mut load_frame = CallbackFrame::new(
            &state,
            effective,
            FramePhase::Load,
            Tick::ZERO,
            self.callback_capacity,
            self.dimension_handle,
            None,
        );
        let initialized = factory.initialize(self.granted, &mut load_frame);
        let (staged, diagnostics, frame_failure) = load_frame.into_parts();
        state.retain_diagnostics(diagnostics);
        let mut instance = match initialized {
            Ok(instance) if frame_failure.is_none() => {
                state.commit(staged);
                instance
            }
            Ok(instance) => {
                discard_builtin_instance(
                    &state,
                    instance,
                    effective,
                    self.callback_capacity,
                    self.dimension_handle,
                    Tick::ZERO,
                );
                return Err(replay_failure(
                    &state,
                    PluginCallbackPhase::Load,
                    frame_failure.unwrap_or_else(|| {
                        PluginFailureKind::AbiProtocol(
                            "load backend recorded an unspecified failure".to_owned(),
                        )
                    }),
                ));
            }
            Err(error) => {
                return Err(replay_failure(
                    &state,
                    PluginCallbackPhase::Load,
                    frame_failure.unwrap_or_else(|| map_builtin_error(error)),
                ))
            }
        };

        if let Err(error) = drive_builtin(
            &mut state,
            &mut instance,
            effective,
            self.callback_capacity,
            self.dimension_handle,
            events,
        ) {
            discard_builtin_instance(
                &state,
                instance,
                effective,
                self.callback_capacity,
                self.dimension_handle,
                events.last().map_or(Tick::ZERO, ScheduledPluginEvent::tick),
            );
            return Err(error);
        }
        shutdown_builtin(
            &mut state,
            instance,
            effective,
            self.callback_capacity,
            self.dimension_handle,
            events.last().map_or(Tick::ZERO, ScheduledPluginEvent::tick),
        )?;
        state
            .report()
            .map_err(|resource| PluginTestHostError::UnsupportedSemanticValue { resource })
    }

    /// Opens a real ABI-system library and replays load, ordered events, and
    /// unload through it.
    ///
    /// The instance receives the intersection of its requested capabilities
    /// and this host's offered grants. Successfully opened libraries remain
    /// resident by ABI-system policy.
    #[allow(clippy::too_many_lines)]
    pub fn replay_dynamic(
        &self,
        library_path: &Path,
        events: &[ScheduledPluginEvent],
    ) -> Result<PluginRun, PluginTestHostError> {
        validate_schedule(events)?;
        let loaded = ferrumc_plugin_abi_sys::load(library_path)?;
        let requested = loaded.metadata().requested_capabilities();
        if requested & !FC_CAPABILITIES_V1 != 0 {
            let state = RuntimeState::from_seed(&self.seed);
            return Err(replay_failure(
                &state,
                PluginCallbackPhase::Load,
                PluginFailureKind::AbiProtocol(
                    "plugin requested capability bits unknown to ABI v1".to_owned(),
                ),
            ));
        }
        let requested_low = u32::try_from(requested).map_err(|_| {
            let state = RuntimeState::from_seed(&self.seed);
            replay_failure(
                &state,
                PluginCallbackPhase::Load,
                PluginFailureKind::AbiProtocol(
                    "plugin capability mask does not fit the SDK".to_owned(),
                ),
            )
        })?;
        let effective = intersect(
            CapabilityManifest::from_bits_truncate(requested_low),
            self.granted,
        );
        let mut state = RuntimeState::from_seed(&self.seed);
        let mut load_frame = CallbackFrame::new(
            &state,
            effective,
            FramePhase::Load,
            Tick::ZERO,
            self.callback_capacity,
            self.dimension_handle,
            None,
        );
        let initialized = loaded.initialize(u64::from(effective.bits()), &mut load_frame);
        let (staged, diagnostics, frame_failure) = load_frame.into_parts();
        state.retain_diagnostics(diagnostics);
        let mut instance = match initialized {
            Ok(instance) if frame_failure.is_none() => {
                state.commit(staged);
                instance
            }
            Ok(instance) => {
                discard_dynamic_instance(
                    &state,
                    instance,
                    effective,
                    self.callback_capacity,
                    self.dimension_handle,
                    Tick::ZERO,
                );
                return Err(replay_failure(
                    &state,
                    PluginCallbackPhase::Load,
                    frame_failure.unwrap_or_else(|| {
                        PluginFailureKind::AbiProtocol(
                            "load backend recorded an unspecified failure".to_owned(),
                        )
                    }),
                ));
            }
            Err(error) => {
                return Err(replay_failure(
                    &state,
                    PluginCallbackPhase::Load,
                    frame_failure.unwrap_or_else(|| map_callback_error(error)),
                ))
            }
        };

        if let Err(error) = drive_dynamic(
            &mut state,
            &mut instance,
            effective,
            self.callback_capacity,
            self.dimension_handle,
            self.shard_handle,
            events,
        ) {
            discard_dynamic_instance(
                &state,
                instance,
                effective,
                self.callback_capacity,
                self.dimension_handle,
                events.last().map_or(Tick::ZERO, ScheduledPluginEvent::tick),
            );
            return Err(error);
        }
        shutdown_dynamic(
            &mut state,
            instance,
            effective,
            self.callback_capacity,
            self.dimension_handle,
            events.last().map_or(Tick::ZERO, ScheduledPluginEvent::tick),
        )?;
        state
            .report()
            .map_err(|resource| PluginTestHostError::UnsupportedSemanticValue { resource })
    }
}

fn drive_builtin(
    state: &mut RuntimeState,
    instance: &mut BuiltinPluginInstance,
    capabilities: CapabilityManifest,
    capacity: usize,
    dimension_handle: u64,
    events: &[ScheduledPluginEvent],
) -> Result<(), PluginTestHostError> {
    for (index, scheduled) in events.iter().enumerate() {
        let phase = event_phase(index, scheduled);
        ensure_subscription(state, scheduled.event())
            .map_err(|failure| replay_failure(state, phase, failure))?;
        let mut frame = CallbackFrame::new(
            state,
            capabilities,
            FramePhase::Event,
            scheduled.tick(),
            capacity,
            dimension_handle,
            Some(scheduled.event().kind()),
        );
        let callback = instance.handle_event(scheduled.event(), scheduled.tick(), &mut frame);
        let callback = match callback {
            Ok(outcome) => frame.admit_builtin_decision(outcome),
            Err(error) => Err(map_builtin_error(error)),
        };
        let decision = frame.validate_decision();
        let (staged, diagnostics, frame_failure) = frame.into_parts();
        state.retain_diagnostics(diagnostics);
        if let Some(failure) = frame_failure {
            return Err(replay_failure(state, phase, failure));
        }
        if let Err(failure) = callback {
            return Err(replay_failure(state, phase, failure));
        }
        if let Err(failure) = decision {
            return Err(replay_failure(state, phase, failure));
        }
        state.commit(staged);
    }
    Ok(())
}

fn drive_dynamic(
    state: &mut RuntimeState,
    instance: &mut PluginInstance,
    capabilities: CapabilityManifest,
    capacity: usize,
    dimension_handle: u64,
    shard_handle: u64,
    events: &[ScheduledPluginEvent],
) -> Result<(), PluginTestHostError> {
    for (index, scheduled) in events.iter().enumerate() {
        let phase = event_phase(index, scheduled);
        ensure_subscription(state, scheduled.event())
            .map_err(|failure| replay_failure(state, phase, failure))?;
        let event = codec::encode_event(scheduled.event(), scheduled.tick().get(), shard_handle)
            .map_err(|failure| replay_failure(state, phase, failure))?;
        let mut frame = CallbackFrame::new(
            state,
            capabilities,
            FramePhase::Event,
            scheduled.tick(),
            capacity,
            dimension_handle,
            Some(scheduled.event().kind()),
        );
        let callback = instance.on_event(&event, &mut frame);
        let decision = frame.validate_decision();
        let (staged, diagnostics, frame_failure) = frame.into_parts();
        state.retain_diagnostics(diagnostics);
        if let Some(failure) = frame_failure {
            return Err(replay_failure(state, phase, failure));
        }
        match callback {
            Ok(status) if status.is_ok() => {}
            Ok(status) => {
                return Err(replay_failure(
                    state,
                    phase,
                    map_event_status(status.code(), scheduled.event().kind(), capabilities),
                ))
            }
            Err(error) => return Err(replay_failure(state, phase, map_callback_error(error))),
        }
        if let Err(failure) = decision {
            return Err(replay_failure(state, phase, failure));
        }
        state.commit(staged);
    }
    Ok(())
}

fn shutdown_builtin(
    state: &mut RuntimeState,
    instance: BuiltinPluginInstance,
    capabilities: CapabilityManifest,
    capacity: usize,
    dimension_handle: u64,
    tick: Tick,
) -> Result<(), PluginTestHostError> {
    let mut frame = CallbackFrame::new(
        state,
        capabilities,
        FramePhase::Unload,
        tick,
        capacity,
        dimension_handle,
        None,
    );
    let result = instance.shutdown(&mut frame);
    let (staged, diagnostics, frame_failure) = frame.into_parts();
    state.retain_diagnostics(diagnostics);
    if let Some(failure) = frame_failure {
        return Err(replay_failure(state, PluginCallbackPhase::Unload, failure));
    }
    result.map_err(|error| {
        replay_failure(state, PluginCallbackPhase::Unload, map_builtin_error(error))
    })?;
    state.commit(staged);
    Ok(())
}

fn shutdown_dynamic(
    state: &mut RuntimeState,
    instance: PluginInstance,
    capabilities: CapabilityManifest,
    capacity: usize,
    dimension_handle: u64,
    tick: Tick,
) -> Result<(), PluginTestHostError> {
    let mut frame = CallbackFrame::new(
        state,
        capabilities,
        FramePhase::Unload,
        tick,
        capacity,
        dimension_handle,
        None,
    );
    let result = instance.shutdown(&mut frame);
    let (staged, diagnostics, frame_failure) = frame.into_parts();
    state.retain_diagnostics(diagnostics);
    if let Some(failure) = frame_failure {
        return Err(replay_failure(state, PluginCallbackPhase::Unload, failure));
    }
    match result {
        Ok(status) if status.is_ok() => {
            state.commit(staged);
            Ok(())
        }
        Ok(status) => Err(replay_failure(
            state,
            PluginCallbackPhase::Unload,
            map_status(status.code()),
        )),
        Err(error) => Err(replay_failure(
            state,
            PluginCallbackPhase::Unload,
            map_callback_error(error),
        )),
    }
}

fn discard_builtin_instance(
    state: &RuntimeState,
    instance: BuiltinPluginInstance,
    capabilities: CapabilityManifest,
    capacity: usize,
    dimension_handle: u64,
    tick: Tick,
) {
    let mut frame = CallbackFrame::new(
        state,
        capabilities,
        FramePhase::Unload,
        tick,
        capacity,
        dimension_handle,
        None,
    );
    let _discarded = instance.shutdown(&mut frame);
}

fn discard_dynamic_instance(
    state: &RuntimeState,
    instance: PluginInstance,
    capabilities: CapabilityManifest,
    capacity: usize,
    dimension_handle: u64,
    tick: Tick,
) {
    let mut frame = CallbackFrame::new(
        state,
        capabilities,
        FramePhase::Unload,
        tick,
        capacity,
        dimension_handle,
        None,
    );
    let _discarded = instance.shutdown(&mut frame);
}

fn validate_schedule(events: &[ScheduledPluginEvent]) -> Result<(), PluginTestHostError> {
    if events.len() > MAX_SCHEDULED_EVENTS {
        return Err(PluginTestHostError::TooManyScheduledEvents {
            len: events.len(),
            maximum: MAX_SCHEDULED_EVENTS,
        });
    }
    for scheduled in events {
        if !is_known_event_kind(scheduled.event().kind()) {
            return Err(PluginTestHostError::UnsupportedSemanticValue {
                resource: "event kind",
            });
        }
    }
    for (index, pair) in events.windows(2).enumerate() {
        let previous = pair[0].tick();
        let current = pair[1].tick();
        if current < previous {
            return Err(PluginTestHostError::DecreasingTick {
                index: index + 1,
                previous,
                current,
            });
        }
    }
    Ok(())
}

fn is_known_event_kind(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::PlayerJoin
            | EventKind::PlayerLeave
            | EventKind::BlockBreak
            | EventKind::AfterBlockPlace
            | EventKind::AfterBlockBreak
            | EventKind::PlayerMove
            | EventKind::BlockPlaceAttempt
            | EventKind::BlockBreakAttempt
            | EventKind::ChatAttempt
            | EventKind::InteractAttempt
            | EventKind::Command
            | EventKind::Timer
    )
}

fn ensure_subscription(state: &RuntimeState, event: &Event) -> Result<(), PluginFailureKind> {
    let kind = event.kind();
    if matches!(
        kind,
        EventKind::PlayerJoin
            | EventKind::PlayerLeave
            | EventKind::BlockBreak
            | EventKind::AfterBlockPlace
            | EventKind::AfterBlockBreak
            | EventKind::PlayerMove
    ) && !state.is_subscribed(kind)
    {
        return Err(PluginFailureKind::EventNotSubscribed(kind));
    }
    Ok(())
}

fn event_phase(index: usize, event: &ScheduledPluginEvent) -> PluginCallbackPhase {
    PluginCallbackPhase::Event {
        index,
        tick: event.tick(),
        kind: event.event().kind(),
    }
}

fn replay_failure(
    state: &RuntimeState,
    phase: PluginCallbackPhase,
    kind: PluginFailureKind,
) -> PluginTestHostError {
    match state.report() {
        Ok(report) => super::PluginReplayFailure::new(phase, kind, report).into(),
        Err(resource) => PluginTestHostError::UnsupportedSemanticValue { resource },
    }
}

fn map_builtin_error(error: BuiltinCallbackError) -> PluginFailureKind {
    match error {
        BuiltinCallbackError::Cooperative(_error) => PluginFailureKind::Cooperative,
        BuiltinCallbackError::Panicked => PluginFailureKind::Panicked,
        BuiltinCallbackError::Poisoned => PluginFailureKind::Poisoned,
        BuiltinCallbackError::CapabilityDenied(capability) => {
            PluginFailureKind::CapabilityDenied(capability)
        }
        BuiltinCallbackError::UnsupportedEvent => PluginFailureKind::UnsupportedEvent,
        _ => PluginFailureKind::AbiProtocol("future built-in adapter error".to_owned()),
    }
}

fn map_callback_error(error: CallbackError) -> PluginFailureKind {
    match error {
        CallbackError::Status(status) => map_status(status.code()),
        other => PluginFailureKind::AbiInvocation(other.to_string()),
    }
}

fn map_status(status: i32) -> PluginFailureKind {
    if status == FC_PLUGIN_PANIC.code() {
        PluginFailureKind::Panicked
    } else if status == FC_CAPABILITY_DENIED.code() {
        PluginFailureKind::AbiStatus(status)
    } else if status == FC_COMMAND_BUFFER_FULL.code() {
        PluginFailureKind::BufferFull
    } else if status == FC_ERROR.code() {
        PluginFailureKind::Cooperative
    } else {
        PluginFailureKind::AbiStatus(status)
    }
}

fn map_event_status(
    status: i32,
    kind: EventKind,
    capabilities: CapabilityManifest,
) -> PluginFailureKind {
    if status == FC_CAPABILITY_DENIED.code() {
        let required = match kind {
            EventKind::PlayerJoin
            | EventKind::PlayerLeave
            | EventKind::BlockBreak
            | EventKind::AfterBlockPlace
            | EventKind::AfterBlockBreak
            | EventKind::PlayerMove => Some(ferrumc_plugin_sdk::Capability::ReceiveEvents),
            EventKind::BlockPlaceAttempt | EventKind::BlockBreakAttempt => {
                Some(ferrumc_plugin_sdk::Capability::VetoBlockEdits)
            }
            EventKind::ChatAttempt | EventKind::InteractAttempt => {
                Some(ferrumc_plugin_sdk::Capability::VetoEvents)
            }
            EventKind::Command => Some(ferrumc_plugin_sdk::Capability::RegisterCommands),
            EventKind::Timer | _ => None,
        };
        if let Some(capability) = required.filter(|capability| !capabilities.grants(*capability)) {
            return PluginFailureKind::CapabilityDenied(capability);
        }
    }
    map_status(status)
}

const fn intersect(
    requested: CapabilityManifest,
    granted: CapabilityManifest,
) -> CapabilityManifest {
    CapabilityManifest::from_bits_truncate(requested.bits() & granted.bits())
}

#[cfg(test)]
mod tests {
    use ferrumc_plugin_sdk::Capability;

    use super::*;

    #[test]
    fn event_capability_status_is_typed_only_when_the_effective_grant_is_missing() {
        let status = FC_CAPABILITY_DENIED.code();
        assert_eq!(
            map_event_status(status, EventKind::ChatAttempt, CapabilityManifest::empty()),
            PluginFailureKind::CapabilityDenied(Capability::VetoEvents)
        );
        assert_eq!(
            map_event_status(
                status,
                EventKind::ChatAttempt,
                CapabilityManifest::empty().with(Capability::VetoEvents)
            ),
            PluginFailureKind::AbiStatus(status)
        );
    }
}
