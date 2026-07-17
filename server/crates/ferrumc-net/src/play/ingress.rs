//! Strict, socket-free serverbound Play ingress.
//!
//! [`PlayIngress`] is the canonical composition of bounded frame acquisition,
//! pre-decompression wire-byte admission, bounded decompression, exact typed
//! Play decode, and the existing frame-rate budget. It exposes partial input
//! separately from complete valid packet activity and becomes terminal after
//! any fatal input error; there is no skip or permissive fallback path.

use std::time::Instant;

use ferrumc_proto::generated::play::ServerboundPlayPacket;

use crate::compression::CompressionState;
use crate::error::{CompressionError, DecodeError};
use crate::inbound::InboundDecoder;
use crate::limits::ConnectionLimits;
use crate::state::ConnectionState;

use super::budget::{BudgetStatus, PacketBudget, WireByteBudget};
use super::disconnect::DisconnectReason;
use super::metrics::PlayMetrics;
use super::reader::decode_serverbound_play;

/// Liveness-relevant activity emitted by strict Play ingress.
///
/// Partial bytes are deliberately not represented: callers may report
/// [`PlayIngressPoll::PartialFrame`] to their frame-completion timer, but only
/// this event may refresh valid-packet progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlayIngressActivity {
    /// One complete frame passed bounds, admission, decompression, typed decode,
    /// and exact packet-body exhaustion.
    CompleteValidPacket,
}

/// One packet accepted by [`PlayIngress`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlayIngressPacket {
    packet: ServerboundPlayPacket,
    packet_budget_status: BudgetStatus,
    wire_bytes: usize,
}

impl PlayIngressPacket {
    /// The strictly decoded serverbound packet.
    pub fn packet(&self) -> &ServerboundPlayPacket {
        &self.packet
    }

    /// The legacy frame-rate budget classification for this valid packet.
    pub fn packet_budget_status(&self) -> BudgetStatus {
        self.packet_budget_status
    }

    /// Whether this valid packet exceeded the advisory frame-rate budget.
    pub fn is_over_packet_budget(&self) -> bool {
        self.packet_budget_status.is_over_budget()
    }

    /// The complete compressed-wire span, including the outer length prefix.
    pub fn wire_bytes(&self) -> usize {
        self.wire_bytes
    }

    /// Consumes the wrapper and returns the typed packet.
    pub fn into_packet(self) -> ServerboundPlayPacket {
        self.packet
    }
}

/// The nonfatal result of polling [`PlayIngress`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PlayIngressPoll {
    /// No bytes are currently buffered.
    Idle,
    /// Some bytes are buffered, but the first outer frame is incomplete.
    PartialFrame,
    /// One complete, strictly valid packet was admitted.
    Packet(PlayIngressPacket),
}

impl PlayIngressPoll {
    /// The valid-packet activity represented by this poll result.
    ///
    /// `Idle` and `PartialFrame` return `None`; only a successfully decoded
    /// packet may refresh a connection's valid-progress deadline.
    pub fn valid_activity(&self) -> Option<PlayIngressActivity> {
        match self {
            Self::Packet(_) => Some(PlayIngressActivity::CompleteValidPacket),
            Self::Idle | Self::PartialFrame => None,
        }
    }
}

/// A fatal strict-ingress failure.
///
/// The first error retains its exact framing, compression, or admission detail.
/// Once it is returned, the ingress is poisoned and later operations return
/// [`Terminated`](Self::Terminated) with the original disconnect reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlayIngressError {
    /// Frame acquisition or typed Play decode failed.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// Compression framing or bounded decompression failed.
    #[error(transparent)]
    Compression(#[from] CompressionError),
    /// The complete outer frame did not fit the pre-decompression byte budget.
    #[error("wire frame of {wire_bytes} bytes exceeded the pre-decompression byte budget")]
    WireBudgetExceeded {
        /// The exact outer wire span, including its length prefix.
        wire_bytes: usize,
    },
    /// The transport ended while an incomplete frame remained buffered.
    #[error("transport ended with {buffered} byte(s) of an incomplete play frame")]
    TruncatedFrame {
        /// The incomplete bytes retained at end of input.
        buffered: usize,
    },
    /// A previous fatal error already terminated this ingress.
    #[error("play ingress is already terminated: {reason:?}")]
    Terminated {
        /// The disconnect reason classified from the original fatal error.
        reason: DisconnectReason,
    },
}

impl PlayIngressError {
    /// The connection-level reason for terminating on this error.
    pub fn disconnect_reason(&self) -> DisconnectReason {
        match self {
            Self::Decode(error) => {
                DisconnectReason::from_disconnect_class(error.disconnect_class())
            }
            Self::Compression(error) => {
                DisconnectReason::from_disconnect_class(error.disconnect_class())
            }
            Self::WireBudgetExceeded { .. } => DisconnectReason::BudgetExceeded,
            Self::TruncatedFrame { .. } => DisconnectReason::MalformedPacket,
            Self::Terminated { reason } => *reason,
        }
    }
}

/// Canonical bounded serverbound Play packet ingress.
///
/// The processing order is fixed:
///
/// 1. locate one complete outer frame and enforce its Play-state size cap;
/// 2. charge its exact compressed-wire span against [`WireByteBudget`];
/// 3. apply bounded [`CompressionState`] decompression;
/// 4. decode one typed Play packet and reject trailing body bytes;
/// 5. charge the existing advisory frame-rate [`PacketBudget`];
/// 6. emit [`PlayIngressPoll::Packet`], the sole valid-activity event.
///
/// `Idle` and `PartialFrame` are nonfatal. Every error is fatal and permanently
/// poisons the ingress so a following pipelined frame cannot be interpreted
/// after malformed input.
#[derive(Debug)]
pub struct PlayIngress {
    decoder: InboundDecoder,
    compression: CompressionState,
    packet_budget: PacketBudget,
    wire_budget: WireByteBudget,
    metrics: PlayMetrics,
    terminal_reason: Option<DisconnectReason>,
}

