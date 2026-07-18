# ferrumc-codec variable-integer corpus

These are raw libFuzzer inputs, not hexadecimal text. The standalone targets in
`server/crates/ferrumc-codec/fuzz` and the stable
`ferrumc-codec/tests/fuzz_corpus.rs` smoke test consume the same files.

The stable coverage bound is exactly 12 inputs per target, 24 total, with no
input longer than 10 bytes. Each file is loaded once; every VarInt seed is
decoded once through the raw API and once through the length API, while every
VarLong seed is decoded once through the raw API. This pins the empty,
one-/two-byte, signed-extreme, noncanonical, unused-final-bit, trailing-byte,
truncated-continuation, and exact continuation-budget branches. It is not an
exhaustive proof over every byte combination; open-ended mutation remains an
explicit local cargo-fuzz activity outside the stable gate.

Do not point an exploratory fuzz run at these committed directories because
libFuzzer may add corpus entries. Copy them into the repository-local
`.codex-tmp/` directory first.

## VarInt

| File | Raw hexadecimal bytes | Expected raw result |
|---|---|---|
| `00_empty.bin` | empty | `UnexpectedEof` |
| `01_zero.bin` | `00` | `0` |
| `02_127.bin` | `7f` | `127` |
| `03_128.bin` | `80 01` | `128` |
| `04_max.bin` | `ff ff ff ff 07` | `i32::MAX` |
| `05_min.bin` | `80 80 80 80 08` | `i32::MIN` |
| `06_negative_one.bin` | `ff ff ff ff 0f` | `-1` (`NegativeLength` through the length API) |
| `07_noncanonical_zero.bin` | `80 00` | `0` |
| `08_unused_high_bits.bin` | `ff ff ff ff 7f` | `-1` |
| `09_trailing.bin` | `01 ff` | `1`, leaving one byte |
| `10_truncated.bin` | `80 80 80 80` | `UnexpectedEof` |
| `11_too_long.bin` | `80 80 80 80 80` | `VarIntTooLong` |

## VarLong

| File | Raw hexadecimal bytes | Expected raw result |
|---|---|---|
| `00_empty.bin` | empty | `UnexpectedEof` |
| `01_zero.bin` | `00` | `0` |
| `02_127.bin` | `7f` | `127` |
| `03_128.bin` | `80 01` | `128` |
| `04_max.bin` | `ff ff ff ff ff ff ff ff 7f` | `i64::MAX` |
| `05_min.bin` | `80 80 80 80 80 80 80 80 80 01` | `i64::MIN` |
| `06_negative_one.bin` | `ff ff ff ff ff ff ff ff ff 01` | `-1` |
| `07_noncanonical_zero.bin` | `80 00` | `0` |
| `08_unused_high_bits.bin` | `ff ff ff ff ff ff ff ff ff 7f` | `-1` |
| `09_trailing.bin` | `01 ff` | `1`, leaving one byte |
| `10_truncated.bin` | `80 80 80 80 80 80 80 80 80` | `UnexpectedEof` |
| `11_too_long.bin` | `80 80 80 80 80 80 80 80 80 80` | `VarLongTooLong` |
