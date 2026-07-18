# ferrumc-plugin-fixture-dynamic

Test-only trusted native plugin fixture for FerrumC's ABI-v1 loader and host
regressions. It is a `cdylib` so tests exercise the real platform boundary.

The source-controlled `plugin.toml.in` is a packaging template rather than an
installable manifest. The shared test-support packager copies a freshly built
platform library, hashes those exact copied bytes, and writes the final
`plugin.toml` beside it. This keeps the manifest checksum truthful across
toolchains, profiles, and targets.

The fixture subscribes to three events:

- `BLOCK_BREAK` emits a player message successfully.
- `AFTER_BLOCK_BREAK` stages the same message and then requests undeclared
  world-read access, returning the host's typed denial.
- `PLAYER_JOIN` stages a message and returns `FC_PLUGIN_PANIC`.

Those deliberately different outcomes let loader, host, and adversarial tests
share one real ABI artifact.
