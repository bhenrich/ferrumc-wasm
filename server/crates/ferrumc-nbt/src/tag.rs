//! The [`NbtTag`] value tree, its [`NbtCompound`] container, and the internal
//! tag-type table.

use crate::error::NbtError;

/// A decoded NBT value.
///
/// One variant per NBT tag type. `TAG_End` has no variant: it is a structural
/// delimiter, not a value, and is handled entirely by the reader and writer.
///
/// The derived `PartialEq` compares floats with IEEE-754 equality, not by bit
/// pattern: `NaN` never equals itself and `+0.0` equals `-0.0`. Encoding and
/// decoding still preserve the exact bits (a round-tripped `NaN` keeps its
/// payload), so byte-level fidelity must be checked with
/// [`f32::to_bits`]/[`f64::to_bits`] rather than `==`.
#[derive(Debug, Clone, PartialEq)]
pub enum NbtTag {
    /// `TAG_Byte`: a signed 8-bit integer.
    Byte(i8),
    /// `TAG_Short`: a signed 16-bit integer.
    Short(i16),
    /// `TAG_Int`: a signed 32-bit integer.
    Int(i32),
    /// `TAG_Long`: a signed 64-bit integer.
    Long(i64),
    /// `TAG_Float`: a 32-bit IEEE-754 float.
    Float(f32),
    /// `TAG_Double`: a 64-bit IEEE-754 float.
    Double(f64),
    /// `TAG_Byte_Array`: a length-prefixed run of signed bytes.
    ByteArray(Vec<i8>),
    /// `TAG_String`: a string encoded on the wire as Java Modified `UTF-8` (see
    /// the crate docs).
    String(String),
    /// `TAG_List`: a homogeneous sequence of unnamed payloads.
    List(Vec<NbtTag>),
    /// `TAG_Compound`: an order-preserving set of named tags.
    Compound(NbtCompound),
    /// `TAG_Int_Array`: a length-prefixed run of signed 32-bit integers.
    IntArray(Vec<i32>),
    /// `TAG_Long_Array`: a length-prefixed run of signed 64-bit integers.
    LongArray(Vec<i64>),
}

impl NbtTag {
    /// The NBT type id byte used to tag this value on the wire.
    pub(crate) fn type_id(&self) -> u8 {
        match self {
            Self::Byte(_) => TagType::Byte.id(),
            Self::Short(_) => TagType::Short.id(),
            Self::Int(_) => TagType::Int.id(),
            Self::Long(_) => TagType::Long.id(),
            Self::Float(_) => TagType::Float.id(),
            Self::Double(_) => TagType::Double.id(),
            Self::ByteArray(_) => TagType::ByteArray.id(),
            Self::String(_) => TagType::String.id(),
            Self::List(_) => TagType::List.id(),
            Self::Compound(_) => TagType::Compound.id(),
            Self::IntArray(_) => TagType::IntArray.id(),
            Self::LongArray(_) => TagType::LongArray.id(),
        }
    }
}

/// An order-preserving collection of named tags (`TAG_Compound`).
///
/// Entries are stored in insertion order so that re-encoding a parsed compound
/// reproduces the same byte layout. Decoding appends entries verbatim and does
/// not de-duplicate names: NBT keys are conventionally unique, but a malformed
/// input carrying a repeated key is preserved rather than silently merged, and
/// [`get`](Self::get) returns the first match.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NbtCompound {
    entries: Vec<(String, NbtTag)>,
}

impl NbtCompound {
    /// Creates an empty compound.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a named tag, preserving insertion order.
    ///
    /// This does not replace an existing entry with the same name; both are
    /// retained. Callers that need unique keys must enforce that themselves.
    pub fn push(&mut self, name: impl Into<String>, tag: NbtTag) {
        self.entries.push((name.into(), tag));
    }

    /// Returns the first tag stored under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&NbtTag> {
        self.entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// The number of entries in the compound.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the compound has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates the entries in insertion order as `(name, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &NbtTag)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }
}

/// The twelve NBT tag types plus the `End` delimiter.
///
/// Kept internal: the public surface speaks in terms of [`NbtTag`] values, not
/// raw type ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagType {
    End,
    Byte,
    Short,
    Int,
    Long,
    Float,
    Double,
    ByteArray,
    String,
    List,
    Compound,
    IntArray,
    LongArray,
}

impl TagType {
    /// Maps a wire type id to its [`TagType`], rejecting unknown ids.
    pub(crate) fn from_id(id: u8) -> core::result::Result<Self, NbtError> {
        let tag = match id {
            0 => Self::End,
            1 => Self::Byte,
            2 => Self::Short,
            3 => Self::Int,
            4 => Self::Long,
            5 => Self::Float,
            6 => Self::Double,
            7 => Self::ByteArray,
            8 => Self::String,
            9 => Self::List,
            10 => Self::Compound,
            11 => Self::IntArray,
            12 => Self::LongArray,
            other => return Err(NbtError::UnknownTagType { id: other }),
        };
        Ok(tag)
    }

    /// The wire type id for this tag type.
    pub(crate) fn id(self) -> u8 {
        match self {
            Self::End => 0,
            Self::Byte => 1,
            Self::Short => 2,
            Self::Int => 3,
            Self::Long => 4,
            Self::Float => 5,
            Self::Double => 6,
            Self::ByteArray => 7,
            Self::String => 8,
            Self::List => 9,
            Self::Compound => 10,
            Self::IntArray => 11,
            Self::LongArray => 12,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_id_matches_tag_type_table() {
        assert_eq!(NbtTag::Byte(0).type_id(), 1);
        assert_eq!(NbtTag::String(String::new()).type_id(), 8);
        assert_eq!(NbtTag::Compound(NbtCompound::new()).type_id(), 10);
        assert_eq!(NbtTag::LongArray(Vec::new()).type_id(), 12);
    }

    #[test]
    fn from_id_round_trips_known_ids() {
        for id in 0..=12u8 {
            let tag = TagType::from_id(id).expect("0..=12 are valid");
            assert_eq!(tag.id(), id);
        }
    }

    #[test]
    fn from_id_rejects_unknown_ids() {
        for id in 13..=255u8 {
            assert_eq!(
                TagType::from_id(id),
                Err(NbtError::UnknownTagType { id }),
                "id={id}"
            );
        }
    }

    #[test]
    fn compound_preserves_insertion_order_and_lookup() {
        let mut compound = NbtCompound::new();
        compound.push("first", NbtTag::Byte(1));
        compound.push("second", NbtTag::Byte(2));

        assert_eq!(compound.len(), 2);
        assert!(!compound.is_empty());
        assert_eq!(compound.get("first"), Some(&NbtTag::Byte(1)));
        assert_eq!(compound.get("missing"), None);

        let names: Vec<&str> = compound.iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["first", "second"]);
    }
}
