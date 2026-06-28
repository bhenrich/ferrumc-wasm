//! Build-time codegen for the item registry tables.
//!
//! The vendored `data/items.json` and `data/item_to_block_mapping.json`
//! snapshots are parsed **once at build time** and turned into static Rust
//! arrays emitted to `OUT_DIR/items_generated.rs`. The runtime stays
//! dependency-free and pays zero boot cost: there is no JSON parsing or `OnceCell`
//! at startup, only `static` arrays and binary-search lookups.
//!
//! `serde`/`serde_json` are pulled in solely as `[build-dependencies]` so this
//! parse can happen ahead of time; they are not linked into the runtime crate.
//!
//! The generator asserts the data's structural invariants (contiguous item ids
//! `0..=N`, every item carries a `max_stack_size` in `1..=255`) and panics
//! loudly on drift, so a botched re-vendor fails the build instead of silently
//! shipping wrong tables.

use std::collections::BTreeMap;
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

    // Re-run only when the vendored data (or this generator) changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", items_path.display());
    println!("cargo:rerun-if-changed={}", mapping_path.display());

    let names_and_stacks = load_items(&items_path);
    let to_block = load_mapping(&mapping_path);
    let source = emit(&names_and_stacks, &to_block);

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let dest = out_dir.join("items_generated.rs");
    fs::write(&dest, source).unwrap_or_else(|e| panic!("failed to write {}: {e}", dest.display()));
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
