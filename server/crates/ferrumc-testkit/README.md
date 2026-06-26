# ferrumc-testkit

Protocol test harness: fake clients, packet fixtures, round-trip helpers, and
replayable packet transcripts. Test-only — it depends on the protocol crates so
other crates' tests can drive and assert against the 1.21.8 packet types.

What it ships:

- `HexFixture` — parse a hex string (whitespace ignored) into bytes, render bytes
  back to hex, and diff two byte runs with a readable, offset-pinpointing report.
- `assert_packet_roundtrip` — encode a generated `ferrumc-proto` packet, decode
  it back, and verify the value and wire bytes round-trip. Returns the encoded
  bytes on success and a classified error on any divergence instead of panicking.
- `PacketScript` — an ordered, directional record of wire bytes with a
  record/replay API and a line-oriented transcript format that round-trips
  through `to_transcript` / `from_transcript`, so a failing flow can be captured
  to a file and replayed deterministically.
- `ScriptedClient` — an in-memory, connection-state-agnostic duplex byte pipe
  modelling a fake client; push serverbound bytes, pull clientbound bytes, and
  assert the recorded traffic against a `PacketScript`. Real server wiring lands
  with M09/M11/M22.

The transcript format is one entry per line: `S <hex>` (serverbound, client to
server) or `C <hex>` (clientbound, server to client). Blank lines and lines
starting with `#` are ignored.

## Invariants

See `INVARIANTS.md` in this directory.
