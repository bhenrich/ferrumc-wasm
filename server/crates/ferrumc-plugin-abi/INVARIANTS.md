# Invariants: ferrumc-plugin-abi

> Rules that must hold for every released ABI declaration in this crate.

## General

- The crate is a dependency leaf and forbids unsafe code.
- Every public item has rustdoc.
- No code in this crate dereferences a pointer, calls an ABI function pointer,
  loads a library, or constructs a Rust borrow from foreign memory.
- No C-facing type contains a Rust reference, `String`, `Vec`, trait object,
  representation-unspecified enum, platform-sized integer, or Rust allocator
  ownership contract.

## ABI v1

- Released status values, record layouts, field meanings, and callback
  signatures are compatibility commitments.
- Opaque resources are fixed-width integer handles. Zero is invalid.
- Byte and UTF-8 views are pointer-plus-length declarations with call-scoped
  lifetimes. The receiving side validates and copies before retaining data.
- Every extensible record begins with `FcAbiHeader`. Optional fields append at
  the tail; fields are never reordered, removed, or inserted into a released
  prefix, and reserved fields are never repurposed.
- A record's `struct_size` describes the prefix supplied by its producer.
  Consumers accept a known required prefix and ignore a larger unknown tail.
- ABI major mismatch is rejected before initialization. Within the current
  major, a host accepts its own minor and every earlier minor, but rejects a
  future minor.
- Required callback slots are non-null. The unsafe boundary validates their raw
  machine words before constructing a typed function-table value.
- The pinned v1 layouts are verified on FerrumC's current 64-bit Linux target.
  Adding other target families requires an explicit compatibility review.
