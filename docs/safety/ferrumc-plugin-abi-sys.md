# Safety: `ferrumc-plugin-abi-sys`

> Required reading before changing the native ABI boundary.

## Why this crate may use unsafe code

`ferrumc-plugin-abi-sys` is FerrumC's designated production unsafe-code
boundary for the new runtime. It must open platform libraries, resolve C
symbols, validate foreign records, invoke C function pointers, copy
explicit-length foreign views, and implement the raw half of plugin-side
callbacks. Every new runtime crate keeps `#![forbid(unsafe_code)]`.

At Packet 46, the old partial dynamic path in `ferrumc-plugin-host` and its
older sample `cdylib` crates still carry pre-existing scoped exceptions. This
crate does not expand those paths. Packets 48 and 53 must retire or replace
them before the branch can truthfully claim one physical unsafe-code crate
across the whole workspace.

Native plugins are operator-trusted code running in the FerrumC process.
Loading a library can execute native initializers before ABI validation runs.
Structural validation catches cooperative incompatibility; it cannot prove
that an arbitrary address is mapped, executable, immutable, non-racing, or
honest about provenance. Those properties remain obligations of the trusted
ABI peer.

## Host-side library boundary

### Opening and symbol lookup

`Library::new` executes trusted native initializers. Immediately after a
successful open, the `Library` is leaked into process-lifetime storage. This
happens before symbol lookup or descriptor validation, so neither a rejected
library nor a dropped safe wrapper can unload code after an initializer may
have started work.

`Library::get` resolves exactly `ferrumc_plugin_entry_v1` as
`FcPluginEntryV1Fn`. Symbol names do not prove signatures; calling the result
relies on the library honoring ABI v1. The resident library keeps all copied
function addresses live.

### Descriptor and function-table reads

Raw validation proceeds in this order:

1. reject null and misaligned record pointers;
2. read only the leading `u32` size;
3. require the complete eight-byte common header;
4. read and negotiate major/minor;
5. require the complete known v1 prefix;
6. read required slots as raw pointer-sized words and reject zero;
7. only then construct a typed record containing bare function pointers.

The crate has compile-time assertions for the locally supported 64-bit layout,
function-pointer word size, record sizes, and final required-slot offsets.
Nonzero does not prove a callable address; callable-address validity and
no-unwind behavior are trusted ABI obligations.

### Explicit-length metadata

Every metadata getter is invoked only after its raw slot passes validation.
The returned length must fit `usize`, remain below the 4 KiB metadata ceiling,
fit the slice bound, and not wrap the address space. A nonempty view must be
non-null. Bytes are copied immediately and validated as UTF-8 before another
getter runs. No borrowed plugin pointer leaves the call.

## Safe host callback facade

Validated loaded callbacks remain private. `LoadedAbiPlugin` and
`PluginInstance` expose owned metadata, owned envelopes, typed statuses, and
safe lifecycle methods. `LoadedAbiPlugin` is a reusable factory: initialization
copies its host-owned metadata and private validated callback set into a fresh
instance. A failed initialization leaves the factory available, and a later
initialization does not reopen the permanently resident library.

Each initialized instance receives a process-resident, nonzero host identity
and a checked `u64` call sequence. The sequence never wraps: reaching
`u64::MAX` produces a permanent typed exhaustion error. Separate instances
receive separate identities, including instances initialized sequentially from
one factory.

During one callback, a bounded thread-local slot records the internally
created frame pointer plus the expected host/call tokens. A host callback first
compares the plugin-supplied scalar tokens with that slot. Only after they
match does it dereference the internally stored pointer. It never converts a
plugin-supplied integer into a pointer. Raw host-table calls from another
thread or with stale tokens find no matching active slot and return
`FC_INVALID_ARGUMENT`; a nested lifecycle/event invocation is rejected earlier
with `CallbackError::ReentrantInvocation`.

Command, request, event, and diagnostic views are size/version checked,
bounded, and copied before safe host code sees them. Host-query output is
written only to the original host-owned allocation after pointer identity and
capacity checks. Buffer exhaustion reports the required length and is terminal
for that query.

## Plugin-side export bridge

The safe dynamic SDK needs to publish ABI records without adding unsafe source
to SDK or plugin crates. The doc-hidden builders in this crate create only this
crate's generic trampoline table, and `export_plugin_v1!` emits the exact C
bootstrap symbol while retaining this crate's lint context. An integration
test compiles the consumer with `#![forbid(unsafe_code)]`.

Initialization validates raw host records, creates a safe call-scoped wrapper,
boxes the SDK-owned instance, and publishes its address only as an opaque
handle. Event handling reconstructs a mutable borrow only under the ABI
contract that the handle came from the matching initializer and is neither
stale nor concurrent. Shutdown reconstructs the `Box` exactly once and consumes
the instance.

Plugin-side event and host-output views receive the same size-first,
version-before-tail, length, null, slice-bound, and address-wrap checks before a
safe borrow is formed. Host table slots are checked as raw words before a typed
table is constructed. Version direction is deliberate: the host-side loader
accepts plugin minor versions up to the host's current minor, while a plugin
consumer accepts same-major host records at its compiled minor or any newer
additive minor and reads only its known size-covered prefix.

The bridge does not catch panics. Packet 55's dynamic SDK adapter owns
`catch_unwind` and converts an unwinding plugin panic to `FC_PLUGIN_PANIC`
before it reaches these C callbacks.

## Audit checklist

- Every unsafe block has an adjacent, operation-specific `SAFETY:` comment.
- No plugin-supplied raw pointer, callback pointer, symbol, or unload-capable
  library handle appears in the host-facing public API.
- The only public raw direction is the doc-hidden plugin-side bootstrap/table
  plumbing required by the safe export macro.
- Size/version validation precedes every tail or required-slot read.
- Declared lengths are bounded and checked before allocation or pointer access.
- Foreign data is copied before its call-scoped lifetime ends.
- Every successfully opened library and every issued host identity remains
  resident until process exit.
- Local layout validation covers Linux aarch64. Other distribution targets
  require their own target-specific gate before support is claimed.
