# Task: <TITLE>

> Copy this template for each task. Fill in every section. If a section doesn't apply, write "N/A" — don't delete it.

## Goal

One sentence. What does success look like?

## Crate(s)

Only touch these crates. If you need to touch something not listed, STOP and ask.

- `crates/ferrumc-<name>`
- `crates/ferrumc-testkit` (if adding test helpers)

## Context

Link to relevant docs, ADRs, and invariants:

- `docs/architecture/<relevant>.md`
- `docs/adr/<relevant>.md`
- `crates/ferrumc-<name>/INVARIANTS.md`

## Required Behavior

Describe what the code must do. Be specific. Include:

- Input/output types
- Error conditions and how to handle them
- Performance constraints (if any)
- Boundary conditions

## Non-Goals

What this task is NOT:

- Do not modify generated protocol files.
- Do not add new dependencies unless justified.
- Do not touch crates outside the scope above.
- Do not optimize prematurely — correctness first.

## Acceptance Criteria

These commands must pass before the task is considered done:

```bash
cargo test -p ferrumc-<name>
cargo clippy -p ferrumc-<name> -- -D warnings
cargo fmt --all --check
cargo doc -p ferrumc-<name> --no-deps
```

Additional checks (if applicable):

```bash
cargo fuzz run <target> -- -max_total_time=10
```

## Invariants

Rules that must hold in the code you write:

- No unbounded allocation from untrusted input.
- No `unwrap()` outside `#[cfg(test)]`.
- Error types must classify the failure mode (not generic strings).
- Every public item has rustdoc.
- Every parser has malformed-input tests.

## Test Requirements

Minimum tests to write:

- [ ] Happy path
- [ ] Empty/zero-length input
- [ ] Maximum boundary input
- [ ] Malformed/truncated input
- [ ] Overflow/underflow conditions
- [ ] Round-trip (encode → decode → compare)

## Done Means

- [ ] All acceptance criteria pass
- [ ] Tests cover happy path AND failure modes
- [ ] New fixtures added to `fixtures/` if applicable
- [ ] Rustdoc on all public items
- [ ] No new warnings from clippy
- [ ] Commit message follows convention: `feat(crate): description`
