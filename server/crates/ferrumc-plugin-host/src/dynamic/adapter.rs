//! The bridge type that makes a dynamically-loaded plugin look like an ordinary
//! in-process [`Plugin`].
//!
//! Once a library is loaded and its vtable validated, this compatibility
//! adapter registers and enables it through [`PluginHost`](crate::PluginHost).
//! Calls that return enter ordinary lifecycle/status and budget bookkeeping;
//! the host cannot recover an abort or an unwind crossing the C boundary.
//!
//! There is deliberately **no `unsafe` here**: the only unsafe work (opening the
//! library and reading the vtable) happened in [`super::ffi`]; calling a
//! validated `extern "C"` function pointer is a safe operation. The adapter just
//! holds the [`Library`] alive and the two lifecycle function pointers.

use libloading::Library;

use ferrumc_plugin_api::abi::{PluginInitFn, PluginShutdownFn, ABI_VERSION, STATUS_OK};
use ferrumc_plugin_api::{Plugin, PluginError, PluginMetadata, SetupContext, TeardownContext};

/// A dynamically-loaded plugin presented to the host as a [`Plugin`].
pub(crate) struct LoadedPlugin {
    /// Metadata copied out of the vtable at load time (never re-read across the
    /// ABI, so it cannot fail or panic later).
    metadata: PluginMetadata,
    /// The plugin's `extern "C"` init function.
    init: PluginInitFn,
    /// The plugin's `extern "C"` shutdown function.
    shutdown: PluginShutdownFn,
    /// Whether `init` has reported success and `shutdown` is therefore owed.
    initialized: bool,
    /// The loaded library image. Keeps the code (which the function pointers
    /// point into) mapped for as long as this adapter lives. Declared last so
    /// that field drop order runs it after everything else; our own [`Drop`]
    /// impl still runs first, while the library is guaranteed loaded.
    _library: Library,
}

impl LoadedPlugin {
    /// Assembles an adapter from the parts [`super::ffi`] extracted from a
    /// validated vtable.
    pub(crate) fn new(
        library: Library,
        metadata: PluginMetadata,
        init: PluginInitFn,
        shutdown: PluginShutdownFn,
    ) -> Self {
        Self {
            metadata,
            init,
            shutdown,
            initialized: false,
            _library: library,
        }
    }

    /// Calls the plugin's shutdown hook exactly once, if init had succeeded.
    fn run_shutdown(&mut self) {
        if self.initialized {
            // Calling a validated `extern "C"` function pointer is safe; the
            // plugin is responsible for not unwinding across the boundary.
            (self.shutdown)();
            self.initialized = false;
        }
    }
}

impl Plugin for LoadedPlugin {
    fn metadata(&self) -> PluginMetadata {
        self.metadata.clone()
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        // Tell the plugin which ABI was negotiated and which capabilities the
        // host actually granted (it may be fewer than it requested).
        let granted = ctx.capabilities().bits();
        let status = (self.init)(ABI_VERSION, granted);
        if status == STATUS_OK {
            self.initialized = true;
            Ok(())
        } else {
            Err(PluginError::setup(format!(
                "dynamic plugin init returned status {status}"
            )))
        }
    }

    fn on_disable(&mut self, _ctx: &mut TeardownContext<'_>) {
        self.run_shutdown();
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        // Last-resort teardown: if the plugin was enabled but never explicitly
        // disabled, still give it a chance to clean up before the library
        // unloads. `run_shutdown` is idempotent via the `initialized` flag.
        self.run_shutdown();
    }
}
