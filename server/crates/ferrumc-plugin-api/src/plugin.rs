//! The in-process [`Plugin`] trait and its lifecycle hooks.

use crate::context::{EventContext, SetupContext, TeardownContext};
use crate::error::PluginError;
use crate::event::PluginEvent;
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

    /// Called once when the plugin is disabled.
    ///
    /// The default implementation does nothing.
    fn on_disable(&mut self, ctx: &mut TeardownContext<'_>) {
        let _ = ctx;
    }
}
