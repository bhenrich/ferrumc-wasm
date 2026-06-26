//! The [`ProtoError`] taxonomy for packet decoding and encoding.

use ferrumc_codec::CodecError;
use ferrumc_nbt::NbtError;

use crate::{Direction, State};

/// Every way decoding or encoding a packet can fail.
///
/// Lower-level byte and NBT failures arrive wrapped (via [`From`]) in
/// [`ProtoError::Codec`] / [`ProtoError::Nbt`], so a caller can still tell a
/// truncated frame apart from an unknown packet id. The enum is
/// `#[non_exhaustive]`: new failure modes may be added without a breaking
/// change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtoError {
    /// A frame carried a packet id that has no packet in the given state and
    /// direction. The connection should be torn down.
    #[error("unknown packet id {id:#04x} for {state:?} {direction:?}")]
    UnknownPacketId {
        /// The connection state the id was decoded in.
        state: State,
        /// The direction the packet was travelling.
        direction: Direction,
        /// The offending wire packet id.
        id: i32,
    },

    /// A byte-level decode/encode failure surfaced from `ferrumc-codec` (short
    /// read, bad `VarInt`, oversized string/array prefix, trailing bytes, ...).
    #[error(transparent)]
    Codec(#[from] CodecError),

    /// An NBT decode/encode failure surfaced from `ferrumc-nbt`.
    #[error(transparent)]
    Nbt(#[from] NbtError),
}
