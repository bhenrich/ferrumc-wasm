# Invariants: ferrumc-plugin-region-guard

- One shared SDK implementation owns the declaration and region policy for
  both compiled-in and trusted native packaging.
- The crate forbids unsafe code. Dynamic export is delegated to the audited SDK
  adapter macro.
- The declaration requests exactly `veto-block-edits` and `submit-intents`.
  It does not request event subscriptions, world reads, command registration,
  permission queries, or storage.
- The protected square is inclusive on both horizontal axes:
  `-16 <= x <= 16` and `-16 <= z <= 16`. Vertical position is irrelevant.
- Coordinate classification uses direct comparisons, so every `i32` coordinate
  is accepted without overflow-prone distance arithmetic.
- A protected attempt stages one bounded message before returning one denial
  with matching feedback. Host callback staging commits both effects together
  or discards both when capacity is unavailable.
- An attempt outside the protected square returns allow and stages no
  operation.
- Plugin behavior is deterministic and uses no clock, random source, network,
  filesystem, thread, executor, channel, or mutable global state.
