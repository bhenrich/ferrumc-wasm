//! Build-time codegen for the item and block-state registry tables.
//!
//! The vendored `data/items.json`, `data/item_to_block_mapping.json`, and
//! `data/blocks.json` snapshots are parsed **once at build time** and turned into
//! static Rust arrays emitted to `OUT_DIR/items_generated.rs` and
//! `OUT_DIR/blocks_generated.rs`. The runtime stays dependency-free and pays zero
//! boot cost: there is no JSON parsing or `OnceCell` at startup, only `static`
//! arrays and binary-search lookups.
//!
//! `serde`/`serde_json` are pulled in solely as `[build-dependencies]` so this
//! parse can happen ahead of time; they are not linked into the runtime crate.
//!
//! The generator asserts the data's structural invariants (contiguous item ids
//! `0..=N`, every item carries a `max_stack_size` in `1..=255`; block-state
//! ranges dense/disjoint, default in range, names unique) and panics loudly on
//! drift, so a botched re-vendor fails the build instead of silently shipping
//! wrong tables.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One item entry in `items.json`: a numeric protocol id plus its data
/// components (we only read `max_stack_size`).
#[derive(Deserialize)]
struct RawItem {
    id: i32,
    components: RawComponents,
}

/// The subset of an item's data components this build reads.
#[derive(Deserialize)]
struct RawComponents {
    #[serde(rename = "minecraft:max_stack_size")]
    max_stack_size: Option<i64>,
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let data_dir = manifest_dir.join("data");
    let items_path = data_dir.join("items.json");
    let mapping_path = data_dir.join("item_to_block_mapping.json");
    let blocks_path = data_dir.join("blocks.json");

