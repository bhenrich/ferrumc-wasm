//! The packaging-independent plugin trait.

use crate::{
    BlockDecision, BlockEvent, ChatAttempt, CommandInvocation, Event, EventContext, EventDecision,
    InteractionAttempt, LoadContext, PlaceAttempt, PluginDeclaration, PluginError, TimerId,
    UnloadContext,
};

/// Plugin logic authored once and driven through either packaging adapter.
///
/// Implementations are `Send` so a host may move an owned instance between
/// worker threads, but are not required to be `Sync`; one instance is called
/// serially. Every facade is borrowed from a synchronous callback context.
///
/// # Cooperative callback errors
///
/// Returning [`Err`](Result::Err) discards every operation staged by that
/// callback. A load error leaves the instance inactive. An error from an
/// event, command, or timer callback is recorded and the instance remains
/// eligible for later calls. An error from a decision callback fails closed:
/// the attempted action is denied without feedback, and staged operations are
/// discarded. An unload error is recorded while retirement still completes.
/// Packaging adapters must preserve these semantics. A caught unwinding panic
/// uses a separate trusted native plugin status and host fail-stop policy.
pub trait Plugin: Send + 'static {
    /// Static identity, version, and requested capabilities.
    const DECLARATION: PluginDeclaration;

    /// Creates one fresh plugin instance.
    fn create() -> Self;

    /// Runs once after the host grants capabilities and creates the instance.
    ///
    /// The default implementation performs no registration.
    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        let _ = context;
        Ok(())
    }

    /// Handles a subscribed read-only notification.
    ///
    /// Decision attempts, command invocations, and timers are routed to their
    /// dedicated methods below. The default ignores the event.
    fn on_event(
        &mut self,
        event: &Event,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        let _ = (event, context);
        Ok(())
    }

    /// Decides a pending block-placement attempt.
    ///
    /// The host calls this only when block-edit veto authority is granted.
    fn before_block_place(
        &mut self,
        attempt: &PlaceAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        let _ = (attempt, context);
        Ok(BlockDecision::Allow)
    }

    /// Decides a pending block-break attempt.
    ///
    /// The host calls this only when block-edit veto authority is granted.
    fn before_block_break(
        &mut self,
        attempt: &BlockEvent,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        let _ = (attempt, context);
        Ok(EventDecision::Allow)
    }

    /// Decides a pending chat message.
    ///
    /// The host calls this only when non-block veto authority is granted.
    fn before_chat(
        &mut self,
        attempt: &ChatAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        let _ = (attempt, context);
        Ok(EventDecision::Allow)
    }

    /// Decides a pending interaction.
    ///
    /// The host calls this only when non-block veto authority is granted.
    fn before_interact(
        &mut self,
        attempt: &InteractionAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        let _ = (attempt, context);
        Ok(EventDecision::Allow)
    }

    /// Handles an invocation routed by a registered nonzero handler ID.
    fn on_command(
        &mut self,
        invocation: &CommandInvocation,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        let _ = (invocation, context);
        Ok(())
    }

    /// Handles a deterministic timer becoming due.
    fn on_timer(
        &mut self,
        timer: TimerId,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        let _ = (timer, context);
        Ok(())
    }

    /// Runs once before the host retires a normally operating instance.
    ///
    /// A host need not invoke this after a cooperative panic status because
    /// the plugin's internal state may be inconsistent.
    fn on_unload(&mut self, context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        let _ = context;
        Ok(())
    }
}
