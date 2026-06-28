//! Item-slot benchmarks: trusted (clientbound) slot/container encode and
//! untrusted (serverbound) slot decode + validation.

use std::num::NonZeroU8;

use ferrumc_codec::{write_var_int, BoundedReader};
use ferrumc_items::component::{ComponentValue, DAMAGE};
use ferrumc_items::{
    encode_container_content_payload, ComponentPatch, ItemId, ItemStack, UntrustedItemStack,
};

use crate::harness::{run_benchmark, timed, Sample};
use crate::report::BenchResult;
use crate::BenchConfig;

/// Group name for these benchmarks.
const GROUP: &str = "items";

/// Item id for `minecraft:stone` (a 64-stack block item).
const STONE_ITEM: i32 = 1;

/// Slots in a player inventory window, used for the container-content payload.
const WINDOW_SLOTS: usize = 46;

/// Builds and runs the `items` benchmark group.
#[must_use]
pub fn benchmarks(config: &BenchConfig) -> Vec<BenchResult> {
    let mut out = Vec::new();
    out.extend(trusted_encode_benchmarks(config));
    out.extend(untrusted_decode_benchmarks(config));
    out
}

/// Trusted (clientbound) encode benchmarks: empty slot, a present stack, a
/// present stack carrying one modeled component, and a full container window.
fn trusted_encode_benchmarks(config: &BenchConfig) -> Vec<BenchResult> {
    let mut out = Vec::new();

    // Empty slot: a single itemCount=0 byte.
    if let Some(spec) = config.spec_if_included(GROUP, "slot_encode_empty", Some("bytes")) {
        let stack = ItemStack::empty();
        out.push(run_benchmark(&spec, || encode_slot_sample(&stack)));
    }

    // Present stack: stone x64, no components.
    if let Some(stone) = ItemId::new(STONE_ITEM) {
        if let Some(spec) = config.spec_if_included(GROUP, "slot_encode_present", Some("bytes")) {
            let stack = ItemStack::new(stone, stack_count(64), ComponentPatch::empty());
            out.push(run_benchmark(&spec, || encode_slot_sample(&stack)));
        }
    }

    // Present stack with one modeled component (a Damage value) to show its cost.
    if let Some(sword) = ItemId::from_name("diamond_sword") {
        if let Some(spec) =
            config.spec_if_included(GROUP, "slot_encode_with_component", Some("bytes"))
        {
            let patch = ComponentPatch::new(vec![ComponentValue::Damage(42)], Vec::new());
            let stack = ItemStack::new(sword, stack_count(1), patch);
            out.push(run_benchmark(&spec, || encode_slot_sample(&stack)));
        }
    }

    // A full inventory window: alternating present/empty slots plus an empty
    // carried item.
    if let Some(stone) = ItemId::new(STONE_ITEM) {
        if let Some(spec) =
            config.spec_if_included(GROUP, "container_content_encode", Some("bytes"))
        {
            let present = ItemStack::new(stone, stack_count(64), ComponentPatch::empty());
            let window: Vec<ItemStack> = (0..WINDOW_SLOTS)
                .map(|i| {
                    if i % 2 == 0 {
                        present.clone()
                    } else {
                        ItemStack::empty()
                    }
                })
                .collect();
            let carried = ItemStack::empty();
            out.push(run_benchmark(&spec, || {
                let (nanos, body) = timed(|| {
                    encode_container_content_payload(&window, &carried).unwrap_or_default()
                });
                Sample {
                    nanos,
                    units: body.len() as u64,
                }
            }));
        }
    }

    out
}

/// Untrusted (serverbound) decode + validation benchmarks over pre-built wire
/// bytes: empty slot, a present stack, and a present stack with one component.
fn untrusted_decode_benchmarks(config: &BenchConfig) -> Vec<BenchResult> {
    let mut out = Vec::new();

    let inputs: [(&str, Vec<u8>); 3] = [
        ("untrusted_decode_empty", untrusted_empty()),
        (
            "untrusted_decode_present",
            untrusted_present(STONE_ITEM, 64),
        ),
        (
            "untrusted_decode_with_component",
            untrusted_with_damage(STONE_ITEM, 64, 42),
        ),
    ];

    for (name, bytes) in &inputs {
        let Some(spec) = config.spec_if_included(GROUP, name, Some("bytes")) else {
            continue;
        };
        let byte_len = bytes.len() as u64;
        out.push(run_benchmark(&spec, || {
            let (nanos, result) = timed(|| {
                let mut reader = BoundedReader::new(bytes);
                UntrustedItemStack::decode(&mut reader).and_then(UntrustedItemStack::into_validated)
            });
            drop(result);
            Sample {
                nanos,
                units: byte_len,
            }
        }));
    }

    out
}

/// Times one trusted-slot encode, returning the sample (units = encoded bytes).
fn encode_slot_sample(stack: &ItemStack) -> Sample {
    let (nanos, buf) = timed(|| {
        let mut buf = Vec::new();
        let _ = stack.encode_slot(&mut buf);
        buf
    });
    Sample {
        nanos,
        units: buf.len() as u64,
    }
}

/// Builds a [`NonZeroU8`] count, clamping `0` to `1`.
fn stack_count(count: u8) -> NonZeroU8 {
    NonZeroU8::new(count).unwrap_or(NonZeroU8::MIN)
}

/// The untrusted wire bytes for an empty slot.
fn untrusted_empty() -> Vec<u8> {
    vec![0]
}

/// The untrusted wire bytes for a present stack with no components.
fn untrusted_present(item_id: i32, count: i32) -> Vec<u8> {
    let mut out = Vec::new();
    write_var_int(&mut out, count);
    write_var_int(&mut out, item_id);
    write_var_int(&mut out, 0); // added component count
    write_var_int(&mut out, 0); // removed component count
    out
}

/// The untrusted wire bytes for a present stack carrying a single length-prefixed
/// `Damage` component.
fn untrusted_with_damage(item_id: i32, count: i32, damage: i32) -> Vec<u8> {
    let mut out = Vec::new();
    write_var_int(&mut out, count);
    write_var_int(&mut out, item_id);
    write_var_int(&mut out, 1); // added component count
    write_var_int(&mut out, 0); // removed component count
    write_var_int(&mut out, DAMAGE);
    // The component data is a length-prefixed ByteArray; its body is one VarInt.
    let mut blob = Vec::new();
    write_var_int(&mut blob, damage);
    write_var_int(&mut out, i32::try_from(blob.len()).unwrap_or(i32::MAX));
    out.extend_from_slice(&blob);
    out
}
