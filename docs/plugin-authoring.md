# FerrumC plugin authoring

FerrumC plugins implement one packaging-independent Rust API and can be adapted
for a compiled-in build or a trusted native plugin library. The shared API is
[`ferrumc-plugin-sdk`](../server/crates/ferrumc-plugin-sdk/README.md); the
complete two-mode example is
[`ferrumc-plugin-region-guard`](../server/plugins/ferrumc-plugin-region-guard/README.md).

> **Deployment status:** the SDK, both packaging adapters, the native bundle
> validator, and the deterministic testhost are implemented. The shipping
> `ferrumc-app` does not yet provide a supported install-and-enable path for an
> SDK plugin. Its configured `plugins_dir` still uses the older lifecycle-only
> loader with a temporary host, does not enable the loaded adapters, and does
> not deliver gameplay callbacks to them. Copying an SDK library into that
> directory will not make it a live gameplay plugin.

## Quick start

From the repository root, run the parity regression and build each packaging
mode:

```bash
cd server
cargo test -p ferrumc-plugin-region-guard --test region_guard \
  region_guard_identical_digest_builtin_vs_dynamic -- --exact
cargo build -p ferrumc-plugin-region-guard --lib
cargo build --locked -p ferrumc-plugin-region-guard --lib --release \
  --no-default-features --features dynamic
```

The regression builds a real platform library, replays the same six-event log
through both adapters, and requires identical effects, final state, and digest.
It proves adapter parity in the deterministic testhost; it is not a shipping
server installation test.

The first build selects the example's default `builtin` feature. The final
build selects only `dynamic` and emits the target-specific library under
`target/release/`.

## Author one plugin

Keep domain logic in a crate that depends on `ferrumc-plugin-sdk`. Make both
packaging crates optional so selecting a build mode does not change the plugin
implementation:

```toml
[lib]
crate-type = ["rlib", "cdylib"]

[features]
default = ["builtin"]
builtin = ["dep:ferrumc-plugin-sdk-builtin"]
dynamic = ["dep:ferrumc-plugin-sdk-dynamic"]

[dependencies]
ferrumc-plugin-sdk.workspace = true
ferrumc-plugin-sdk-builtin = { workspace = true, optional = true }
ferrumc-plugin-sdk-dynamic = { workspace = true, optional = true }
```

Implement `Plugin`, declare only the capabilities the callbacks need, and put
the packaging glue behind features:

```rust
#![forbid(unsafe_code)]

use ferrumc_plugin_sdk::{
    Capability, CapabilityManifest, Event, EventContext, EventKind, LoadContext,
    Plugin, PluginDeclaration, PluginError, PluginVersion,
};

pub struct JoinGreeter;

impl Plugin for JoinGreeter {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "join-greeter",
        "Join Greeter",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty()
            .with(Capability::ReceiveEvents)
            .with(Capability::SubmitIntents),
    );

    fn create() -> Self {
        Self
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        context.events()?.subscribe(EventKind::PlayerJoin)?;
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &Event,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        if let Event::PlayerJoin(join) = event {
            context
                .operations()?
                .message(join.player(), "Welcome to FerrumC!")?;
        }
        Ok(())
    }
}

#[cfg(feature = "builtin")]
pub fn builtin_factory() -> Result<
    ferrumc_plugin_sdk_builtin::BuiltinPluginFactory,
    ferrumc_plugin_sdk::DeclarationError,
> {
    ferrumc_plugin_sdk_builtin::BuiltinPluginFactory::new::<JoinGreeter>()
}

#[cfg(feature = "dynamic")]
ferrumc_plugin_sdk_dynamic::export_plugin!(crate::JoinGreeter);
```

Call `export_plugin!` exactly once in a dynamic artifact. The macro provides the
audited ABI entrypoint, so ordinary plugin code needs no `unsafe`.

## Lifecycle and callback outcomes

