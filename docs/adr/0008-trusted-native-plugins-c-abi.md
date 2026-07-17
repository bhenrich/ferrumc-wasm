# ADR-0008: Trusted Native Plugins Through a Versioned C ABI

**Status:** Proposed
**Date:** 2026-07-18
**References:** [ADR-0006](0006-c-abi-dynamic-plugins.md)

## Context

ADR-0006 chose a C ABI because Rust does not provide a stable ABI for dynamic
libraries. This ADR defines the trust, safety, lifecycle, and compatibility
costs of that choice before FerrumC expands the dynamic plugin surface beyond
metadata and lifecycle calls.

A dynamic plugin is arbitrary native code executing inside the FerrumC process.
The operator who installs it accepts process-wide trust responsibility. A
plugin can read files, open sockets, spawn threads, corrupt memory, deadlock,
crash, or terminate the process.

Capability manifests limit which FerrumC host facades the host agrees to expose.
They are not operating-system permissions and are not a security boundary.
Native code can call the operating system without going through a host facade.
Manifest, hash, and ABI validation can reject incompatible cooperative inputs;
they cannot constrain native behavior in library initializers or callbacks.

## Decision

FerrumC will support **trusted native plugins** through a versioned C ABI. That
phrase is the required public term. No public copy, SDK page, rustdoc, or error
may describe this runtime as a "sandbox"; capability checks do not change the
process-wide trust model.

### Proposed crate and unsafe-code boundary

The implementation will be divided into four crates:

- `ferrumc-plugin-abi` will define safe ABI record types, status codes, and the
  major/minor version policy.
- `ferrumc-plugin-abi-sys` will own raw pointers, symbol lookup, and raw C
  function tables. It is the only production crate permitted to contain
  `unsafe`.
- `ferrumc-plugin-loader` will provide the safe lifecycle wrapper and validate
  libraries, manifests, versions, records, and function tables before use.
- `ferrumc-plugin-sdk` will provide the author-facing API and safe packaging
  adapters.

Every other production crate will retain `#![forbid(unsafe_code)]`. Raw pointers
and borrowed plugin memory must not escape `ferrumc-plugin-abi-sys`; higher
layers will receive validated, host-owned values.

### ABI boundary rules

The boundary uses these rules:

- Entrypoints and callbacks use `extern "C"`.
- Shared records use `#[repr(C)]`, fixed-width integer fields, and explicit
  status codes. Rust references, trait objects, `String`, `Vec`, and
  representation-unspecified Rust enums do not cross the boundary.
- Resources cross as opaque handles. Variable data crosses as pointers paired
  with explicit lengths and a documented call-scoped lifetime.
- Allocation and destruction stay with the side that owns the allocation.
  Data needed after a call is copied into host-owned storage before the call
  returns.
- Extensible records carry `struct_size`; optional fields may only be appended.
- ABI compatibility is negotiated with explicit `abi_major` and `abi_minor`
  values. A major mismatch is rejected before plugin initialization. A host
  accepts every earlier minor of its current major and uses only fields covered
  by the plugin's declared sizes.
- Callable surfaces are versioned function tables. Required entries are
  validated before the first call, and later compatible additions append to a
  size-delimited table.

These rules make layout and ownership reviewable. They do not make native code
safe or trustworthy.

### Lifecycle and failure limits

A hung native function cannot be safely preempted inside the process. A
watchdog may detect and report a deadline violation, request fail-stop
shutdown, or disable future cooperative calls, but it cannot safely interrupt
the executing instruction stream.

An executing library cannot be safely hot-unloaded. Loaded plugin libraries
remain resident until process exit; FerrumC provides no live unload or hot
reload path.

An SDK boundary may use `catch_unwind` to convert an unwinding Rust panic into a
typed status before it crosses the C ABI. `catch_unwind` catches only unwinding
panics. It cannot contain `panic=abort`, `std::process::abort`, segmentation
faults, undefined behavior, foreign exceptions, deadlocks, hostile actions, or
malicious memory corruption. A segfault, undefined behavior, `abort`, or other
hostile native action may corrupt or terminate FerrumC and may affect the
machine with the operator's process permissions.

### Portability and compatibility cost

Plugin binaries are platform-specific. Operating system, architecture, calling
convention, and library-format differences require matching artifacts and
validation. A binary built for one target is not assumed to work on another.

FerrumC accepts a permanent ABI-maintenance burden. Released layouts, status
values, ownership rules, version negotiation, and supported function-table
prefixes become compatibility commitments. Breaking changes require a new ABI
major and an explicit transition policy; additive changes use a new minor and
size-delimited tails.

### WASM

WASM was considered and rejected. It is not a later phase of this design. The
selected model is operator-trusted native code, and FerrumC will not carry a
second runtime and marshalling model for a different trust boundary.
Reconsidering WASM requires both a superseding ADR and an explicitly changed
trust model.

## Consequences

- Plugin authors can target a versioned C-compatible binary boundary while
  using safe Rust wrappers for ordinary authoring.
- FerrumC will validate every host-visible record and expose only declared host
  facades, but those checks govern host API use rather than operating-system
  authority.
- Operators must treat every installed binary like any other native executable
  they choose to run with the server's permissions.
- Hung, crashing, aborting, or memory-unsafe plugins can stop or corrupt the
  whole server. In-process recovery is not promised.
- Libraries stay loaded for the lifetime of the process.
- Distribution requires target-specific plugin artifacts.
- ABI evolution, compatibility tests, and old-version support are permanent
  project work rather than a one-time loader cost.
