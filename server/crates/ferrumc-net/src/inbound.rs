//! Inbound (serverbound) frame decoding: pure helpers plus the accumulating
//! [`InboundDecoder`].
//!
//! The decode path is deliberately split in two so an incomplete frame is never
//! confused with a corrupt one:
//!
//! 1. **Frame extraction** reads the `VarInt` length prefix against the current
//!    state's cap and checks the body is fully present. A truncated prefix or a
//!    not-yet-complete body yields [`DecodeOutcome::NeedMore`], not an error.
//! 2. **Typed dispatch** runs the `ferrumc-proto` per-state serverbound decoder
//!    over the *exact* frame body. Because the whole frame is present by this
//!    point, any short read is a malformed body, never "need more".

use bytes::{Buf, BytesMut};

use ferrumc_codec::{BoundedReader, CodecError, FrameLengthReader};
use ferrumc_proto::generated::{configuration, handshake, login, status};

use crate::compression::CompressionState;
use crate::error::{DecodeError, FrameDecodeError};
use crate::limits::ConnectionLimits;
use crate::state::ConnectionState;

/// A decoded serverbound packet, tagged by the state it was decoded in.
///
/// Each variant wraps the `ferrumc-proto` per-state serverbound dispatch enum,
/// except [`Play`](Self::Play): no typed play packets are generated in this
/// milestone, so a play frame's raw body is surfaced verbatim for a later
/// milestone to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundPacket {
    /// A handshake-state packet.
    Handshake(handshake::ServerboundHandshakePacket),
    /// A status-state packet.
    Status(status::ServerboundStatusPacket),
    /// A login-state packet.
    Login(login::ServerboundLoginPacket),
    /// A configuration-state packet.
    Configuration(configuration::ServerboundConfigurationPacket),
    /// The raw body of a play-state frame; typed play packets are not yet
    /// modelled.
    Play(bytes::Bytes),
}

impl InboundPacket {
    /// The connection state this packet belongs to.
    pub fn state(&self) -> ConnectionState {
        match self {
            Self::Handshake(_) => ConnectionState::Handshaking,
            Self::Status(_) => ConnectionState::Status,
            Self::Login(_) => ConnectionState::Login,
            Self::Configuration(_) => ConnectionState::Configuration,
            Self::Play(_) => ConnectionState::Play,
        }
    }
}

/// The result of attempting to decode one frame from the front of a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeOutcome {
    /// A packet was decoded; `consumed` bytes (prefix + body) should be removed
    /// from the front of the input.
    Decoded {
        /// The decoded packet.
        packet: InboundPacket,
        /// Number of leading bytes consumed from the input.
        consumed: usize,
    },
    /// The buffer does not yet hold a complete frame; feed more bytes and retry.
    NeedMore,
}

/// Decodes a single serverbound frame from the front of `buf`.
///
/// This is the pure, allocation-light core of the inbound path: it never
/// mutates `buf` and reports how many bytes a complete frame consumed via
/// [`DecodeOutcome::Decoded`]. A truncated prefix or an incomplete body returns
/// [`DecodeOutcome::NeedMore`]; only genuine corruption returns an error.
pub fn decode_inbound_frame(
    buf: &[u8],
    state: ConnectionState,
    limits: &ConnectionLimits,
) -> Result<DecodeOutcome, DecodeError> {
    let max = limits.max_frame_size(state);
    let Some(span) = locate_frame(buf, state, max)? else {
        return Ok(DecodeOutcome::NeedMore);
    };
    let body = buf
        .get(span.body_start..span.body_end)
        .ok_or(DecodeError::MalformedBody { state })?;
    let packet = decode_body(state, body)?;
    Ok(DecodeOutcome::Decoded {
        packet,
        consumed: span.body_end,
    })
}

