//! Encoding an [`NbtTag`] tree back into bytes.
//!
//! [`write_named_root`] mirrors [`read_named_root`](crate::read_named_root) and
//! [`write_network_root`] mirrors [`read_network_root`](crate::read_network_root):
//! the bytes one produces are accepted by the matching reader.
//!
//! Both require the root to be a [`NbtTag::Compound`]. The writer takes an
//! [`NbtLimits`] for symmetry with the reader: it enforces `max_depth` so a
//! pathologically nested in-memory tree cannot overflow the stack during
//! encoding. It additionally enforces the structural caps the format itself
//! imposes — strings must fit a `u16` length prefix, sequences must fit an
//! `i32` length, and a list must be homogeneous — surfacing violations as
//! [`NbtError::StringTooLong`], [`NbtError::ListTooLong`],
//! [`NbtError::HeterogeneousList`], and [`NbtError::DepthExceeded`]. The other
//! `NbtLimits` caps (`max_bytes`, `max_list_len`, `max_string_bytes`) are
//! reader-side concerns and are not applied here.

use crate::error::NbtError;
use crate::limits::NbtLimits;
use crate::mutf8;
use crate::tag::{NbtTag, TagType};
use crate::Result;

/// Largest length expressible by a `u16` prefix (NBT string length).
const U16_LEN_MAX: usize = 65_535;

/// Largest length expressible by an `i32` prefix (NBT sequence length).
const I32_LEN_MAX: usize = 2_147_483_647;

/// Encodes a file-form root: `[type=10][name][compound payload]`.
///
/// `tag` must be a [`NbtTag::Compound`]; otherwise
/// [`NbtError::UnexpectedRootTag`] is returned. Nesting deeper than
/// `limits.max_depth()` is rejected with [`NbtError::DepthExceeded`].
pub fn write_named_root(name: &str, tag: &NbtTag, limits: &NbtLimits) -> Result<Vec<u8>> {
    require_compound(tag)?;
    let mut out = Vec::new();
    out.push(TagType::Compound.id());
    write_string(&mut out, name)?;
    // The root compound is depth 1, matching the reader's accounting.
    write_payload(&mut out, tag, 1, limits)?;
    Ok(out)
}

/// Encodes a network-form root: `[type=10][compound payload]` with no name.
///
/// `tag` must be a [`NbtTag::Compound`]; otherwise
/// [`NbtError::UnexpectedRootTag`] is returned. Nesting deeper than
/// `limits.max_depth()` is rejected with [`NbtError::DepthExceeded`].
pub fn write_network_root(tag: &NbtTag, limits: &NbtLimits) -> Result<Vec<u8>> {
    require_compound(tag)?;
    let mut out = Vec::new();
    out.push(TagType::Compound.id());
    write_payload(&mut out, tag, 1, limits)?;
    Ok(out)
}

/// Rejects a root that is not a compound.
fn require_compound(tag: &NbtTag) -> Result<()> {
    if matches!(tag, NbtTag::Compound(_)) {
        Ok(())
    } else {
        Err(NbtError::UnexpectedRootTag { id: tag.type_id() })
    }
}

