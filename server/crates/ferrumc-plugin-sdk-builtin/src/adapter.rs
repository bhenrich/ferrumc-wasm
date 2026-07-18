//! Type-erased factory, instance, and callback routing.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};

use ferrumc_plugin_sdk::{
    BlockDecision, Capability, CapabilityManifest, DeclarationError, Event, EventContext,
    EventDecision, EventKind, HostServices, LoadContext, Plugin, PluginDeclaration, PluginError,
    Tick, UnloadContext,
};

use crate::services::MaskedServices;
use crate::BuiltinCallbackError;

/// Successful result of routing one shared-SDK event.
///
/// Decision values remain distinct from ordinary completion so the caller can
/// fold them through the same policy used for trusted native packaging.
/// Before committing a decision callback, the caller must admit its returned
/// decision into the same bounded command stage. A full or failed admission
/// discards every staged mutating effect from that callback.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallbackOutcome {
    /// A notification, command, or timer callback completed.
    Complete,
    /// A block-placement attempt produced a block decision.
    BlockDecision(BlockDecision),
    /// A break, chat, or interaction attempt produced an event decision.
    EventDecision(EventDecision),
}

/// Type-erased constructor for one compiled-in SDK plugin type.
///
/// A factory stores only validated static declaration data and a safe
/// constructor function. It performs no global registration and has no host
/// runtime dependency.
#[derive(Clone, Copy)]
pub struct BuiltinPluginFactory {
    declaration: PluginDeclaration,
    initialize: InitializeFn,
}

type InitializeFn = fn(
    CapabilityManifest,
    &mut dyn HostServices,
) -> Result<Box<dyn ErasedPlugin>, BuiltinCallbackError>;

impl std::fmt::Debug for BuiltinPluginFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuiltinPluginFactory")
            .field("declaration", &self.declaration)
            .finish_non_exhaustive()
    }
}

impl BuiltinPluginFactory {
    /// Validates and type-erases plugin type `P`.
    pub fn new<P: Plugin>() -> Result<Self, DeclarationError> {
        P::DECLARATION.validate()?;
        Ok(Self {
            declaration: P::DECLARATION,
            initialize: initialize_plugin::<P>,
        })
    }

    /// Returns the validated plugin declaration.
    pub const fn declaration(&self) -> PluginDeclaration {
        self.declaration
    }

    /// Returns the capabilities requested by the plugin declaration.
    pub const fn requested_capabilities(&self) -> CapabilityManifest {
        self.declaration.requested_capabilities()
    }

    /// Creates and loads one owned plugin instance.
    ///
    /// `services` must provide fresh caller-owned bounded transactional staging
    /// for mutating effects. The adapter neither commits nor flushes it. The
    /// caller commits those effects only when this method returns `Ok` and
    /// discards them for every error. Reads need no commit, and diagnostics may
    /// remain as observability for a failed callback.
    ///
    /// The instance's effective capabilities are frozen to the intersection
    /// of the declaration's request and `granted`. The wrapper passed to the
    /// plugin reports that intersection even if `services` reports more.
    /// `services` must cover the full intersection or initialization returns a
    /// typed capability denial before plugin construction.
    pub fn initialize(
        &self,
        granted: CapabilityManifest,
        services: &mut dyn HostServices,
    ) -> Result<BuiltinPluginInstance, BuiltinCallbackError> {
        let capabilities = intersect_capabilities(self.requested_capabilities(), granted);
        require_backend_capabilities(capabilities, services)?;
        let plugin = (self.initialize)(capabilities, services)?;
        Ok(BuiltinPluginInstance {
            declaration: self.declaration,
            capabilities,
            plugin: Some(plugin),
        })
    }
}

/// One active, type-erased compiled-in plugin instance.
///
/// The instance exposes declaration data and shared-SDK callbacks, but no
/// concrete recovery or downcast:
///
/// ```compile_fail
/// use ferrumc_plugin_sdk_builtin::BuiltinPluginInstance;
///
/// struct ConcretePlugin;
///
/// fn recover(instance: BuiltinPluginInstance) -> ConcretePlugin {
///     instance.into_inner()
/// }
/// ```
///
/// Hosts should finish the lifecycle with [`shutdown`](Self::shutdown).
/// Dropping an active instance skips `Plugin::on_unload`; its private drop
/// guard still catches a plugin destructor panic so it cannot surprise the
/// host during ordinary cleanup. Because implicit drop has no result channel,
/// that caught destructor panic is not reportable; explicit shutdown reports
/// it as [`BuiltinCallbackError::Panicked`].
#[must_use = "a built-in plugin instance should be driven and shut down"]
pub struct BuiltinPluginInstance {
    declaration: PluginDeclaration,
    capabilities: CapabilityManifest,
    plugin: Option<Box<dyn ErasedPlugin>>,
}

impl std::fmt::Debug for BuiltinPluginInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuiltinPluginInstance")
            .field("declaration", &self.declaration)
            .field("capabilities", &self.capabilities)
            .field(
                "state",
                &if self.plugin.is_some() {
                    "active"
                } else {
                    "poisoned"
                },
            )
            .finish()
    }
}

