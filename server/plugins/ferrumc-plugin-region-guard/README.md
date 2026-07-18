# `FerrumC` region-guard example

`ferrumc-plugin-region-guard` demonstrates one plugin implementation packaged
as either a compiled-in Rust plugin or a trusted native plugin. Both adapters
consume the same [`RegionGuardPlugin`](crate::RegionGuardPlugin) declaration and
callbacks.

The guard protects every block whose `x` and `z` coordinates are both inside
the inclusive `-16..=16` square. Height does not affect the policy. A placement
or break attempt inside that square emits one bounded player message and is
denied with matching feedback. Attempts outside the square are allowed without
emitting an operation.

The declaration requests exactly two host facades:

- `veto-block-edits`, for the placement and break decisions;
- `submit-intents`, for the bounded player message.

The default feature exposes [`builtin_factory`](crate::builtin_factory). Build
the trusted native artifact from the workspace root with:

```text
cargo build -p ferrumc-plugin-region-guard --no-default-features --features dynamic
```

The trusted native adapter exports the versioned ABI without any
plugin-authored unsafe code. Native libraries execute in the `FerrumC` process
with operator-granted process-wide trust; capability declarations scope
cooperative host-facade access rather than operating-system authority.
