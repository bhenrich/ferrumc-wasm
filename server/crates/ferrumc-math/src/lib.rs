#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Strongly-typed coordinates and geometry for the server.
//!
//! No public API anywhere in the project should pass a raw `(i32, i32)` or
//! `(f64, f64, f64)` tuple to mean a position. Instead it uses the integer
//! coordinate newtypes ([`BlockPos`], [`ChunkPos`], [`SectionPos`],
//! [`RegionPos`], [`ShardPos`], [`LocalBlockPos`]) and the floating-point
//! geometry types ([`Vec3`], [`Aabb`]) defined here.
//!
//! # Coordinate spaces and floor division
//!
//! Minecraft nests several integer coordinate spaces, each a power-of-two
//! coarsening of the one below it:
//!
//! - a 16x16x16 cube of blocks is a [`SectionPos`];
//! - a 16x16 column of sections (one full-height column of blocks) is a
//!   [`ChunkPos`];
//! - an 8x8 square of chunks is a [`ShardPos`] (the simulation shard);
//! - a 32x32 square of chunks is a [`RegionPos`] (the Anvil region file).
//!
//! Every "zoom out" conversion uses an arithmetic right shift, which is floor
//! division for two's-complement integers. This is the whole point: it makes
//! negative coordinates map correctly. Block `x = -1` lives in chunk `x = -1`,
//! not `0`, because `-1 >> 4 == -1` (whereas truncating division would give
//! `0`). Truncating toward zero would leave a one-block-wide seam of
//! mis-assigned coordinates straddling every axis at the origin.

// ferrumc-core is the mandated shared-types dependency. The coordinate/geometry
// types are self-contained, but `WorldIntent` (in `intent`) genuinely uses
// core's `PlayerId` / `TextComponent`, so the dependency is now live.
mod aabb;
mod block_pos;
mod chunk_pos;
mod cuboid;
mod direction;
mod intent;
mod local_block_pos;
mod region_pos;
mod section_pos;
mod shard_pos;
mod vec3;

pub use aabb::Aabb;
pub use block_pos::BlockPos;
pub use chunk_pos::ChunkPos;
pub use cuboid::Cuboid;
pub use direction::Direction;
pub use intent::WorldIntent;
pub use local_block_pos::LocalBlockPos;
pub use region_pos::RegionPos;
pub use section_pos::SectionPos;
pub use shard_pos::ShardPos;
pub use vec3::Vec3;
