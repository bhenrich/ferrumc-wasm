//! The [`CodecError`] taxonomy and the crate-local [`Result`] alias.

/// A convenience alias for results produced by this crate.
pub type Result<T> = core::result::Result<T, CodecError>;

/// Every way decoding or bounded encoding can fail.
///
/// Each variant classifies a *distinct* failure mode so callers (and the
/// connection-teardown logic above them) can react precisely instead of
/// matching on an opaque string. The enum is `#[non_exhaustive]`: new failure
/// modes may be added without a breaking change, so downstream `match`es must
/// include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CodecError {
    /// A `VarInt` did not terminate within its 5-byte budget.
    #[error("VarInt exceeded the maximum of 5 bytes")]
    VarIntTooLong,

    /// A `VarLong` did not terminate within its 10-byte budget.
    #[error("VarLong exceeded the maximum of 10 bytes")]
    VarLongTooLong,

    /// A length prefix decoded to a negative value, which is never valid.
    #[error("length prefix was negative: {length}")]
    NegativeLength {
        /// The offending decoded length.
        length: i32,
    },

    /// A frame's declared length exceeded the configured cap.
    #[error("frame length {length} exceeds the maximum of {max} bytes")]
    FrameTooLarge {
        /// The declared frame length.
        length: usize,
        /// The configured maximum frame size.
        max: usize,
    },

    /// A string exceeded its character limit, or its byte prefix was longer
    /// than any string of that character limit could possibly be.
    #[error("string length {length} exceeds the maximum of {max}")]
    StringTooLong {
        /// The offending length (characters, or bytes for the pre-read check).
        length: usize,
        /// The maximum permitted (characters, or bytes for the pre-read check).
        max: usize,
    },

    /// A length-prefixed byte blob declared more bytes than its limit allows.
    #[error("byte blob length {length} exceeds the maximum of {max} bytes")]
    BytesTooLong {
        /// The declared blob length.
        length: usize,
        /// The configured maximum blob size.
        max: usize,
    },

    /// A read needed more bytes than the input had remaining.
    #[error("unexpected end of input: needed {needed} byte(s) but only {remaining} remain")]
    UnexpectedEof {
        /// The total number of bytes the read required.
        needed: usize,
        /// How many bytes were actually available.
        remaining: usize,
    },

    /// A string's bytes were not valid UTF-8.
    #[error("invalid UTF-8 in bounded string")]
    InvalidUtf8(#[from] core::str::Utf8Error),

    /// A strict read finished with bytes still unconsumed.
    #[error("{remaining} trailing byte(s) remained after decoding")]
    TrailingBytes {
        /// How many bytes were left over.
        remaining: usize,
    },
}