    // Re-run only when the vendored data (or this generator) changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", items_path.display());
    println!("cargo:rerun-if-changed={}", mapping_path.display());
    println!("cargo:rerun-if-changed={}", blocks_path.display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    let names_and_stacks = load_items(&items_path);
    let to_block = load_mapping(&mapping_path);
    let source = emit(&names_and_stacks, &to_block);
    let dest = out_dir.join("items_generated.rs");
    fs::write(&dest, source).unwrap_or_else(|e| panic!("failed to write {}: {e}", dest.display()));

    let blocks = load_blocks(&blocks_path);
    let blocks_source = emit_blocks(&blocks);
    let blocks_dest = out_dir.join("blocks_generated.rs");
    fs::write(&blocks_dest, blocks_source)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", blocks_dest.display()));
}

/// One block entry in `blocks.json`: the bare resource name, its protocol id,
/// the dense `[minStateId, maxStateId]` state range, the default state, the
/// property list (in encoding order), and the collision-box class used to derive
/// `is_solid_cube`.
#[derive(Deserialize)]
struct RawBlock {
    id: u16,
    name: String,
    #[serde(rename = "defaultState")]
    default_state: u32,
    #[serde(rename = "minStateId")]
    min_state_id: u32,
    #[serde(rename = "maxStateId")]
    max_state_id: u32,
    states: Vec<RawState>,
    #[serde(rename = "boundingBox")]
    bounding_box: String,
}

/// One property of a block. `bool` properties carry no `values` array in the
/// snapshot (their values are synthesised as `["true", "false"]`); `enum`/`int`
/// properties carry an explicit ordered `values` list.
#[derive(Deserialize)]
struct RawState {
    name: String,
    #[serde(rename = "type")]
    typ: String,
    num_values: usize,
    #[serde(default)]
    values: Option<Vec<String>>,
}

/// A property's runtime shape after normalisation: its name, whether it is a
/// boolean (vs. an explicitly enumerated enum/int), and its ordered value list.
struct Property {
    name: String,
    is_bool: bool,
    values: Vec<String>,
}

/// A block's runtime shape after normalisation and invariant checking.
struct Block {
    id: u16,
    name: String,
    default_state: u32,
    min_state: u32,
    max_state: u32,
    is_solid_cube: bool,
    properties: Vec<Property>,
}

/// Parses `blocks.json` into normalised [`Block`]s, asserting the block-state
/// encoding invariants: `min <= default <= max`, the dense range width equals the
/// product of the property cardinalities (the little-endian linear encoding), all
/// names are unique, and no two blocks' state ranges overlap.
fn load_blocks(blocks_path: &Path) -> Vec<Block> {
    let raw = fs::read_to_string(blocks_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", blocks_path.display()));
    // blocks.json is a JSON ARRAY of block entries.
    let blocks: Vec<RawBlock> = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("blocks.json is not the expected schema: {e}"));

    let mut seen_names: BTreeSet<String> = BTreeSet::new();
    // (min, max) ranges collected to assert disjointness after parsing.
    let mut ranges: Vec<(u32, u32, String)> = Vec::with_capacity(blocks.len());

    let normalised: Vec<Block> = blocks
        .into_iter()
        .map(|b| {
            assert!(
                b.min_state_id <= b.default_state && b.default_state <= b.max_state_id,
                "block {} default {} outside [{}, {}]",
                b.name,
                b.default_state,
                b.min_state_id,
                b.max_state_id
            );
            assert!(
                seen_names.insert(b.name.clone()),
                "duplicate block name {}",
                b.name
            );

            let mut cardinality_product: u64 = 1;
            let properties: Vec<Property> = b
                .states
                .into_iter()
                .map(|s| {
                    cardinality_product *= s.num_values as u64;
                    let is_bool = s.typ == "bool";
                    let values: Vec<String> = if is_bool {
                        // Boolean properties enumerate [true, false] (index 0 =
                        // true), matching the vanilla state-id encoding.
                        vec!["true".to_string(), "false".to_string()]
                    } else {
                        s.values.unwrap_or_else(|| {
                            panic!("block {} property {} has no values array", b.name, s.name)
                        })
                    };
                    assert_eq!(
                        values.len(),
                        s.num_values,
                        "block {} property {} num_values {} disagrees with its {} values",
                        b.name,
                        s.name,
                        s.num_values,
                        values.len()
                    );
                    Property {
                        name: s.name,
                        is_bool,
                        values,
                    }
                })
                .collect();

            let span = u64::from(b.max_state_id - b.min_state_id) + 1;
            assert_eq!(
                span, cardinality_product,
                "block {} state span {} disagrees with the product of its property \
                 cardinalities {} (the linear state encoding would be wrong)",
                b.name, span, cardinality_product
            );

            ranges.push((b.min_state_id, b.max_state_id, b.name.clone()));

            Block {
                id: b.id,
                name: b.name,
                default_state: b.default_state,
                min_state: b.min_state_id,
                max_state: b.max_state_id,
                is_solid_cube: b.bounding_box == "block",
                properties,
            }
        })
        .collect();

    // State ranges must be pairwise disjoint so a state id maps to exactly one
    // block (the reverse lookup the neighbour query relies on).
    ranges.sort_unstable_by_key(|(min, _, _)| *min);
    for pair in ranges.windows(2) {
        let (_, prev_max, prev_name) = &pair[0];
        let (next_min, _, next_name) = &pair[1];
        assert!(
            *next_min > *prev_max,
            "block-state ranges overlap: {prev_name} ends at {prev_max} but {next_name} starts at {next_min}"
        );
    }

    normalised
}

/// Renders the generated Rust source for the block-state tables.
fn emit_blocks(blocks: &[Block]) -> String {
    let count = blocks.len();

    // Sorted (name, index) for name -> block binary search.
    let mut by_name: Vec<(&str, usize)> = blocks
        .iter()
        .enumerate()
        .map(|(idx, b)| (b.name.as_str(), idx))
        .collect();
    by_name.sort_unstable_by(|a, b| a.0.cmp(b.0));

    // Sorted (min, max, index) for state-id -> block range binary search.
    let mut by_min_state: Vec<(u32, u32, usize)> = blocks
        .iter()
        .enumerate()
        .map(|(idx, b)| (b.min_state, b.max_state, idx))
        .collect();
    by_min_state.sort_unstable_by_key(|(min, _, _)| *min);

    let mut out = String::new();
    out.push_str("// @generated by build.rs from data/blocks.json.\n");
    out.push_str("// Do not edit by hand; re-vendor the JSON and rebuild instead.\n\n");

    let _ = writeln!(out, "/// Number of blocks in the pinned 1.21.8 registry.");
    let _ = writeln!(out, "pub const BLOCK_COUNT: usize = {count};\n");

    // BLOCKS: index -> metadata, in blocks.json (id) order. Properties and their
    // value lists are inlined as rvalue-promoted `&[..]` slices, so the whole
    // table is one `'static` constant with no runtime construction.
    let _ = writeln!(out, "static BLOCKS: [BlockMetadata; {count}] = [");
    for b in blocks {
        out.push_str("    BlockMetadata { ");
        let _ = write!(
            out,
            "name: {:?}, id: {}, default_state: {}, min_state: {}, max_state: {}, is_solid_cube: {}, ",
            b.name, b.id, b.default_state, b.min_state, b.max_state, b.is_solid_cube
        );
        if b.properties.is_empty() {
            out.push_str("properties: &[] },\n");
        } else {
            out.push_str("properties: &[");
            for p in &b.properties {
                let typ = if p.is_bool {
                    "PropertyType::Bool"
                } else {
                    "PropertyType::Enum"
                };
                let _ = write!(
                    out,
                    "Property {{ name: {:?}, typ: {typ}, cardinality: {}, values: &[",
                    p.name,
                    p.values.len()
                );
                for v in &p.values {
                    let _ = write!(out, "{v:?}, ");
                }
                out.push_str("] }, ");
            }
            out.push_str("] },\n");
        }
    }
    out.push_str("];\n\n");

    // BLOCKS_BY_NAME: sorted (bare name, BLOCKS index) for binary search.
    let _ = writeln!(out, "static BLOCKS_BY_NAME: [(&str, usize); {count}] = [");
    for (name, idx) in &by_name {
        let _ = writeln!(out, "    ({name:?}, {idx}),");
    }
    out.push_str("];\n\n");

    // BLOCKS_BY_MIN_STATE: sorted (min, max, BLOCKS index) for state-id -> block
    // range binary search.
    let _ = writeln!(
        out,
        "static BLOCKS_BY_MIN_STATE: [(u32, u32, usize); {count}] = ["
    );
    for (min, max, idx) in &by_min_state {
        let _ = writeln!(out, "    ({min}, {max}, {idx}),");
    }
    out.push_str("];\n");

    out
}

/// Parses `items.json` into an id-indexed `(name, max_stack)` table, asserting
/// the ids are contiguous `0..=N` and every item has a max stack in `1..=255`.
fn load_items(items_path: &Path) -> Vec<(String, u8)> {
    let raw = fs::read_to_string(items_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", items_path.display()));
    // items.json is a dict keyed by the BARE item name (e.g. "stone").
    let items: BTreeMap<String, RawItem> = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("items.json is not the expected schema: {e}"));

    let count = items.len();
    // Invariant: ids are contiguous 0..=count-1. Fill an id-indexed table and
    // verify every slot is occupied, panicking loudly on any gap or duplicate.
    let mut by_id: Vec<Option<(String, u8)>> = vec![None; count];
    for (name, item) in &items {
        let idx = usize::try_from(item.id)
            .unwrap_or_else(|_| panic!("item {name} has negative id {}", item.id));
        assert!(
            idx < count,
            "item {name} id {idx} is out of range for a {count}-entry registry (ids must be contiguous 0..={})",
            count - 1
        );
        let max_stack = item
            .components
            .max_stack_size
            .unwrap_or_else(|| panic!("item {name} (id {idx}) has no minecraft:max_stack_size"));
        let max_stack = u8::try_from(max_stack).unwrap_or_else(|_| {
            panic!("item {name} (id {idx}) max_stack_size {max_stack} does not fit a u8")
        });
        // The runtime clamps counts to `1..=max_stack` (a `u8::clamp`), which
        // panics if `max_stack == 0` (min > max). A 0 here means hostile input
        // would crash the server, so reject a botched re-vendor at build time.
        assert!(
            max_stack >= 1,
            "item {name} (id {idx}) has max_stack_size 0; it must be >= 1"
        );
        let slot = &mut by_id[idx];
        assert!(
            slot.is_none(),
            "duplicate item id {idx}: {name} collides with {}",
            slot.as_ref().map_or("?", |(n, _)| n.as_str())
        );
        // Names are bare in the snapshot; store the canonical namespaced form.
        *slot = Some((format!("minecraft:{name}"), max_stack));
    }

    by_id
        .into_iter()
        .enumerate()
        .map(|(idx, slot)| {
            slot.unwrap_or_else(|| panic!("item id {idx} missing: ids are not contiguous"))
        })
        .collect()
}

/// Parses `item_to_block_mapping.json` into a sorted `(item_id, block_state_id)`
/// table for binary search.
fn load_mapping(mapping_path: &Path) -> Vec<(i32, u32)> {
    let raw = fs::read_to_string(mapping_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", mapping_path.display()));
    // item_to_block_mapping.json is a dict of stringified item id -> stringified
    // block-state id (e.g. "1" -> "1").
    let mapping: BTreeMap<String, String> = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("item_to_block_mapping.json is not the expected schema: {e}"));

    let mut to_block: Vec<(i32, u32)> = mapping
        .iter()
        .map(|(k, v)| {
            let item_id: i32 = k
                .parse()
                .unwrap_or_else(|e| panic!("item_to_block key {k:?} is not an i32: {e}"));
            let block_state: u32 = v
                .parse()
                .unwrap_or_else(|e| panic!("item_to_block value {v:?} is not a u32: {e}"));
            (item_id, block_state)
        })
        .collect();
    to_block.sort_unstable_by_key(|(item_id, _)| *item_id);
    to_block
}

/// Renders the generated Rust source for the parsed tables.
fn emit(names_and_stacks: &[(String, u8)], to_block: &[(i32, u32)]) -> String {
    let count = names_and_stacks.len();

    // Sorted (name, id) table for name -> id binary search.
    let mut by_name: Vec<(&str, i32)> = names_and_stacks
        .iter()
        .enumerate()
        .map(|(idx, (name, _))| (name.as_str(), i32::try_from(idx).expect("id fits i32")))
        .collect();
    by_name.sort_unstable_by(|a, b| a.0.cmp(b.0));

    let mut out = String::new();
    out.push_str(
        "// @generated by build.rs from data/items.json + data/item_to_block_mapping.json.\n",
    );
    out.push_str("// Do not edit by hand; re-vendor the JSON and rebuild instead.\n\n");

    let _ = writeln!(out, "/// Number of items in the pinned 1.21.8 registry.");
    let _ = writeln!(out, "pub const ITEM_COUNT: usize = {count};\n");

    // ITEM_NAMES: id -> canonical namespaced name.
    let _ = writeln!(out, "static ITEM_NAMES: [&str; {count}] = [");
    for (name, _) in names_and_stacks {
        let _ = writeln!(out, "    {name:?},");
    }
    out.push_str("];\n\n");

    // ITEM_MAX_STACK: id -> max stack size.
    let _ = writeln!(out, "static ITEM_MAX_STACK: [u8; {count}] = [");
    for (_, max_stack) in names_and_stacks {
        let _ = writeln!(out, "    {max_stack},");
    }
    out.push_str("];\n\n");

    // ITEM_BY_NAME: sorted (name, id) for binary search.
    let _ = writeln!(out, "static ITEM_BY_NAME: [(&str, i32); {count}] = [");
    for (name, id) in &by_name {
        let _ = writeln!(out, "    ({name:?}, {id}),");
    }
    out.push_str("];\n\n");

    // ITEM_TO_BLOCK: sorted (item_id, block_state_id) for binary search.
    let mlen = to_block.len();
    let _ = writeln!(out, "static ITEM_TO_BLOCK: [(i32, u32); {mlen}] = [");
    for (item_id, block_state) in to_block {
        let _ = writeln!(out, "    ({item_id}, {block_state}),");
    }
    out.push_str("];\n");

    out
}
