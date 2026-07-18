# ferrumc-plugin-sdk-builtin

`ferrumc-plugin-sdk-builtin` packages a
[`ferrumc_plugin_sdk::Plugin`] as a compiled-in `FerrumC` plugin. It type-erases
the concrete plugin behind a factory and an owned instance while preserving the
shared SDK contexts, capability facades, event routes, and bounded operations.

The adapter is host-independent. It does not register itself globally and does
not know how a server stores or applies commands. For each lifecycle or event
call, the caller supplies a fresh [`ferrumc_plugin_sdk::HostServices`] backend
with bounded transactional staging for mutating effects. The caller commits
subscriptions, registrations, world operations, storage writes, timer changes,
and decision admission only when the adapter returns `Ok`, and discards those
effects for every error, including a cooperative plugin error or caught panic.
Reads are not commit effects, and diagnostics may remain as observability for a
failed callback.

For a block-place, block-break, chat, or interaction decision callback, every
error is fail-closed: the caller denies the attempted action without feedback
in addition to discarding its staged mutating effects. A successful decision
must itself be admitted into the same bounded command stage before commit. If
that admission is full or fails, the caller discards every mutating effect from
the callback.

The effective grant is the plugin's requested capabilities intersected with
the host's explicit grant. Every fresh callback backend must cover that frozen
grant. A backend missing one of those capabilities is rejected before plugin
code runs, while extra backend capabilities remain hidden from the plugin.

```rust
use ferrumc_plugin_sdk::{
    CapabilityManifest, HostServices, Plugin, PluginDeclaration, PluginVersion,
};
use ferrumc_plugin_sdk_builtin::BuiltinPluginFactory;

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

fn initialize(
    services: &mut dyn HostServices,
) -> Result<(), Box<dyn std::error::Error>> {
    let factory = BuiltinPluginFactory::new::<Example>()?;
    let instance = factory.initialize(CapabilityManifest::empty(), services)?;
    instance.shutdown(services)?;
    Ok(())
}
```

`BuiltinPluginInstance` exposes no downcast, concrete recovery, raw simulation
state, mutable chunk, socket, database handle, runtime handle, or command
sender. A caught event panic reports
[`BuiltinCallbackError::Panicked`](crate::BuiltinCallbackError::Panicked),
forgets the potentially inconsistent plugin state, and makes later calls report
[`BuiltinCallbackError::Poisoned`](crate::BuiltinCallbackError::Poisoned).
Explicit shutdown reports a plugin destructor panic as
[`BuiltinCallbackError::Panicked`](crate::BuiltinCallbackError::Panicked). An
instance dropped without shutdown has no result channel, so its private guard
catches but cannot report a destructor panic; hosts should always call
`shutdown`.
