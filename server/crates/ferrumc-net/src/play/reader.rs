//! [`PlayReader`]: decodes serverbound play packets from the inbound byte
//! stream, applying the per-connection packet budget.
//!
//! The reader builds on the M08 framing types: it owns an [`InboundDecoder`]
//! driven in the [`Play`](ConnectionState::Play) state, so a frame is extracted
//! and size-capped exactly as in every other phase. The Play-specific step is
//! the typed dispatch — the raw frame body is decoded into a
//! [`ServerboundPlayPacket`] — followed by a [`PacketBudget`] charge that
//! classifies the frame as within or over budget. It performs no I/O and never
//! mutates world or simulation state.

use std::time::Instant;

use ferrumc_codec::BoundedReader;
use ferrumc_proto::generated::play::ServerboundPlayPacket;

use crate::compression::CompressionState;
use crate::error::{DecodeError, FrameDecodeError};
use crate::inbound::{InboundDecoder, InboundPacket};
use crate::limits::ConnectionLimits;
use crate::state::ConnectionState;

use super::budget::{BudgetStatus, PacketBudget};
use super::metrics::PlayMetrics;

/// A decoded serverbound play packet together with its budget classification.
#[derive(Debug, Clone, PartialEq)]
pub struct InboundPlayPacket {
    packet: ServerboundPlayPacket,
    budget_status: BudgetStatus,
}

impl InboundPlayPacket {
    /// The decoded packet.
    pub fn packet(&self) -> &ServerboundPlayPacket {
        &self.packet
    }

    /// Whether this frame was within or over the connection's packet budget.
    pub fn budget_status(&self) -> BudgetStatus {
        self.budget_status
    }

    /// `true` when this frame exceeded the packet budget.
    pub fn is_over_budget(&self) -> bool {
        self.budget_status.is_over_budget()
    }

    /// Consumes the wrapper and returns the owned packet.
    pub fn into_packet(self) -> ServerboundPlayPacket {
        self.packet
    }
}

/// Decodes serverbound play frames from an accumulating byte stream and charges
/// each against a token-bucket budget.
///
/// Feed bytes read off the socket with [`push`](Self::push), then pull decoded
/// packets with [`next_packet`](Self::next_packet) (or
/// [`next_packet_compressed`](Self::next_packet_compressed) once compression is
/// negotiated). Each successful pull returns an [`InboundPlayPacket`] tagged with
/// its [`BudgetStatus`]; the over-budget flag is advisory — the reader keeps
/// decoding so the caller can decide whether to throttle or disconnect.
///
/// The inbound buffer is bounded by [`ConnectionLimits`] exactly as in the M08
/// decoder, so a peer that streams bytes without completing a frame is cut off.
#[derive(Debug)]
pub struct PlayReader {
    decoder: InboundDecoder,
    budget: PacketBudget,
    metrics: PlayMetrics,
}

impl PlayReader {
    /// Creates a reader enforcing `limits` and rate-limiting with `budget`.
    pub fn new(limits: ConnectionLimits, budget: PacketBudget) -> Self {
        Self {
            decoder: InboundDecoder::new(limits),
            budget,
            metrics: PlayMetrics::new(),
        }
    }

    /// The number of buffered bytes not yet consumed by a decoded frame.
    pub fn buffered_len(&self) -> usize {
        self.decoder.buffered_len()
    }

    /// This reader's packet budget.
    pub fn budget(&self) -> &PacketBudget {
        &self.budget
    }

    /// This reader's metrics counters.
    pub fn metrics(&self) -> &PlayMetrics {
        &self.metrics
    }

    /// Appends freshly read bytes to the accumulation buffer.
    ///
    /// Returns [`DecodeError::BufferOverflow`] if the append would exceed the
    /// inbound buffer ceiling, leaving the buffer untouched so the caller can
    /// disconnect.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        self.decoder.push(bytes)
    }

    /// Decodes the next serverbound play packet, charging it against the budget
    /// as of `now`.
    ///
    /// Returns `Ok(Some(_))` when a full frame decoded, `Ok(None)` when more
    /// bytes are needed (buffer left intact), or a [`FrameDecodeError`] on
    /// genuine corruption.
    pub fn next_packet(
        &mut self,
        now: Instant,
    ) -> Result<Option<InboundPlayPacket>, FrameDecodeError> {
        let raw = self.decoder.next_packet(ConnectionState::Play)?;
        self.finish(raw, now)
    }

    /// Decodes the next serverbound play packet through the compression layer,
    /// charging it against the budget as of `now`.
    ///
    /// The post-`SetCompression` counterpart to [`next_packet`](Self::next_packet):
    /// the frame body is inflated by `compression` (a verbatim pass-through when
    /// disabled) before typed dispatch. Same return contract as
    /// [`next_packet`](Self::next_packet).
    pub fn next_packet_compressed(
        &mut self,
        now: Instant,
        compression: &CompressionState,
    ) -> Result<Option<InboundPlayPacket>, FrameDecodeError> {
        let raw = self
            .decoder
            .next_packet_compressed(ConnectionState::Play, compression)?;
        self.finish(raw, now)
    }

    /// Turns a raw play frame (if one was produced) into a typed, budget-charged
    /// [`InboundPlayPacket`], recording the relevant metrics.
    fn finish(
        &mut self,
        raw: Option<InboundPacket>,
        now: Instant,
    ) -> Result<Option<InboundPlayPacket>, FrameDecodeError> {
        let body = match raw {
            None => return Ok(None),
            Some(InboundPacket::Play(body)) => body,
            // The decoder was driven in the Play state, so every yielded frame is
            // a raw play body. Any other variant is impossible; treat it
            // defensively as malformed rather than panicking.
            Some(_) => {
                return Err(FrameDecodeError::Decode(DecodeError::MalformedBody {
                    state: ConnectionState::Play,
                }))
            }
        };

        let packet = decode_serverbound_play(&body)?;
        let budget_status = self.budget.charge(now, 1);
        self.metrics.record_frame_decoded(body.len());
        if budget_status.is_over_budget() {
            self.metrics.record_over_budget();
        }
        Ok(Some(InboundPlayPacket {
            packet,
            budget_status,
        }))
    }
}

