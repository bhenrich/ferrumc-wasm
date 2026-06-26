//! Outbound (clientbound) packet encoding: the [`OutboundPacket`] sum type and
//! the [`OutboundEncoder`].
//!
//! Encoding mirrors the inbound path: a packet is serialized to its `id + body`
//! form, the body length is checked against the state's cap, and a `VarInt`
//! length prefix is written ahead of the body. The result is a complete,
//! length-delimited frame ready for the (M09) write side. Everything here is
//! sync and performs no I/O.

use bytes::{BufMut, BytesMut};

use ferrumc_codec::write_var_int;
use ferrumc_proto::generated::{configuration, login, status};

use crate::compression::CompressionState;
use crate::error::{EncodeError, FrameEncodeError};
use crate::limits::ConnectionLimits;
use crate::state::ConnectionState;

/// A clientbound packet awaiting serialization, tagged by its state.
///
/// Each variant wraps the `ferrumc-proto` per-state clientbound dispatch enum.
/// The handshake state has no clientbound packets, so it has no variant;
/// [`Play`](Self::Play) carries a raw body, since typed play packets are not
/// modelled in this milestone.
#[derive(Debug, Clone, PartialEq)]
pub enum OutboundPacket {
    /// A status-state packet.
    Status(status::ClientboundStatusPacket),
    /// A login-state packet.
    Login(login::ClientboundLoginPacket),
    /// A configuration-state packet.
    Configuration(configuration::ClientboundConfigurationPacket),
    /// The raw body of a play-state frame; typed play packets are not yet
    /// modelled.
    Play(bytes::Bytes),
}

impl OutboundPacket {
    /// The connection state this packet belongs to.
    pub fn state(&self) -> ConnectionState {
        match self {
            Self::Status(_) => ConnectionState::Status,
            Self::Login(_) => ConnectionState::Login,
            Self::Configuration(_) => ConnectionState::Configuration,
            Self::Play(_) => ConnectionState::Play,
        }
    }
}

/// A sync, I/O-free encoder that serializes clientbound packets into complete,
/// length-delimited frames.
///
/// This is the outbound half of the reader/writer split: M09 hands it typed
/// packets and an output buffer, and it appends `VarInt`-length-prefixed frames.
/// The configured [`ConnectionLimits`] bound the encoded frame so the server
/// never emits a frame larger than a conforming client would accept.
#[derive(Debug)]
pub struct OutboundEncoder {
    limits: ConnectionLimits,
}

impl OutboundEncoder {
    /// Creates an encoder enforcing `limits`.
    pub fn new(limits: ConnectionLimits) -> Self {
        Self { limits }
    }

    /// The limits this encoder enforces.
    pub fn limits(&self) -> &ConnectionLimits {
        &self.limits
    }

    /// Serializes `packet` into a complete length-delimited frame appended to
    /// `out`.
    ///
    /// The packet's own [`state`](OutboundPacket::state) selects the size cap, so
    /// the frame can never be mislabelled. Returns
    /// [`EncodeError::FrameTooLarge`] if the serialized body exceeds that cap and
    /// leaves `out` unmodified in that case.
    pub fn encode(&self, packet: &OutboundPacket, out: &mut BytesMut) -> Result<(), EncodeError> {
        let state = packet.state();
        let max = self.limits.max_frame_size(state);

        // Serialize the body (id + fields) separately so its length is known
        // before the prefix is written.
        let mut body = BytesMut::new();
        encode_body(packet, &mut body)?;

        let length = body.len();
        if length > max {
            return Err(EncodeError::FrameTooLarge { state, length, max });
        }
        // `length <= max` is a frame cap far below `i32::MAX`, so the cast is
        // lossless; guard it anyway rather than assume.
        let prefix =
            i32::try_from(length).map_err(|_| EncodeError::FrameTooLarge { state, length, max })?;
        write_var_int(out, prefix);
        out.put_slice(&body);
        Ok(())
    }

