//! Property-based and corpus malformed-input tests for the inbound framing,
//! packet-dispatch, and compression decode paths.
//!
//! The module `#[cfg(test)]` suites pin specific malformed frames; this file
//! asserts the crate-wide invariants for *arbitrary* bytes in *every* connection
//! state. For any input the decode paths must
//!
//! * never panic, overflow, or hang,
//! * always terminate (a drained decoder makes progress or stops cleanly),
//! * never decode a frame that consumes zero or more-than-available bytes, and
//! * never inflate beyond the decompressed-output cap.
//!
//! For the server's own compress→decompress framing, the round trip is identity.

use bytes::BytesMut;
use proptest::prelude::*;

use ferrumc_net::{
    decode_inbound_frame, CompressionState, ConnectionLimits, ConnectionState, DecodeOutcome,
    InboundDecoder,
};

/// Every connection state, so each property runs against all per-state caps and
/// dispatch tables (including the raw-bodied [`ConnectionState::Play`]).
const STATES: [ConnectionState; 5] = [
    ConnectionState::Handshaking,
    ConnectionState::Status,
    ConnectionState::Login,
    ConnectionState::Configuration,
    ConnectionState::Play,
];

proptest! {
    /// `decode_inbound_frame` is total: any bytes in any state yield a clean
    /// `Result`, and a decoded frame consumes between 1 and `len` bytes.
    #[test]
    fn decode_frame_is_total(
        bytes in prop::collection::vec(any::<u8>(), 0..600),
        state_idx in 0usize..STATES.len(),
    ) {
        let state = STATES[state_idx];
        let limits = ConnectionLimits::default();
        match decode_inbound_frame(&bytes, state, &limits) {
            Ok(DecodeOutcome::Decoded { consumed, .. }) => {
                prop_assert!(consumed >= 1);
                prop_assert!(consumed <= bytes.len());
            }
            Ok(DecodeOutcome::NeedMore) | Err(_) => {}
        }
    }

    /// Draining a decoder fed arbitrary bytes always terminates and never lets
    /// the accumulation buffer exceed its ceiling. Each decoded frame consumes
    /// at least one byte, so the loop cannot spin forever.
    #[test]
    fn decoder_drain_terminates(
        chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..8),
        state_idx in 0usize..STATES.len(),
    ) {
        let state = STATES[state_idx];
        let limits = ConnectionLimits::default();
        let mut decoder = InboundDecoder::new(limits);
        for chunk in &chunks {
            // A push that would overflow is rejected, not appended; ignore it.
            let _ = decoder.push(chunk);
        }
        let ceiling = limits.max_inbound_buffer();
        let mut iters = 0usize;
        loop {
            prop_assert!(decoder.buffered_len() <= ceiling);
            iters += 1;
            prop_assert!(iters < 100_000, "decode loop must terminate");
            match decoder.next_packet(state) {
                // A decoded frame consumed >= 1 byte; loop to drain the next.
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }

    /// The compression-aware decode path and the frame-skip recovery path are
    /// total for any bytes, any threshold, and any state.
    #[test]
    fn compressed_decode_is_total(
        bytes in prop::collection::vec(any::<u8>(), 0..600),
        threshold in 0i32..512,
        state_idx in 0usize..STATES.len(),
    ) {
        let state = STATES[state_idx];
        let compression = CompressionState::from_threshold(threshold);
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        let _ = decoder.push(&bytes);
        let _ = decoder.next_packet_compressed(state, &compression);
        let _ = decoder.skip_frame(state);
    }

    /// `decompress` is total: arbitrary bytes never panic, and a success never
    /// exceeds the decompressed-output cap. Both negotiated and disabled states
    /// are exercised.
    #[test]
    fn decompress_is_total(
        bytes in prop::collection::vec(any::<u8>(), 0..600),
        threshold in 0usize..512,
    ) {
        let enabled = CompressionState::enabled(threshold);
        if let Ok(out) = enabled.decompress(&bytes) {
            prop_assert!(out.len() <= enabled.max_decompressed());
        }
        // Disabled is a verbatim pass-through; it must also never panic.
        let _ = CompressionState::disabled().decompress(&bytes);
    }

    /// The server's own compress→decompress framing is identity for any packet
    /// and any threshold, both enabled and disabled. This pins the encode/decode
    /// symmetry across the threshold boundary and the empty-packet edge.
    #[test]
    fn compress_decompress_round_trips(
        packet in prop::collection::vec(any::<u8>(), 0..1024),
        threshold in 0usize..256,
    ) {
        let enabled = CompressionState::enabled(threshold);
        let mut out = BytesMut::new();
        enabled.compress(&packet, &mut out).expect("bounded packet compresses");
        prop_assert_eq!(enabled.decompress(&out).unwrap(), packet.clone());

        let disabled = CompressionState::disabled();
        let mut out = BytesMut::new();
        disabled.compress(&packet, &mut out).expect("pass-through compress");
        prop_assert_eq!(disabled.decompress(&out).unwrap(), packet);
    }
}

// ---------------------------------------------------------------------------
// Hand-crafted regression corpus.
// ---------------------------------------------------------------------------

/// Regression: an empty packet at threshold 0 must round-trip. Threshold 0
/// selects the compressed branch for every non-empty packet, but an empty
/// packet cannot be signalled compressed — `data_length == 0` is reserved for
/// the uncompressed marker — so it must be emitted uncompressed. Before the fix,
/// `compress` declared a `data_length` of 0 *and* appended a zlib stream, which
/// `decompress` then read back as the raw (uncompressed) body, corrupting it.
#[test]
fn empty_packet_at_threshold_zero_round_trips() {
    let state = CompressionState::enabled(0);
    let mut out = BytesMut::new();
    state.compress(&[], &mut out).unwrap();
    assert_eq!(state.decompress(&out).unwrap(), Vec::<u8>::new());
}

/// A frame whose declared length is enormous but whose body is absent is never
/// decoded as a complete frame: it is "need more", an oversize rejection, or a
/// malformed-prefix error — never a panic or a huge buffer reservation.
#[test]
fn oversized_declared_frame_does_not_allocate() {
    let mut bytes = Vec::new();
    ferrumc_codec::write_var_int(&mut bytes, i32::MAX);
    for state in STATES {
        let outcome = decode_inbound_frame(&bytes, state, &ConnectionLimits::default());
        match outcome {
            // Caps smaller than i32::MAX reject; the play/config caps are large
            // enough that the (absent) body simply has not arrived yet.
            Ok(DecodeOutcome::NeedMore) | Err(_) => {}
            Ok(DecodeOutcome::Decoded { .. }) => {
                panic!("an absent body must never decode as a complete frame")
            }
        }
    }
}