impl BuiltinPluginInstance {
    /// Returns the plugin declaration copied from its factory.
    pub const fn declaration(&self) -> PluginDeclaration {
        self.declaration
    }

    /// Returns the frozen requested-and-granted capability intersection.
    pub const fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    /// Routes one of the shared SDK's twelve event values.
    ///
    /// `services` must provide fresh caller-owned bounded transactional staging
    /// for mutating effects.
    /// It must cover the instance's frozen effective capabilities; broader
    /// grants remain masked, while a missing grant is denied before plugin code.
    /// Commit staged mutations only for `Ok`; discard them for all errors.
    /// Reads need no commit, and diagnostics may remain as failed-callback
    /// observability. A cooperative error retains the instance. For a
    /// block-place, block-break, chat, or interaction attempt, every error also
    /// requires the caller to fail closed by denying the action without
    /// feedback. A successful decision must be admitted into the same bounded
    /// command stage before commit; a full or failed admission discards every
    /// staged mutation from the callback. A caught panic returns
    /// [`BuiltinCallbackError::Panicked`], forgets the potentially inconsistent
    /// plugin state, and makes later calls return
    /// [`BuiltinCallbackError::Poisoned`].
    pub fn handle_event(
        &mut self,
        event: &Event,
        tick: Tick,
        services: &mut dyn HostServices,
    ) -> Result<CallbackOutcome, BuiltinCallbackError> {
        if self.plugin.is_none() {
            return Err(BuiltinCallbackError::Poisoned);
        }
        require_backend_capabilities(self.capabilities, services)?;
        require_event_capability(self.capabilities, event)?;

        let result = {
            let plugin = self.plugin.as_mut().ok_or(BuiltinCallbackError::Poisoned)?;
            catch_unwind(AssertUnwindSafe(|| {
                let mut masked = MaskedServices::new(self.capabilities, services);
                plugin.handle_event(event, tick, &mut masked)
            }))
        };

        match result {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(error)) => Err(BuiltinCallbackError::Cooperative(error)),
            Err(payload) => {
                if let Some(plugin) = self.plugin.take() {
                    std::mem::forget(plugin);
                }
                discard_panic_payload(payload);
                Err(BuiltinCallbackError::Panicked)
            }
        }
    }

    /// Runs unload, destroys the plugin, and consumes the instance.
    ///
    /// The caller owns the supplied bounded transactional mutation stage and
    /// commits its effects only when this method returns `Ok`. Reads need no
    /// commit, and diagnostics may remain as failed-callback observability.
    /// Retirement still consumes the instance after a cooperative unload error
    /// or backend-capability mismatch. Panics from unload or plugin destruction
    /// become [`BuiltinCallbackError::Panicked`].
    pub fn shutdown(mut self, services: &mut dyn HostServices) -> Result<(), BuiltinCallbackError> {
        if let Err(error) = require_backend_capabilities(self.capabilities, services) {
            let Some(plugin) = self.plugin.take() else {
                return Err(BuiltinCallbackError::Poisoned);
            };
            drop_plugin(plugin)?;
            return Err(error);
        }

        let Some(mut plugin) = self.plugin.take() else {
            return Err(BuiltinCallbackError::Poisoned);
        };

        let unload = catch_unwind(AssertUnwindSafe(|| {
            let mut masked = MaskedServices::new(self.capabilities, services);
            plugin.unload(&mut masked)
        }));

        let callback_result = match unload {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(BuiltinCallbackError::Cooperative(error)),
            Err(payload) => {
                std::mem::forget(plugin);
                discard_panic_payload(payload);
                return Err(BuiltinCallbackError::Panicked);
            }
        };

        drop_plugin(plugin)?;
        callback_result
    }
}

impl Drop for BuiltinPluginInstance {
    fn drop(&mut self) {
        if let Some(plugin) = self.plugin.take() {
            let _caught = drop_plugin(plugin);
        }
    }
}

trait ErasedPlugin: Send {
    fn handle_event(
        &mut self,
        event: &Event,
        tick: Tick,
        services: &mut dyn HostServices,
    ) -> Result<CallbackOutcome, PluginError>;

    fn unload(&mut self, services: &mut dyn HostServices) -> Result<(), PluginError>;
}

struct PluginState<P> {
    plugin: P,
}

