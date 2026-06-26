//! Decoding NBT byte streams into the [`NbtTag`] tree under [`NbtLimits`].
//!
//! Two roots are supported:
//!
//! * [`read_named_root`] — the file form: a type byte, a name, then the root
//!   `TAG_Compound` payload.
//! * [`read_network_root`] — the 1.20.2+ network form: a type byte then the
//!   root `TAG_Compound` payload, with no name.
//!
//! Both require the input slice to be consumed exactly; trailing bytes are
//! rejected as [`CodecError::TrailingBytes`](ferrumc_codec::CodecError) wrapped
//! in [`NbtError::Codec`]. Callers framing NBT alongside other data must slice
//! the input down to the tag first.
//!
//! When the NBT is embedded inside a larger buffer (for example a packet with
//! trailing fields), use the consumed-bytes variants instead:
//!
//! * [`read_named_root_with_consumed`]
//! * [`read_network_root_with_consumed`]
//!
//! They parse exactly one root, return the number of bytes it consumed, and
//! leave the trailing bytes for the caller. For these variants `max_bytes`
//! bounds the bytes the NBT is *allowed to consume* rather than the length of
//! the whole input slice (which includes the trailing data).

use ferrumc_codec::BoundedReader;

use crate::error::NbtError;
use crate::limits::NbtLimits;
use crate::tag::{NbtCompound, NbtTag, TagType};
use crate::Result;

/// Decodes a file-form root: `[type=10][name][compound payload]`.
///
/// Returns the root name (commonly empty) alongside the decoded
/// [`NbtTag::Compound`]. Errors if the root tag is not a compound, if any limit
/// is exceeded, or if bytes remain after the root.
pub fn read_named_root(data: &[u8], limits: &NbtLimits) -> Result<(String, NbtTag)> {
    let mut reader = enter(data, limits)?;
    let (name, tag) = parse_named(&mut reader, limits)?;
    reader.finish()?;
    Ok((name, tag))
}

/// Decodes a network-form root: `[type=10][compound payload]` with no name.
///
/// Errors if the root tag is not a compound, if any limit is exceeded, or if
/// bytes remain after the root.
pub fn read_network_root(data: &[u8], limits: &NbtLimits) -> Result<NbtTag> {
    let mut reader = enter(data, limits)?;
    let tag = parse_network(&mut reader, limits)?;
    reader.finish()?;
    Ok(tag)
}

/// Decodes a file-form root embedded in a larger buffer, reporting its length.
///
/// Mirrors [`read_named_root`] but does **not** reject trailing bytes: it parses
/// exactly one root and returns the decoded name and tag alongside the number of
/// bytes the root consumed, so `data[consumed..]` remains for the caller.
///
/// Unlike [`read_named_root`], `max_bytes` bounds the bytes the NBT may
/// *consume*, not the length of `data` (which includes the trailing payload).
/// The reader sees at most `max_bytes` bytes, so a root that tries to read
/// further hits [`CodecError::UnexpectedEof`](ferrumc_codec::CodecError) and is
/// rejected, while a small root inside a large buffer parses normally. Depth,
/// list-length, and string-byte limits are identical to [`read_named_root`].
pub fn read_named_root_with_consumed(
    data: &[u8],
    limits: &NbtLimits,
) -> Result<(String, NbtTag, usize)> {
    let mut reader = enter_view(data, limits);
    let (name, tag) = parse_named(&mut reader, limits)?;
    Ok((name, tag, reader.position()))
}

/// Decodes a network-form root embedded in a larger buffer, reporting its length.
///
/// Mirrors [`read_network_root`] but does **not** reject trailing bytes: it
/// parses exactly one root and returns the decoded tag alongside the number of
/// bytes it consumed, so `data[consumed..]` remains for the caller.
///
/// As with [`read_named_root_with_consumed`], `max_bytes` bounds the bytes the
/// NBT may *consume* rather than the length of `data`; a root that reads past
/// that cap hits [`CodecError::UnexpectedEof`](ferrumc_codec::CodecError) and is
/// rejected.
pub fn read_network_root_with_consumed(data: &[u8], limits: &NbtLimits) -> Result<(NbtTag, usize)> {
    let mut reader = enter_view(data, limits);
    let tag = parse_network(&mut reader, limits)?;
    Ok((tag, reader.position()))
}

/// Parses a file-form root from an already-prepared reader.
///
/// Shared by [`read_named_root`] and [`read_named_root_with_consumed`]; neither
/// the byte-length check nor the trailing-byte check live here, so each caller
/// applies the policy appropriate to its framing.
fn parse_named(reader: &mut BoundedReader<'_>, limits: &NbtLimits) -> Result<(String, NbtTag)> {
    ensure_compound_root(reader)?;
    let name = read_string(reader, limits)?;
    let compound = parse_compound(reader, 1, limits)?;
    Ok((name, NbtTag::Compound(compound)))
}

