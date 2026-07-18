#![forbid(unsafe_code)]

use ferrumc_plugin_abi::{
    FcPluginDescriptorV1, FcPluginFunctionsV1, FcSemanticVersion, FcStatus, FC_OK,
};
use ferrumc_plugin_abi_sys::{
    export_plugin_v1, plugin_descriptor_v1, plugin_functions_v1, PluginBridge, PluginCall,
    PluginEvent,
};

struct SafeBridge;

static FUNCTIONS: FcPluginFunctionsV1 = plugin_functions_v1::<SafeBridge>();
static DESCRIPTOR: FcPluginDescriptorV1 = plugin_descriptor_v1::<SafeBridge>();

impl PluginBridge for SafeBridge {
    type Instance = ();

    const ID: &'static str = "safe.export";
    const NAME: &'static str = "Safe Export";
    const TARGET: &'static str = "aarch64-unknown-linux-gnu";
    const VERSION: FcSemanticVersion = FcSemanticVersion::new(1, 0, 0);
    const REQUESTED_CAPABILITIES: u64 = 0;

    fn functions() -> &'static FcPluginFunctionsV1 {
        &FUNCTIONS
    }

    fn descriptor() -> &'static FcPluginDescriptorV1 {
        &DESCRIPTOR
    }

    fn initialize(
        _call: &mut PluginCall<'_>,
        _granted_capabilities: u64,
    ) -> Result<Self::Instance, FcStatus> {
        Ok(())
    }

    fn on_event(
        _instance: &mut Self::Instance,
        _call: &mut PluginCall<'_>,
        _event: PluginEvent<'_>,
    ) -> FcStatus {
        FC_OK
    }

    fn shutdown(_instance: Self::Instance, _call: &mut PluginCall<'_>) -> FcStatus {
        FC_OK
    }
}

export_plugin_v1!(SafeBridge);

#[test]
fn downstream_forbid_unsafe_crate_exports_the_exact_bootstrap() {
    assert_eq!(ferrumc_plugin_entry_v1(), std::ptr::from_ref(&DESCRIPTOR));
}
