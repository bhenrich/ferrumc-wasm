//! Capability-gated contexts the host passes to plugin lifecycle hooks.
//!
//! A context bundles the facades a plugin may use during one host call, plus the
//! [`CapabilityManifest`] it was granted. Every accessor checks the relevant
//! capability first, so an under-privileged plugin is denied a facade rather
//! than silently handed one.

use crate::capability::{Capability, CapabilityManifest};
use crate::command::CommandRegistrar;
use crate::error::CapabilityError;
use crate::event::EventRegistrar;
use crate::permission::PermissionApi;
use crate::sink::CommandSink;
use crate::storage::PluginStorageApi;
use crate::world::WorldView;

/// The context passed to [`Plugin::on_enable`](crate::Plugin::on_enable).
///
/// During setup a plugin subscribes to events and registers commands (each
/// gated by a capability) and may read or seed its persistent storage. No world
/// or sink is available here: setup does not run inside a tick.
pub struct SetupContext<'a> {
    capabilities: CapabilityManifest,
    events: &'a mut EventRegistrar,
    commands: &'a mut CommandRegistrar,
    storage: &'a dyn PluginStorageApi,
}

impl<'a> SetupContext<'a> {
    /// Builds a setup context. Called by the host.
    pub fn new(
        capabilities: CapabilityManifest,
        events: &'a mut EventRegistrar,
        commands: &'a mut CommandRegistrar,
        storage: &'a dyn PluginStorageApi,
    ) -> Self {
        Self {
            capabilities,
            events,
            commands,
            storage,
        }
    }

    /// Returns the capabilities granted to this plugin.
    pub const fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    /// Returns the event registrar, if the plugin holds the
    /// [`ReceiveEvents`](Capability::ReceiveEvents) capability.
    pub fn events(&mut self) -> Result<&mut EventRegistrar, CapabilityError> {
        self.capabilities.require(Capability::ReceiveEvents)?;
        Ok(&mut *self.events)
    }

    /// Returns the command registrar, if the plugin holds the
    /// [`RegisterCommands`](Capability::RegisterCommands) capability.
    pub fn commands(&mut self) -> Result<&mut CommandRegistrar, CapabilityError> {
        self.capabilities.require(Capability::RegisterCommands)?;
        Ok(&mut *self.commands)
    }

    /// Returns the namespaced storage facade, if the plugin holds the
    /// [`Storage`](Capability::Storage) capability.
    pub fn storage(&self) -> Result<&dyn PluginStorageApi, CapabilityError> {
        self.capabilities.require(Capability::Storage)?;
        Ok(self.storage)
    }
}

/// The context passed to [`Plugin::on_event`](crate::Plugin::on_event).
///
/// During an event a plugin reads the world, submits mutation intents, queries
/// permissions, and uses its storage — each gated by a capability.
pub struct EventContext<'a> {
    capabilities: CapabilityManifest,
    world: &'a dyn WorldView,
    sink: &'a mut dyn CommandSink,
    permissions: &'a dyn PermissionApi,
    storage: &'a dyn PluginStorageApi,
}

impl<'a> EventContext<'a> {
    /// Builds an event context. Called by the host.
    pub fn new(
        capabilities: CapabilityManifest,
        world: &'a dyn WorldView,
        sink: &'a mut dyn CommandSink,
        permissions: &'a dyn PermissionApi,
        storage: &'a dyn PluginStorageApi,
    ) -> Self {
        Self {
            capabilities,
            world,
            sink,
            permissions,
            storage,
        }
    }

    /// Returns the capabilities granted to this plugin.
    pub const fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    /// Returns the read-only world view, if the plugin holds the
    /// [`ReadWorld`](Capability::ReadWorld) capability.
    pub fn world(&self) -> Result<&dyn WorldView, CapabilityError> {
        self.capabilities.require(Capability::ReadWorld)?;
        Ok(self.world)
    }

    /// Returns the mutation-intent sink, if the plugin holds the
    /// [`SubmitIntents`](Capability::SubmitIntents) capability.
    pub fn sink(&mut self) -> Result<&mut dyn CommandSink, CapabilityError> {
        self.capabilities.require(Capability::SubmitIntents)?;
        Ok(&mut *self.sink)
    }

    /// Returns the permission query facade, if the plugin holds the
    /// [`ReadPermissions`](Capability::ReadPermissions) capability.
    pub fn permissions(&self) -> Result<&dyn PermissionApi, CapabilityError> {
        self.capabilities.require(Capability::ReadPermissions)?;
        Ok(self.permissions)
    }

    /// Returns the namespaced storage facade, if the plugin holds the
    /// [`Storage`](Capability::Storage) capability.
    pub fn storage(&self) -> Result<&dyn PluginStorageApi, CapabilityError> {
        self.capabilities.require(Capability::Storage)?;
        Ok(self.storage)
    }
}

/// The context passed to [`Plugin::on_disable`](crate::Plugin::on_disable).
///
/// During teardown a plugin may flush state to its storage. No world or sink is
/// available, mirroring [`SetupContext`].
pub struct TeardownContext<'a> {
    capabilities: CapabilityManifest,
    storage: &'a dyn PluginStorageApi,
}

impl<'a> TeardownContext<'a> {
    /// Builds a teardown context. Called by the host.
    pub fn new(capabilities: CapabilityManifest, storage: &'a dyn PluginStorageApi) -> Self {
        Self {
            capabilities,
            storage,
        }
    }

    /// Returns the capabilities granted to this plugin.
    pub const fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    /// Returns the namespaced storage facade, if the plugin holds the
    /// [`Storage`](Capability::Storage) capability.
    pub fn storage(&self) -> Result<&dyn PluginStorageApi, CapabilityError> {
        self.capabilities.require(Capability::Storage)?;
        Ok(self.storage)
    }
}
