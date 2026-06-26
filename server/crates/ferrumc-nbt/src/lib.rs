#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! NBT (Named Binary Tag) decoding and encoding hardened against hostile input.
//!
//! The format is big-endian. Decoding starts from raw bytes plus an
//! [`NbtLimits`] budget and yields an [`NbtTag`] tree; every limit
//! (`max_depth`, `max_bytes`, `max_list_len`, `max_string_bytes`) is enforced
//! before the corresponding allocation, so a malicious length can never drive a
//! large reservation. Encoding turns an [`NbtTag`] back into bytes the matching
//! reader accepts.
//!
//! # Roots
//!
//! * [`read_named_root`] / [`write_named_root`] — the file form, where the root
//!   `TAG_Compound` is preceded by a name.
//! * [`read_network_root`] / [`write_network_root`] — the 1.20.2+ network form,
//!   where the root `TAG_Compound` has no name.
//!
//! Both roots must be a `TAG_Compound`, and the readers reject trailing bytes.
//! To decode a root embedded in a larger buffer (a packet with trailing
//! fields), use [`read_named_root_with_consumed`] /
//! [`read_network_root_with_consumed`], which parse one root, report the bytes
//! it consumed, and leave the remainder for the caller.
//!
//! # Design decisions
//!
//! * **Depth accounting.** The root compound is depth 1. Descending into a
//!   nested `TAG_Compound` or `TAG_List` adds one level; arrays do not nest and
//!   never contribute. `depth > max_depth` is [`NbtError::DepthExceeded`].
//! * **Total bytes.** For the whole-slice readers, `max_bytes` caps the length
//!   of the input slice itself, so it transitively bounds every read and
//!   allocation underneath it. For the `*_with_consumed` variants — where the
//!   slice also holds trailing data — `max_bytes` instead bounds the bytes the
//!   root is allowed to consume: the reader sees at most that many bytes, so a
//!   root that reads further is rejected at end of input.
//! * **Array limits.** Byte, int, and long arrays share the `max_list_len` cap
//!   with lists rather than carrying a separate knob.
//! * **`UTF-8` handling.** Strings are validated as strict `UTF-8`. Java's
//!   Modified `UTF-8` (the `0xC0 0x80` encoding of `NUL` and surrogate-pair
//!   encodings of astral characters) is rejected as [`NbtError::InvalidUtf8`]
//!   rather than silently accepted. This is sufficient for the current
//!   milestone, whose strings are standard `UTF-8`.

mod error;
mod limits;
mod read;
mod tag;
mod write;

pub use error::{NbtError, Result};
pub use limits::NbtLimits;
pub use read::{
    read_named_root, read_named_root_with_consumed, read_network_root,
    read_network_root_with_consumed,
};
pub use tag::{NbtCompound, NbtTag};
pub use write::{write_named_root, write_network_root};