All callbacks are synchronous. A context lends its facades only for that call;
safe Rust cannot retain one for later use or move it into background work.
Timers use deterministic tick delays rather than wall-clock scheduling.

| Phase | Author API | Contract |
|---|---|---|
| Declaration | `Plugin::DECLARATION` | Defines the ID, display name, plugin version, and requested capabilities. Adapters validate it before exposing the plugin. |
| Construction | `Plugin::create` | Creates one instance after the host has selected a grant. An instance is called serially. |
| Load | `Plugin::on_load` | Runs once for an activating instance. Subscribe to events, register pure-data commands, initialize namespaced storage, schedule timers, or emit diagnostics here. |
| Calls | `on_event`, `before_*`, `on_command`, `on_timer` | Receive read-only values and call-scoped facades. World changes are bounded operations submitted to the host, never direct mutations. |
| Unload | `Plugin::on_unload` | Runs during normal retirement. It can use storage, timers, and diagnostics. A host may skip it after a reported panic because plugin state may be inconsistent. |

Mutating effects are staged per callback. A successful callback lets the host
commit the stage; a returned `PluginError`, capability denial, buffer failure,
or reported panic discards it. Read operations are not staged. Diagnostics may
remain as evidence of a failed call. Errors from block, chat, and interaction
decision callbacks fail closed: the attempted action is denied without
feedback. A cooperative error from a notification, command, or timer is
recorded while the instance remains eligible for later calls.

The host still performs authoritative validation before applying a submitted
operation. A successful `set_block`, teleport, or message submission means the
bounded stage accepted the request, not that gameplay state has already
changed.

## Capabilities

A declaration requests capabilities; the host chooses what to grant. The
built-in adapter freezes the intersection of the request and the explicit host
grant. The native manifest and exported descriptor must request exactly the
same set. Missing access returns a typed error.

| Identifier | What it authorizes |
|---|---|
| `read-world` | `EventContext::world`: loaded-chunk, block-state, and player-position queries through a read-only view. |
| `submit-intents` | `EventContext::operations`: bounded block-write, teleport, and player-message requests. |
| `register-commands` | `LoadContext::commands` and later command callbacks routed by a nonzero handler ID. |
| `receive-events` | Load-time subscriptions and passive player, movement, and committed block notifications. |
| `read-permissions` | `EventContext::permissions`: read-only resolution of validated permission nodes. |
| `storage` | Bounded key-value access in the host-selected plugin namespace during load, calls, and unload. |
| `veto-block-edits` | Block-place and block-break decision callbacks, including placement replacement. |
| `veto-events` | Chat and interaction decision callbacks. |

Diagnostics and deterministic timers are packaging services and have no ABI-v1
capability bit.

One current ABI-v1 detail affects `set_block`: the dynamic adapter must obtain a
current-dimension handle through the world-read request before it can submit
the block operation. A dynamic plugin that calls `set_block` therefore needs
both `submit-intents` and `read-world`. If `read-world` is absent, the call
returns a typed failure; the adapter does not substitute another handle or
expand the grant.

Capabilities govern cooperative FerrumC facade access. They are not
operating-system permissions and are not a security boundary.

## Packaging

### Compiled-in

`BuiltinPluginFactory::new::<P>()` validates the declaration and type-erases the
plugin. Initialization requires an explicit host grant and a fresh
`HostServices` backend. Every later callback also receives a fresh backend with
the frozen grant and a bounded mutation stage.

The adapter has no global registry or automatic app registration. Using a
third-party built-in plugin in the shipping server currently requires explicit
application wiring; there is no configuration-only path.

### Trusted native plugin

Build the library with the `dynamic` feature and an unwinding panic strategy.
The adapter exports the versioned C ABI, encodes hot-path events in a checked
binary format, and copies call data across the boundary under explicit bounds.
The library is specific to its operating system, architecture, calling
convention, and library format.

The new native loader expects each candidate to be an immediate child directory
containing a strict `plugin.toml` and the named library. A manifest has this
shape:

