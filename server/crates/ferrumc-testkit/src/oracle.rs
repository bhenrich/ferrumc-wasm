//! The strict frame oracle: assert that an encoded clientbound packet forms a
//! well-shaped, length-delimited wire frame and (optionally) matches a committed
//! golden fixture byte for byte.
//!
//! # Why this exists
//!
//! Self-written fake-client tests can pass while a real vanilla client rejects
//! the same bytes — the fake decoder is as wrong as the encoder. This oracle
//! removes that false confidence at the byte level. It does **not** decode the
//! way "our" client does and call it a day; it pins the exact, framed wire bytes
//! and enforces the structural invariants a conforming client relies on:
//!
//! - **(a)** the `VarInt` frame-length prefix equals the body length,
//! - **(b)** the leading `VarInt` packet id is the one expected,
//! - **(c)** decoding consumes the whole body with **no trailing bytes** (the
//!   "trailing garbage" a real client refuses), and the value round-trips, and
//! - **(d)** when a golden fixture is supplied, the full frame equals it exactly.
//!
//! # The canonical frame
//!
//! The generated `Packet::encode` writes `[VarInt id][body]` with no length
//! prefix; `ferrumc-net`'s `OutboundEncoder` prepends the `VarInt` frame length
//! later, and with compression disabled that output is byte-identical to the
//! plain encode. So the canonical, pre-negotiation wire frame this oracle builds
//! and stores is `[VarInt len][VarInt id][body]`. See [`frame`].

use std::fmt::Debug;

use bytes::BytesMut;

use ferrumc_codec::{write_var_int, BoundedReader, CodecError};
use ferrumc_proto::ProtoError;

use crate::hex::{HexDiff, HexFixture};
use crate::roundtrip::{assert_packet_roundtrip, RoundtripError};

