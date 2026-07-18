#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Trusted native packaging for the shared `FerrumC` plugin SDK.

mod bridge;
mod codec;
mod panic;
mod services;

/// Exact Cargo target triple published in ABI v1 plugin metadata.
///
/// This includes `aarch64-unknown-linux-gnu` when cross-compiling for
/// `FerrumC`'s reviewed 64-bit Linux `AArch64` ABI target.
pub const TARGET_TRIPLE: &str = env!("FERRUMC_PLUGIN_TARGET");

#[doc(hidden)]
#[macro_export]
macro_rules! __ferrumc_export_plugin_impl {
    ($plugin:ty) => {
        #[doc(hidden)]
        mod __ferrumc_plugin_sdk_dynamic_export {
            type PluginType = $plugin;

            #[cfg(not(panic = "unwind"))]
            compile_error!("FerrumC dynamic plugins require panic = \"unwind\"");

            const _: [(); 1] = [(); $crate::__private::declaration_array_len::<PluginType>()];

            struct Bridge;

            static FUNCTIONS: $crate::__private::FcPluginFunctionsV1 =
                $crate::__private::plugin_functions_v1::<Bridge>();
            static DESCRIPTOR: $crate::__private::FcPluginDescriptorV1 =
                $crate::__private::plugin_descriptor_v1::<Bridge>();

            impl $crate::__private::PluginBridge for Bridge {
                type Instance = $crate::__private::DynamicInstance<PluginType>;

                const ID: &'static str =
                    <PluginType as $crate::__private::Plugin>::DECLARATION.id();
                const NAME: &'static str =
                    <PluginType as $crate::__private::Plugin>::DECLARATION.name();
                const TARGET: &'static str = $crate::TARGET_TRIPLE;
                const VERSION: $crate::__private::FcSemanticVersion =
                    $crate::__private::plugin_version::<PluginType>();
                const REQUESTED_CAPABILITIES: u64 =
                    $crate::__private::requested_capabilities::<PluginType>();

                fn functions() -> &'static $crate::__private::FcPluginFunctionsV1 {
                    &FUNCTIONS
                }

                fn descriptor() -> &'static $crate::__private::FcPluginDescriptorV1 {
                    &DESCRIPTOR
                }

                fn initialize(
                    call: &mut $crate::__private::PluginCall<'_>,
                    granted_capabilities: u64,
                ) -> Result<Self::Instance, $crate::__private::FcStatus> {
                    $crate::__private::initialize::<PluginType>(call, granted_capabilities)
                }

                fn on_event(
                    instance: &mut Self::Instance,
                    call: &mut $crate::__private::PluginCall<'_>,
                    event: $crate::__private::PluginEvent<'_>,
                ) -> $crate::__private::FcStatus {
                    $crate::__private::on_event::<PluginType>(instance, call, event)
                }

                fn shutdown(
                    instance: Self::Instance,
                    call: &mut $crate::__private::PluginCall<'_>,
                ) -> $crate::__private::FcStatus {
                    $crate::__private::shutdown::<PluginType>(instance, call)
                }
            }

            $crate::__private::export_plugin_v1!(Bridge);
        }
    };
}

/// Exports one packaging-independent SDK plugin through `FerrumC`'s ABI v1.
///
/// Invoke this macro exactly once at the root of a plugin crate built as a
/// `cdylib`. The plugin crate and its final binary must use unwinding panics so
/// the bridge can translate an author callback panic to `FC_PLUGIN_PANIC`.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ident) => {
        $crate::__ferrumc_export_plugin_impl!(super::$plugin);
    };
    ($plugin:path) => {
        $crate::__ferrumc_export_plugin_impl!($plugin);
    };
}

/// Implementation exports consumed by [`export_plugin!`].
#[doc(hidden)]
pub mod __private {
    pub use crate::bridge::{
        declaration_array_len, initialize, on_event, plugin_version, requested_capabilities,
        shutdown, DynamicInstance,
    };
    pub use ferrumc_plugin_abi::{
        FcPluginDescriptorV1, FcPluginFunctionsV1, FcSemanticVersion, FcStatus,
    };
    pub use ferrumc_plugin_abi_sys::{
        export_plugin_v1, plugin_descriptor_v1, plugin_functions_v1, PluginBridge, PluginCall,
        PluginEvent,
    };
    pub use ferrumc_plugin_sdk::Plugin;
}
