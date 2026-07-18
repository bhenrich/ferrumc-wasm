# Invariants: ferrumc-plugin-loader

> Rules that must hold for trusted native plugin discovery and validation.

## Safe surface

- The crate forbids unsafe code, and every public item has rustdoc.
- Raw pointers, native callbacks, and library handles never cross this crate's
  safe API.
- Failures use classified errors and retain enough path and field context for
  deterministic diagnosis.

## Discovery and bounds

- Discovery examines only immediate child directories of the configured plugin
  root, and only a child containing `plugin.toml` is a candidate.
- Candidate processing and the final accepted plugin sequence are
  deterministic; accepted plugins are ordered lexicographically by plugin ID.
- Duplicate plugin IDs are rejected before any candidate library is opened.
- Manifest bytes, manifest-controlled strings, paths, counts, and checksum text
  are checked against fixed bounds before dependent work.
- A library path is relative, contains no escaping traversal, and resolves
  beneath its candidate directory.
- The manifest's canonical lowercase SHA-256 digest is verified immediately
  before native loading, and the same bytes are verified again immediately
  afterward.

## Compatibility and identity

- The native descriptor target exactly matches `FERRUMC_HOST_TARGET`.
- ABI compatibility follows the published major/minor policy, and the declared
  server API requirement must contain the running FerrumC API version.
- Every requested capability is known, and the native descriptor's capability
  mask exactly matches the manifest request.
- Plugin identity, name, ABI, and semantic-version numeric core match exactly
  between the validated manifest and native descriptor. ABI v1 does not
  represent semantic-version prerelease or build metadata.

## Factory and residency

- Success returns a reusable, uninitialized safe factory. Its caller supplies
  host services and owns initialization and shutdown sequencing.
- Opening a native library is permanent for the process lifetime, even when a
  later descriptor check rejects that library.
- The loader exposes no reload, removal, hot-unload, or unload operation.

## Trust model

- Double checksum verification detects ordinary changes during loading, but a
  privileged adversary can restore the original bytes before the second check;
  it is an integrity check, not a security guarantee.
- Trusted native plugin code has the operating-system authority of FerrumC.
  Capability declarations scope cooperative host facades and are not a security
  boundary.
