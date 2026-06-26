//! [`assert_packet_roundtrip`]: encode a proto packet, decode it back, and
//! confirm the value survives the trip unchanged.

use std::fmt;

use bytes::BytesMut;
use ferrumc_codec::{BoundedReader, CodecError};
use ferrumc_proto::ProtoError;

/// Why a packet round-trip did not reproduce the original value.
///
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RoundtripError {
    /// Encoding the original packet failed.
    #[error("encoding the packet failed: {0}")]
    Encode(#[source] ProtoError),

    /// Reading the leading `VarInt` packet id back off the wire failed.
    #[error("reading the leading packet id failed: {0}")]
    PacketId(#[source] CodecError),

    /// Decoding the encoded bytes back into a packet failed.
    #[error("decoding the packet failed: {0}")]
    Decode(#[source] ProtoError),

    /// Bytes were left unread after the packet body decoded.
    #[error("{remaining} byte(s) left unread after decoding the packet")]
    TrailingBytes {
        /// Number of unconsumed bytes.
        remaining: usize,
    },

    /// The re-decoded packet was not equal to the original. The fields hold the
    /// `Debug` rendering of each side so the mismatch is visible in test output.
    #[error(
        "re-decoded packet differs from the original\n  expected: {expected}\n    actual: {actual}"
    )]
    Mismatch {
        /// `Debug` rendering of the original packet.
        expected: String,
        /// `Debug` rendering of the re-decoded packet.
        actual: String,
    },
}

/// Encodes `packet`, decodes the bytes back, and verifies the round-trip.
///
/// This targets the generated `ferrumc-proto` per-packet codecs, whose `encode`
/// writes a leading `VarInt` packet id and whose `decode` expects that id
/// already consumed. The helper therefore: encodes into a [`BytesMut`], reads
/// the leading id off the front, decodes the body, and checks that nothing
/// trails and that the decoded value equals `packet`.
///
/// On success it returns the encoded wire bytes (id included), which a caller
/// can compare against a [`HexFixture`](crate::HexFixture). On any divergence it
/// returns a [`RoundtripError`] instead of panicking, so callers drive the
/// assertion from test code with `.unwrap()` / `?`.
///
/// Pass the codecs as function items, e.g.
/// `assert_packet_roundtrip(&pkt, Handshake::encode, Handshake::decode)`.
pub fn assert_packet_roundtrip<T, E, D>(
    packet: &T,
    encode: E,
    decode: D,
) -> Result<Vec<u8>, RoundtripError>
where
    T: PartialEq + fmt::Debug,
    E: Fn(&T, &mut BytesMut) -> Result<(), ProtoError>,
    D: Fn(&mut BoundedReader<'_>) -> Result<T, ProtoError>,
{
    let mut buf = BytesMut::new();
    encode(packet, &mut buf).map_err(RoundtripError::Encode)?;
    let wire = buf.to_vec();

    let mut reader = BoundedReader::new(&buf);
    reader.read_var_int().map_err(RoundtripError::PacketId)?;
    let decoded = decode(&mut reader).map_err(RoundtripError::Decode)?;

    let remaining = reader.remaining();
    if remaining != 0 {
        return Err(RoundtripError::TrailingBytes { remaining });
    }
    if decoded != *packet {
        return Err(RoundtripError::Mismatch {
            expected: format!("{packet:?}"),
            actual: format!("{decoded:?}"),
        });
    }
    Ok(wire)
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use ferrumc_codec::BoundedReader;
    use ferrumc_proto::generated::status::PingRequest;
    use ferrumc_proto::ProtoError;

    use super::{assert_packet_roundtrip, RoundtripError};

    #[test]
    fn round_trips_a_real_packet_and_returns_wire_bytes() {
        let packet = PingRequest::new(0x0123_4567_89ab_cdef);
        let wire = assert_packet_roundtrip(&packet, PingRequest::encode, PingRequest::decode)
            .expect("round-trip");
        // 0x01 id byte followed by the 8-byte big-endian payload.
        assert_eq!(wire[0], PingRequest::PACKET_ID as u8);
        assert_eq!(wire.len(), 9);
    }

    #[test]
    fn mismatch_is_reported_not_panicked() {
        // A decode that fabricates a different value must surface as a Mismatch.
        let packet = PingRequest::new(1);
        let err = assert_packet_roundtrip(
            &packet,
            PingRequest::encode,
            |reader: &mut BoundedReader<'_>| {
                PingRequest::decode(reader).map(|_| PingRequest::new(999))
            },
        )
        .expect_err("should mismatch");
        assert!(matches!(err, RoundtripError::Mismatch { .. }));
    }

    #[test]
    fn encode_failure_is_classified() {
        let packet = PingRequest::new(1);
        let err = assert_packet_roundtrip(
            &packet,
            |_p: &PingRequest, _buf: &mut BytesMut| {
                Err(ProtoError::UnknownPacketId {
                    state: ferrumc_proto::State::Status,
                    direction: ferrumc_proto::Direction::Serverbound,
                    id: 0x7F,
                })
            },
            PingRequest::decode,
        )
        .expect_err("encode fails");
        assert!(matches!(err, RoundtripError::Encode(_)));
    }
}
