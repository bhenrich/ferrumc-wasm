use ferrumc_plugin_abi::FcStatus;
use ferrumc_plugin_abi_sys::{
    CallbackError, HostServices, InvocationLimits, LoadedAbiPlugin, OwnedEvent,
    OwnedPluginMetadata, PluginInstance,
};

use crate::manifest::PluginManifest;

/// A validated native plugin that has not been initialized.
///
/// The native library is already permanently resident. This value is a safe,
/// reusable factory for independent instances; it exposes neither callback
/// pointers nor a platform library handle.
pub struct LoadedPlugin {
    manifest: PluginManifest,
    boundary: LoadedAbiPlugin,
}

impl LoadedPlugin {
    pub(crate) fn new(manifest: PluginManifest, boundary: LoadedAbiPlugin) -> Self {
        Self { manifest, boundary }
    }

    /// Returns the fully validated manifest.
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Returns host-owned metadata copied from the native descriptor.
    pub const fn metadata(&self) -> &OwnedPluginMetadata {
        self.boundary.metadata()
    }

    /// Initializes a fresh instance with the manifest's validated capabilities.
    pub fn initialize(
        &self,
        services: &mut dyn HostServices,
    ) -> Result<ActivePlugin, CallbackError> {
        self.initialize_with_limits(InvocationLimits::DEFAULT, services)
    }

    /// Initializes a fresh instance with explicit bounded callback limits.
    pub fn initialize_with_limits(
        &self,
        limits: InvocationLimits,
        services: &mut dyn HostServices,
    ) -> Result<ActivePlugin, CallbackError> {
        let instance = self.boundary.initialize_with_limits(
            self.manifest.capabilities().bits(),
            limits,
            services,
        )?;
        Ok(ActivePlugin {
            manifest: self.manifest.clone(),
            instance,
        })
    }
}

/// One initialized native plugin instance.
///
/// Event callbacks are synchronous. Shutdown consumes this value, ensuring the
/// instance's opaque handle cannot be used afterward.
pub struct ActivePlugin {
    manifest: PluginManifest,
    instance: PluginInstance,
}

impl ActivePlugin {
    /// Returns the validated manifest for this instance.
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Returns host-owned metadata copied from the native descriptor.
    pub const fn metadata(&self) -> &OwnedPluginMetadata {
        self.instance.metadata()
    }

    /// Invokes the plugin for one owned event.
    pub fn on_event(
        &mut self,
        event: &OwnedEvent,
        services: &mut dyn HostServices,
    ) -> Result<FcStatus, CallbackError> {
        self.instance.on_event(event, services)
    }

    /// Shuts this instance down exactly once.
    pub fn shutdown(self, services: &mut dyn HostServices) -> Result<FcStatus, CallbackError> {
        self.instance.shutdown(services)
    }
}

/// A deterministically ordered set of validated loaded plugins.
pub struct LoadedPlugins {
    plugins: Vec<LoadedPlugin>,
}

impl LoadedPlugins {
    pub(crate) fn new(plugins: Vec<LoadedPlugin>) -> Self {
        Self { plugins }
    }

    /// Returns the number of loaded plugins.
    pub const fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Returns whether no plugin manifest was discovered.
    pub const fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Iterates in deterministic plugin-ID order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &LoadedPlugin> {
        self.plugins.iter()
    }

    /// Returns the plugin with the exact identifier, if loaded.
    pub fn get(&self, id: &str) -> Option<&LoadedPlugin> {
        self.plugins
            .binary_search_by(|plugin| plugin.manifest().id().cmp(id))
            .ok()
            .map(|index| &self.plugins[index])
    }

    /// Consumes the set in deterministic plugin-ID order.
    pub fn into_plugins(self) -> Vec<LoadedPlugin> {
        self.plugins
    }
}
