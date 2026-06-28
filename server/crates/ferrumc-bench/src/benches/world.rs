//! World-model benchmarks: flat chunk generation, whole-chunk network encode,
//! and paletted-section encode (single-valued and indirect).

use ferrumc_math::ChunkPos;
use ferrumc_registry::block_state;
use ferrumc_world::{
    encode_chunk_section_data, BlockStateId, FlatWorldGenerator, PalettedContainer, SECTION_VOLUME,
};

use crate::harness::{run_benchmark, timed, Sample};
use crate::report::BenchResult;
use crate::BenchConfig;

/// Group name for these benchmarks.
const GROUP: &str = "world";

/// Direct-palette wire width for block states in 1.21.8 (`ceil(log2(state count))`).
const BLOCK_DIRECT_WIRE_BITS: u8 = 15;

/// Edge length of a chunk section (16 blocks).
const SECTION_EDGE: usize = 16;

/// Builds and runs the `world` benchmark group.
#[must_use]
pub fn benchmarks(config: &BenchConfig) -> Vec<BenchResult> {
    let mut out = Vec::new();
    let generator = FlatWorldGenerator::new();

    // (a) Flat chunk generation only: one chunk per iteration.
    if let Some(spec) = config.spec_if_included(GROUP, "flat_chunk_generate", None) {
        out.push(run_benchmark(&spec, || {
            let (nanos, chunk) = timed(|| generator.generate(ChunkPos::ORIGIN));
            drop(chunk);
            Sample { nanos, units: 1 }
        }));
    }

    // (a) Flat chunk network-encode: bytes/sec for the section-data blob.
    if let Some(spec) = config.spec_if_included(GROUP, "flat_chunk_encode", Some("bytes")) {
        let chunk = generator.generate(ChunkPos::ORIGIN);
        let mut result = run_benchmark(&spec, || {
            let (nanos, blob) = timed(|| encode_chunk_section_data(&chunk).unwrap_or_default());
            Sample {
                nanos,
                units: blob.len() as u64,
            }
        });
        // Record the encoded size so the throughput is interpretable.
        let bytes = encode_chunk_section_data(&chunk).map_or(0.0, |b| b.len() as f64);
        result.add_metric("encoded_bytes", "bytes", bytes);
        out.push(result);
    }

    // (b) Section encode: single-valued (all-air) container.
    if let Some(spec) = config.spec_if_included(GROUP, "section_encode_single_air", Some("bytes")) {
        let container: PalettedContainer<SECTION_VOLUME> = PalettedContainer::new();
        out.push(run_benchmark(&spec, || {
            let (nanos, buf) = encode_section(&container);
            Sample {
                nanos,
                units: buf.len() as u64,
            }
        }));
    }

    // (b) Section encode: indirect palette (a flat-surface block mix).
    if let Some(spec) = config.spec_if_included(GROUP, "section_encode_indirect", Some("bytes")) {
        let container = build_surface_section();
        out.push(run_benchmark(&spec, || {
            let (nanos, buf) = encode_section(&container);
            Sample {
                nanos,
                units: buf.len() as u64,
            }
        }));
    }

    out
}

/// Times one network encode of a paletted section, returning the elapsed
/// nanoseconds and the encoded bytes.
fn encode_section(container: &PalettedContainer<SECTION_VOLUME>) -> (u64, Vec<u8>) {
    timed(|| {
        let mut buf = Vec::new();
        let _ = container.encode_network(&mut buf, BLOCK_DIRECT_WIRE_BITS);
        buf
    })
}

/// Builds a section resembling a flat-world surface section: a stone base, three
/// dirt layers, a grass cap, and air above. The four distinct states promote the
/// container to a 4-bit indirect palette (256 packed longs), exercising the
/// common indirect encode path.
fn build_surface_section() -> PalettedContainer<SECTION_VOLUME> {
    let mut container: PalettedContainer<SECTION_VOLUME> = PalettedContainer::new();
    for y in 0..SECTION_EDGE {
        let state = match y {
            0..=11 => BlockStateId::new(block_state::STONE),
            12..=14 => BlockStateId::new(block_state::DIRT),
            15 => BlockStateId::new(block_state::GRASS_BLOCK),
            _ => BlockStateId::AIR,
        };
        if state.is_air() {
            continue;
        }
        for z in 0..SECTION_EDGE {
            for x in 0..SECTION_EDGE {
                let index = (y * SECTION_EDGE + z) * SECTION_EDGE + x;
                let _ = container.set(index, state);
            }
        }
    }
    container
}