```toml
id = "region-guard"
name = "Region Guard"
version = "1.0.0"
abi_major = 1
abi_minor = 0
server_api = "=0.2.0-dev"
library = "libferrumc_plugin_region_guard.so"
library_sha256 = "<64 lowercase hexadecimal digits for the copied library>"
capabilities = ["submit-intents", "veto-block-edits"]
```

The filename is platform-specific. Generate the SHA-256 value from the exact
bytes placed beside the manifest. Identity, display name, numeric plugin
version, ABI version, and capabilities must match the exported descriptor.
The loader also requires an exact build-target match and checks that the
running FerrumC API version satisfies `server_api`.

This bundle format is implemented and tested by `ferrumc-plugin-loader`, but it
is not wired into the shipping app's active gameplay host yet. Treat bundle
creation as packaging preparation, not as a supported deployment procedure.

## Trust, hangs, panics, and crashes

A trusted native plugin is arbitrary code in the FerrumC process. Installing
one grants it the server process's authority: it can read files available to
the process, open sockets, spawn threads, corrupt memory, deadlock, crash, or
terminate the process. Manifest validation, target checks, and checksums can
reject cooperative mistakes; they do not constrain what native code does,
including code run by a library initializer.

Native callbacks cannot be safely preempted. Timing and watchdog mechanisms can
observe or report an overrun and can stop future cooperative calls after the
current call returns, but they cannot safely interrupt a hung instruction
stream. A hung callback can therefore block its calling thread indefinitely.

The dynamic SDK catches an unwinding Rust panic inside the plugin boundary and
returns `FC_PLUGIN_PANIC`. The current native host policy discards that
callback's staged effects, records the failure, and disables future calls to
that plugin. The built-in adapter catches an unwinding callback panic, discards
the stage, and poisons that instance.

Those paths do not handle `panic=abort`, `std::process::abort`, segmentation
faults, undefined behavior, foreign exceptions, deadlocks, hostile actions, or
malicious memory corruption. Any of those can corrupt or terminate FerrumC and
may affect the machine with the server process's permissions. There is no
crash-proofing promise and no safe in-process recovery promise.

Opened native libraries remain resident until process exit, including a
library rejected by checks that can run only after opening it. Logical
retirement can prevent later callbacks, but FerrumC has no live hot-unload or
hot-reload operation.

See
[ADR-0008](adr/0008-trusted-native-plugins-c-abi.md)
for the governing trust and lifecycle decision.

## Versioning

Three independent versions appear in a native bundle:

- `PluginVersion` / manifest `version` is the plugin author's semantic release
  version. Its major, minor, and patch numbers must match the exported
  descriptor. ABI v1 carries only that numeric core; the manifest remains the
  source for any prerelease or build metadata.
- `abi_major` and `abi_minor` version the binary records and function tables.
  A major mismatch is rejected. Within one major, a host accepts its current
  minor and earlier minors and rejects later minors. The current ABI is 1.0, so
  the current host accepts 1.0.
- `server_api` is a semantic-version requirement checked against the running
  FerrumC package version. Use the narrowest range actually tested; the example
  above pins the current `0.2.0-dev` development API exactly.

An accepted ABI number does not imply that a binary built for another target
can be loaded, and it does not remove the need to test against the selected
FerrumC API version. Rebuild and retest artifacts for each target and server
release you distribute.

## Current operational boundary

Today, use the SDK contract tests, `PluginTestHost`, and the region-guard parity
regression to develop plugin logic and verify both adapters. Do not advertise
an SDK library as installable on the shipping binary yet:

- the app's live host contains hard-coded plugins written against the older
  internal plugin API;
- `plugins_dir` uses a separate legacy loader and a temporary host;
- loaded entries are not enabled there; and
- the live connection-side dispatcher never sends those entries gameplay
  events or decisions.

Production SDK activation, durable production plugin storage, and
simulation-tick-owned dispatch remain separate integration work.
