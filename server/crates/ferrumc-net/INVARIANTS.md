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
- The crate depends only on `ferrumc-proto` and `ferrumc-codec`; it never
  reaches into world, sim, or storage.