/// Decodes one complete play frame body into a typed [`ServerboundPlayPacket`].
///
/// `body` is the exact frame body (length prefix already stripped, and inflated
/// if compression was active): a `VarInt` packet id followed by the packet
/// fields. Reads the id, dispatches to `ferrumc-proto`, and rejects any trailing
/// bytes. Because the whole frame is present, a short read is a
/// [`MalformedBody`](DecodeError::MalformedBody), an unknown id is an
/// [`UnknownPacket`](DecodeError::UnknownPacket), and leftover bytes are
/// [`TrailingBytes`](DecodeError::TrailingBytes) — none is ever "need more".
pub(crate) fn decode_serverbound_play(body: &[u8]) -> Result<ServerboundPlayPacket, DecodeError> {
    let state = ConnectionState::Play;
    let mut reader = BoundedReader::new(body);
    let id = reader
        .read_var_int()
        .map_err(|_| DecodeError::MalformedBody { state })?;
    let packet = ServerboundPlayPacket::decode(id, &mut reader)
        .map_err(|err| DecodeError::from_proto(state, &err))?;
    let trailing = reader.remaining();
    if trailing != 0 {
        return Err(DecodeError::TrailingBytes { state, trailing });
    }
    Ok(packet)
}

#[cfg(test)]
mod tests {
    // Decoded position coordinates are exact, representable values, so exact
    // float comparison is intentional here.
    #![allow(clippy::float_cmp)]

    use bytes::{BufMut, BytesMut};

    use ferrumc_codec::write_var_int;
    use ferrumc_proto::generated::play::{ServerboundKeepAlive, SetPlayerPosition};

    use super::*;
    use crate::error::DisconnectClass;

    /// Wraps a body in a `VarInt` length prefix, producing a full frame.
    fn frame(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_var_int(&mut out, i32::try_from(body.len()).unwrap());
        out.extend_from_slice(body);
        out
    }

    /// A serverbound `KeepAlive` body: id `0x1b` + an i64.
    fn keep_alive_body(id: i64) -> Vec<u8> {
        let mut body = Vec::new();
        write_var_int(&mut body, ServerboundKeepAlive::PACKET_ID);
        body.put_i64(id);
        body
    }

    /// A serverbound `SetPlayerPosition` body: id `0x1d` + 3×f64 + flags.
    fn position_body(x: f64) -> Vec<u8> {
        let mut body = Vec::new();
        write_var_int(&mut body, SetPlayerPosition::PACKET_ID);
        body.put_f64(x);
        body.put_f64(64.0);
        body.put_f64(0.0);
        body.put_u8(0);
        body
    }