    /// Serializes `packet` into a complete length-delimited frame, applying
    /// `compression` to the body, and appends it to `out`.
    ///
    /// This is the post-`SetCompression` counterpart to [`encode`](Self::encode):
    /// the `packet_id + body` is run through `compression` (gaining a
    /// `data_length` prefix and, at/above the threshold, a `zlib` stream) before
    /// the outer `VarInt` frame-length prefix is written. When `compression` is
    /// [disabled](CompressionState::disabled) the body is emitted verbatim, so
    /// the output is byte-identical to [`encode`](Self::encode) and a caller can
    /// use this method uniformly and simply enable compression mid-stream.
    ///
    /// The compressed (on-wire) body is checked against the state's frame cap, so
    /// the server never emits a frame a conforming client would refuse. Returns a
    /// [`FrameEncodeError`] (a server-side fault) and leaves `out` unmodified on
    /// failure.
    pub fn encode_compressed(
        &self,
        packet: &OutboundPacket,
        out: &mut BytesMut,
        compression: &CompressionState,
    ) -> Result<(), FrameEncodeError> {
        let state = packet.state();
        let max = self.limits.max_frame_size(state);

        let mut id_body = BytesMut::new();
        encode_body(packet, &mut id_body).map_err(FrameEncodeError::Encode)?;

        // The compression layer prepends the `data_length` prefix and, above the
        // threshold, deflates the payload; below it (or when disabled) the body
        // passes through.
        let mut frame_body = BytesMut::new();
        compression
            .compress(&id_body, &mut frame_body)
            .map_err(FrameEncodeError::Compression)?;

        let length = frame_body.len();
        if length > max {
            return Err(FrameEncodeError::Encode(EncodeError::FrameTooLarge {
                state,
                length,
                max,
            }));
        }
        let prefix = i32::try_from(length).map_err(|_| {
            FrameEncodeError::Encode(EncodeError::FrameTooLarge { state, length, max })
        })?;
        write_var_int(out, prefix);
        out.put_slice(&frame_body);
        Ok(())
    }
}