/// Parses a network-form root from an already-prepared reader.
///
/// Shared by [`read_network_root`] and [`read_network_root_with_consumed`]; see
/// [`parse_named`] for why the framing checks live in the callers.
fn parse_network(reader: &mut BoundedReader<'_>, limits: &NbtLimits) -> Result<NbtTag> {
    ensure_compound_root(reader)?;
    let compound = parse_compound(reader, 1, limits)?;
    Ok(NbtTag::Compound(compound))
}

/// Caps the total input size before any parsing, then wraps it in a reader.
///
/// Bounding the slice up front means every later read is implicitly capped by
/// `max_bytes`, so no length read from the stream can drive an oversized
/// allocation. Used by the whole-slice readers, which also reject trailing
/// bytes; embedded readers use [`enter_view`] instead.
fn enter<'a>(data: &'a [u8], limits: &NbtLimits) -> Result<BoundedReader<'a>> {
    if data.len() > limits.max_bytes() {
        return Err(NbtError::MaxBytesExceeded {
            len: data.len(),
            max: limits.max_bytes(),
        });
    }
    Ok(BoundedReader::new(data))
}

/// Wraps at most `max_bytes` of `data` in a reader for embedded decoding.
///
/// The trailing payload of an embedded NBT lives beyond the root, so capping the
/// whole slice (as [`enter`] does) would be wrong. Instead the reader is handed
/// a view of at most `max_bytes` bytes: a root within that budget parses and
/// reports its true length, while one that tries to read further runs off the
/// end of the view and fails with
/// [`CodecError::UnexpectedEof`](ferrumc_codec::CodecError).
fn enter_view<'a>(data: &'a [u8], limits: &NbtLimits) -> BoundedReader<'a> {
    let view = &data[..data.len().min(limits.max_bytes())];
    BoundedReader::new(view)
}

/// Reads and validates the leading root type byte, which must be `TAG_Compound`.
fn ensure_compound_root(reader: &mut BoundedReader<'_>) -> Result<()> {
    let id = reader.read_u8()?;
    if TagType::from_id(id)? != TagType::Compound {
        return Err(NbtError::UnexpectedRootTag { id });
    }
    Ok(())
}

/// Parses a compound body: `[type][name][payload]...` terminated by `TAG_End`.
///
/// `depth` is the depth of this compound (the root compound is depth 1).
fn parse_compound(
    reader: &mut BoundedReader<'_>,
    depth: usize,
    limits: &NbtLimits,
) -> Result<NbtCompound> {
    if depth > limits.max_depth() {
        return Err(NbtError::DepthExceeded {
            max: limits.max_depth(),
        });
    }

    let mut compound = NbtCompound::new();
    loop {
        let id = reader.read_u8()?;
        if id == 0 {
            // TAG_End terminates the compound; it carries no name or payload.
            return Ok(compound);
        }
        let tag_type = TagType::from_id(id)?;
        let name = read_string(reader, limits)?;
        let value = parse_payload(reader, tag_type, depth + 1, limits)?;
        compound.push(name, value);
    }
}

/// Parses a list body: `[element type][i32 length][payload]*`.
///
/// `depth` is the depth of this list; its elements are parsed one level deeper.
fn parse_list(reader: &mut BoundedReader<'_>, depth: usize, limits: &NbtLimits) -> Result<NbtTag> {
    if depth > limits.max_depth() {
        return Err(NbtError::DepthExceeded {
            max: limits.max_depth(),
        });
    }

    let element_id = reader.read_u8()?;
    let len = read_len(reader)?;
    let element_type = TagType::from_id(element_id)?;

    if element_type == TagType::End {
        // An empty list legitimately declares element type TAG_End. A non-empty
        // one cannot: there are no End "values" to read.
        if len == 0 {
            return Ok(NbtTag::List(Vec::new()));
        }
        return Err(NbtError::MalformedList);
    }

    if len > limits.max_list_len() {
        return Err(NbtError::ListTooLong {
            len,
            max: limits.max_list_len(),
        });
    }

    // Grow as we read rather than pre-allocating from the declared length: a
    // short stream then fails on EOF long before the allocation gets large.
    let mut items = Vec::new();
    for _ in 0..len {
        items.push(parse_payload(reader, element_type, depth + 1, limits)?);
    }
    Ok(NbtTag::List(items))
}

