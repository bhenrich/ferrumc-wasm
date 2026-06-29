#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Pure world model: the chunk column, its block storage, and a flat-world
//! generator.
//!
//! This crate has no threads, no database, and no packets. It owns the value
//! types for a chunk's blocks, bottom-up:
//!
//! - [`BlockStateId`] — a newtype over the protocol block-state id.
//! - [`PackedArray`] — a bit-packed array of fixed-width entries (Minecraft
//!   chunk-storage layout).
//! - [`PalettedContainer`] — block-state storage with an automatically promoted
//!   palette ([`ContainerKind::Single`] → [`ContainerKind::Indirect`] →
//!   [`ContainerKind::Direct`]).
//! - [`ChunkSection`] — a 16x16x16 block container indexed by
//!   [`ferrumc_math::LocalBlockPos`].
//! - [`Chunk`] — a full-height stack of [`SECTION_COUNT`] sections spanning the
//!   overworld, addressed by absolute [`ferrumc_math::BlockPos`], with
//!   [`DirtySections`] tracking, a per-column [`Heightmap`], a bounded
//!   [`BlockEntity`] map (signs, ...), and a documented lighting placeholder
//!   ([`ChunkLight`]).
//! - [`FlatWorldGenerator`] — deterministically fills a [`Chunk`] with a flat
//!   overworld profile (bedrock floor, stone fill, dirt, grass surface, air).
//!
//! The network encoder turns a [`Chunk`] into the wire bytes a 1.21.8 client
//! expects: [`encode_chunk_section_data`] for the paletted section blob,
//! [`pack_motion_blocking_heightmap`] for the heightmap long array, and
//! [`ChunkLightData`] for the light masks and full-bright sky arrays.

mod block_entity;
mod block_state;
mod chunk;
mod chunk_section;
mod dirty;
mod error;
mod generator;
mod heightmap;
mod network;
mod packed_array;
mod paletted_container;

pub use block_entity::{
    is_chest_state, sign_kind_for_state, BlockEntity, ChestInventory, Sign, SignFace, SignKind,
    CHEST_SLOTS, SIGN_LINES,
};
pub use block_state::BlockStateId;
pub use chunk::{Chunk, ChunkLight, MAX_BLOCK_ENTITIES, SECTION_COUNT};
pub use chunk_section::{ChunkSection, SECTION_VOLUME};
pub use dirty::DirtySections;
pub use error::WorldError;
pub use generator::FlatWorldGenerator;
pub use heightmap::{Heightmap, HeightmapKind};
pub use network::{
    encode_chunk_section_data, pack_motion_blocking_heightmap, ChunkLightData, LIGHT_SECTION_BYTES,
    LIGHT_SECTION_COUNT,
};
pub use packed_array::PackedArray;
pub use paletted_container::{ContainerKind, PalettedContainer};