/// Writes the bare payload of a tag (no type byte, no name).
///
/// `depth` is the depth of `tag`: the root compound is depth 1, and each nested
/// list or compound descends one level. Arrays do not nest and never count,
/// mirroring the reader's accounting.
fn write_payload(out: &mut Vec<u8>, tag: &NbtTag, depth: usize, limits: &NbtLimits) -> Result<()> {
    match tag {
        NbtTag::Byte(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtTag::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtTag::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtTag::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtTag::Float(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtTag::Double(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtTag::ByteArray(values) => {
            write_seq_len(out, values.len())?;
            for &v in values {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        NbtTag::String(text) => write_string(out, text)?,
        NbtTag::List(items) => {
            if depth > limits.max_depth() {
                return Err(NbtError::DepthExceeded {
                    max: limits.max_depth(),
                });
            }
            // An empty list declares element type TAG_End, matching vanilla.
            let element_id = items.first().map_or(TagType::End.id(), NbtTag::type_id);
            // The format gives a list one element type; a mixed-type list would
            // silently corrupt on decode, so reject it instead of encoding it.
            if let Some(mismatch) = items.iter().find(|item| item.type_id() != element_id) {
                return Err(NbtError::HeterogeneousList {
                    expected: element_id,
                    found: mismatch.type_id(),
                });
            }
            out.push(element_id);
            write_seq_len(out, items.len())?;
            for item in items {
                write_payload(out, item, depth + 1, limits)?;
            }
        }
        NbtTag::Compound(compound) => {
            if depth > limits.max_depth() {
                return Err(NbtError::DepthExceeded {
                    max: limits.max_depth(),
                });
            }
            for (name, value) in compound.iter() {
                out.push(value.type_id());
                write_string(out, name)?;
                write_payload(out, value, depth + 1, limits)?;
            }
            out.push(TagType::End.id());
        }
        NbtTag::IntArray(values) => {
            write_seq_len(out, values.len())?;
            for &v in values {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        NbtTag::LongArray(values) => {
            write_seq_len(out, values.len())?;
            for &v in values {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
    }
    Ok(())
}

/// Writes an `i32` big-endian sequence length, rejecting overflow.
fn write_seq_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    let len = i32::try_from(len).map_err(|_| NbtError::ListTooLong {
        len,
        max: I32_LEN_MAX,
    })?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

/// Writes a `TAG_String`: a `u16` big-endian byte length then the string in Java
/// Modified UTF-8 (see [`mutf8`]).
///
/// The length prefix counts the *encoded* Modified UTF-8 bytes, which is what the
/// reader (and a real client) consume; it is computed without allocating so an
/// over-long string is rejected before any bytes are written.
fn write_string(out: &mut Vec<u8>, text: &str) -> Result<()> {
    let len = mutf8::encoded_len(text);
    let prefix = u16::try_from(len).map_err(|_| NbtError::StringTooLong {
        len,
        max: U16_LEN_MAX,
    })?;
    out.extend_from_slice(&prefix.to_be_bytes());
    mutf8::encode(out, text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::NbtLimits;
    use crate::read::{read_named_root, read_network_root};
    use crate::tag::NbtCompound;

    fn sample_compound() -> NbtTag {
        let mut inner = NbtCompound::new();
        inner.push("flag", NbtTag::Byte(1));

        let mut root = NbtCompound::new();
        root.push("byte", NbtTag::Byte(-5));
        root.push("short", NbtTag::Short(300));
        root.push("int", NbtTag::Int(-70_000));
        root.push("long", NbtTag::Long(9_000_000_000));
        root.push("float", NbtTag::Float(1.5));
        root.push("double", NbtTag::Double(-2.25));
        root.push("bytes", NbtTag::ByteArray(vec![-1, 0, 1]));
        root.push("text", NbtTag::String("hello".to_owned()));
        root.push(
            "list",
            NbtTag::List(vec![NbtTag::Int(1), NbtTag::Int(2), NbtTag::Int(3)]),
        );
        root.push("nested", NbtTag::Compound(inner));
        root.push("ints", NbtTag::IntArray(vec![1, -2, 3]));
        root.push("longs", NbtTag::LongArray(vec![-1, 2]));
        NbtTag::Compound(root)
    }

    #[test]
    fn named_root_round_trips_through_reader() {
        let tag = sample_compound();
        let bytes = write_named_root("level", &tag, &NbtLimits::default()).expect("write");
        let (name, parsed) = read_named_root(&bytes, &NbtLimits::default()).expect("read");
        assert_eq!(name, "level");
        assert_eq!(parsed, tag);
    }

    #[test]
    fn network_root_round_trips_through_reader() {
        let tag = sample_compound();
        let bytes = write_network_root(&tag, &NbtLimits::default()).expect("write");
        let parsed = read_network_root(&bytes, &NbtLimits::default()).expect("read");
        assert_eq!(parsed, tag);
    }

    #[test]
    fn non_compound_root_is_rejected() {
        assert_eq!(
            write_network_root(&NbtTag::Byte(1), &NbtLimits::default()),
            Err(NbtError::UnexpectedRootTag { id: 1 })
        );
        assert_eq!(
            write_named_root("x", &NbtTag::Int(0), &NbtLimits::default()),
            Err(NbtError::UnexpectedRootTag { id: 3 })
        );
    }

    #[test]
    fn string_too_long_for_u16_prefix_is_rejected() {
        let mut root = NbtCompound::new();
        root.push("big", NbtTag::String("a".repeat(70_000)));
        let tag = NbtTag::Compound(root);
        assert_eq!(
            write_network_root(&tag, &NbtLimits::default()),
            Err(NbtError::StringTooLong {
                len: 70_000,
                max: U16_LEN_MAX,
            })
        );
    }

    #[test]
    fn empty_list_encodes_end_element_type() {
        let mut root = NbtCompound::new();
        root.push("empty", NbtTag::List(Vec::new()));
        let tag = NbtTag::Compound(root);

        let bytes = write_network_root(&tag, &NbtLimits::default()).expect("write");
        let parsed = read_network_root(&bytes, &NbtLimits::default()).expect("read");
        assert_eq!(parsed, tag);
    }

    #[test]
    fn heterogeneous_list_is_rejected_without_corruption() {
        let mut root = NbtCompound::new();
        // First element is a Byte (id 1), second is a Compound (id 10).
        root.push(
            "mixed",
            NbtTag::List(vec![NbtTag::Byte(1), NbtTag::Compound(NbtCompound::new())]),
        );
        let tag = NbtTag::Compound(root);

        assert_eq!(
            write_network_root(&tag, &NbtLimits::default()),
            Err(NbtError::HeterogeneousList {
                expected: 1,
                found: 10,
            })
        );
    }

    #[test]
    fn over_deep_tree_is_depth_exceeded_not_overflow() {
        // Build a chain of compounds well past the limit under test. The writer
        // must stop and return an error instead of recursing unbounded. The
        // tree is kept modest so the test's own recursive Drop stays safe.
        let mut tag = NbtTag::Compound(NbtCompound::new());
        for _ in 0..256 {
            let mut parent = NbtCompound::new();
            parent.push("c", tag);
            tag = NbtTag::Compound(parent);
        }

        let limits = NbtLimits::default().with_max_depth(16);
        assert_eq!(
            write_network_root(&tag, &limits),
            Err(NbtError::DepthExceeded { max: 16 })
        );
    }

    #[test]
    fn over_deep_list_tree_is_depth_exceeded() {
        // Nesting through lists must be bounded the same way as compounds.
        let mut tag = NbtTag::List(Vec::new());
        for _ in 0..256 {
            tag = NbtTag::List(vec![tag]);
        }
        let mut root = NbtCompound::new();
        root.push("deep", tag);

        let limits = NbtLimits::default().with_max_depth(16);
        assert_eq!(
            write_network_root(&NbtTag::Compound(root), &limits),
            Err(NbtError::DepthExceeded { max: 16 })
        );
    }

    #[test]
    fn astral_string_round_trips_and_is_modified_utf8() {
        // An emoji in a TAG_String must survive write -> read and must be written
        // in the Modified UTF-8 surrogate-pair form, never the four-byte standard
        // UTF-8 sequence whose 0xF0 lead a real client's NBT reader rejects.
        let mut root = NbtCompound::new();
        root.push("msg", NbtTag::String("hi \u{1F600}".to_owned()));
        let tag = NbtTag::Compound(root);

        let bytes = write_network_root(&tag, &NbtLimits::default()).expect("write");
        assert!(
            !bytes.iter().any(|&b| (0xF0..=0xF4).contains(&b)),
            "no standard-UTF-8 four-byte astral lead may appear on the wire"
        );
        let parsed = read_network_root(&bytes, &NbtLimits::default()).expect("read");
        assert_eq!(parsed, tag);
    }

    #[test]
    fn nan_floats_round_trip_bit_identically() {
        // The derived PartialEq treats NaN != NaN, so the proptest round-trip
        // excludes NaN. Pin the byte-level preservation explicitly here.
        let mut root = NbtCompound::new();
        root.push("f", NbtTag::Float(f32::NAN));
        root.push("d", NbtTag::Double(f64::NAN));
        let tag = NbtTag::Compound(root);

        let bytes = write_network_root(&tag, &NbtLimits::default()).expect("write");
        let parsed = read_network_root(&bytes, &NbtLimits::default()).expect("read");
        let NbtTag::Compound(out) = parsed else {
            panic!("compound");
        };
        let Some(NbtTag::Float(f)) = out.get("f") else {
            panic!("float present");
        };
        let Some(NbtTag::Double(d)) = out.get("d") else {
            panic!("double present");
        };
        assert_eq!(f.to_bits(), f32::NAN.to_bits());
        assert_eq!(d.to_bits(), f64::NAN.to_bits());
    }
}
