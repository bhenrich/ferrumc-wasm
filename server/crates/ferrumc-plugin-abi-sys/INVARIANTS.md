# Invariants: ferrumc-plugin-abi-sys

- This is the designated unsafe boundary for the new plugin runtime. No new
  unsafe operation may be added to the legacy host/sample exceptions while
  their Packet 48/53 replacement is pending.
- Every unsafe block has an adjacent `SAFETY:` comment naming its exact
  invariant.
- Raw record headers and required slot words are validated before a typed
  record containing function pointers is constructed.
- ABI major/minor validation precedes every read outside the common header.
- Version direction is explicit: the host accepts same-major plugin minors no
  newer than itself; plugin trampolines accept same-major host records no older
  than the plugin's compiled minor and read only the known prefix.
- Declared lengths are bounded and representable before pointer access or
  allocation.
- Variable plugin data is copied immediately into host-owned storage; borrowed
  plugin memory never leaves the call.
- Plugin-supplied callback pointers remain private and are invoked only by safe
  host-facing methods in this crate. The sole public table exception consists
  of doc-hidden builders for this crate's own generic plugin-side trampolines,
  consumed by the safe dynamic SDK/export macro.
- Host callbacks resolve one bounded per-thread active-call slot. Plugin
  tokens are compared before only the internally registered frame pointer is
  used; foreign-thread, stale, and nested calls are rejected.
- Each initialized plugin receives a process-resident nonzero host identity and
  a non-reusing checked call sequence; exhaustion is permanent and typed.
- Every successfully opened library remains resident until process exit,
  including a library rejected after its initializers ran.
- There is no unload or hot-reload API.
- The bootstrap symbol is exactly `ferrumc_plugin_entry_v1`.
- Capability declarations scope cooperative host facades; they do not constrain
  arbitrary native behavior.
- Validation assumes the operator-trusted library honors pointer provenance,
  immutability, callable-address, and no-unwind requirements that an in-process
  loader cannot prove.