impl PlayIngress {
    /// Creates strict Play ingress with explicit framing, compression, and
    /// admission policies.
    pub fn new(
        limits: ConnectionLimits,
        compression: CompressionState,
        packet_budget: PacketBudget,
        wire_budget: WireByteBudget,
    ) -> Self {
        Self {
            decoder: InboundDecoder::new(limits),
            compression,
            packet_budget,
            wire_budget,
            metrics: PlayMetrics::new(),
            terminal_reason: None,
        }
    }

    /// Appends newly received compressed-wire bytes.
    ///
    /// # Errors
    ///
    /// A bounded-buffer overflow is returned as [`PlayIngressError::Decode`] and
    /// terminates the ingress. Calls after any fatal error return
    /// [`PlayIngressError::Terminated`].
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), PlayIngressError> {
        self.ensure_open()?;
        match self.decoder.push(bytes) {
            Ok(()) => Ok(()),
            Err(error) => {
                let error = PlayIngressError::Decode(error);
                Err(self.terminate(error))
            }
        }
    }

    /// Polls the first buffered outer frame as of `now`.
    ///
    /// `Idle` means no bytes are buffered. `PartialFrame` means bytes are
    /// buffered but do not yet form a complete frame; neither state charges a
    /// budget or emits valid activity. A complete frame is charged exactly once
    /// before decompression.
    ///
    /// # Errors
    ///
    /// Any framing, wire-budget, compression, or typed-body failure terminates
    /// the ingress. Later calls return [`PlayIngressError::Terminated`].
    pub fn poll(&mut self, now: Instant) -> Result<PlayIngressPoll, PlayIngressError> {
        self.ensure_open()?;
        let raw = match self.decoder.acquire_raw_frame(ConnectionState::Play) {
            Ok(raw) => raw,
            Err(error) => {
                let error = PlayIngressError::Decode(error);
                return Err(self.terminate(error));
            }
        };
        let Some(raw) = raw else {
            return if self.decoder.buffered_len() == 0 {
                Ok(PlayIngressPoll::Idle)
            } else {
                Ok(PlayIngressPoll::PartialFrame)
            };
        };

        let wire_bytes = raw.wire_len();
        if self.wire_budget.admit(now, wire_bytes).is_over_budget() {
            let error = PlayIngressError::WireBudgetExceeded { wire_bytes };
            return Err(self.terminate(error));
        }

        let body = match self.compression.decompress(raw.body()) {
            Ok(body) => body,
            Err(error) => {
                let error = PlayIngressError::Compression(error);
                return Err(self.terminate(error));
            }
        };
        let packet = match decode_serverbound_play(&body) {
            Ok(packet) => packet,
            Err(error) => {
                let error = PlayIngressError::Decode(error);
                return Err(self.terminate(error));
            }
        };

        let packet_budget_status = self.packet_budget.charge(now, 1);
        self.metrics.record_frame_decoded(body.len());
        if packet_budget_status.is_over_budget() {
            self.metrics.record_over_budget();
        }
        Ok(PlayIngressPoll::Packet(PlayIngressPacket {
            packet,
            packet_budget_status,
            wire_bytes,
        }))
    }

    /// Reports transport EOF after the caller has drained every complete frame.
    ///
    /// Clean EOF with no buffered bytes succeeds. EOF with an incomplete frame
    /// is a typed truncation error and terminates the ingress.
    ///
    /// # Errors
    ///
    /// Returns [`PlayIngressError::TruncatedFrame`] for buffered partial input,
    /// or [`PlayIngressError::Terminated`] after an earlier fatal error.
    pub fn end_of_input(&mut self) -> Result<(), PlayIngressError> {
        self.ensure_open()?;
        let buffered = self.decoder.buffered_len();
        if buffered == 0 {
            return Ok(());
        }
        let error = PlayIngressError::TruncatedFrame { buffered };
        Err(self.terminate(error))
    }

    /// The number of received bytes not yet consumed by a complete frame.
    pub fn buffered_len(&self) -> usize {
        self.decoder.buffered_len()
    }

    /// The fixed compression policy used to interpret outer frame bodies.
    pub fn compression(&self) -> CompressionState {
        self.compression
    }

    /// The advisory valid-frame rate budget.
    pub fn packet_budget(&self) -> &PacketBudget {
        &self.packet_budget
    }

    /// The fatal pre-decompression wire-byte budget.
    pub fn wire_budget(&self) -> &WireByteBudget {
        &self.wire_budget
    }

    /// Strict ingress counters.
    pub fn metrics(&self) -> &PlayMetrics {
        &self.metrics
    }

    /// Whether a fatal error has permanently poisoned this ingress.
    pub fn is_terminated(&self) -> bool {
        self.terminal_reason.is_some()
    }

    /// Rejects work after a prior fatal error.
    fn ensure_open(&self) -> Result<(), PlayIngressError> {
        match self.terminal_reason {
            Some(reason) => Err(PlayIngressError::Terminated { reason }),
            None => Ok(()),
        }
    }

    /// Records the first terminal classification and returns its detailed error.
    fn terminate(&mut self, error: PlayIngressError) -> PlayIngressError {
        self.terminal_reason = Some(error.disconnect_reason());
        error
    }
}