impl<P: Plugin> ErasedPlugin for PluginState<P> {
    fn handle_event(
        &mut self,
        event: &Event,
        tick: Tick,
        services: &mut dyn HostServices,
    ) -> Result<CallbackOutcome, PluginError> {
        let mut context = EventContext::new(tick, services);
        match event {
            Event::PlayerJoin(_)
            | Event::PlayerLeave(_)
            | Event::BlockBreak(_)
            | Event::AfterBlockPlace(_)
            | Event::AfterBlockBreak(_)
            | Event::PlayerMove(_) => {
                self.plugin.on_event(event, &mut context)?;
                Ok(CallbackOutcome::Complete)
            }
            Event::BlockPlaceAttempt(attempt) => self
                .plugin
                .before_block_place(attempt, &mut context)
                .map(CallbackOutcome::BlockDecision),
            Event::BlockBreakAttempt(attempt) => self
                .plugin
                .before_block_break(attempt, &mut context)
                .map(CallbackOutcome::EventDecision),
            Event::ChatAttempt(attempt) => self
                .plugin
                .before_chat(attempt, &mut context)
                .map(CallbackOutcome::EventDecision),
            Event::InteractAttempt(attempt) => self
                .plugin
                .before_interact(attempt, &mut context)
                .map(CallbackOutcome::EventDecision),
            Event::Command(invocation) => {
                self.plugin.on_command(invocation, &mut context)?;
                Ok(CallbackOutcome::Complete)
            }
            Event::Timer(timer) => {
                self.plugin.on_timer(*timer, &mut context)?;
                Ok(CallbackOutcome::Complete)
            }
            _ => Err(PluginError::failed(
                "future SDK event has no built-in callback mapping",
            )),
        }
    }

    fn unload(&mut self, services: &mut dyn HostServices) -> Result<(), PluginError> {
        let mut context = UnloadContext::new(services);
        self.plugin.on_unload(&mut context)
    }
}

fn initialize_plugin<P: Plugin>(
    capabilities: CapabilityManifest,
    services: &mut dyn HostServices,
) -> Result<Box<dyn ErasedPlugin>, BuiltinCallbackError> {
    let create = catch_unwind(AssertUnwindSafe(P::create));
    let mut plugin = match create {
        Ok(plugin) => plugin,
        Err(payload) => {
            discard_panic_payload(payload);
            return Err(BuiltinCallbackError::Panicked);
        }
    };

    let load = catch_unwind(AssertUnwindSafe(|| {
        let mut masked = MaskedServices::new(capabilities, services);
        let mut context = LoadContext::new(&mut masked);
        plugin.on_load(&mut context)
    }));

    match load {
        Ok(Ok(())) => Ok(Box::new(PluginState { plugin })),
        Ok(Err(error)) => {
            drop_concrete_plugin(plugin)?;
            Err(BuiltinCallbackError::Cooperative(error))
        }
        Err(payload) => {
            std::mem::forget(plugin);
            discard_panic_payload(payload);
            Err(BuiltinCallbackError::Panicked)
        }
    }
}

fn require_event_capability(
    capabilities: CapabilityManifest,
    event: &Event,
) -> Result<(), BuiltinCallbackError> {
    let required = match event.kind() {
        EventKind::PlayerJoin
        | EventKind::PlayerLeave
        | EventKind::BlockBreak
        | EventKind::AfterBlockPlace
        | EventKind::AfterBlockBreak
        | EventKind::PlayerMove => Some(Capability::ReceiveEvents),
        EventKind::BlockPlaceAttempt | EventKind::BlockBreakAttempt => {
            Some(Capability::VetoBlockEdits)
        }
        EventKind::ChatAttempt | EventKind::InteractAttempt => Some(Capability::VetoEvents),
        EventKind::Command => Some(Capability::RegisterCommands),
        EventKind::Timer => None,
        _ => return Err(BuiltinCallbackError::UnsupportedEvent),
    };
    if let Some(capability) = required {
        if !capabilities.grants(capability) {
            return Err(BuiltinCallbackError::CapabilityDenied(capability));
        }
    }
    Ok(())
}

const fn intersect_capabilities(
    requested: CapabilityManifest,
    granted: CapabilityManifest,
) -> CapabilityManifest {
    CapabilityManifest::from_bits_truncate(requested.bits() & granted.bits())
}

fn require_backend_capabilities(
    capabilities: CapabilityManifest,
    services: &dyn HostServices,
) -> Result<(), BuiltinCallbackError> {
    let available = services.capabilities();
    for capability in Capability::ALL {
        if capabilities.grants(capability) && !available.grants(capability) {
            return Err(BuiltinCallbackError::CapabilityDenied(capability));
        }
    }
    Ok(())
}

fn drop_concrete_plugin<P: Plugin>(plugin: P) -> Result<(), BuiltinCallbackError> {
    match catch_unwind(AssertUnwindSafe(|| drop(plugin))) {
        Ok(()) => Ok(()),
        Err(payload) => {
            discard_panic_payload(payload);
            Err(BuiltinCallbackError::Panicked)
        }
    }
}

fn drop_plugin(plugin: Box<dyn ErasedPlugin>) -> Result<(), BuiltinCallbackError> {
    match catch_unwind(AssertUnwindSafe(|| drop(plugin))) {
        Ok(()) => Ok(()),
        Err(payload) => {
            discard_panic_payload(payload);
            Err(BuiltinCallbackError::Panicked)
        }
    }
}

fn discard_panic_payload(payload: Box<dyn Any + Send>) {
    if let Err(second_payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(second_payload);
    }
}
