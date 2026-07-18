# ferrumc-codec cargo-fuzz targets

This standalone package keeps nightly/libFuzzer dependencies outside the stable
server workspace. The ordinary stable gate is
`cargo test -p ferrumc-codec --test fuzz_corpus`; it replays every committed raw
seed from `fixtures/fuzz/ferrumc-codec`.

Running the deep targets requires an operator-provided nightly toolchain,
`cargo-fuzz`, and an already available `libfuzzer-sys` dependency. Repository
automation does not install or fetch them, and the stable corpus gate does not
claim that the opt-in target was compiled. Populate the repository-local Cargo
home shown below from an approved dependency source before invoking the target.

Deep fuzzing is local and opt-in. Do not run it against the committed corpus
directories because libFuzzer may add or minimize entries. From
`server/crates/ferrumc-codec`, first copy one corpus into repository-local
scratch and direct every generated artifact there:

```text
mkdir -p ../../../.codex-tmp/p60-codec/var-int-corpus
mkdir -p ../../../.codex-tmp/p60-codec/artifacts
mkdir -p ../../../.codex-tmp/p60-codec/cargo-home
cp ../../../fixtures/fuzz/ferrumc-codec/var_int/*.bin \
  ../../../.codex-tmp/p60-codec/var-int-corpus/
CARGO_HOME=../../../.codex-tmp/p60-codec/cargo-home \
CARGO_TARGET_DIR=../../../.codex-tmp/p60-codec/target \
  cargo +nightly fuzz run var_int \
  ../../../.codex-tmp/p60-codec/var-int-corpus \
  -- -runs=256 \
  -artifact_prefix=../../../.codex-tmp/p60-codec/artifacts/
```

Use the equivalent `var_long` paths for the VarLong target. The explicit
`-runs=256` bound is only a local sanity run, not the open-ended deep campaign.
Remove the `../../../.codex-tmp/p60-codec` tree after the run. Never create
`target`, `corpus`, `artifacts`, or `coverage` directories below this package.
