# ferrumc-plugin-abi

Versioned C-layout declarations and compatibility policy for `FerrumC`'s trusted
native plugin ABI.

This crate contains inert values only. It never dereferences a foreign pointer,
invokes a callback, loads a library, allocates across the ABI boundary, or
interprets plugin-owned memory. `ferrumc-plugin-abi-sys` owns those operations
and must validate every size, pointer, length, and required function slot before
use.

ABI v1 uses fixed-width scalars, opaque integer handles, explicit-length
call-scoped views, size-prefixed append-only records, and versioned function
tables. Capability bits scope `FerrumC` host facades; they are not a security
boundary.

## Invariants

See `INVARIANTS.md` in this directory.
