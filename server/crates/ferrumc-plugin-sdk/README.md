# ferrumc-plugin-sdk

`ferrumc-plugin-sdk` is the packaging-independent API for `FerrumC` plugin
authors. A plugin implements one [`Plugin`](crate::Plugin) type and can later be
packaged through either the built-in or trusted native plugin adapter.

See the canonical [plugin-authoring guide](../../../docs/plugin-authoring.md)
for lifecycle, capability, packaging, trust, and current deployment details.

The SDK exposes only call-scoped, capability-gated facades. World access is
read-only. Changes are bounded operations submitted for later validation and
application. Storage is fixed to the current plugin namespace by the host.
Timers use deterministic ticks. No facade exposes simulation shards, mutable
chunks, entity stores, sockets, database handles, a `Tokio` runtime, or a sender.

All author callbacks are synchronous. Facades borrow the callback context and
cannot be retained by safe plugin code after that callback returns.

```rust
use ferrumc_plugin_sdk::{
    Capability, CapabilityManifest, Event, EventContext, LoadContext, Plugin,
    PluginDeclaration, PluginError, PluginVersion, UnloadContext,
};

struct Greeter;

impl Plugin for Greeter {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "example-greeter",
        "Example Greeter",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty()
            .with(Capability::ReceiveEvents)
            .with(Capability::SubmitIntents),
    );

    fn create() -> Self {
        Self
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        context.events()?.subscribe(ferrumc_plugin_sdk::EventKind::PlayerJoin)?;
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &Event,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        if let Event::PlayerJoin(event) = event {
            context
                .operations()?
                .message(event.player(), "Welcome to FerrumC!")?;
        }
        Ok(())
    }

    fn on_unload(&mut self, _context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }
}
```
