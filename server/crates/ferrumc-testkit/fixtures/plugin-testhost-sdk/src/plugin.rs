//! Shared plugin logic compiled through both plugin packaging adapters.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use ferrumc_plugin_sdk::{
    BlockDecision, BlockEvent, BlockPos, CapabilityManifest, ChatAttempt, ChunkPos,
    CommandDefinition, CommandInvocation, CommandNode, CommandNodeKind, DiagnosticLevel, Event,
    EventContext, EventDecision, EventKind, Feedback, HandlerId, InteractionAttempt, LoadContext,
    PermissionNode, PlaceAttempt, Plugin, PluginDeclaration, PluginError, PluginVersion, TimerId,
    UnloadContext, Vec3,
};

/// Command handler registered and exercised by the fixture.
pub const FIXTURE_HANDLER_RAW: u64 = 41;

/// Timer exercised by the fixture.
pub const FIXTURE_TIMER_RAW: u64 = 17;

/// Placement state that fills a twelve-slot callback stage before its decision.
pub const CAPACITY_TRIGGER_STATE: u32 = 777;

/// Placement state that selects an allow decision.
pub const DECISION_ALLOW_STATE: u32 = 6;

/// Placement state that selects a deny decision.
pub const DECISION_DENY_STATE: u32 = 7;

/// Event position that emits observability without changing semantic state.
pub const DIAGNOSTIC_ONLY_POS: BlockPos = BlockPos::new(i32::MIN, 0, i32::MAX);

/// A deterministic SDK plugin that exercises every shared facade.
pub struct TesthostFixturePlugin;

impl TesthostFixturePlugin {
    fn handler() -> Result<HandlerId, PluginError> {
        HandlerId::new(FIXTURE_HANDLER_RAW)
            .ok_or_else(|| PluginError::failed("fixture handler must be nonzero"))
    }

    fn timer() -> Result<TimerId, PluginError> {
        TimerId::new(FIXTURE_TIMER_RAW)
            .ok_or_else(|| PluginError::failed("fixture timer must be nonzero"))
    }

    fn permission() -> Result<PermissionNode, PluginError> {
        PermissionNode::parse("ferrumc.fixture.allowed")
            .map_err(|error| PluginError::failed(error.to_string()))
    }
}

