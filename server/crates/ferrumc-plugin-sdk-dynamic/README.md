# ferrumc-plugin-sdk-dynamic

`ferrumc-plugin-sdk-dynamic` packages a
[`ferrumc_plugin_sdk::Plugin`] as a trusted native `FerrumC` plugin. Plugin logic
uses the same callback contexts and capability facades as built-in packaging;
this crate translates those calls to the versioned C ABI.

The adapter uses checked little-endian binary payloads for every event, command,
request, and response. It does not serialize hot-path events as JSON. Opaque
world handles and the ABI function table stay behind one callback-scoped
services object.

ABI v1 requires `SET_BLOCK` to target a dimension handle, while the current
host gates the `DIMENSION` request behind `ReadWorld`. A dynamic plugin granted
`SubmitIntents` without `ReadWorld` therefore receives a typed unavailable
error from `set_block`; the adapter does not substitute the event's shard
handle or broaden the plugin's grant.

Invoke [`export_plugin!`] exactly once from a plugin `cdylib`:

```no_run
use ferrumc_plugin_sdk::{
    CapabilityManifest, Plugin, PluginDeclaration, PluginVersion,
};

struct Example;

impl Plugin for Example {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "example",
        "Example",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty(),
    );

    fn create() -> Self {
        Self
    }
}

ferrumc_plugin_sdk_dynamic::export_plugin!(Example);

fn main() {}
```

The plugin artifact must use `panic = "unwind"`. Each lifecycle or event
callback is wrapped with `catch_unwind`; an unwinding panic becomes
`FC_PLUGIN_PANIC` before the C boundary. This does not contain aborts, native
memory faults, undefined behavior, deadlocks, foreign exceptions, or malicious
native code; trusted native code retains process-wide authority.