/// The byte range a complete frame's body occupies within the input buffer.
///
/// `body_start..body_end` is the frame body (the length prefix is already
/// stripped); `body_end` is also the total number of bytes the frame consumed
/// from the front of the buffer (prefix + body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameSpan {
    /// Offset of the first body byte (one past the length prefix).
    pub body_start: usize,
    /// Offset one past the last body byte; equals the bytes consumed.
    pub body_end: usize,
}

/// Locates one complete length-delimited frame at the front of `buf`.
///
/// Reads the `VarInt` length prefix against `max` and checks the declared body
/// is fully present. A truncated prefix or a not-yet-complete body is reported as
/// `Ok(None)` ("need more"), never an error; only a genuinely malformed or
/// oversized prefix returns a [`DecodeError`]. The frame body is *not* decoded —
/// this is the shared front half of both the plain and the compressed inbound
/// paths.
pub(crate) fn locate_frame(
    buf: &[u8],
    state: ConnectionState,
    max: usize,
) -> Result<Option<FrameSpan>, DecodeError> {
    let mut reader = BoundedReader::new(buf);

    let frame_len = match FrameLengthReader::new(max).read_length(&mut reader) {
        Ok(len) => len,
        // A prefix that runs off the end of the buffer is simply not here yet.
        Err(CodecError::UnexpectedEof { .. }) => return Ok(None),
        Err(CodecError::FrameTooLarge { length, .. }) => {
            return Err(DecodeError::FrameTooLarge { state, length, max })
        }
        Err(CodecError::NegativeLength { length }) => {
            return Err(DecodeError::NegativeLength { length })
        }
        // VarIntTooLong, or any future prefix-level codec failure, is a
        // malformed length prefix.
        Err(_) => return Err(DecodeError::BadLengthVarInt),
    };

    let prefix_len = reader.position();
    // The body has not fully arrived yet; wait for more bytes without consuming.
    if reader.remaining() < frame_len {
        return Ok(None);
    }

    let body_end = prefix_len
        .checked_add(frame_len)
        .filter(|end| *end <= buf.len())
        .ok_or(DecodeError::MalformedBody { state })?;
    Ok(Some(FrameSpan {
        body_start: prefix_len,
        body_end,
    }))
}

/// Decodes a complete frame body into a typed packet for `state`.
///
/// `body` is exactly the frame's bytes — the length prefix is already stripped.
/// A play frame is surfaced raw; every other state reads the packet id and
/// dispatches to `ferrumc-proto`, then rejects any trailing bytes.
///
/// Shared by the plain path ([`decode_inbound_frame`]) and the compressed path
/// ([`InboundDecoder::next_packet_compressed`]), which feeds in the *inflated*
/// `packet_id + body` bytes.
pub(crate) fn decode_body(
    state: ConnectionState,
    body: &[u8],
) -> Result<InboundPacket, DecodeError> {
    // Play has no typed packets yet: hand back the raw body untouched.
    if state.is_play() {
        return Ok(InboundPacket::Play(bytes::Bytes::copy_from_slice(body)));
    }

    let mut reader = BoundedReader::new(body);
    // The whole frame is present, so a failure to read the id means the body is
    // malformed (e.g. an empty frame carrying no id at all).
    let id = reader
        .read_var_int()
        .map_err(|_| DecodeError::MalformedBody { state })?;

    let packet = match state {
        ConnectionState::Handshaking => InboundPacket::Handshake(
            handshake::ServerboundHandshakePacket::decode(id, &mut reader)
                .map_err(|err| DecodeError::from_proto(state, &err))?,
        ),
        ConnectionState::Status => InboundPacket::Status(
            status::ServerboundStatusPacket::decode(id, &mut reader)
                .map_err(|err| DecodeError::from_proto(state, &err))?,
        ),
        ConnectionState::Login => InboundPacket::Login(
            login::ServerboundLoginPacket::decode(id, &mut reader)
                .map_err(|err| DecodeError::from_proto(state, &err))?,
        ),
        ConnectionState::Configuration => InboundPacket::Configuration(
            configuration::ServerboundConfigurationPacket::decode(id, &mut reader)
                .map_err(|err| DecodeError::from_proto(state, &err))?,
        ),
        // Unreachable: `is_play` above already returned for the play state. The
        // arm exists to keep the match exhaustive without a panic.
        ConnectionState::Play => {
            return Ok(InboundPacket::Play(bytes::Bytes::copy_from_slice(body)))
        }
    };

    let trailing = reader.remaining();
    if trailing != 0 {
        return Err(DecodeError::TrailingBytes { state, trailing });
    }
    Ok(packet)
}

