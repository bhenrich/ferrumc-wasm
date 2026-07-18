# `FerrumC` plugin ABI system boundary

Audited native boundary for `FerrumC`'s trusted native plugin ABI.

This crate alone loads platform libraries, resolves the ABI v1 bootstrap
symbol, validates raw size/version/function-table prefixes, copies
call-scoped plugin data into host-owned values, and invokes validated
callbacks. Its host-facing loader exposes no plugin-supplied raw pointer,
callback pointer, or unload-capable library handle.

The opposite, plugin-authored direction has one deliberate bridge for the safe
dynamic SDK: doc-hidden builders publish this crate's own generic trampoline
table, and `export_plugin_v1!` generates the required raw C bootstrap return.
That bridge keeps raw handling and the exported-symbol attribute in this crate
while downstream SDK and plugin source retain `forbid(unsafe_code)`.

Every library that opens successfully remains resident until process exit,
including libraries whose descriptor is rejected after load. Loading a native
library executes operator-trusted native code before ABI validation can run.

See `INVARIANTS.md` and ADR-0008 before changing this crate.
