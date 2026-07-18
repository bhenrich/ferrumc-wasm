#![forbid(unsafe_code)]

use ferrumc_plugin_sdk::{
    BlockDecision, BlockEvent, BlockPos, Capability, CapabilityManifest, ChatAttempt, ChunkPos,
    CommandArgumentValue, CommandDefinition, CommandInvocation, CommandNode, CommandNodeKind,
    DiagnosticLevel, Event, EventContext, EventDecision, EventKind, Feedback, HandlerId,
    IntegerBounds, InteractionAttempt, LoadContext, PermissionNode, PlaceAttempt, Plugin,
    PluginDeclaration, PluginError, PluginVersion, TimerId, UnloadContext, Vec3,
};

const PANIC_TIMER: u64 = 99;
const PANIC_PAYLOAD_DROP_TIMER: u64 = 100;

struct FixturePlugin;

struct PanicPayload;

impl Drop for PanicPayload {
    fn drop(&mut self) {
        panic!("induced panic-payload drop");
    }
}

impl Drop for FixturePlugin {
    fn drop(&mut self) {
        #[cfg(feature = "panic-drop")]
        panic!("induced fixture drop unwind");
    }
}

impl FixturePlugin {
    fn timer(raw: u64) -> Result<TimerId, PluginError> {
        TimerId::new(raw).ok_or_else(|| PluginError::failed("fixture timer id must be nonzero"))
    }

    fn handler(raw: u64) -> Result<HandlerId, PluginError> {
        HandlerId::new(raw).ok_or_else(|| PluginError::failed("fixture handler id must be nonzero"))
    }

    fn permission() -> Result<PermissionNode, PluginError> {
        PermissionNode::parse("ferrumc.fixture.use")
            .map_err(|error| PluginError::failed(error.to_string()))
    }
}

impl Plugin for FixturePlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "sdk-dynamic-fixture",
        "SDK Dynamic Fixture",
        PluginVersion::new(1, 2, 3),
        CapabilityManifest::all(),
    );

    fn create() -> Self {
        #[cfg(feature = "panic-create")]
        panic!("induced fixture create unwind");

        Self
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        #[cfg(feature = "panic-load")]
        panic!("induced fixture load unwind");

        if context.capabilities() != CapabilityManifest::all() {
            context
                .diagnostics()
                .emit(DiagnosticLevel::Info, "fixture minimally loaded")?;
            return Ok(());
        }

        context.events()?.subscribe(EventKind::PlayerJoin)?;

        let root = CommandNode::new(None, CommandNodeKind::Literal, "sdkfixture")?
            .with_handler(Self::handler(7)?)
            .with_required_level(2)?;
        let word = CommandNode::new(Some(0), CommandNodeKind::Word, "target")?;
        let greedy = CommandNode::new(Some(1), CommandNodeKind::GreedyText, "message")?
            .with_required_permission(Self::permission()?);
        let integer = CommandNode::new(
            Some(2),
            CommandNodeKind::Integer(IntegerBounds::new(-4, 9)?),
            "count",
        )?
        .with_handler(Self::handler(8)?);
        context
            .commands()?
            .register(&CommandDefinition::new(vec![root, word, greedy, integer])?)?;

        {
            let mut storage = context.storage()?;
            storage.put("boot", b"binary-value")?;
            storage.delete("stale")?;
        }

        {
            let mut timers = context.timers();
            timers.schedule(Self::timer(1)?, 20)?;
            timers.cancel(Self::timer(2)?)?;
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
                {
                    let mut world = context.world()?;
                    let _loaded = world.is_chunk_loaded(ChunkPos::new(-3, 9))?;
                }
                context
                    .operations()?
                    .message(event.player(), "binary hello")?;
            }
            Event::PlayerLeave(event) => {
                {
                    let mut world = context.world()?;
                    let _block = world.block_state_id(BlockPos::new(4, 5, 6))?;
                }
                context
                    .operations()?
                    .teleport(event.player(), Vec3::new(1.25, 64.0, -2.5))?;
            }
            Event::BlockBreak(event) => {
                if event.pos().x() == i32::MIN {
                    panic!("induced ordinary-event unwind");
                }
                if context.capabilities().grants(Capability::ReadWorld) {
                    let mut world = context.world()?;
                    let _position = world.player_position(event.player())?;
                }
                context.operations()?.set_block(event.pos(), 0x1122_3344)?;
            }
            Event::AfterBlockPlace(event) => {
                let permission = Self::permission()?;
                let _resolution = context
                    .permissions()?
                    .resolve(event.player(), &permission)?;
            }
            Event::AfterBlockBreak(_) => {
                let _value = context.storage()?.get("fixture-key")?;
            }
            Event::PlayerMove(_) => {
                let _keys = context.storage()?.keys()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn before_block_place(
        &mut self,
        attempt: &PlaceAttempt,
        _context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        if attempt.block_state_id() == u32::MAX {
            panic!("induced decision unwind");
        }
        Ok(BlockDecision::Replace(0x5566_7788))
    }

    fn before_block_break(
        &mut self,
        _attempt: &BlockEvent,
        _context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        Ok(EventDecision::Deny(Some(Feedback::new("break denied")?)))
    }

    fn before_chat(
        &mut self,
        _attempt: &ChatAttempt,
        _context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        Ok(EventDecision::Allow)
    }

    fn before_interact(
        &mut self,
        _attempt: &InteractionAttempt,
        _context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        Ok(EventDecision::Deny(None))
    }

    fn on_command(
        &mut self,
        invocation: &CommandInvocation,
        _context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        if invocation.handler().get() == PANIC_TIMER {
            panic!("induced command unwind");
        }
        if !invocation.arguments().is_empty() {
            let valid = invocation.arguments().len() == 2
                && invocation.arguments()[0].name() == "target"
                && matches!(
                    invocation.arguments()[0].value(),
                    CommandArgumentValue::Text(value) if value == "spawn"
                )
                && invocation.arguments()[1].name() == "count"
                && matches!(
                    invocation.arguments()[1].value(),
                    CommandArgumentValue::Integer(-17)
                );
            if !valid {
                return Err(PluginError::failed(
                    "typed command arguments did not round-trip",
                ));
            }
        }
        Ok(())
    }

    fn on_timer(
        &mut self,
        timer: TimerId,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        if timer.get() == PANIC_TIMER {
            panic!("induced fixture unwind");
        }
        if timer.get() == PANIC_PAYLOAD_DROP_TIMER {
            std::panic::panic_any(PanicPayload);
        }
        context
            .diagnostics()
            .emit(DiagnosticLevel::Debug, "fixture timer")?;
        Ok(())
    }

    fn on_unload(&mut self, context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        #[cfg(feature = "panic-unload")]
        panic!("induced fixture unload unwind");

        if context.capabilities().grants(Capability::Storage) {
            context.storage()?.delete("boot")?;
        }
        context.timers().cancel(Self::timer(1)?)?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "fixture unloaded")?;
        Ok(())
    }
}

ferrumc_plugin_sdk_dynamic::export_plugin!(crate::FixturePlugin);