/// Serializes a packet's `id + fields` body (no length prefix) into `out`.
///
/// Shared by the plain path ([`OutboundEncoder::encode`]) and the compressed
/// path ([`OutboundEncoder::encode_compressed`]), which then frames the result.
pub(crate) fn encode_body(packet: &OutboundPacket, out: &mut BytesMut) -> Result<(), EncodeError> {
    match packet {
        OutboundPacket::Status(p) => p.encode(out)?,
        OutboundPacket::Login(p) => p.encode(out)?,
        OutboundPacket::Configuration(p) => p.encode(out)?,
        OutboundPacket::Play(raw) => out.put_slice(raw),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ferrumc_codec::{BoundedReader, BoundedString, FrameLengthReader};
    use ferrumc_proto::generated::login::{ClientboundLoginPacket, SetCompression};
    use ferrumc_proto::generated::status::{ClientboundStatusPacket, PongResponse};

    use super::*;
    use crate::compression::CompressionState;
    use crate::inbound::{decode_inbound_frame, DecodeOutcome, InboundPacket};
    use crate::state::ConnectionState;

    /// Generous cap for re-reading the encoder's own frames in tests.
    const PARSE_FRAME_CAP: usize = 1 << 20;

    #[test]
    fn encodes_a_length_prefixed_frame() {
        let encoder = OutboundEncoder::new(ConnectionLimits::default());
        let packet = OutboundPacket::Status(ClientboundStatusPacket::PongResponse(
            PongResponse::new(0x1234),
        ));
        let mut out = BytesMut::new();
        encoder.encode(&packet, &mut out).unwrap();

        // First byte is the VarInt length prefix; the body is id (0x01) + i64.
        assert_eq!(out[0] as usize, out.len() - 1);
        assert_eq!(out[1], 0x01);
        assert_eq!(out.len(), 1 + 1 + 8);
    }

    #[test]
    fn state_is_taken_from_the_packet_variant() {
        let packet = OutboundPacket::Login(ClientboundLoginPacket::SetCompression(
            SetCompression::new(256),
        ));
        assert_eq!(packet.state(), ConnectionState::Login);
    }

    #[test]
    fn play_frame_round_trips_through_the_decoder() {
        // A raw play frame the encoder wraps must decode back to the same body.
        let encoder = OutboundEncoder::new(ConnectionLimits::default());
        let body = bytes::Bytes::from_static(&[0x09, 0x08, 0x07]);
        let packet = OutboundPacket::Play(body.clone());
        let mut out = BytesMut::new();
        encoder.encode(&packet, &mut out).unwrap();

        let outcome = decode_inbound_frame(&out, ConnectionState::Play, encoder.limits()).unwrap();
        let DecodeOutcome::Decoded {
            packet: decoded,
            consumed,
        } = outcome
        else {
            panic!("expected a decoded frame");
        };
        assert_eq!(consumed, out.len());
        assert_eq!(decoded, InboundPacket::Play(body));
    }

    #[test]
    fn oversized_frame_is_rejected_and_output_untouched() {
        // A 1-byte status cap is smaller than any encoded status frame.
        let encoder = OutboundEncoder::new(ConnectionLimits::new(4096, 1, 4096, 4096, 4096));
        let packet =
            OutboundPacket::Status(ClientboundStatusPacket::PongResponse(PongResponse::new(0)));
        let mut out = BytesMut::new();
        let err = encoder.encode(&packet, &mut out).unwrap_err();
        assert!(matches!(
            err,
            EncodeError::FrameTooLarge {
                state: ConnectionState::Status,
                max: 1,
                ..
            }
        ));
        assert!(out.is_empty(), "output must be untouched on failure");
    }

    #[test]
    fn round_trips_a_status_request_style_frame() {
        // Encode a clientbound status response, then confirm the wire framing is
        // self-consistent: prefix length equals the body length.
        let encoder = OutboundEncoder::new(ConnectionLimits::default());
        let json = BoundedString::<32_767>::new("{\"x\":1}".to_string()).unwrap();
        let packet = OutboundPacket::Status(ClientboundStatusPacket::StatusResponse(
            ferrumc_proto::generated::status::StatusResponse::new(json),
        ));
        let mut out = BytesMut::new();
        encoder.encode(&packet, &mut out).unwrap();
        assert_eq!(out[0] as usize, out.len() - 1);
    }

    #[test]
    fn encode_compressed_disabled_matches_plain_encode() {
        // A disabled compression state must produce byte-identical output, so a
        // caller can use the compressed entry point uniformly before negotiation.
        let encoder = OutboundEncoder::new(ConnectionLimits::default());
        let packet =
            OutboundPacket::Status(ClientboundStatusPacket::PongResponse(PongResponse::new(7)));

        let mut plain = BytesMut::new();
        encoder.encode(&packet, &mut plain).unwrap();
        let mut compressed = BytesMut::new();
        encoder
            .encode_compressed(&packet, &mut compressed, &CompressionState::disabled())
            .unwrap();
        assert_eq!(plain, compressed);
    }

    #[test]
    fn encode_compressed_enabled_round_trips_through_decompress() {
        // With compression enabled the framed body decompresses back to the exact
        // `id + body` the plain encoder would have produced.
        let encoder = OutboundEncoder::new(ConnectionLimits::default());
        let compression = CompressionState::enabled(0);
        let packet = OutboundPacket::Login(ClientboundLoginPacket::SetCompression(
            SetCompression::new(256),
        ));

        let mut out = BytesMut::new();
        encoder
            .encode_compressed(&packet, &mut out, &compression)
            .unwrap();

        // Strip the outer frame-length prefix, then decompress the body.
        let mut reader = BoundedReader::new(&out);
        let len = FrameLengthReader::new(PARSE_FRAME_CAP)
            .read_length(&mut reader)
            .unwrap();
        let body = &out[reader.position()..reader.position() + len];
        let inner = compression.decompress(body).unwrap();

        let mut expected = BytesMut::new();
        encode_body(&packet, &mut expected).unwrap();
        assert_eq!(inner, expected.as_ref());
    }

    #[test]
    fn encode_compressed_oversized_frame_is_rejected() {
        // The on-wire (compressed) body is still capped by the state's limit.
        let encoder = OutboundEncoder::new(ConnectionLimits::new(4096, 1, 4096, 4096, 4096));
        let packet =
            OutboundPacket::Status(ClientboundStatusPacket::PongResponse(PongResponse::new(0)));
        let mut out = BytesMut::new();
        let err = encoder
            .encode_compressed(&packet, &mut out, &CompressionState::disabled())
            .unwrap_err();
        assert!(matches!(
            err,
            crate::error::FrameEncodeError::Encode(EncodeError::FrameTooLarge {
                state: ConnectionState::Status,
                ..
            })
        ));
        assert!(out.is_empty(), "output must be untouched on failure");
    }
}
