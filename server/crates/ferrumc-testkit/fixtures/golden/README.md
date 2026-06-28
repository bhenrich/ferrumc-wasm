# Golden wire-frame fixtures

Each `<packet>.hex` file holds the full **uncompressed wire frame** of one
clientbound packet, as lowercase hex (whitespace is ignored when parsing):

```
[VarInt length][VarInt packet id][body...]
```

This is exactly what `ferrumc-net`'s `OutboundEncoder` emits with compression
disabled.

## These are drift-regression snapshots, NOT independent vanilla truth

The bytes were generated from the **current** FerrumC encoders. They catch
encoder/codegen drift at the byte level: if an encoder or the proto generator
changes a packet's wire shape, the matching test fails and the change becomes
reviewable as a hex diff. They do **not** independently prove the bytes are what
a real Minecraft 1.21.8 client accepts — the node-client smoke test is the
eventual independent oracle for that. To guard against blessing a wrong shape,
`tests/golden.rs` also hand-asserts structural invariants (length prefix == body
length, expected id, no trailing bytes, plus per-packet facts cross-checked
against `fixtures/protocol/1_21_8/protocol.json`) on top of the byte equality.

## Regenerating

After an intentional wire-shape change, re-bless and review the diff:

```
FERRUMC_BLESS_GOLDEN=1 cargo test -p ferrumc-testkit --test golden
```

Then inspect `git diff` on these files before committing.