/// A sync, I/O-free decoder that accumulates raw inbound bytes and yields typed
/// serverbound packets one frame at a time.
///
/// This is the inbound half of the reader/writer split: M09 feeds it bytes read
/// off the socket via [`push`](Self::push) and pulls decoded packets via
/// [`next_packet`](Self::next_packet), advancing the [`ConnectionState`] between
/// frames as the handshake/login/configuration handshake dictates. It performs
/// no I/O and holds no global state.
///
/// The accumulation buffer is bounded by
/// [`ConnectionLimits::max_inbound_buffer`]: a peer that streams bytes without
/// ever completing a drainable frame is rejected rather than allowed to grow the
/// buffer without limit.
#[derive(Debug)]
pub struct InboundDecoder {
    buffer: BytesMut,
    limits: ConnectionLimits,
}

impl InboundDecoder {
    /// Creates a decoder enforcing `limits`.
    pub fn new(limits: ConnectionLimits) -> Self {
        Self {
            buffer: BytesMut::new(),
            limits,
        }
    }

    /// The limits this decoder enforces.
    pub fn limits(&self) -> &ConnectionLimits {
        &self.limits
    }

    /// The number of buffered bytes not yet consumed by a decoded frame.
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Appends freshly read bytes to the accumulation buffer.
    ///
    /// Returns [`DecodeError::BufferOverflow`] if the append would push the
    /// buffer past [`ConnectionLimits::max_inbound_buffer`]; the bytes are not
    /// appended in that case, so the buffer is left untouched and the caller can
    /// tear the connection down.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        let max = self.limits.max_inbound_buffer();
        let buffered = self.buffer.len().saturating_add(bytes.len());
        if buffered > max {
            return Err(DecodeError::BufferOverflow { buffered, max });
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// Attempts to decode the next serverbound packet for `state`.
    ///
    /// Returns `Ok(Some(packet))` when a full frame was decoded and consumed,
    /// `Ok(None)` when more bytes are needed (the buffer is left intact), or an
    /// error on genuine corruption. The caller drives `state`: after a frame
    /// that triggers a state transition (e.g. the handshake), pass the new state
    /// on the next call.
    pub fn next_packet(
        &mut self,
        state: ConnectionState,
    ) -> Result<Option<InboundPacket>, DecodeError> {
        match decode_inbound_frame(&self.buffer, state, &self.limits)? {
            DecodeOutcome::NeedMore => Ok(None),
            DecodeOutcome::Decoded { packet, consumed } => {
                self.buffer.advance(consumed);
                Ok(Some(packet))
            }
        }
    }

    /// Attempts to decode the next serverbound packet for `state`, applying
    /// `compression` to the frame body first.
    ///
    /// This is the post-`SetCompression` counterpart to
    /// [`next_packet`](Self::next_packet): once compression is negotiated the
    /// frame body carries a `data_length` prefix and (when at/above the
    /// threshold) a `zlib` stream, which `compression` strips and inflates before
    /// the typed decode runs. When `compression` is
    /// [disabled](CompressionState::disabled) it is a verbatim pass-through, so a
    /// caller can use this method uniformly for the whole connection lifetime and
    /// simply enable compression mid-stream.
    ///
    /// Returns `Ok(Some(packet))` when a full frame was decoded and consumed,
    /// `Ok(None)` when more bytes are needed (the buffer is left intact), or a
    /// [`FrameDecodeError`] on genuine corruption (framing, `zlib`, or typed
    /// decode). The on-wire frame is bounded by the state's frame cap before
    /// decompression, and the inflated size by the compression cap, so a single
    /// frame can never drive an unbounded allocation.
    pub fn next_packet_compressed(
        &mut self,
        state: ConnectionState,
        compression: &CompressionState,
    ) -> Result<Option<InboundPacket>, FrameDecodeError> {
        let max = self.limits.max_frame_size(state);
        let Some(span) = locate_frame(&self.buffer, state, max)? else {
            return Ok(None);
        };
        // Decompress yields an owned buffer, releasing the borrow on `self.buffer`
        // before it is advanced below.
        let inner = {
            let frame_body = self
                .buffer
                .get(span.body_start..span.body_end)
                .ok_or(DecodeError::MalformedBody { state })?;
            compression.decompress(frame_body)?
        };
        let packet = decode_body(state, &inner)?;
        self.buffer.advance(span.body_end);
        Ok(Some(packet))
    }

    /// Skips the next complete frame for `state`, consuming its on-wire bytes
    /// without decoding the body.
    ///
    /// This is the recovery counterpart to
    /// [`next_packet_compressed`](Self::next_packet_compressed): that method
    /// leaves the buffer intact when a frame decodes to an unknown packet id, so
    /// a caller that chooses to *ignore* such a well-framed-but-unmodelled packet
    /// (rather than tear the connection down) uses this to advance past it. Only
    /// the length-delimited on-wire span is consumed — the (possibly compressed)
    /// body is never inflated, so skipping is independent of compression.
    ///
    /// Returns `Ok(true)` when a complete frame was located and skipped,
    /// `Ok(false)` when the buffer does not yet hold a full frame (nothing is
    /// consumed), or a [`DecodeError`] if the frame's length prefix is itself
    /// malformed or oversized.
    pub fn skip_frame(&mut self, state: ConnectionState) -> Result<bool, DecodeError> {
        let max = self.limits.max_frame_size(state);
        match locate_frame(&self.buffer, state, max)? {
            Some(span) => {
                self.buffer.advance(span.body_end);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::{BufMut, BytesMut};

    use ferrumc_codec::{write_var_int, BoundedString};
    use ferrumc_proto::generated::handshake::ServerboundHandshakePacket;
    use ferrumc_proto::generated::login::ServerboundLoginPacket;
    use ferrumc_proto::generated::status::ServerboundStatusPacket;

    use super::*;
    use crate::compression::CompressionState;
    use crate::error::{DisconnectClass, FrameDecodeError};

    /// Wraps a packet body in a `VarInt` length prefix, producing a full frame.
    fn frame(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_var_int(&mut out, i32::try_from(body.len()).unwrap());
        out.extend_from_slice(body);
        out
    }

    /// Builds a valid serverbound handshake packet body (id + fields).
    fn handshake_body(next_state: i32) -> Vec<u8> {
        let mut body = Vec::new();
        write_var_int(&mut body, 0x00); // packet id
        write_var_int(&mut body, 772); // protocol version
        BoundedString::<255>::new("localhost".to_string())
            .unwrap()
            .write(&mut body);
        body.put_u16(25565); // port
        write_var_int(&mut body, next_state);
        body
    }

    #[test]
    fn decodes_a_complete_handshake() {
        let limits = ConnectionLimits::default();
        let buf = frame(&handshake_body(2));
        let outcome = decode_inbound_frame(&buf, ConnectionState::Handshaking, &limits).unwrap();
        match outcome {
            DecodeOutcome::Decoded { packet, consumed } => {
                assert_eq!(consumed, buf.len());
                let InboundPacket::Handshake(ServerboundHandshakePacket::Handshake(hs)) = packet
                else {
                    panic!("expected handshake packet");
                };
                assert_eq!(hs.protocol_version(), 772);
                assert_eq!(hs.next_state(), 2);
            }
            DecodeOutcome::NeedMore => panic!("expected a decoded frame"),
        }
    }

    #[test]
    fn decoder_yields_then_drains() {
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        let buf = frame(&handshake_body(1));
        decoder.push(&buf).unwrap();
        let packet = decoder.next_packet(ConnectionState::Handshaking).unwrap();
        assert!(matches!(packet, Some(InboundPacket::Handshake(_))));
        assert_eq!(decoder.buffered_len(), 0);
        // Nothing left: the next pull needs more bytes.
        assert!(decoder
            .next_packet(ConnectionState::Handshaking)
            .unwrap()
            .is_none());
    }

    #[test]
    fn two_frames_in_one_buffer_decode_in_order_across_states() {
        // A handshake selecting status, immediately followed by a status request.
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        let mut wire = frame(&handshake_body(1));
        let mut status_req = Vec::new();
        write_var_int(&mut status_req, 0x00); // StatusRequest id, empty body
        wire.extend_from_slice(&frame(&status_req));
        decoder.push(&wire).unwrap();

        let first = decoder.next_packet(ConnectionState::Handshaking).unwrap();
        assert!(matches!(first, Some(InboundPacket::Handshake(_))));
        // Caller advances the state after the handshake.
        let second = decoder.next_packet(ConnectionState::Status).unwrap();
        assert!(matches!(
            second,
            Some(InboundPacket::Status(
                ServerboundStatusPacket::StatusRequest(_)
            ))
        ));
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn partial_prefix_needs_more_not_error() {
        // A two-byte VarInt prefix with only its first (continuation) byte present.
        let limits = ConnectionLimits::default();
        let buf = [0x80u8];
        assert_eq!(
            decode_inbound_frame(&buf, ConnectionState::Handshaking, &limits).unwrap(),
            DecodeOutcome::NeedMore
        );
    }

    #[test]
    fn partial_body_needs_more_not_error() {
        // Declares a 10-byte frame but supplies only 3 body bytes.
        let limits = ConnectionLimits::default();
        let mut buf = Vec::new();
        write_var_int(&mut buf, 10);
        buf.extend_from_slice(&[0x00, 0x01, 0x02]);
        assert_eq!(
            decode_inbound_frame(&buf, ConnectionState::Handshaking, &limits).unwrap(),
            DecodeOutcome::NeedMore
        );
    }

    #[test]
    fn empty_buffer_needs_more() {
        let limits = ConnectionLimits::default();
        assert_eq!(
            decode_inbound_frame(&[], ConnectionState::Handshaking, &limits).unwrap(),
            DecodeOutcome::NeedMore
        );
    }

    #[test]
    fn dribbled_bytes_eventually_decode() {
        // Feed a handshake frame one byte at a time; only the final byte completes
        // it, and no intermediate push is treated as an error.
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        let buf = frame(&handshake_body(2));
        for (i, byte) in buf.iter().enumerate() {
            decoder.push(&[*byte]).unwrap();
            let pulled = decoder.next_packet(ConnectionState::Handshaking).unwrap();
            if i + 1 < buf.len() {
                assert!(pulled.is_none(), "byte {i} should not complete the frame");
            } else {
                assert!(matches!(pulled, Some(InboundPacket::Handshake(_))));
            }
        }
    }

    #[test]
    fn oversized_frame_rejected_per_state() {
        // 8 KiB exceeds the 4 KiB handshake cap but is well under the play cap.
        let limits = ConnectionLimits::default();
        let mut buf = Vec::new();
        write_var_int(&mut buf, 8 * 1024);

        let err = decode_inbound_frame(&buf, ConnectionState::Handshaking, &limits).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::FrameTooLarge {
                state: ConnectionState::Handshaking,
                length: 8192,
                max: 4096,
            }
        ));
        assert_eq!(err.disconnect_class(), DisconnectClass::FrameTooLarge);

        // The identical prefix in the play state only reports "need more" — the
        // body simply has not arrived, proving the cap is state-specific.
        assert_eq!(
            decode_inbound_frame(&buf, ConnectionState::Play, &limits).unwrap(),
            DecodeOutcome::NeedMore
        );
    }

    #[test]
    fn frame_at_exactly_the_cap_is_accepted() {
        // A play frame whose body is exactly the cap must pass the size check
        // (it then fails to decode as a packet, but that is a separate concern).
        let limits = ConnectionLimits::new(0, 0, 0, 0, 4);
        let mut buf = Vec::new();
        write_var_int(&mut buf, 4);
        buf.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        let outcome = decode_inbound_frame(&buf, ConnectionState::Play, &limits).unwrap();
        assert!(matches!(outcome, DecodeOutcome::Decoded { .. }));
    }

    #[test]
    fn bad_length_varint_is_rejected() {
        // Six continuation bytes never terminate within the 5-byte VarInt budget.
        let limits = ConnectionLimits::default();
        let buf = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x00];
        let err = decode_inbound_frame(&buf, ConnectionState::Handshaking, &limits).unwrap_err();
        assert_eq!(err, DecodeError::BadLengthVarInt);
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn negative_length_is_rejected() {
        // VarInt -1 decodes as a negative, never-valid length.
        let limits = ConnectionLimits::default();
        let buf = [0xFFu8, 0xFF, 0xFF, 0xFF, 0x0F];
        let err = decode_inbound_frame(&buf, ConnectionState::Handshaking, &limits).unwrap_err();
        assert_eq!(err, DecodeError::NegativeLength { length: -1 });
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn unknown_packet_id_in_strict_state_is_rejected() {
        // 0x42 is not a serverbound login packet id.
        let limits = ConnectionLimits::default();
        let mut body = Vec::new();
        write_var_int(&mut body, 0x42);
        let buf = frame(&body);
        let err = decode_inbound_frame(&buf, ConnectionState::Login, &limits).unwrap_err();
        assert_eq!(
            err,
            DecodeError::UnknownPacket {
                state: ConnectionState::Login,
                id: 0x42,
            }
        );
        assert_eq!(err.disconnect_class(), DisconnectClass::ProtocolViolation);
    }

    #[test]
    fn skip_frame_recovers_after_an_unknown_packet_id() {
        // A serverbound configuration Plugin Message (0x02, not modelled) followed
        // by a modelled Ack Finish Configuration (0x03). The unknown frame errors
        // but leaves the buffer intact; skip_frame consumes exactly it so the next
        // frame decodes cleanly — the recovery the connection layer relies on to
        // tolerate config packets the vanilla client sends but the slice ignores.
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        let compression = CompressionState::disabled();

        let mut unknown = Vec::new();
        write_var_int(&mut unknown, 0x02); // custom_payload / brand: not modelled
        unknown.extend_from_slice(b"hello"); // arbitrary payload
        let mut ack = Vec::new();
        write_var_int(&mut ack, 0x03); // AckFinishConfiguration, empty body

        let mut wire = frame(&unknown);
        wire.extend_from_slice(&frame(&ack));
        decoder.push(&wire).unwrap();

        let err = decoder
            .next_packet_compressed(ConnectionState::Configuration, &compression)
            .unwrap_err();
        assert!(matches!(
            err,
            FrameDecodeError::Decode(DecodeError::UnknownPacket {
                state: ConnectionState::Configuration,
                id: 0x02,
            })
        ));

        // The unknown frame is still buffered; skipping consumes exactly it.
        assert!(decoder.skip_frame(ConnectionState::Configuration).unwrap());

        let next = decoder
            .next_packet_compressed(ConnectionState::Configuration, &compression)
            .unwrap();
        assert!(matches!(next, Some(InboundPacket::Configuration(_))));
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn skip_frame_on_an_incomplete_buffer_consumes_nothing() {
        // Only a partial frame is present: skip_frame reports false and leaves the
        // buffer untouched, never advancing past bytes that have not arrived.
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        decoder.push(&[0x05u8, 0x00]).unwrap(); // claims 5 body bytes, only 1 present
        assert!(!decoder.skip_frame(ConnectionState::Configuration).unwrap());
        assert_eq!(decoder.buffered_len(), 2);
    }

    #[test]
    fn malformed_body_is_rejected() {
        // A valid LoginStart id (0x00) but a truncated body: the name prefix
        // claims more bytes than the frame holds.
        let limits = ConnectionLimits::default();
        let mut body = Vec::new();
        write_var_int(&mut body, 0x00); // LoginStart id
        write_var_int(&mut body, 50); // name byte-length prefix, but no bytes follow
        let buf = frame(&body);
        let err = decode_inbound_frame(&buf, ConnectionState::Login, &limits).unwrap_err();
        assert_eq!(
            err,
            DecodeError::MalformedBody {
                state: ConnectionState::Login,
            }
        );
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn empty_frame_has_no_packet_id() {
        // A zero-length frame carries no packet id and is malformed, not "need
        // more" — the frame is fully present.
        let limits = ConnectionLimits::default();
        let buf = [0x00u8]; // length prefix 0, empty body
        let err = decode_inbound_frame(&buf, ConnectionState::Status, &limits).unwrap_err();
        assert_eq!(
            err,
            DecodeError::MalformedBody {
                state: ConnectionState::Status,
            }
        );
    }

    #[test]
    fn trailing_bytes_after_packet_are_rejected() {
        // A complete StatusRequest (empty body, id 0x00) plus two junk bytes,
        // all inside one declared frame length.
        let limits = ConnectionLimits::default();
        let mut body = Vec::new();
        write_var_int(&mut body, 0x00); // StatusRequest id
        body.extend_from_slice(&[0xDE, 0xAD]); // trailing junk inside the frame
        let buf = frame(&body);
        let err = decode_inbound_frame(&buf, ConnectionState::Status, &limits).unwrap_err();
        assert_eq!(
            err,
            DecodeError::TrailingBytes {
                state: ConnectionState::Status,
                trailing: 2,
            }
        );
        assert_eq!(err.disconnect_class(), DisconnectClass::ProtocolViolation);
    }

    #[test]
    fn trailing_bytes_after_frame_are_left_buffered() {
        // Two whole frames concatenated: decoding the first must consume exactly
        // its own bytes and leave the second untouched in the buffer.
        let limits = ConnectionLimits::default();
        let mut login_body = Vec::new();
        write_var_int(&mut login_body, 0x03); // LoginAcknowledged, empty body
        let first = frame(&login_body);
        let mut wire = first.clone();
        wire.extend_from_slice(&frame(&login_body));

        let outcome = decode_inbound_frame(&wire, ConnectionState::Login, &limits).unwrap();
        match outcome {
            DecodeOutcome::Decoded { packet, consumed } => {
                assert_eq!(consumed, first.len());
                assert!(matches!(
                    packet,
                    InboundPacket::Login(ServerboundLoginPacket::LoginAcknowledged(_))
                ));
            }
            DecodeOutcome::NeedMore => panic!("expected a decoded frame"),
        }
    }

    #[test]
    fn play_frame_surfaces_raw_body() {
        let limits = ConnectionLimits::default();
        let body = [0x01u8, 0x02, 0x03, 0x04];
        let buf = frame(&body);
        let outcome = decode_inbound_frame(&buf, ConnectionState::Play, &limits).unwrap();
        match outcome {
            DecodeOutcome::Decoded { packet, consumed } => {
                assert_eq!(consumed, buf.len());
                assert_eq!(
                    packet,
                    InboundPacket::Play(bytes::Bytes::copy_from_slice(&body))
                );
                assert_eq!(packet.state(), ConnectionState::Play);
            }
            DecodeOutcome::NeedMore => panic!("expected a decoded frame"),
        }
    }

    #[test]
    fn push_past_the_buffer_ceiling_overflows() {
        // A tiny ceiling makes the overflow easy to trigger; the rejected bytes
        // must not be appended.
        let limits = ConnectionLimits::new(4, 4, 4, 4, 4); // overall max 4, +5 prefix = 9
        let mut decoder = InboundDecoder::new(limits);
        let err = decoder.push(&[0u8; 16]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::BufferOverflow {
                buffered: 16,
                max: 9,
            }
        ));
        assert_eq!(err.disconnect_class(), DisconnectClass::FrameTooLarge);
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn packet_state_helper_matches_variant() {
        let limits = ConnectionLimits::default();
        let buf = frame(&handshake_body(2));
        let DecodeOutcome::Decoded { packet, .. } =
            decode_inbound_frame(&buf, ConnectionState::Handshaking, &limits).unwrap()
        else {
            panic!("expected decoded frame");
        };
        assert_eq!(packet.state(), ConnectionState::Handshaking);
    }

    /// Wraps a raw `id + body` packet in the compressed framing for `compression`,
    /// then the outer length prefix, producing a full on-wire frame.
    fn compressed_frame(compression: &CompressionState, id_body: &[u8]) -> Vec<u8> {
        let mut frame_body = BytesMut::new();
        compression.compress(id_body, &mut frame_body).unwrap();
        frame(&frame_body)
    }

    #[test]
    fn compressed_path_round_trips_a_handshake() {
        // Threshold 0 compresses every packet, so the frame carries a real zlib
        // stream that the decode path must inflate before dispatch.
        let compression = CompressionState::enabled(0);
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        decoder
            .push(&compressed_frame(&compression, &handshake_body(2)))
            .unwrap();

        let packet = decoder
            .next_packet_compressed(ConnectionState::Handshaking, &compression)
            .unwrap();
        assert!(matches!(packet, Some(InboundPacket::Handshake(_))));
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn compressed_path_disabled_matches_plain_decode() {
        // A disabled compression state is a verbatim pass-through, so the same
        // plain frame decodes identically through the compressed entry point.
        let compression = CompressionState::disabled();
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        decoder.push(&frame(&handshake_body(1))).unwrap();
        let packet = decoder
            .next_packet_compressed(ConnectionState::Handshaking, &compression)
            .unwrap();
        assert!(matches!(packet, Some(InboundPacket::Handshake(_))));
    }

    #[test]
    fn compressed_path_partial_frame_needs_more() {
        let compression = CompressionState::enabled(0);
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        // Declare a 10-byte frame but supply only part of it.
        let mut buf = Vec::new();
        write_var_int(&mut buf, 10);
        buf.extend_from_slice(&[0x00, 0x01]);
        decoder.push(&buf).unwrap();
        assert!(decoder
            .next_packet_compressed(ConnectionState::Handshaking, &compression)
            .unwrap()
            .is_none());
    }

    #[test]
    fn compressed_path_malformed_data_length_is_rejected() {
        // A fully present frame whose data-length prefix is an overlong VarInt:
        // a compression-layer failure, surfaced (and classified) as such.
        let compression = CompressionState::enabled(0);
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        decoder
            .push(&frame(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]))
            .unwrap();
        let err = decoder
            .next_packet_compressed(ConnectionState::Handshaking, &compression)
            .unwrap_err();
        assert!(matches!(err, FrameDecodeError::Compression(_)));
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn compressed_path_oversized_frame_is_rejected() {
        // The outer frame cap still applies before decompression: an 8 KiB frame
        // in the handshake state (4 KiB cap) is rejected as a framing failure.
        let compression = CompressionState::enabled(0);
        let mut decoder = InboundDecoder::new(ConnectionLimits::default());
        let mut buf = Vec::new();
        write_var_int(&mut buf, 8 * 1024);
        decoder.push(&buf).unwrap();
        let err = decoder
            .next_packet_compressed(ConnectionState::Handshaking, &compression)
            .unwrap_err();
        assert!(matches!(
            err,
            FrameDecodeError::Decode(DecodeError::FrameTooLarge { .. })
        ));
        assert_eq!(err.disconnect_class(), DisconnectClass::FrameTooLarge);
    }
}
