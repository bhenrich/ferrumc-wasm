# ferrumc-net framing/decompression corpus

These are raw libFuzzer envelope inputs, not hexadecimal text. The first byte
selects disabled (`00`) or enabled (`01`) compression; the remainder is the raw
outer-frame wire fed to `InboundDecoder`. The zero-byte seed has no selector and
therefore performs no decode. The standalone target in
`server/crates/ferrumc-net/fuzz` and the stable
`ferrumc-net/tests/fuzz_corpus.rs` smoke test consume the same files.

The stable coverage bound is exactly 24 inputs, 23 push attempts, and 22 decode
attempts, with no input longer than 519 bytes. The zero-byte seed has no
selector, and the accumulation-overflow seed is rejected atomically by `push`;
every other selected wire is decoded once in `ConnectionState::Play`. All five
frame caps are 512 bytes, so the inbound accumulation ceiling is 517 bytes.
Enabled compression uses threshold 16 and a 256-byte decompressed-output cap.

The corpus pins incomplete and malformed outer prefixes, negative and oversized
frame lengths, exact frame and accumulation bounds, uncompressed marker
threshold behavior, valid zlib streams, exact decompression capacity, declared
zip-bomb rejection before inflation, corrupt/trailing zlib, declared-size
mismatches, and malformed/negative/below-threshold `data_length` values. It is
not an exhaustive proof over every byte combination; open-ended mutation
remains an explicit local cargo-fuzz activity outside the stable gate.

Do not point an exploratory fuzz run at this committed directory because
libFuzzer may add corpus entries. Copy it into repository-local `.codex-tmp/`
first.

## Seed inventory

Outer length prefixes and compressed `data_length` values use Minecraft
`VarInt`. Repeated-byte bodies are described compactly; the stable test pins
every exact filename and byte size, then checks successful decoded bodies and
typed failures.

| File | Envelope after selector | Expected result |
|---|---|---|
| `00_empty.bin` | no selector | bounded no-op |
| `01_plain_empty_wire.bin` | empty | need more, 0 buffered |
| `02_plain_truncated_prefix.bin` | `80` | need more, 1 buffered |
| `03_plain_bad_length_varint.bin` | `80 80 80 80 80` | `BadLengthVarInt` |
| `04_plain_negative_length.bin` | `ff ff ff ff 0f` | `NegativeLength { -1 }` |
| `05_plain_short_body.bin` | length 2, one body byte | need more, 2 buffered |
| `06_plain_valid_one_byte.bin` | length 1, body `ab` | raw body `ab` |
| `07_plain_exact_frame_limit.bin` | length 512, body `5a` × 512 | exact 512-byte success |
| `08_plain_frame_over_limit.bin` | length 513, body `5b` × 513 | `FrameTooLarge { 513, 512 }` |
| `09_plain_push_overflow.bin` | 518 zero bytes | `BufferOverflow { 518, 517 }` |
| `10_compressed_marker_zero_valid.bin` | outer body `00 7f` | raw body `7f` |
| `11_compressed_marker_zero_at_threshold.bin` | marker 0 + `10` × 16 | `UncompressedAtOrAboveThreshold { 16, 16 }` |
| `12_compressed_declared_16_valid.bin` | declared 16 + zlib(`41` × 16) | body `41` × 16 |
| `13_compressed_exact_output_limit.bin` | declared 256 + zlib(`42` × 256) | body `42` × 256 |
| `14_compressed_declared_bomb.bin` | declared 4096 + zlib(4096 zero bytes) | `DeclaredTooLarge { 4096, 256 }` |
| `15_compressed_corrupt_zlib.bin` | declared 16 + `00 01 02` | `MalformedZlib` |
| `16_compressed_size_mismatch.bin` | declared 17 + zlib(`41` × 16) | `SizeMismatch { 17, 16 }` |
| `17_compressed_oversized_output.bin` | declared 16 + zlib(`43` × 17) | `Oversized { 16 }` |
| `18_compressed_trailing_zlib.bin` | declared 16 + zlib(`41` × 16) + `ff` | `MalformedZlib` |
| `19_compressed_bad_data_length.bin` | `80 80 80 80 80` | `BadDataLength` |
| `20_compressed_negative_data_length.bin` | `ff ff ff ff 0f` | `NegativeDataLength { -1 }` |
| `21_compressed_below_threshold.bin` | declared 15 + zlib(`44` × 15) | `BelowThreshold { 15, 16 }` |
| `22_compressed_zero_outer_body.bin` | outer length 0 | `BadDataLength` |
| `23_compressed_truncated_outer.bin` | length 3, only `00 ab` | need more, 3 buffered |
