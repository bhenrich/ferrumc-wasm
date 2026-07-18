# ferrumc-nbt root-reader corpus

These are raw libFuzzer inputs, not hexadecimal text. The standalone target in
`server/crates/ferrumc-nbt/fuzz` and the stable
`ferrumc-nbt/tests/fuzz_corpus.rs` smoke test consume the same files.

The stable coverage bound is exactly 32 inputs, with no input longer than 65
bytes. Every input is decoded once through each of the four public root-reader
APIs under both the default limits and the fixed corpus limits, for exactly 256
parser invocations per test run. The corpus limits are depth 4, total bytes 64,
list/array length 4, and Modified UTF-8 string bytes 8.

This pins both root forms; all scalar and sequence types; exact depth, byte,
list, and string boundaries; Modified UTF-8; truncation; trailing bytes; invalid
lengths; unknown tags; and the intentional difference between whole-slice and
consumed-byte readers. It is not an exhaustive proof over every byte
combination; open-ended mutation remains an explicit local cargo-fuzz activity
outside the stable gate.

Do not point an exploratory fuzz run at this committed directory because
libFuzzer may add corpus entries. Copy it into the repository-local
`.codex-tmp/` directory first.

## Shared valid bodies

The table abbreviates four exact byte sequences:

- `S` contains empty-named Byte, Short, Int, Long, Float, and Double entries at
  signed or bit-pattern boundaries, followed by `TAG_End`:
  `01 0000 80 02 0000 8000 03 0000 80000000`
  `04 0000 8000000000000000 05 0000 3f800000`
  `06 0000 3ff0000000000000 00`.
- `Q` contains an exact-limit ByteArray, an exact-limit Byte list, an empty
  Compound, a one-element IntArray, and a one-element LongArray, followed by
  `TAG_End`:
  `07 0000 00000004 00 7f 80 ff`
  `09 0000 01 00000004 00 01 7f ff`
  `0a 0000 00 0b 0000 00000001 80000000`
  `0c 0000 00000001 8000000000000000 00`.
- `M` is an empty-named String containing NUL plus U+1F600, whose Java Modified
  UTF-8 payload is exactly eight bytes:
  `08 0000 0008 c080 eda0bd edb880 00`.
- `D` is three nested empty-named Compounds followed by four `TAG_End` bytes:
  `0a0000 0a0000 0a0000 00 00 00 00`.

The 64-byte network root is `0a`, thirteen `01 0000 00` entries, two
`02 0000 0000` entries, then `00`. The 64-byte named root is `0a 0000`,
fifteen `01 0000 00` entries, then `00`. Their duplicate empty keys are legal,
order-preserving NBT entries and are intentional.

## Seeds

Expected results below are for the filename's matching root form under the fixed
corpus limits. Unless two results are shown, both the whole-slice and
consumed-byte reader return the listed result.

| File | Raw hexadecimal bytes | Expected result |
|---|---|---|
| `00_empty.bin` | empty | `UnexpectedEof { needed: 1, remaining: 0 }` through all readers |
| `01_network_empty.bin` | `0a 00` | empty Compound, consumed 2 |
| `02_network_scalars.bin` | `0a S` | 6-entry Compound, consumed 47 |
| `03_network_sequences.bin` | `0a Q` | 5-entry Compound, consumed 55 |
| `04_network_mutf8_boundary.bin` | `0a M` | 1-entry Compound, consumed 15 |
| `05_network_depth_boundary.bin` | `0a D` | 1-entry Compound, consumed 14 |
| `06_network_bytes_boundary.bin` | 64-byte network root described above | 15-entry Compound, consumed 64 |
| `07_network_unexpected_root.bin` | `01` | `UnexpectedRootTag { id: 1 }` |
| `08_network_unknown_root.bin` | `63` | `UnknownTagType { id: 99 }` |
| `09_network_truncated.bin` | `0a` | `UnexpectedEof { needed: 1, remaining: 0 }` |
| `10_network_negative_list.bin` | `0a 09 0000 01 ffffffff` | `NegativeLength { len: -1 }` |
| `11_network_list_over_limit.bin` | `0a 09 0000 01 00000005` | `ListTooLong { len: 5, max: 4 }` |
| `12_network_invalid_mutf8.bin` | `0a 08 0000 0001 ff` | `InvalidUtf8` |
| `13_network_trailing.bin` | `0a 00 aa` | whole: `TrailingBytes { remaining: 1 }`; consumed: empty Compound, 2 |
| `14_network_depth_over_limit.bin` | `0a` plus four `0a0000` entries | `DepthExceeded { max: 4 }` |
| `15_network_bytes_over_limit.bin` | 64-byte network root plus `aa` | whole: `MaxBytesExceeded { len: 65, max: 64 }`; consumed: 15-entry Compound, 64 |
| `16_named_empty.bin` | `0a 0000 00` | name `""`, empty Compound, consumed 4 |
| `17_named_scalars.bin` | `0a 0001 72 S` | name `"r"`, 6-entry Compound, consumed 50 |
| `18_named_sequences.bin` | `0a 0000 Q` | name `""`, 5-entry Compound, consumed 57 |
| `19_named_mutf8_boundary.bin` | `0a 0000 M` | name `""`, 1-entry Compound, consumed 17 |
| `20_named_depth_boundary.bin` | `0a 0000 D` | name `""`, 1-entry Compound, consumed 16 |
| `21_named_bytes_boundary.bin` | 64-byte named root described above | name `""`, 15-entry Compound, consumed 64 |
| `22_named_unexpected_root.bin` | `01 aa` | `UnexpectedRootTag { id: 1 }` |
| `23_named_unknown_root.bin` | `63 aa` | `UnknownTagType { id: 99 }` |
| `24_named_truncated_name.bin` | `0a 00` | `UnexpectedEof { needed: 2, remaining: 1 }` |
| `25_named_nonempty_end_list.bin` | `0a 0000 09 0000 00 00000001` | `MalformedList` |
| `26_named_array_over_limit.bin` | `0a 0000 07 0000 7fffffff` | `ListTooLong { len: 2147483647, max: 4 }` |
| `27_named_string_over_limit.bin` | `0a 0000 08 0000 0009` | `StringTooLong { len: 9, max: 8 }` |
| `28_named_unknown_nested.bin` | `0a 0000 63` | `UnknownTagType { id: 99 }` |
| `29_named_trailing.bin` | `0a 0000 00 aa` | whole: `TrailingBytes { remaining: 1 }`; consumed: name `""`, empty Compound, 4 |
| `30_named_bytes_over_limit.bin` | 64-byte named root plus `aa` | whole: `MaxBytesExceeded { len: 65, max: 64 }`; consumed: name `""`, 15-entry Compound, 64 |
| `31_named_invalid_root_name.bin` | `0a 0001 ff` | `InvalidUtf8` |
