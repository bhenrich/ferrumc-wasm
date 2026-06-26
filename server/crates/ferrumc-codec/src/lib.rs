#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Bounded binary primitives: `VarInt`, `VarLong`, [`BoundedReader`],
//! [`BoundedString`], [`BoundedBytes`] and [`FrameLengthReader`].
//!
//! Everything here is written to survive hostile input. Decoders never trust a
//! length they were handed: they bound-check before allocating, refuse to
//! overread their input, and reject `VarInt`/`VarLong` values that run past
//! their byte budget. All multi-byte integers use big-endian order, matching
//! the Minecraft Java protocol (1.21.8).

// ferrumc-core is a mandated dependency that currently exports no usable items.
// Bind it anonymously so the link is intentional rather than dead weight.
use ferrumc_core as _;

mod blob;
mod error;
mod frame;
mod reader;
mod string;
mod writer;

pub use blob::BoundedBytes;
pub use error::{CodecError, Result};
pub use frame::FrameLengthReader;
pub use reader::BoundedReader;
pub use string::BoundedString;
pub use writer::{write_var_int, write_var_long};

/// Mask selecting the seven data bits carried by each `VarInt`/`VarLong` byte.
pub(crate) const SEGMENT_BITS: u8 = 0x7F;

/// The high bit of a `VarInt`/`VarLong` byte: set means "another byte follows".
pub(crate) const CONTINUE_BIT: u8 = 0x80;

/// Maximum number of bytes a 32-bit `VarInt` may occupy on the wire.
pub(crate) const MAX_VAR_INT_BYTES: usize = 5;

/// Maximum number of bytes a 64-bit `VarLong` may occupy on the wire.
pub(crate) const MAX_VAR_LONG_BYTES: usize = 10;