/// Parses a single payload of the given type. Containers descend at `depth`.
fn parse_payload(
    reader: &mut BoundedReader<'_>,
    tag_type: TagType,
    depth: usize,
    limits: &NbtLimits,
) -> Result<NbtTag> {
    let tag = match tag_type {
        TagType::Byte => NbtTag::Byte(reader.read_i8()?),
        TagType::Short => NbtTag::Short(reader.read_i16()?),
        TagType::Int => NbtTag::Int(reader.read_i32()?),
        TagType::Long => NbtTag::Long(reader.read_i64()?),
        TagType::Float => NbtTag::Float(reader.read_f32()?),
        TagType::Double => NbtTag::Double(reader.read_f64()?),
        TagType::ByteArray => parse_byte_array(reader, limits)?,
        TagType::String => NbtTag::String(read_string(reader, limits)?),
        TagType::List => parse_list(reader, depth, limits)?,
        TagType::Compound => NbtTag::Compound(parse_compound(reader, depth, limits)?),
        TagType::IntArray => parse_int_array(reader, limits)?,
        TagType::LongArray => parse_long_array(reader, limits)?,
        // Unreachable in practice: compound and list readers strip TAG_End
        // before dispatching here. Treated defensively as a malformed stream.
        TagType::End => return Err(NbtError::MalformedList),
    };
    Ok(tag)
}

/// Parses a `TAG_Byte_Array` payload: `[i32 length][bytes]`.
fn parse_byte_array(reader: &mut BoundedReader<'_>, limits: &NbtLimits) -> Result<NbtTag> {
    let len = read_len(reader)?;
    if len > limits.max_list_len() {
        return Err(NbtError::ListTooLong {
            len,
            max: limits.max_list_len(),
        });
    }
    // `read_bytes` borrows the already-bounded input, so the only allocation is
    // the exact-size conversion below.
    let raw = reader.read_bytes(len)?;
    let bytes = raw.iter().map(|&b| i8::from_ne_bytes([b])).collect();
    Ok(NbtTag::ByteArray(bytes))
}

/// Parses a `TAG_Int_Array` payload: `[i32 length][i32]*`.
fn parse_int_array(reader: &mut BoundedReader<'_>, limits: &NbtLimits) -> Result<NbtTag> {
    let len = read_len(reader)?;
    if len > limits.max_list_len() {
        return Err(NbtError::ListTooLong {
            len,
            max: limits.max_list_len(),
        });
    }
    let mut values = Vec::new();
    for _ in 0..len {
        values.push(reader.read_i32()?);
    }
    Ok(NbtTag::IntArray(values))
}

/// Parses a `TAG_Long_Array` payload: `[i32 length][i64]*`.
fn parse_long_array(reader: &mut BoundedReader<'_>, limits: &NbtLimits) -> Result<NbtTag> {
    let len = read_len(reader)?;
    if len > limits.max_list_len() {
        return Err(NbtError::ListTooLong {
            len,
            max: limits.max_list_len(),
        });
    }
    let mut values = Vec::new();
    for _ in 0..len {
        values.push(reader.read_i64()?);
    }
    Ok(NbtTag::LongArray(values))
}

/// Reads an `i32` sequence length and validates it as non-negative.
fn read_len(reader: &mut BoundedReader<'_>) -> Result<usize> {
    let raw = reader.read_i32()?;
    if raw < 0 {
        return Err(NbtError::NegativeLength { len: raw });
    }
    // A non-negative i32 always fits a 32- or 64-bit usize.
    Ok(raw as usize)
}

