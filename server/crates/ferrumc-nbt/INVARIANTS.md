# Invariants: ferrumc-nbt

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- **No panics on any input.** Every byte read goes through `ferrumc_codec::BoundedReader`; there is no slice indexing and no `unwrap`/`expect` outside `#[cfg(test)]`. Malformed input always returns an `NbtError`, never a panic.
- **No allocation from an untrusted length before it is bounded.**
  - `max_bytes` caps the input slice up front, so every later read is transitively bounded.
  - List and byte/int/long array lengths are checked against `max_list_len` before any element is read, and the result vectors grow incrementally rather than being pre-reserved from the declared length.
  - String byte lengths are checked against `max_string_bytes` before the bytes are read.
- **Depth accounting is explicit.** The root compound is depth 1; descending into a nested `TAG_Compound` or `TAG_List` adds one level; arrays do not nest. `depth > max_depth` is `DepthExceeded`.
- **Negative lengths are always rejected** (`NegativeLength`); a non-empty list declaring element type `TAG_End` is `MalformedList`.
- **Big-endian, Modified UTF-8.** All multi-byte integers are big-endian (via the reader). `TAG_String` is encoded and decoded as Java Modified `UTF-8` (the `DataInput.readUTF` scheme a 1.21.8 client requires): `NUL` is `0xC0 0x80` and astral characters are six-byte surrogate pairs. Byte sequences that are not valid Modified `UTF-8` are rejected as `InvalidUtf8`.
- **Roots are compounds and self-delimiting.** Both root readers require a `TAG_Compound` and reject trailing bytes. The writers emit bytes the matching readers accept and enforce the format's structural caps (`u16` string length, `i32` sequence length).
