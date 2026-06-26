//! Property-based round-trip tests over the public API: for any in-bounds tree,
//! `read(write(tag)) == tag` for both root forms.

use ferrumc_nbt::{
    read_named_root, read_network_root, write_named_root, write_network_root, NbtCompound,
    NbtLimits, NbtTag,
};
use proptest::prelude::*;

/// Builds a compound from a list of values, assigning unique `k{i}` names so the
/// generated trees never depend on duplicate-key behaviour.
fn compound_from(values: Vec<NbtTag>) -> NbtCompound {
    let mut compound = NbtCompound::new();
    for (index, value) in values.into_iter().enumerate() {
        compound.push(format!("k{index}"), value);
    }
    compound
}

/// A strategy for arbitrary, bounded NBT values.
///
/// Lists are kept homogeneous (the format requires it). Floats exclude `NaN`,
/// which is never equal to itself and so cannot satisfy a round-trip equality.
fn arb_tag() -> impl Strategy<Value = NbtTag> {
    let leaf = prop_oneof![
        any::<i8>().prop_map(NbtTag::Byte),
        any::<i16>().prop_map(NbtTag::Short),
        any::<i32>().prop_map(NbtTag::Int),
        any::<i64>().prop_map(NbtTag::Long),
        any::<f32>()
            .prop_filter("no NaN", |f| !f.is_nan())
            .prop_map(NbtTag::Float),
        any::<f64>()
            .prop_filter("no NaN", |f| !f.is_nan())
            .prop_map(NbtTag::Double),
        "[a-zA-Z0-9 ]{0,16}".prop_map(NbtTag::String),
        prop::collection::vec(any::<i8>(), 0..8).prop_map(NbtTag::ByteArray),
        prop::collection::vec(any::<i32>(), 0..8).prop_map(NbtTag::IntArray),
        prop::collection::vec(any::<i64>(), 0..8).prop_map(NbtTag::LongArray),
    ];

    leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            // Homogeneous lists of a few representative element types.
            prop::collection::vec(any::<i8>(), 0..6)
                .prop_map(|v| NbtTag::List(v.into_iter().map(NbtTag::Byte).collect())),
            prop::collection::vec(any::<i32>(), 0..6)
                .prop_map(|v| NbtTag::List(v.into_iter().map(NbtTag::Int).collect())),
            prop::collection::vec("[a-z]{0,6}", 0..6)
                .prop_map(|v| NbtTag::List(v.into_iter().map(NbtTag::String).collect())),
            // A compound whose values are themselves arbitrary tags.
            prop::collection::vec(inner.clone(), 0..5)
                .prop_map(|values| NbtTag::Compound(compound_from(values))),
            // A list of compounds, exercising nesting through both container kinds.
            prop::collection::vec(prop::collection::vec(inner, 0..3), 0..4).prop_map(|groups| {
                NbtTag::List(
                    groups
                        .into_iter()
                        .map(|values| NbtTag::Compound(compound_from(values)))
                        .collect(),
                )
            }),
        ]
    })
}

/// A strategy for an arbitrary root compound.
fn arb_root() -> impl Strategy<Value = NbtCompound> {
    prop::collection::vec(arb_tag(), 0..6).prop_map(compound_from)
}

proptest! {
    #[test]
    fn network_root_round_trips(root in arb_root()) {
        let tag = NbtTag::Compound(root);
        let bytes = write_network_root(&tag, &NbtLimits::default()).expect("write");
        let parsed = read_network_root(&bytes, &NbtLimits::default()).expect("read");
        prop_assert_eq!(parsed, tag);
    }

    #[test]
    fn named_root_round_trips(root in arb_root(), name in "[a-zA-Z0-9_]{0,12}") {
        let tag = NbtTag::Compound(root);
        let bytes = write_named_root(&name, &tag, &NbtLimits::default()).expect("write");
        let (parsed_name, parsed) = read_named_root(&bytes, &NbtLimits::default()).expect("read");
        prop_assert_eq!(parsed_name, name);
        prop_assert_eq!(parsed, tag);
    }
}