/// Reads a `TAG_String`: a `u16` byte length, bounded, then validated `UTF-8`.
fn read_string(reader: &mut BoundedReader<'_>, limits: &NbtLimits) -> Result<String> {
    let len = usize::from(reader.read_u16()?);
    if len > limits.max_string_bytes() {
        return Err(NbtError::StringTooLong {
            len,
            max: limits.max_string_bytes(),
        });
    }
    let bytes = reader.read_bytes(len)?;
    let text = core::str::from_utf8(bytes).map_err(|_| NbtError::InvalidUtf8)?;
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::{write_named_root, write_network_root};
    use ferrumc_codec::CodecError;

    /// `[0x0A][u16=0 name][body...]` — wraps a compound body in a named root.
    fn named_root(body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x0A, 0x00, 0x00];
        bytes.extend_from_slice(body);
        bytes
    }

    /// `[0x0A][body...]` — wraps a compound body in a network (nameless) root.
    fn network_root(body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x0A];
        bytes.extend_from_slice(body);
        bytes
    }

    fn default_limits() -> NbtLimits {
        NbtLimits::default()
    }

    #[test]
    fn reads_every_scalar_type() {
        // Entry layout per tag: [type][u16 name len][name][payload].
        let mut body = Vec::new();
        body.extend_from_slice(&[0x01, 0x00, 0x01, b'b', 0x05]); // Byte = 5
        body.extend_from_slice(&[0x02, 0x00, 0x01, b's', 0x00, 0x09]); // Short = 9
        body.extend_from_slice(&[0x03, 0x00, 0x01, b'i', 0x00, 0x00, 0x00, 0x2A]); // Int = 42
        body.extend_from_slice(&[
            0x04, 0x00, 0x01, b'l', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07,
        ]); // Long = 7
        body.push(0x00); // TAG_End

        let bytes = named_root(&body);
        let (name, tag) = read_named_root(&bytes, &default_limits()).expect("valid");
        assert_eq!(name, "");
        let NbtTag::Compound(compound) = tag else {
            panic!("root is a compound");
        };
        assert_eq!(compound.get("b"), Some(&NbtTag::Byte(5)));
        assert_eq!(compound.get("s"), Some(&NbtTag::Short(9)));
        assert_eq!(compound.get("i"), Some(&NbtTag::Int(42)));
        assert_eq!(compound.get("l"), Some(&NbtTag::Long(7)));
    }

    #[test]
    fn reads_network_root_without_name() {
        // [0x0A][Int "x" = 7][End]
        let bytes = [0x0A, 0x03, 0x00, 0x01, b'x', 0x00, 0x00, 0x00, 0x07, 0x00];
        let tag = read_network_root(&bytes, &default_limits()).expect("valid");
        let NbtTag::Compound(compound) = tag else {
            panic!("root is a compound");
        };
        assert_eq!(compound.get("x"), Some(&NbtTag::Int(7)));
    }

    #[test]
    fn reads_nested_list_of_compounds() {
        // List "l" of 1 Compound, each {Byte "v" = 3}.
        let body = [
            0x09, 0x00, 0x01, b'l', // List "l"
            0x0A, // element type = Compound
            0x00, 0x00, 0x00, 0x01, // length = 1
            0x01, 0x00, 0x01, b'v', 0x03, // Byte "v" = 3
            0x00, // end inner compound
            0x00, // end root compound
        ];
        let bytes = named_root(&body);
        let (_, tag) = read_named_root(&bytes, &default_limits()).expect("valid");
        let NbtTag::Compound(root) = tag else {
            panic!("compound");
        };
        let Some(NbtTag::List(items)) = root.get("l") else {
            panic!("list present");
        };
        assert_eq!(items.len(), 1);
        let NbtTag::Compound(inner) = &items[0] else {
            panic!("compound element");
        };
        assert_eq!(inner.get("v"), Some(&NbtTag::Byte(3)));
    }

    #[test]
    fn empty_list_with_end_element_type_is_ok() {
        let body = [
            0x09, 0x00, 0x01, b'l', 0x00, 0x00, 0x00, 0x00, 0x00, // List len 0, elem End
            0x00, // end root
        ];
        let bytes = named_root(&body);
        let (_, tag) = read_named_root(&bytes, &default_limits()).expect("valid");
        let NbtTag::Compound(root) = tag else {
            panic!("compound");
        };
        assert_eq!(root.get("l"), Some(&NbtTag::List(Vec::new())));
    }

    #[test]
    fn zero_length_string_and_arrays_are_ok() {
        let body = [
            0x08, 0x00, 0x01, b's', 0x00, 0x00, // String "s" = ""
            0x07, 0x00, 0x01, b'a', 0x00, 0x00, 0x00, 0x00, // ByteArray "a" len 0
            0x0B, 0x00, 0x01, b'i', 0x00, 0x00, 0x00, 0x00, // IntArray "i" len 0
            0x00,
        ];
        let bytes = named_root(&body);
        let (_, tag) = read_named_root(&bytes, &default_limits()).expect("valid");
        let NbtTag::Compound(root) = tag else {
            panic!("compound");
        };
        assert_eq!(root.get("s"), Some(&NbtTag::String(String::new())));
        assert_eq!(root.get("a"), Some(&NbtTag::ByteArray(Vec::new())));
        assert_eq!(root.get("i"), Some(&NbtTag::IntArray(Vec::new())));
    }

    #[test]
    fn unknown_tag_type_is_rejected() {
        let body = [0x63]; // 99 is not a valid tag id
        let bytes = named_root(&body);
        assert_eq!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::UnknownTagType { id: 99 })
        );
    }

    #[test]
    fn truncated_input_is_codec_eof() {
        let bytes = [0x0A, 0x00]; // root type then a 1-byte-short name length
        assert!(matches!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn empty_input_is_codec_eof() {
        assert!(matches!(
            read_named_root(&[], &default_limits()),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn negative_byte_array_length_is_rejected() {
        let body = [0x07, 0x00, 0x01, b'a', 0xFF, 0xFF, 0xFF, 0xFF];
        let bytes = named_root(&body);
        assert_eq!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::NegativeLength { len: -1 })
        );
    }

    #[test]
    fn negative_list_length_is_rejected() {
        let body = [0x09, 0x00, 0x01, b'l', 0x01, 0xFF, 0xFF, 0xFF, 0xFF];
        let bytes = named_root(&body);
        assert_eq!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::NegativeLength { len: -1 })
        );
    }

    #[test]
    fn non_empty_end_list_is_malformed() {
        let body = [0x09, 0x00, 0x01, b'l', 0x00, 0x00, 0x00, 0x00, 0x01];
        let bytes = named_root(&body);
        assert_eq!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::MalformedList)
        );
    }

    #[test]
    fn invalid_utf8_string_is_rejected() {
        // String "s" of length 1 with byte 0xFF, which is not valid UTF-8.
        let body = [0x08, 0x00, 0x01, b's', 0x00, 0x01, 0xFF, 0x00];
        let bytes = named_root(&body);
        assert_eq!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::InvalidUtf8)
        );
    }

    #[test]
    fn root_must_be_a_compound() {
        // A bare Byte tag as the root.
        assert_eq!(
            read_named_root(&[0x01], &default_limits()),
            Err(NbtError::UnexpectedRootTag { id: 1 })
        );
    }

    #[test]
    fn root_with_unknown_id_is_unknown_tag_type() {
        assert_eq!(
            read_named_root(&[0x63], &default_limits()),
            Err(NbtError::UnknownTagType { id: 99 })
        );
    }

    #[test]
    fn input_over_max_bytes_is_rejected() {
        let body = [0x00]; // empty compound
        let bytes = named_root(&body);
        let limits = default_limits().with_max_bytes(2);
        assert_eq!(
            read_named_root(&bytes, &limits),
            Err(NbtError::MaxBytesExceeded {
                len: bytes.len(),
                max: 2,
            })
        );
    }

    #[test]
    fn list_over_max_len_is_rejected() {
        // List "l" of 4 bytes, but the limit allows only 2.
        let body = [
            0x09, 0x00, 0x01, b'l', 0x01, 0x00, 0x00, 0x00, 0x04, 0x01, 0x02, 0x03, 0x04,
        ];
        let bytes = named_root(&body);
        let limits = default_limits().with_max_list_len(2);
        assert_eq!(
            read_named_root(&bytes, &limits),
            Err(NbtError::ListTooLong { len: 4, max: 2 })
        );
    }

    #[test]
    fn string_over_max_bytes_is_rejected() {
        // String "s" of 3 bytes "abc", limit 2.
        let body = [0x08, 0x00, 0x01, b's', 0x00, 0x03, b'a', b'b', b'c', 0x00];
        let bytes = named_root(&body);
        let limits = default_limits().with_max_string_bytes(2);
        assert_eq!(
            read_named_root(&bytes, &limits),
            Err(NbtError::StringTooLong { len: 3, max: 2 })
        );
    }

    #[test]
    fn depth_just_over_limit_is_rejected() {
        // Root {a: {}} — the nested compound sits at depth 2.
        let body = [
            0x0A, 0x00, 0x01, b'a', // Compound "a"
            0x00, // end inner
            0x00, // end root
        ];
        let bytes = named_root(&body);

        // max_depth = 2 admits the nested compound.
        assert!(read_named_root(&bytes, &default_limits().with_max_depth(2)).is_ok());
        // max_depth = 1 rejects it: the inner compound would be depth 2.
        assert_eq!(
            read_named_root(&bytes, &default_limits().with_max_depth(1)),
            Err(NbtError::DepthExceeded { max: 1 })
        );
    }

    #[test]
    fn trailing_bytes_after_root_are_rejected() {
        let mut bytes = named_root(&[0x00]); // valid empty compound
        bytes.push(0xAA); // one byte of junk
        assert!(matches!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::Codec(CodecError::TrailingBytes { remaining: 1 }))
        ));
    }

    #[test]
    fn depth_via_nested_lists_is_rejected() {
        // root {l: [[[]]]} — the lists sit at depths 2, 3 and 4 respectively.
        let inner = NbtTag::List(Vec::new());
        let mid = NbtTag::List(vec![inner]);
        let outer = NbtTag::List(vec![mid]);
        let mut root = NbtCompound::new();
        root.push("l", outer);
        let bytes =
            write_named_root("", &NbtTag::Compound(root), &default_limits()).expect("write");

        // The deepest list is at depth 4; max_depth = 4 admits it, 3 rejects it.
        assert!(read_named_root(&bytes, &default_limits().with_max_depth(4)).is_ok());
        assert_eq!(
            read_named_root(&bytes, &default_limits().with_max_depth(3)),
            Err(NbtError::DepthExceeded { max: 3 })
        );
    }

    #[test]
    fn arrays_do_not_count_toward_depth() {
        // root {l: [ IntArray ]} — the array is the list's single element. If
        // arrays counted as a level it would sit at depth 3; they do not, so the
        // tree parses under max_depth = 2, which only admits the compound (1) and
        // the list (2).
        let mut with_array = NbtCompound::new();
        with_array.push("l", NbtTag::List(vec![NbtTag::IntArray(vec![1, 2, 3])]));
        let array_bytes =
            write_named_root("", &NbtTag::Compound(with_array), &default_limits()).expect("write");
        assert!(read_named_root(&array_bytes, &default_limits().with_max_depth(2)).is_ok());

        // Swapping the array for a nested list — which does count — exceeds the
        // same limit, confirming the array was genuinely free.
        let mut with_list = NbtCompound::new();
        with_list.push("l", NbtTag::List(vec![NbtTag::List(Vec::new())]));
        let list_bytes =
            write_named_root("", &NbtTag::Compound(with_list), &default_limits()).expect("write");
        assert_eq!(
            read_named_root(&list_bytes, &default_limits().with_max_depth(2)),
            Err(NbtError::DepthExceeded { max: 2 })
        );
    }

    #[test]
    fn negative_int_array_length_is_rejected() {
        let body = [0x0B, 0x00, 0x01, b'i', 0xFF, 0xFF, 0xFF, 0xFF];
        let bytes = named_root(&body);
        assert_eq!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::NegativeLength { len: -1 })
        );
    }

    #[test]
    fn int_array_over_max_len_is_rejected() {
        // IntArray "i" declares 4 elements; the limit allows only 2.
        let body = [0x0B, 0x00, 0x01, b'i', 0x00, 0x00, 0x00, 0x04];
        let bytes = named_root(&body);
        let limits = default_limits().with_max_list_len(2);
        assert_eq!(
            read_named_root(&bytes, &limits),
            Err(NbtError::ListTooLong { len: 4, max: 2 })
        );
    }

    #[test]
    fn truncated_int_array_payload_is_eof() {
        // Declares 4 ints but only 2 are present.
        let body = [
            0x0B, 0x00, 0x01, b'i', 0x00, 0x00, 0x00, 0x04, // length = 4
            0x00, 0x00, 0x00, 0x01, // int 1
            0x00, 0x00, 0x00, 0x02, // int 2 (then nothing)
        ];
        let bytes = named_root(&body);
        assert!(matches!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn negative_long_array_length_is_rejected() {
        let body = [0x0C, 0x00, 0x01, b'l', 0xFF, 0xFF, 0xFF, 0xFF];
        let bytes = named_root(&body);
        assert_eq!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::NegativeLength { len: -1 })
        );
    }

    #[test]
    fn long_array_over_max_len_is_rejected() {
        let body = [0x0C, 0x00, 0x01, b'l', 0x00, 0x00, 0x00, 0x03];
        let bytes = named_root(&body);
        let limits = default_limits().with_max_list_len(2);
        assert_eq!(
            read_named_root(&bytes, &limits),
            Err(NbtError::ListTooLong { len: 3, max: 2 })
        );
    }

    #[test]
    fn truncated_long_array_payload_is_eof() {
        // Declares 2 longs but only 1 (8 bytes) is present.
        let body = [
            0x0C, 0x00, 0x01, b'l', 0x00, 0x00, 0x00, 0x02, // length = 2
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // long 1 (then nothing)
        ];
        let bytes = named_root(&body);
        assert!(matches!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn compound_truncated_before_end_is_eof() {
        // A valid Byte entry, then the stream ends with no TAG_End: the reader
        // loops back for the next type byte and hits EOF.
        let body = [0x01, 0x00, 0x01, b'b', 0x05];
        let bytes = named_root(&body);
        assert!(matches!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn unknown_list_element_type_is_rejected() {
        // List "l" declaring element type id 99, which is invalid.
        let body = [0x09, 0x00, 0x01, b'l', 0x63, 0x00, 0x00, 0x00, 0x01];
        let bytes = named_root(&body);
        assert_eq!(
            read_named_root(&bytes, &default_limits()),
            Err(NbtError::UnknownTagType { id: 99 })
        );
    }

    #[test]
    fn network_root_must_be_a_compound() {
        assert_eq!(
            read_network_root(&[0x01], &default_limits()),
            Err(NbtError::UnexpectedRootTag { id: 1 })
        );
    }

    #[test]
    fn network_root_with_unknown_id_is_unknown_tag_type() {
        assert_eq!(
            read_network_root(&[0x63], &default_limits()),
            Err(NbtError::UnknownTagType { id: 99 })
        );
    }

    #[test]
    fn network_root_trailing_bytes_are_rejected() {
        // [0x0A][End] is a valid empty network root; append one junk byte.
        let bytes = [0x0A, 0x00, 0xAA];
        assert!(matches!(
            read_network_root(&bytes, &default_limits()),
            Err(NbtError::Codec(CodecError::TrailingBytes { remaining: 1 }))
        ));
    }

    /// A small but non-trivial compound used by the consumed-bytes tests.
    fn sample_root() -> NbtTag {
        let mut root = NbtCompound::new();
        root.push("i", NbtTag::Int(42));
        root.push("s", NbtTag::String("hi".to_owned()));
        NbtTag::Compound(root)
    }

    #[test]
    fn named_consumed_count_is_exact_and_leaves_trailing() {
        let tag = sample_root();
        let nbt = write_named_root("root", &tag, &default_limits()).expect("write");
        let nbt_len = nbt.len();

        let sentinel = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut buffer = nbt;
        buffer.extend_from_slice(&sentinel);

        let (name, decoded, consumed) =
            read_named_root_with_consumed(&buffer, &default_limits()).expect("valid");
        assert_eq!(name, "root");
        assert_eq!(decoded, tag);
        assert_eq!(consumed, nbt_len);
        // The exact NBT length is reported, so the sentinel is left untouched.
        assert_eq!(&buffer[consumed..], &sentinel);
    }

    #[test]
    fn network_consumed_count_is_exact_and_leaves_trailing() {
        let tag = sample_root();
        let nbt = write_network_root(&tag, &default_limits()).expect("write");
        let nbt_len = nbt.len();

        let sentinel = [0x01, 0x02, 0x03];
        let mut buffer = nbt;
        buffer.extend_from_slice(&sentinel);

        let (decoded, consumed) =
            read_network_root_with_consumed(&buffer, &default_limits()).expect("valid");
        assert_eq!(decoded, tag);
        assert_eq!(consumed, nbt_len);
        assert_eq!(&buffer[consumed..], &sentinel);
    }

    #[test]
    fn small_root_in_buffer_larger_than_max_bytes_still_parses() {
        let tag = sample_root();
        let nbt = write_network_root(&tag, &default_limits()).expect("write");
        let nbt_len = nbt.len();

        // The buffer is far larger than max_bytes, but the root itself fits
        // within it: max_bytes bounds the bytes consumed, not the slice length.
        let mut buffer = nbt;
        buffer.extend_from_slice(&vec![0xAA; 4096]);
        let limits = default_limits().with_max_bytes(nbt_len + 1);

        let (decoded, consumed) = read_network_root_with_consumed(&buffer, &limits).expect("valid");
        assert_eq!(decoded, tag);
        assert_eq!(consumed, nbt_len);
    }

    #[test]
    fn root_consuming_more_than_max_bytes_is_rejected() {
        let tag = sample_root();
        let nbt = write_network_root(&tag, &default_limits()).expect("write");
        let nbt_len = nbt.len();

        // Plenty of trailing data, so the slice is long; the root needs more
        // than max_bytes to decode, so it must hit EOF inside the capped view.
        let mut buffer = nbt;
        buffer.extend_from_slice(&[0x00; 64]);
        let limits = default_limits().with_max_bytes(nbt_len - 1);

        assert!(matches!(
            read_network_root_with_consumed(&buffer, &limits),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn consumed_at_exact_max_bytes_boundary_parses() {
        let tag = sample_root();
        let nbt = write_network_root(&tag, &default_limits()).expect("write");
        let nbt_len = nbt.len();

        let mut buffer = nbt;
        buffer.extend_from_slice(&[0x00; 16]);
        // max_bytes exactly equal to the root length is enough to consume it.
        let limits = default_limits().with_max_bytes(nbt_len);

        let (decoded, consumed) = read_network_root_with_consumed(&buffer, &limits).expect("valid");
        assert_eq!(decoded, tag);
        assert_eq!(consumed, nbt_len);
    }

    #[test]
    fn consumed_truncated_input_is_codec_eof() {
        // Named: root type then a 1-byte-short name length, not enough to finish.
        assert!(matches!(
            read_named_root_with_consumed(&[0x0A, 0x00], &default_limits()),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
        // Network: just the root type, then EOF before the compound body's first
        // entry id ([0x0A, 0x00] would be a *valid* empty network root).
        assert!(matches!(
            read_network_root_with_consumed(&[0x0A], &default_limits()),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn consumed_empty_input_is_codec_eof() {
        assert!(matches!(
            read_named_root_with_consumed(&[], &default_limits()),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn consumed_root_must_be_a_compound() {
        // A bare Byte tag as the root, with trailing junk that must be ignored.
        assert_eq!(
            read_network_root_with_consumed(&[0x01, 0xAA, 0xBB], &default_limits()),
            Err(NbtError::UnexpectedRootTag { id: 1 })
        );
    }

    #[test]
    fn consumed_variant_does_not_reject_trailing_bytes() {
        // The whole-slice reader rejects this trailing byte; the consumed
        // variant must instead report a consumed length that excludes it.
        let mut bytes = named_root(&[0x00]); // valid empty compound
        let root_len = bytes.len();
        bytes.push(0xAA); // one byte the consumed variant should leave unread

        let (_, _, consumed) =
            read_named_root_with_consumed(&bytes, &default_limits()).expect("valid");
        assert_eq!(consumed, root_len);
        assert_eq!(&bytes[consumed..], &[0xAA]);
    }

    // The consumed-bytes variants share the `parse_*` helpers with the
    // whole-slice readers, so a regression there could surface only on one
    // path. These tests drive the *internal* limit failures (not just EOF)
    // through both consumed entry points, with trailing bytes present, to pin
    // that the limit error — not a truncation or trailing-byte error — wins.

    #[test]
    fn consumed_named_list_too_long_is_rejected() {
        // List "l" of 4 bytes; the limit allows only 2. The length check fires
        // before the elements (or the trailing junk) are ever read.
        let body = [
            0x09, 0x00, 0x01, b'l', 0x01, 0x00, 0x00, 0x00, 0x04, 0x01, 0x02, 0x03, 0x04,
        ];
        let mut bytes = named_root(&body);
        bytes.extend_from_slice(&[0xDE, 0xAD]); // trailing bytes, must be ignored
        let limits = default_limits().with_max_list_len(2);
        assert_eq!(
            read_named_root_with_consumed(&bytes, &limits),
            Err(NbtError::ListTooLong { len: 4, max: 2 })
        );
    }

    #[test]
    fn consumed_network_list_too_long_is_rejected() {
        let body = [
            0x09, 0x00, 0x01, b'l', 0x01, 0x00, 0x00, 0x00, 0x04, 0x01, 0x02, 0x03, 0x04,
        ];
        let mut bytes = network_root(&body);
        bytes.extend_from_slice(&[0xDE, 0xAD]); // trailing bytes, must be ignored
        let limits = default_limits().with_max_list_len(2);
        assert_eq!(
            read_network_root_with_consumed(&bytes, &limits),
            Err(NbtError::ListTooLong { len: 4, max: 2 })
        );
    }

    #[test]
    fn consumed_named_string_too_long_is_rejected() {
        // String "s" of 3 bytes "abc"; the limit allows only 2.
        let body = [0x08, 0x00, 0x01, b's', 0x00, 0x03, b'a', b'b', b'c', 0x00];
        let mut bytes = named_root(&body);
        bytes.extend_from_slice(&[0xFF, 0xFF]); // trailing bytes, must be ignored
        let limits = default_limits().with_max_string_bytes(2);
        assert_eq!(
            read_named_root_with_consumed(&bytes, &limits),
            Err(NbtError::StringTooLong { len: 3, max: 2 })
        );
    }

    #[test]
    fn consumed_network_string_too_long_is_rejected() {
        let body = [0x08, 0x00, 0x01, b's', 0x00, 0x03, b'a', b'b', b'c', 0x00];
        let mut bytes = network_root(&body);
        bytes.extend_from_slice(&[0xFF, 0xFF]); // trailing bytes, must be ignored
        let limits = default_limits().with_max_string_bytes(2);
        assert_eq!(
            read_network_root_with_consumed(&bytes, &limits),
            Err(NbtError::StringTooLong { len: 3, max: 2 })
        );
    }

    #[test]
    fn consumed_named_depth_exceeded_is_rejected() {
        // Root {a: {}} — the nested compound sits at depth 2, which max_depth = 1
        // forbids. The depth check fires before the trailing byte is reached.
        let body = [
            0x0A, 0x00, 0x01, b'a', // Compound "a"
            0x00, // end inner
            0x00, // end root
        ];
        let mut bytes = named_root(&body);
        bytes.push(0xAA); // trailing byte, must be ignored
        let limits = default_limits().with_max_depth(1);
        assert_eq!(
            read_named_root_with_consumed(&bytes, &limits),
            Err(NbtError::DepthExceeded { max: 1 })
        );
    }

    #[test]
    fn consumed_network_depth_exceeded_is_rejected() {
        let body = [
            0x0A, 0x00, 0x01, b'a', // Compound "a"
            0x00, // end inner
            0x00, // end root
        ];
        let mut bytes = network_root(&body);
        bytes.push(0xAA); // trailing byte, must be ignored
        let limits = default_limits().with_max_depth(1);
        assert_eq!(
            read_network_root_with_consumed(&bytes, &limits),
            Err(NbtError::DepthExceeded { max: 1 })
        );
    }

    #[test]
    fn consumed_named_root_consuming_more_than_max_bytes_is_rejected() {
        // Mirror of the network coverage: plenty of trailing data, so the slice
        // is long, but the root needs more than max_bytes to decode and so must
        // hit EOF inside the capped view rather than reading the trailing bytes.
        let tag = sample_root();
        let nbt = write_named_root("root", &tag, &default_limits()).expect("write");
        let nbt_len = nbt.len();

        let mut buffer = nbt;
        buffer.extend_from_slice(&[0x00; 64]);
        let limits = default_limits().with_max_bytes(nbt_len - 1);

        assert!(matches!(
            read_named_root_with_consumed(&buffer, &limits),
            Err(NbtError::Codec(CodecError::UnexpectedEof { .. }))
        ));
    }

    #[test]
    fn consumed_named_at_exact_max_bytes_boundary_parses() {
        let tag = sample_root();
        let nbt = write_named_root("root", &tag, &default_limits()).expect("write");
        let nbt_len = nbt.len();

        let mut buffer = nbt;
        buffer.extend_from_slice(&[0x00; 16]);
        // max_bytes exactly equal to the root length is enough to consume it.
        let limits = default_limits().with_max_bytes(nbt_len);

        let (name, decoded, consumed) =
            read_named_root_with_consumed(&buffer, &limits).expect("valid");
        assert_eq!(name, "root");
        assert_eq!(decoded, tag);
        assert_eq!(consumed, nbt_len);
    }
}
