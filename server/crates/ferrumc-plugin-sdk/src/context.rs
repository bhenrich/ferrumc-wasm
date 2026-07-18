//! Capability-gated, call-scoped plugin contexts.

use crate::{
    Capability, CapabilityManifest, CommandRegistrations, Diagnostics, EventSubscriptions,
    FacadeError, HostServices, NamespacedStorage, PermissionQueries, Tick, Timers, WorldOperations,
    WorldView,
};

/// Context passed to [`Plugin::on_load`](crate::Plugin::on_load).
///
/// Load can subscribe to events, register pure-data commands, initialize
/// namespaced storage, schedule tick timers, and emit diagnostics.
pub struct LoadContext<'call> {
    capabilities: CapabilityManifest,
    services: &'call mut dyn HostServices,
}

impl<'call> LoadContext<'call> {
    /// Constructs a context from an adapter's single call-scoped backend.
    ///
    /// This constructor is intended for packaging adapters and the
    /// deterministic testhost. Plugin implementations receive contexts from a
    /// host rather than constructing them.
    #[doc(hidden)]
    pub fn new(services: &'call mut dyn HostServices) -> Self {
        let capabilities = services.capabilities();
        Self {
            capabilities,
            services,
        }
    }

    /// Returns the actual granted capabilities.
    pub const fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    /// Returns the event-subscription facade when event receipt is granted.
    pub fn events(&mut self) -> Result<EventSubscriptions<'_>, FacadeError> {
        self.require(Capability::ReceiveEvents)?;
        Ok(EventSubscriptions::new(&mut *self.services))
    }

    /// Returns the command-registration facade when registration is granted.
    pub fn commands(&mut self) -> Result<CommandRegistrations<'_>, FacadeError> {
        self.require(Capability::RegisterCommands)?;
        Ok(CommandRegistrations::new(&mut *self.services))
    }

    /// Returns namespaced storage when storage access is granted.
    pub fn storage(&mut self) -> Result<NamespacedStorage<'_>, FacadeError> {
        self.require(Capability::Storage)?;
        Ok(NamespacedStorage::new(&mut *self.services))
    }

    /// Returns deterministic tick timers.
    pub fn timers(&mut self) -> Timers<'_> {
        Timers::new(&mut *self.services)
    }

    /// Returns the bounded diagnostic facade.
    pub fn diagnostics(&mut self) -> Diagnostics<'_> {
        Diagnostics::new(&mut *self.services)
    }

    fn require(&self, capability: Capability) -> Result<(), FacadeError> {
        self.capabilities.require(capability).map_err(Into::into)
    }
}

/// Context passed to event, decision, command, and timer callbacks.
///
/// Every accessor reborrows the one backend for no longer than the returned
/// facade's lifetime. A facade therefore cannot be retained past the callback:
///
/// ```compile_fail
/// use ferrumc_plugin_sdk::{EventContext, FacadeError, WorldView};
///
/// fn retain(
///     context: &mut EventContext<'_>,
/// ) -> Result<WorldView<'static>, FacadeError> {
///     context.world()
/// }
/// ```
pub struct EventContext<'call> {
    capabilities: CapabilityManifest,
    tick: Tick,
    services: &'call mut dyn HostServices,
}

impl<'call> EventContext<'call> {
    /// Constructs an event context from a tick and adapter backend.
    ///
    /// This constructor is intended for packaging adapters and the
    /// deterministic testhost.
    #[doc(hidden)]
    pub fn new(tick: Tick, services: &'call mut dyn HostServices) -> Self {
        let capabilities = services.capabilities();
        Self {
            capabilities,
            tick,
            services,
        }
    }

    /// Returns the actual granted capabilities.
    pub const fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    /// Returns the deterministic tick associated with this callback.
    pub const fn tick(&self) -> Tick {
        self.tick
    }

    /// Returns the read-only current-world facade when world reads are granted.
    pub fn world(&mut self) -> Result<WorldView<'_>, FacadeError> {
        self.require(Capability::ReadWorld)?;
        Ok(WorldView::new(&mut *self.services))
    }

    /// Returns bounded world operations when intent submission is granted.
    pub fn operations(&mut self) -> Result<WorldOperations<'_>, FacadeError> {
        self.require(Capability::SubmitIntents)?;
        Ok(WorldOperations::new(&mut *self.services))
    }

    /// Returns read-only permission queries when permission access is granted.
    pub fn permissions(&mut self) -> Result<PermissionQueries<'_>, FacadeError> {
        self.require(Capability::ReadPermissions)?;
        Ok(PermissionQueries::new(&mut *self.services))
    }

    /// Returns namespaced storage when storage access is granted.
    pub fn storage(&mut self) -> Result<NamespacedStorage<'_>, FacadeError> {
        self.require(Capability::Storage)?;
        Ok(NamespacedStorage::new(&mut *self.services))
    }

    /// Returns deterministic tick timers.
    pub fn timers(&mut self) -> Timers<'_> {
        Timers::new(&mut *self.services)
    }

    /// Returns the bounded diagnostic facade.
    pub fn diagnostics(&mut self) -> Diagnostics<'_> {
        Diagnostics::new(&mut *self.services)
    }

    fn require(&self, capability: Capability) -> Result<(), FacadeError> {
        self.capabilities.require(capability).map_err(Into::into)
    }
}

/// Context passed to [`Plugin::on_unload`](crate::Plugin::on_unload).
///
/// Unload can flush namespaced state, cancel timers, and emit diagnostics. It
/// has no world-query, intent, permission, subscription, or registration
/// accessor.
pub struct UnloadContext<'call> {
    capabilities: CapabilityManifest,
    services: &'call mut dyn HostServices,
}

impl<'call> UnloadContext<'call> {
    /// Constructs an unload context from an adapter backend.
    ///
    /// This constructor is intended for packaging adapters and the
    /// deterministic testhost.
    #[doc(hidden)]
    pub fn new(services: &'call mut dyn HostServices) -> Self {
        let capabilities = services.capabilities();
        Self {
            capabilities,
            services,
        }
    }

    /// Returns the actual granted capabilities.
    pub const fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    /// Returns namespaced storage when storage access is granted.
    pub fn storage(&mut self) -> Result<NamespacedStorage<'_>, FacadeError> {
        self.capabilities
            .require(Capability::Storage)
            .map_err(FacadeError::from)?;
        Ok(NamespacedStorage::new(&mut *self.services))
    }

    /// Returns deterministic tick timers for cleanup.
    pub fn timers(&mut self) -> Timers<'_> {
        Timers::new(&mut *self.services)
    }

    /// Returns the bounded diagnostic facade.
    pub fn diagnostics(&mut self) -> Diagnostics<'_> {
        Diagnostics::new(&mut *self.services)
    }
}
