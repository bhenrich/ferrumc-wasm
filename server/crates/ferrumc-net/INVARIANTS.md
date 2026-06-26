# Invariants: ferrumc-net

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- An incomplete frame (truncated length prefix or not-yet-complete body) is
  reported as `DecodeOutcome::NeedMore`, never as a `DecodeError`. The input
  buffer is left untouched so the caller can retry after reading more bytes.
- A frame is rejected by its declared length against the current state's cap
  *before* its body is buffered. The per-state caps are the primary
  hostile-allocation defense.
- The inbound accumulation buffer is bounded by
  `ConnectionLimits::max_inbound_buffer`; a peer that streams bytes without ever
  completing a drainable frame is cut off, never allowed to grow it without
  limit.
- Typed dispatch runs against the *exact* frame body. Once the whole frame is
  present, a short read is `MalformedBody`, and leftover bytes inside the frame
  are `TrailingBytes` — neither is ever confused with "need more".
- Every `DecodeError` maps to exactly one `DisconnectClass` so M09 has an
  unambiguous teardown action.
- The crate depends only on `ferrumc-proto`, `ferrumc-codec`, `tokio`, and
  `flate2`; it never reaches into world, sim, or storage.

## Compression (`CompressionState`)

- The decompressed-output cap is the OOM defense, not the on-wire size: a
  compressed frame is rejected when its *declared* uncompressed size exceeds the
  cap, before the output buffer is allocated. A small frame can never force a
  large allocation.
- The inflate output buffer is sized to the declared length, so a stream that
  expands beyond what it declared fills the buffer and is rejected (`Oversized`)
  rather than over-allocating.
- A compressed packet whose declared uncompressed size is below the threshold is
  a protocol violation: conforming clients must send sub-threshold packets
  uncompressed (`data_length == 0`).
- Inflation must consume the whole payload and produce exactly the declared size;
  a short read, an overrun, trailing bytes, or a corrupt stream are all rejected.
- `compress`/`decompress` are pass-throughs while compression is disabled, so the
  framing layer calls them uniformly regardless of negotiation.

## Live server (`StatusServer`)

- Concurrent connections are bounded by a `Semaphore` slot acquired *before*
  `accept`; overflow is absorbed by the OS accept backlog, never by spawning an
  unbounded number of tasks.
- Every socket read and write is wrapped in the configured I/O timeout, so a
  stalled or slow-loris peer cannot pin a connection task indefinitely.
- A single failed `accept` closes nothing but that attempt; it never tears the
  acceptor down.
- Shutdown is cooperative: the acceptor stops accepting, signals live
  connections via a `watch` channel, and drains the connection tasks before
  `run` returns.
- The status path serves only `Handshaking -> Status`. A `next_state` other than
  status (e.g. login) closes the connection cleanly rather than proceeding.
- The status-response JSON string fields are JSON-escaped before serialization.
