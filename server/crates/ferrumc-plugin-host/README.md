# ferrumc-plugin-host

`ferrumc-plugin-host` owns the bounded plugin registry, lifecycle state,
capability-gated facades, stable registration order, and synchronous
dispatch for:

- compiled-in implementations of `ferrumc_plugin_api::Plugin`; and
- strict trusted-native factories already validated by
  `ferrumc-plugin-loader`.

The host supports event subscriptions, block-place and block-break decisions,
bounded intent delivery, per-plugin storage namespaces, command aggregation for
compiled-in plugins, lifecycle statistics, and elapsed-time budget reporting.

## Shipping trusted-native path

At startup, `ferrumc-app` builds one long-lived `PluginHost`. Depending on
configuration it registers the built-in samples, loads every strict bundle
under `plugins_dir` through `ferrumc-plugin-loader`, registers each resulting
`LoadedPlugin`, and enables it before accepting connections. Live connections
share that same host. `builtin_plugins` defaults to `true` and controls the
three in-tree built-in samples.

Each immediate child directory containing `plugin.toml` is a bundle candidate.
Candidates are validated and returned in deterministic plugin-ID order. Any
directory-read, load, validation, duplicate-ID, registration, initialization,
or enable failure aborts startup.

The production trusted-native surface is deliberately narrow:

- `receive-events` delivers subscribed `AfterBlockPlace`, `AfterBlockBreak`,
  and integer-block-boundary `PlayerMove` notifications;
- `veto-block-edits` participates in place decisions (`Allow`, `Deny`, or
  `Replace`) and break decisions (`Allow` or `Deny`);
- `submit-intents` accepts bounded player-message and teleport intents.

The app does not grant world reads, command registration, permission reads,
plugin storage, or non-block vetoes to trusted-native bundles. Timers have no
ABI-v1 capability bit and their scheduling operations are not implemented by
this production host. `set_block` is unavailable because its ABI-v1 facade
requires a live current-dimension resource obtained through world reads. Join,
leave, chat, and interaction events are not delivered to trusted-native bundles
in production.

Production dispatch currently runs on the connection side, outside the
simulation tick. Native envelopes therefore use tick `0` and
`FcResourceHandle::INVALID` as explicit metadata-unavailable sentinels.
`AfterBlockPlace` and `AfterBlockBreak` mean that an edit passed connection-side
admission and was routed toward simulation; they do not prove that a simulation
tick committed it.

See the [plugin authoring guide](../../../docs/plugin-authoring.md) for the
bundle format and exact production capability table.

## Lifecycle and failure boundaries

Compiled-in plugin hooks are wrapped in `catch_unwind`. If a retained instance
unwinds during enable, dispatch, a decision, or disable, that registration
becomes terminally `Disabled(Panicked)` and no later `Plugin` trait hook is
called on that retained value. This catch-and-disable rule is not transactional:
storage writes, registered command handlers, submitted intents, or shared-state
changes completed before the unwind can remain visible. The boxed plugin value
is still dropped normally. Under `panic=abort`, the process terminates before
`catch_unwind` can return.

A trusted-native event or block-decision callback that returns
`FC_PLUGIN_PANIC` normally has its current bounded stage discarded, its active
instance retired without another plugin callback, and its registration
terminally disabled. This cooperative status path does not handle
`panic=abort`, `std::process::abort`, segmentation faults, undefined behavior,
foreign exceptions, deadlocks, hangs, or malicious memory corruption. Any of
those may corrupt, block, or terminate FerrumC.

Native callbacks retain intents and, for a block-decision callback, its decision
in a callback-local stage. A non-success callback status, boundary error, or
capability denial discards the stage. Every service error on a decision stage,
and a decision command routed to another stage, also discards it. Invalid
event-resource provenance and an unavailable dimension facade are
stage-poisoning on every route. Other validation or capacity errors on
notification and initialization stages reject only the offending operation, so
earlier valid effects may remain if the callback returns `FC_OK`. After success,
staged intents are submitted to the caller-owned `CommandSink` in order and a
block decision is returned for registration-order folding. If a later sink
submission fails, earlier intents already accepted by the sink can remain
visible.

`CallBudget` measures successful compiled enable/event/decision hooks,
successful trusted-native initialization, and returning trusted-native
event/decision calls. Dispatch overruns can disable later calls when configured.
Metadata and shutdown calls are not timed, and the budget cannot preempt a
running callback.

Strict-path native libraries remain resident until process exit, including
libraries opened before a later validation or startup failure. The shipping app
exposes no live install, reload, or unload operation.

Trusted native plugins execute in the server process with that process's
authority. Capability manifests restrict cooperative FerrumC facade access;
they are not operating-system permissions or a security boundary.

## Storage

The host chooses each plugin's storage namespace and never accepts a namespace
from plugin code. The backend API is synchronous; individual keys and values
are length-bounded. `PluginHost::in_memory` and the current shipping app use
`InMemoryPluginStorage`, so that composition is not durable across restart.

## Legacy lifecycle loader

The exported `PluginLoader` is a compatibility path for the earlier
`ferrumc_plugin_api::abi` lifecycle-only C ABI. It scans platform libraries,
copies metadata from an operator-trusted vtable, and registers an adapter as a
compiled-in `Plugin`; registration does not enable it. That ABI exposes only
metadata, `init`, and `shutdown`, so it does not provide the strict bundle
validator or trusted-native event/intent runtime used by the shipping app.

This compatibility loader retains its `Library` handle until the adapter is
dropped and attempts `shutdown` before releasing that handle. Returned entry
failures are classified and directory scanning continues, but native aborts,
invalid pointers, hangs, and other process-wide failures cannot be recovered.

## Safety

The crate uses `#![deny(unsafe_code)]`. The legacy lifecycle loader requires
`libloading` and raw C-vtable inspection, so its operations are confined to the
scoped `dynamic::ffi` module. The assumptions and pointer lifetimes are
documented in
[`docs/safety/ferrumc-plugin-host.md`](../../../docs/safety/ferrumc-plugin-host.md).
The strict trusted-native registration and dispatch path consumes validated
Rust values exported by `ferrumc-plugin-loader`; its raw ABI boundary is in
`ferrumc-plugin-abi-sys`.

See [INVARIANTS.md](INVARIANTS.md) for the rules enforced within this crate and
[ADR-0008](../../../docs/adr/0008-trusted-native-plugins-c-abi.md) for the
governing trust decision.