    #[test]
    fn decodes_a_keep_alive_within_budget() {
        let now = Instant::now();
        let mut reader = PlayReader::new(
            ConnectionLimits::default(),
            PacketBudget::with_defaults(now),
        );
        reader.push(&frame(&keep_alive_body(42))).unwrap();

        let decoded = reader.next_packet(now).unwrap().expect("a frame");
        assert_eq!(
            decoded.packet(),
            &ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(42))
        );
        assert!(!decoded.is_over_budget());
        assert_eq!(reader.metrics().frames_decoded(), 1);
        assert!(reader.metrics().bytes_in() > 0);
        assert_eq!(reader.metrics().over_budget(), 0);
    }

    #[test]
    fn partial_frame_needs_more() {
        let now = Instant::now();
        let mut reader = PlayReader::new(
            ConnectionLimits::default(),
            PacketBudget::with_defaults(now),
        );
        // Declare 10 bytes but supply only 2.
        let mut buf = Vec::new();
        write_var_int(&mut buf, 10);
        buf.extend_from_slice(&[0x00, 0x01]);
        reader.push(&buf).unwrap();
        assert!(reader.next_packet(now).unwrap().is_none());
    }

    #[test]
    fn over_budget_frames_are_flagged_not_errored() {
        let now = Instant::now();
        // A budget of 1 token, no refill within the same instant.
        let budget = PacketBudget::new(now, 1.0, 1.0);
        let mut reader = PlayReader::new(ConnectionLimits::default(), budget);
        // Two pipelined keep-alive frames.
        reader.push(&frame(&keep_alive_body(1))).unwrap();
        reader.push(&frame(&keep_alive_body(2))).unwrap();

        let first = reader.next_packet(now).unwrap().expect("first frame");
        assert!(first.budget_status().is_within_budget());
        let second = reader.next_packet(now).unwrap().expect("second frame");
        assert!(second.is_over_budget());
        // The over-budget frame is still decoded and counted.
        assert_eq!(reader.metrics().frames_decoded(), 2);
        assert_eq!(reader.metrics().over_budget(), 1);
    }

    #[test]
    fn movement_frame_decodes() {
        let now = Instant::now();
        let mut reader = PlayReader::new(
            ConnectionLimits::default(),
            PacketBudget::with_defaults(now),
        );
        reader.push(&frame(&position_body(12.5))).unwrap();
        let decoded = reader.next_packet(now).unwrap().expect("a frame");
        let ServerboundPlayPacket::SetPlayerPosition(pos) = decoded.packet() else {
            panic!("expected a position packet");
        };
        assert_eq!(pos.x(), 12.5);
    }

    #[test]
    fn unknown_play_id_is_rejected() {
        // 0x77 is not a serverbound play packet id.
        let err = decode_serverbound_play(&[0x77]).unwrap_err();
        assert_eq!(
            err,
            DecodeError::UnknownPacket {
                state: ConnectionState::Play,
                id: 0x77,
            }
        );
        assert_eq!(err.disconnect_class(), DisconnectClass::ProtocolViolation);
    }

    #[test]
    fn empty_body_has_no_packet_id() {
        let err = decode_serverbound_play(&[]).unwrap_err();
        assert_eq!(
            err,
            DecodeError::MalformedBody {
                state: ConnectionState::Play,
            }
        );
    }

    #[test]
    fn truncated_body_is_malformed() {
        // KeepAlive id but only 3 of the 8 i64 bytes present.
        let mut body = Vec::new();
        write_var_int(&mut body, ServerboundKeepAlive::PACKET_ID);
        body.extend_from_slice(&[0x00, 0x01, 0x02]);
        let err = decode_serverbound_play(&body).unwrap_err();
        assert_eq!(
            err,
            DecodeError::MalformedBody {
                state: ConnectionState::Play,
            }
        );
    }

    #[test]
    fn trailing_bytes_after_packet_are_rejected() {
        // A complete KeepAlive followed by junk inside the same body.
        let mut body = keep_alive_body(9);
        body.extend_from_slice(&[0xDE, 0xAD]);
        let err = decode_serverbound_play(&body).unwrap_err();
        assert_eq!(
            err,
            DecodeError::TrailingBytes {
                state: ConnectionState::Play,
                trailing: 2,
            }
        );
        assert_eq!(err.disconnect_class(), DisconnectClass::ProtocolViolation);
    }

    #[test]
    fn bad_packet_id_varint_is_malformed() {
        // Six continuation bytes never terminate within the 5-byte VarInt budget.
        let err = decode_serverbound_play(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).unwrap_err();
        assert_eq!(
            err,
            DecodeError::MalformedBody {
                state: ConnectionState::Play,
            }
        );
    }

    #[test]
    fn oversized_play_frame_is_rejected() {
        let now = Instant::now();
        // A 1-byte play cap rejects any real frame at the framing layer.
        let limits = ConnectionLimits::new(4096, 4096, 4096, 4096, 1);
        let mut reader = PlayReader::new(limits, PacketBudget::with_defaults(now));
        let mut buf = Vec::new();
        write_var_int(&mut buf, 2);
        reader.push(&buf).unwrap();
        let err = reader.next_packet(now).unwrap_err();
        assert!(matches!(
            err,
            FrameDecodeError::Decode(DecodeError::FrameTooLarge {
                state: ConnectionState::Play,
                ..
            })
        ));
    }

    #[test]
    fn compressed_path_round_trips() {
        let now = Instant::now();
        let compression = CompressionState::enabled(0);
        let mut reader = PlayReader::new(
            ConnectionLimits::default(),
            PacketBudget::with_defaults(now),
        );
        let mut frame_body = BytesMut::new();
        compression
            .compress(&keep_alive_body(5), &mut frame_body)
            .unwrap();
        reader.push(&frame(&frame_body)).unwrap();

        let decoded = reader
            .next_packet_compressed(now, &compression)
            .unwrap()
            .expect("a frame");
        assert_eq!(
            decoded.packet(),
            &ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(5))
        );
    }
}