impl Plugin for TesthostFixturePlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "testhost-fixture",
        "Testhost Fixture",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::all(),
    );

    fn create() -> Self {
        Self
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        {
            let mut events = context.events()?;
            for kind in [
                EventKind::PlayerJoin,
                EventKind::PlayerLeave,
                EventKind::BlockBreak,
                EventKind::AfterBlockPlace,
                EventKind::AfterBlockBreak,
                EventKind::PlayerMove,
            ] {
                events.subscribe(kind)?;
            }
        }

        let command = CommandDefinition::new(vec![CommandNode::new(
            None,
            CommandNodeKind::Literal,
            "fixture",
        )?
        .with_handler(Self::handler()?)])?;
        context.commands()?.register(&command)?;

        {
            let mut storage = context.storage()?;
            storage.put("boot", b"ready")?;
            storage.delete("obsolete")?;
        }
        {
            let mut timers = context.timers();
            timers.schedule(Self::timer()?, 4)?;
            let unused = TimerId::new(18)
                .ok_or_else(|| PluginError::failed("fixture timer must be nonzero"))?;
            timers.cancel(unused)?;
        }
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "fixture loaded")?;
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &Event,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        match event {
            Event::PlayerJoin(event) => {
                let loaded = {
                    let mut world = context.world()?;
                    let loaded = world.is_chunk_loaded(ChunkPos::new(0, 0))?;
                    let block = world.block_state_id(BlockPos::new(2, 70, -3))?;
                    let position = world.player_position(event.player())?;
                    (loaded, block, position)
                };
                let allowed = context
                    .permissions()?
                    .has_permission(event.player(), &Self::permission()?)?;
                let block = loaded
                    .1
                    .map_or_else(|| "none".to_owned(), |state| state.to_string());
                let position = loaded.2.map_or_else(
                    || "none".to_owned(),
                    |position| format!("{},{},{}", position.x, position.y, position.z),
                );
                context.operations()?.message(
                    event.player(),
                    format!(
                        "join loaded={} block={block} position={position} allowed={allowed}",
                        loaded.0
                    ),
                )?;
            }
            Event::PlayerLeave(event) => {
                context
                    .operations()?
                    .teleport(event.player(), Vec3::new(4.5, 80.0, -8.25))?;
            }
            Event::BlockBreak(event) => {
                context.operations()?.set_block(event.pos(), 0)?;
            }
            Event::AfterBlockPlace(event) => {
                context
                    .storage()?
                    .put("placed", &event.block_state_id().to_le_bytes())?;
            }
            Event::AfterBlockBreak(event) => {
                if event.pos() == DIAGNOSTIC_ONLY_POS {
                    context
                        .diagnostics()
                        .emit(DiagnosticLevel::Trace, "diagnostic-only event")?;
                    return Ok(());
                }
                let stored = context.storage()?.get("boot")?;
                let stored = match stored.as_deref() {
                    Some(b"ready") => "ready",
                    Some(_) => "other",
                    None => "none",
                };
                context
                    .operations()?
                    .message(event.player(), format!("after-break stored={stored}"))?;
            }
            Event::PlayerMove(event) => {
                let keys = context.storage()?.keys()?;
                context
                    .operations()?
                    .message(event.player(), format!("move keys={}", keys.join(",")))?;
            }
            _ => {}
        }
        Ok(())
    }

    fn before_block_place(
        &mut self,
        attempt: &PlaceAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        let previous = context.world()?.block_state_id(attempt.pos())?;
        let previous =
            previous.map_or_else(|| "none".to_owned(), |block_state| block_state.to_string());
        context
            .operations()?
            .set_block(attempt.pos(), attempt.block_state_id().saturating_add(1))?;
        context
            .operations()?
            .message(attempt.player(), format!("place previous={previous}"))?;
        if attempt.block_state_id() == CAPACITY_TRIGGER_STATE {
            for index in 0..10 {
                context
                    .operations()?
                    .message(attempt.player(), format!("capacity filler {index}"))?;
            }
        }
        if attempt.block_state_id() == u32::MAX {
            return Err(PluginError::failed("requested fixture rollback"));
        }
        match attempt.block_state_id() {
            DECISION_ALLOW_STATE => Ok(BlockDecision::Allow),
            DECISION_DENY_STATE => Ok(BlockDecision::Deny(Some(Feedback::new(
                "fixture denied placement",
            )?))),
            _ => Ok(BlockDecision::Replace(88)),
        }
    }

    fn before_block_break(
        &mut self,
        attempt: &BlockEvent,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        context
            .operations()?
            .message(attempt.player(), "break inspected")?;
        Ok(EventDecision::Deny(Some(Feedback::new(
            "fixture denied break",
        )?)))
    }

    fn before_chat(
        &mut self,
        attempt: &ChatAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        context
            .operations()?
            .message(attempt.player(), format!("chat: {}", attempt.message()))?;
        if attempt.message() == "rollback" {
            context.storage()?.put("rolled-back", b"must-not-commit")?;
            context
                .diagnostics()
                .emit(DiagnosticLevel::Warn, "rollback diagnostic")?;
            return Err(PluginError::failed("requested fixture rollback"));
        }
        if attempt.message() == "panic" {
            context
                .diagnostics()
                .emit(DiagnosticLevel::Error, "panic diagnostic")?;
            panic!("requested fixture panic");
        }
        Ok(EventDecision::Allow)
    }

    fn before_interact(
        &mut self,
        attempt: &InteractionAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        context
            .operations()?
            .message(attempt.player(), "interaction inspected")?;
        Ok(EventDecision::Deny(None))
    }

    fn on_command(
        &mut self,
        invocation: &CommandInvocation,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        if invocation.handler() != Self::handler()? {
            return Err(PluginError::failed("unexpected fixture handler"));
        }
        context.storage()?.put("command", b"invoked")?;
        context
            .operations()?
            .message(invocation.player(), "command invoked")?;
        Ok(())
    }

    fn on_timer(
        &mut self,
        timer: TimerId,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        if timer != Self::timer()? {
            return Err(PluginError::failed("unexpected fixture timer"));
        }
        context.timers().cancel(timer)?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Debug, "fixture timer fired")?;
        Ok(())
    }

    fn on_unload(&mut self, context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        context.storage()?.delete("boot")?;
        context.timers().cancel(Self::timer()?)?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "fixture unloaded")?;
        Ok(())
    }
}
