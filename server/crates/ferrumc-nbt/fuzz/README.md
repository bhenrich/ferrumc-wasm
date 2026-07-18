# ferrumc-nbt cargo-fuzz target

This standalone package keeps nightly/libFuzzer dependencies outside the stable
server workspace. The ordinary stable gate is
`cargo test -p ferrumc-nbt --test fuzz_corpus`; it replays every committed raw
seed from `fixtures/fuzz/ferrumc-nbt`.

Running the deep target requires an operator-provided nightly toolchain,
`cargo-fuzz`, and an already available `libfuzzer-sys` dependency. Repository
automation does not install or fetch them, and the stable corpus gate does not
claim that the opt-in target was compiled. Populate the repository-local Cargo
home shown below from an approved dependency source before invoking the target.

Deep fuzzing is local and opt-in. Do not run it against the committed corpus
directory because libFuzzer may add or minimize entries. From
`server/crates/ferrumc-nbt`, first copy the corpus into repository-local scratch
and direct every generated artifact there:

```text
mkdir -p ../../../.codex-tmp/p60-nbt/root-readers-corpus
mkdir -p ../../../.codex-tmp/p60-nbt/artifacts
mkdir -p ../../../.codex-tmp/p60-nbt/cargo-home
cp ../../../fixtures/fuzz/ferrumc-nbt/root_readers/*.bin \
  ../../../.codex-tmp/p60-nbt/root-readers-corpus/
CARGO_HOME=../../../.codex-tmp/p60-nbt/cargo-home \
CARGO_TARGET_DIR=../../../.codex-tmp/p60-nbt/target \
  cargo +nightly fuzz run root_readers \
  ../../../.codex-tmp/p60-nbt/root-readers-corpus \
  -- -runs=256 \
  -artifact_prefix=../../../.codex-tmp/p60-nbt/artifacts/
```

The explicit `-runs=256` bound is only a local sanity run, not the open-ended
deep campaign. Remove the `../../../.codex-tmp/p60-nbt` tree after the run.
Never create `target`, `corpus`, `artifacts`, or `coverage` directories below
this package.
