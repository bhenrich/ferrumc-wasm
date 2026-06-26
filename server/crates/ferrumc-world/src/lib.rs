#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Pure world model: chunk-section block storage with a paletted container.
//!
//! This crate has no threads, no database, and no packets. It owns the value
//! types for a chunk section's blocks:
//!
//! - [`BlockStateId`] — a newtype over the protocol block-state id.
//! - [`PackedArray`] — a bit-packed array of fixed-width entries (Minecraft
//!   chunk-storage layout).
//! - [`PalettedContainer`] — block-state storage with an automatically promoted
//!   palette ([`ContainerKind::Single`] → [`ContainerKind::Indirect`] →
//!   [`ContainerKind::Direct`]).
//! - [`ChunkSection`] — a 16x16x16 block container indexed by
//!   [`ferrumc_math::LocalBlockPos`].

mod block_state;
mod chunk_section;
mod error;
mod packed_array;
mod paletted_container;

pub use block_state::BlockStateId;
pub use chunk_section::{ChunkSection, SECTION_VOLUME};
pub use error::WorldError;
pub use packed_array::PackedArray;
pub use paletted_container::{ContainerKind, PalettedContainer};