/// Why the strict frame oracle rejected an encoded packet.
///
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FrameOracleError {
    /// The encode/decode round-trip failed: encoding errored, decoding errored,
    /// trailing bytes were left after the body, or the re-decoded value differed
    /// from the original. Carries the underlying [`RoundtripError`].
    #[error("packet round-trip failed: {0}")]
    Roundtrip(#[source] RoundtripError),

    /// The leading `VarInt` packet id did not match the expected wire id.
    #[error("packet id mismatch: expected {expected:#x}, got {actual:#x}")]
    PacketId {
        /// The wire id the caller asserted the packet should carry.
        expected: i32,
        /// The wire id actually read from the front of the encoded body.
        actual: i32,
    },

    /// The framed `VarInt` length prefix did not equal the body byte count, so a
    /// real client would either under-read (leaving trailing garbage) or
    /// over-read past the frame.
    #[error("frame-length prefix {prefix} does not match body length {body_len}")]
    LengthPrefix {
        /// The length declared by the leading `VarInt` prefix.
        prefix: usize,
        /// The number of bytes that actually follow the prefix.
        body_len: usize,
    },

    /// The leading frame-length `VarInt` itself could not be read (truncated or
    /// overlong prefix).
    #[error("frame-length prefix is malformed: {0}")]
    MalformedFrame(#[source] CodecError),

    /// The encoded frame drifted from its committed golden fixture. Carries the
    /// [`HexDiff`] pinpointing the first differing offset.
    #[error("encoded bytes drifted from the golden fixture:\n{0}")]
    Golden(#[source] HexDiff),
}

/// Frames an `id + body` buffer the way `OutboundEncoder` does with compression
/// disabled: a `VarInt` length prefix followed by the bytes.
///
/// The length is the byte count of `id_body`; lengths beyond the non-negative
/// `VarInt` domain saturate to [`i32::MAX`] rather than panic (no real packet
/// approaches that bound).
#[must_use]
pub fn frame(id_body: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(id_body.len() + 5);
    let len = i32::try_from(id_body.len()).unwrap_or(i32::MAX);
    write_var_int(&mut framed, len);
    framed.extend_from_slice(id_body);
    framed
}

/// Reads the leading frame-length `VarInt` of `framed` and asserts it equals the
/// number of bytes that follow it — the "no trailing garbage / no over-read"
/// check a conforming client enforces.
///
/// Returns [`FrameOracleError::MalformedFrame`] if the prefix cannot be read and
/// [`FrameOracleError::LengthPrefix`] if it disagrees with the remaining length.
pub fn assert_frame_length(framed: &[u8]) -> Result<(), FrameOracleError> {
    let mut reader = BoundedReader::new(framed);
    let prefix = reader
        .read_var_int_len()
        .map_err(FrameOracleError::MalformedFrame)?;
    let body_len = reader.remaining();
    if prefix != body_len {
        return Err(FrameOracleError::LengthPrefix { prefix, body_len });
    }
    Ok(())
}

/// The strict frame oracle. Encodes `packet`, then asserts every invariant a
/// real client relies on and returns the full uncompressed wire frame
/// `[VarInt len][VarInt id][body]`.
///
/// Concretely it checks:
/// - **(b/c/d-internal)** via [`assert_packet_roundtrip`]: encode succeeds, the
///   leading id is consumed, decode leaves **no trailing bytes**, and the
///   re-decoded value equals `packet`;
/// - **(b)** the leading `VarInt` id equals `expected_id`;
/// - **(a)** the framed length prefix equals the body length; and
/// - **(d)** when `golden` is `Some`, the full frame equals it byte for byte.
///
/// Pass the generated codecs as function items, e.g.
/// `assert_wire_frame(&pkt, JoinGame::encode, JoinGame::decode, JoinGame::PACKET_ID, None)`.
/// Returns a [`FrameOracleError`] (never panics) so the calling test drives the
/// assertion with `.unwrap()` / `?`.
pub fn assert_wire_frame<T, E, D>(
    packet: &T,
    encode: E,
    decode: D,
    expected_id: i32,
    golden: Option<&HexFixture>,
) -> Result<Vec<u8>, FrameOracleError>
where
    T: PartialEq + Debug,
    E: Fn(&T, &mut BytesMut) -> Result<(), ProtoError>,
    D: Fn(&mut BoundedReader<'_>) -> Result<T, ProtoError>,
{
    // Covers encode success, id-consumed-on-decode, the no-trailing-bytes check,
    // and value round-trip equality; yields the `id + body` wire bytes.
    let id_body =
        assert_packet_roundtrip(packet, encode, decode).map_err(FrameOracleError::Roundtrip)?;

    // (b) The leading VarInt must be exactly the expected wire id. This read
    // cannot fail once the round-trip above succeeded, but stay panic-free.
    let mut reader = BoundedReader::new(&id_body);
    let actual_id = reader
        .read_var_int()
        .map_err(|err| FrameOracleError::Roundtrip(RoundtripError::PacketId(err)))?;
    if actual_id != expected_id {
        return Err(FrameOracleError::PacketId {
            expected: expected_id,
            actual: actual_id,
        });
    }

    // (a) Wrap the body the way the outbound encoder does and confirm the prefix
    // matches the body length.
    let framed = frame(&id_body);
    assert_frame_length(&framed)?;

    // (d) Optional byte-exact comparison against the committed golden.
    if let Some(golden) = golden {
        golden
            .verify_eq(&framed)
            .map_err(FrameOracleError::Golden)?;
    }

    Ok(framed)
}

#[cfg(test)]
mod tests {
    use bytes::{BufMut, BytesMut};
    use ferrumc_codec::BoundedReader;
    use ferrumc_proto::generated::status::PingRequest;

    use super::{assert_frame_length, assert_wire_frame, frame, FrameOracleError};
    use crate::hex::HexFixture;

    /// `frame` writes the body length as the leading `VarInt`, then the body.
    #[test]
    fn frame_prepends_the_body_length() {
        let framed = frame(&[0xAA, 0xBB, 0xCC]);
        assert_eq!(framed, vec![0x03, 0xAA, 0xBB, 0xCC]);
    }

    /// A well-formed frame passes the length check.
    #[test]
    fn assert_frame_length_accepts_matching_prefix() {
        assert!(assert_frame_length(&frame(&[1, 2, 3, 4])).is_ok());
    }

    /// A prefix that overstates the body (claims 3 bytes, only 2 follow) is the
    /// over-read a client would hit; it must be flagged, not accepted.
    #[test]
    fn assert_frame_length_rejects_length_mismatch() {
        let err = assert_frame_length(&[0x03, 0x01, 0x02]).expect_err("mismatch");
        assert!(matches!(
            err,
            FrameOracleError::LengthPrefix {
                prefix: 3,
                body_len: 2
            }
        ));
    }

    /// A truncated prefix (continuation bit set, input ends) cannot be read.
    #[test]
    fn assert_frame_length_rejects_malformed_prefix() {
        let err = assert_frame_length(&[0x80]).expect_err("malformed");
        assert!(matches!(err, FrameOracleError::MalformedFrame(_)));
    }

    /// The happy path returns the full `[len][id][body]` frame and accepts a
    /// matching golden fixture.
    #[test]
    fn assert_wire_frame_pins_a_real_packet() {
        let packet = PingRequest::new(0x0123_4567_89ab_cdef);
        let framed = assert_wire_frame(
            &packet,
            PingRequest::encode,
            PingRequest::decode,
            PingRequest::PACKET_ID,
            None,
        )
        .expect("oracle");
        // [len=9][id=0x01][8-byte payload].
        assert_eq!(framed[0], 9);
        assert_eq!(framed[1], PingRequest::PACKET_ID as u8);
        assert_eq!(framed.len(), 1 + 1 + 8);

        // Feeding the produced bytes back as the golden must pass.
        let golden = HexFixture::from_bytes(framed.clone());
        assert!(assert_wire_frame(
            &packet,
            PingRequest::encode,
            PingRequest::decode,
            PingRequest::PACKET_ID,
            Some(&golden),
        )
        .is_ok());
    }

    /// A wrong `expected_id` is reported as a packet-id mismatch.
    #[test]
    fn assert_wire_frame_rejects_wrong_expected_id() {
        let packet = PingRequest::new(1);
        let err = assert_wire_frame(
            &packet,
            PingRequest::encode,
            PingRequest::decode,
            0x7E,
            None,
        )
        .expect_err("wrong id");
        assert!(matches!(
            err,
            FrameOracleError::PacketId {
                expected: 0x7E,
                actual: 0x01
            }
        ));
    }

    /// A corrupted golden surfaces as a byte-drift error, not a pass.
    #[test]
    fn assert_wire_frame_rejects_corrupted_golden() {
        let packet = PingRequest::new(1);
        let corrupt = HexFixture::from_bytes(vec![0x00, 0x00]);
        let err = assert_wire_frame(
            &packet,
            PingRequest::encode,
            PingRequest::decode,
            PingRequest::PACKET_ID,
            Some(&corrupt),
        )
        .expect_err("drift");
        assert!(matches!(err, FrameOracleError::Golden(_)));
    }

    /// Trailing bytes the decoder does not consume are exactly what a real client
    /// rejects; the oracle must surface them through the round-trip error.
    #[test]
    fn assert_wire_frame_rejects_trailing_bytes() {
        let packet = PingRequest::new(1);
        let err = assert_wire_frame(
            &packet,
            |p: &PingRequest, buf: &mut BytesMut| {
                PingRequest::encode(p, buf)?;
                buf.put_u8(0xFF); // a stray byte the decoder will not read
                Ok(())
            },
            PingRequest::decode,
            PingRequest::PACKET_ID,
            None,
        )
        .expect_err("trailing");
        assert!(matches!(
            err,
            FrameOracleError::Roundtrip(crate::RoundtripError::TrailingBytes { remaining: 1 })
        ));
    }

    /// The decode source is a real `BoundedReader`; this keeps the import used and
    /// documents the expected closure shape.
    #[test]
    fn decode_closure_shape_compiles() {
        let packet = PingRequest::new(2);
        let decode = |reader: &mut BoundedReader<'_>| PingRequest::decode(reader);
        assert!(assert_wire_frame(
            &packet,
            PingRequest::encode,
            decode,
            PingRequest::PACKET_ID,
            None,
        )
        .is_ok());
    }
}
