//! The in-process [`Plugin`] trait and its lifecycle hooks.

use crate::context::{EventContext, SetupContext, TeardownContext};
use crate::error::PluginError;
use crate::event::{BlockBreakAttempt, BlockPlaceAttempt, PluginBlockDecision, PluginEvent};
use crate::metadata::PluginMetadata;

/// An in-process plugin: a Rust type the host owns and drives through its
/// lifecycle.
///
/// This is the v0, in-process model: the host holds the plugin as a boxed trait
/// object and calls these hooks directly. (Dynamic-library loading is a later
/// milestone.) Implementations must be [`Send`] so the host can move them
/// between simulation worker threads; they are never required to be `Sync`,
/// because the host calls them one at a time.
///
/// # Lifecycle
///
/// 1. The host reads [`Plugin::metadata`] once at registration.
/// 2. [`Plugin::on_enable`] runs once when the plugin is enabled; the plugin
///    subscribes to events and registers commands here.
/// 3. [`Plugin::on_event`] runs for each subscribed event that is dispatched.
/// 4. [`Plugin::on_disable`] runs once when the plugin is disabled.
///
/// In addition, a plugin holding the
/// [`VetoBlockEdits`](crate::Capability::VetoBlockEdits) capability participates
/// in the block-decision surface: the host calls
/// [`Plugin::before_block_place`] / [`Plugin::before_block_break`] at the intent
/// boundary (returning a [`PluginBlockDecision`]) and delivers the after-the-fact
/// [`PluginEvent::AfterBlockPlace`] / [`PluginEvent::AfterBlockBreak`]
/// notifications through [`Plugin::on_event`].
///
/// # Panics and isolation
///
/// Any hook may panic; the host catches the panic, disables the plugin, and
/// keeps running. A plugin that has panicked is never called again, so it does
/// not matter that its internal state may be inconsistent afterward.
pub trait Plugin: Send {
    /// Returns the plugin's static metadata.
    ///
    /// Called once at registration; should be cheap and side-effect-free.
    fn metadata(&self) -> PluginMetadata;

    /// Called once when the plugin is enabled.
    ///
    /// Subscribe to events via [`SetupContext::events`] and register commands
    /// via [`SetupContext::commands`]. Returning [`Err`] (or panicking) leaves
    /// the plugin disabled.
    ///
    /// The default implementation enables successfully and does nothing.
    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        let _ = ctx;
        Ok(())
    }

    /// Called for each dispatched event the plugin subscribed to.
    ///
    /// The default implementation ignores the event.
    fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
        let _ = (event, ctx);
    }

    /// Decides whether (and how) a pending block *placement* proceeds, at the
    /// intent boundary — before the edit reaches the simulation.
    ///
    /// Gated behind the [`VetoBlockEdits`](crate::Capability::VetoBlockEdits)
    /// capability: the host only consults plugins that hold it. The default
    /// implementation returns [`PluginBlockDecision::Allow`], so a plugin opts in
    /// only by overriding this.
    ///
    /// # Isolation and fail-safe
    ///
    /// This runs on the host's call path with the same panic isolation as every
    /// other hook. If it panics, the host catches it, disables the plugin, and —
    /// because this guards a placement — treats the contribution as
    /// [`Deny`](PluginBlockDecision::Deny) so a broken plugin fails safe rather
    /// than letting the edit through.
    ///
    /// UNSTABLE / dev-only: part of the in-development block-decision surface (see
    /// [`crate::event`]); its shape may change without a compatibility guarantee.
    fn before_block_place(
        &mut self,
        ev: &BlockPlaceAttempt,
        ctx: &mut EventContext<'_>,
    ) -> PluginBlockDecision {
        let _ = (ev, ctx);
        PluginBlockDecision::Allow
    }

    /// Decides whether (and how) a pending block *break* proceeds, at the intent
    /// boundary — before the edit reaches the simulation.
    ///
    /// The break-side counterpart of [`Plugin::before_block_place`]; the same
    /// capability gate, panic isolation, and fail-safe-to-`Deny` rules apply.
    ///
    /// UNSTABLE / dev-only: part of the in-development block-decision surface.
    fn before_block_break(
        &mut self,
        ev: &BlockBreakAttempt,
        ctx: &mut EventContext<'_>,
    ) -> PluginBlockDecision {
        let _ = (ev, ctx);
        PluginBlockDecision::Allow
    }

    /// Called once when the plugin is disabled.
    ///
    /// The default implementation does nothing.
    fn on_disable(&mut self, ctx: &mut TeardownContext<'_>) {
        let _ = ctx;
    }
}
