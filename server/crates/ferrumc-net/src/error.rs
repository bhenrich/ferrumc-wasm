//! Decode/encode error taxonomies and their [`DisconnectClass`] mapping.

use ferrumc_proto::ProtoError;

use crate::state::ConnectionState;

/// How a connection should be torn down in response to a [`DecodeError`].
///
/// M09 (the live connection layer) maps each class to a concrete action: a
/// resource-exhaustion close, a protocol-violation kick, or a malformed-input
/// kick. Grouping the many [`DecodeError`] variants into a few classes keeps
/// that decision table small. The enum is `#[non_exhaustive]`: new classes may
/// be added without a breaking change, so downstream `match`es must include a
/// wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DisconnectClass {
    /// The peer advertised, or accumulated, more bytes than a size cap allows.
    /// A resource-exhaustion attempt: close without ceremony.
    FrameTooLarge,
    /// The peer broke the protocol contract: an unknown packet for the state, or
    /// trailing bytes after a complete packet. Kick on protocol violation.
    ProtocolViolation,
    /// A frame or packet body was structurally malformed: a bad length `VarInt`,
    /// a negative length, or a body that failed to decode.
    Malformed,
}

/// Every way decoding an inbound frame can fail.
///
/// "Need more bytes" is deliberately *not* a variant: an incomplete frame is a
/// normal, recoverable condition reported as [`crate::DecodeOutcome::NeedMore`],
/// never an error. Each variant here classifies a genuine, fatal failure and
/// carries the connection [`ConnectionState`] it occurred in for diagnostics.
///
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// A frame's declared length exceeded the cap for its state.
    #[error("frame length {length} exceeds the {state:?} maximum of {max} bytes")]
    FrameTooLarge {
        /// The state whose cap was exceeded.
        state: ConnectionState,
        /// The declared frame body length.
        length: usize,
        /// The configured maximum for `state`.
        max: usize,
    },

    /// The accumulation buffer would exceed its ceiling before a full frame
    /// became available.
    #[error("inbound buffer of {buffered} bytes exceeds the maximum of {max}")]
    BufferOverflow {
        /// The buffer size that would have resulted.
        buffered: usize,
        /// The configured inbound buffer ceiling.
        max: usize,
    },

    /// The frame-length prefix was not a valid `VarInt` (it ran past the 5-byte
    /// budget).
    #[error("malformed frame-length VarInt")]
    BadLengthVarInt,

    /// The frame-length prefix decoded to a negative value.
    #[error("frame-length prefix was negative: {length}")]
    NegativeLength {
        /// The offending decoded length.
        length: i32,
    },

    /// The frame carried a packet id with no packet in the current state.
    #[error("unknown packet id {id:#04x} for state {state:?}")]
    UnknownPacket {
        /// The state the id was decoded in.
        state: ConnectionState,
        /// The offending wire packet id.
        id: i32,
    },

    /// The frame's packet body failed to decode: a short read against a fully
    /// present frame, a bad field, or a bad `VarInt`/length inside the body.
    #[error("malformed packet body in state {state:?}")]
    MalformedBody {
        /// The state the body was decoded in.
        state: ConnectionState,
    },

    /// The frame contained bytes left over after a packet decoded fully.
    #[error("{trailing} trailing byte(s) after packet in state {state:?}")]
    TrailingBytes {
        /// The state the packet was decoded in.
        state: ConnectionState,
        /// How many bytes remained unconsumed inside the frame.
        trailing: usize,
    },
}

impl DecodeError {
    /// Classifies this error into the [`DisconnectClass`] M09 should act on.
    pub fn disconnect_class(&self) -> DisconnectClass {
        match self {
            Self::FrameTooLarge { .. } | Self::BufferOverflow { .. } => {
                DisconnectClass::FrameTooLarge
            }
            Self::UnknownPacket { .. } | Self::TrailingBytes { .. } => {
                DisconnectClass::ProtocolViolation
            }
            Self::BadLengthVarInt | Self::NegativeLength { .. } | Self::MalformedBody { .. } => {
                DisconnectClass::Malformed
            }
        }
    }

    /// Maps a `ferrumc-proto` decode failure into a [`DecodeError`] tagged with
    /// `state`.
    ///
    /// An unknown packet id stays distinguishable as
    /// [`UnknownPacket`](Self::UnknownPacket); every byte- or NBT-level failure
    /// collapses into [`MalformedBody`](Self::MalformedBody), since by this point
    /// the whole frame is present and any short read means the body — not the
    /// stream — is malformed.
    pub(crate) fn from_proto(state: ConnectionState, err: &ProtoError) -> Self {
        match err {
            ProtoError::UnknownPacketId { id, .. } => Self::UnknownPacket { state, id: *id },
            _ => Self::MalformedBody { state },
        }
    }
}

/// Every way encoding an outbound packet into a frame can fail.
///
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// The serialized frame exceeded the cap for its state. Outbound data is the
    /// server's own, so this signals a server-side bug (an oversized payload)
    /// rather than hostile input.
    #[error("encoded frame of {length} bytes exceeds the {state:?} maximum of {max} bytes")]
    FrameTooLarge {
        /// The state whose cap was exceeded.
        state: ConnectionState,
        /// The serialized frame body length.
        length: usize,
        /// The configured maximum for `state`.
        max: usize,
    },

    /// The packet body failed to serialize (surfaced from `ferrumc-proto`, e.g.
    /// an NBT encode failure).
    #[error(transparent)]
    Proto(#[from] ProtoError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_failures_map_to_frame_too_large() {
        assert_eq!(
            DecodeError::FrameTooLarge {
                state: ConnectionState::Handshaking,
                length: 9000,
                max: 4096,
            }
            .disconnect_class(),
            DisconnectClass::FrameTooLarge
        );
        assert_eq!(
            DecodeError::BufferOverflow {
                buffered: 10,
                max: 5,
            }
            .disconnect_class(),
            DisconnectClass::FrameTooLarge
        );
    }

    #[test]
    fn protocol_failures_map_to_protocol_violation() {
        assert_eq!(
            DecodeError::UnknownPacket {
                state: ConnectionState::Login,
                id: 0x7F,
            }
            .disconnect_class(),
            DisconnectClass::ProtocolViolation
        );
        assert_eq!(
            DecodeError::TrailingBytes {
                state: ConnectionState::Status,
                trailing: 3,
            }
            .disconnect_class(),
            DisconnectClass::ProtocolViolation
        );
    }

    #[test]
    fn structural_failures_map_to_malformed() {
        assert_eq!(
            DecodeError::BadLengthVarInt.disconnect_class(),
            DisconnectClass::Malformed
        );
        assert_eq!(
            DecodeError::NegativeLength { length: -1 }.disconnect_class(),
            DisconnectClass::Malformed
        );
        assert_eq!(
            DecodeError::MalformedBody {
                state: ConnectionState::Configuration,
            }
            .disconnect_class(),
            DisconnectClass::Malformed
        );
    }

    #[test]
    fn proto_unknown_id_is_preserved() {
        let proto = ProtoError::UnknownPacketId {
            state: ferrumc_proto::State::Login,
            direction: ferrumc_proto::Direction::Serverbound,
            id: 0x42,
        };
        assert_eq!(
            DecodeError::from_proto(ConnectionState::Login, &proto),
            DecodeError::UnknownPacket {
                state: ConnectionState::Login,
                id: 0x42,
            }
        );
    }
}
