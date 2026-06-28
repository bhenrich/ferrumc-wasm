//! The benchmark group builders.
//!
//! Each submodule exposes `benchmarks(&BenchConfig) -> Vec<BenchResult>`, which
//! constructs and runs every benchmark in that group (honouring the config's
//! warmup/iteration counts and optional name/group filter). [`crate::run_all`]
//! concatenates them.

pub mod items;
pub mod placement;
pub mod sim;
pub mod world;
