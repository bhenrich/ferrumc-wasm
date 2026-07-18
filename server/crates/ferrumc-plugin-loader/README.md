# ferrumc-plugin-loader

Safe discovery and validation for `FerrumC` trusted native plugins.

The loader scans only immediate child directories of its configured plugin
root. A child is a candidate only when it contains `plugin.toml`. Directory
entries and accepted plugin IDs are ordered deterministically, so filesystem
enumeration order cannot change validation or load order. Duplicate plugin IDs
are rejected before any native library is opened.

Manifest reads and all manifest-controlled strings, paths, counts, and checksum
text are subject to fixed implementation bounds. The manifest schema is strict,
the declared library path must be relative and remain beneath its candidate
directory, and the canonical lowercase SHA-256 digest must match the library
bytes before the native library is opened.

Validation requires:

- an exact native-descriptor-to-host target match;
- a supported ABI major and minor;
- a server API semantic-version requirement satisfied by `FerrumC`;
- only known capability names and an exact manifest-to-descriptor capability
  mask match; and
- exact manifest-to-descriptor identity, name, ABI, and semantic-version numeric
  core matches.

Checksum comparisons happen immediately before and after native loading, so an
ordinary change during that operation is detected. This is a stable-file
integrity check, not a security guarantee: a privileged adversary that restores
the original bytes before the second comparison can evade it. Trusted native
plugin code has the operating-system authority of the `FerrumC` process.
Capability declarations only scope cooperative `FerrumC` host facades; they are
not a security boundary.

Successful validation returns a reusable, uninitialized safe factory. The
caller supplies host services and controls plugin initialization and shutdown.
Raw pointers, callbacks, and native library handles never escape the audited
native boundary.

Every successfully opened library remains resident for the process lifetime,
including a library rejected by checks that can run only after opening it.
There is no reload, removal, hot-unload, or unload path.

This crate inherits the workspace prohibition on unsafe code.

## Invariants

See `INVARIANTS.md` in this directory.
