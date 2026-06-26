//! [`BoundedString`]: a length-prefixed UTF-8 string with a length cap.

use bytes::BufMut;

use crate::error::{CodecError, Result};
use crate::reader::BoundedReader;
use crate::writer::write_length_prefix;

/// A UTF-8 string limited to `MAX_CHARS` UTF-16 code units.
///
/// On the wire a string is a `VarInt` byte-length prefix followed by that many
/// UTF-8 bytes (Minecraft Java protocol). Minecraft bounds strings by
/// `String.length()` — i.e. **UTF-16 code units** — while the prefix counts
/// *bytes*, so decoding enforces two limits:
///
/// 1. The byte prefix may not exceed `MAX_CHARS * 4` — a conservative upper
///    bound, since no string of `MAX_CHARS` code units can be longer (a UTF-16
///    code unit costs at most 3 UTF-8 bytes, and astral characters cost 2 code
///    units for 4 bytes). This rejects an oversized prefix *before* any bytes
///    are read.
/// 2. After decoding, the UTF-16 code-unit count may not exceed `MAX_CHARS`.
///
/// The limit is measured in UTF-16 code units to match Java's
/// `String.length()`, so an astral-plane character (e.g. an emoji) counts as 2,
/// exactly as vanilla counts it. This type enforces the structural cap; the
/// exact protocol field limits are applied at the proto-wiring layer.
///
/// ```
/// use ferrumc_codec::BoundedString;
///
/// let s = BoundedString::<16>::new("hello".to_string());
/// assert!(s.is_ok());
/// assert!(BoundedString::<3>::new("too long".to_string()).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedString<const MAX_CHARS: usize>(String);

impl<const MAX_CHARS: usize> BoundedString<MAX_CHARS> {
    /// Upper bound on the UTF-8 byte length we will encode, fixed at the
    /// non-negative `VarInt` length domain. Guards [`write`](Self::write)
    /// against emitting a prefix that would decode back as a negative length.
    /// (For any sane `MAX_CHARS` the code-unit limit bounds the byte length far
    /// below this; the guard only matters for absurdly large caps.)
    const fn max_encoded_bytes() -> usize {
        i32::MAX as usize
    }

    /// Wraps an owned string, rejecting it if it exceeds `MAX_CHARS` UTF-16
    /// code units (matching Java `String.length()`).
    pub fn new(value: String) -> Result<Self> {
        let units = value.encode_utf16().count();
        if units > MAX_CHARS {
            return Err(CodecError::StringTooLong {
                length: units,
                max: MAX_CHARS,
            });
        }
        // The wire prefix is the UTF-8 byte length encoded as a VarInt; reject
        // a length that would not fit the non-negative VarInt domain.
        if value.len() > Self::max_encoded_bytes() {
            return Err(CodecError::StringTooLong {
                length: value.len(),
                max: Self::max_encoded_bytes(),
            });
        }
        Ok(Self(value))
    }

    /// Borrows the string contents.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper, returning the owned [`String`].
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Decodes a length-prefixed UTF-8 string from `reader`.
    ///
    /// Returns [`CodecError::NegativeLength`] for a negative prefix,
    /// [`CodecError::StringTooLong`] if the byte prefix or decoded UTF-16
    /// code-unit count exceeds the limit, [`CodecError::UnexpectedEof`] if the
    /// input is truncated, and [`CodecError::InvalidUtf8`] for malformed UTF-8.
    pub fn read(reader: &mut BoundedReader<'_>) -> Result<Self> {
        let byte_len = reader.read_var_int_len()?;
        // Reject an impossibly long prefix before touching the buffer, so a
        // hostile prefix can't make us read (and validate) megabytes of input.
        let max_bytes = MAX_CHARS.saturating_mul(4);
        if byte_len > max_bytes {
            return Err(CodecError::StringTooLong {
                length: byte_len,
                max: max_bytes,
            });
        }
        let bytes = reader.read_bytes(byte_len)?;
        let text = core::str::from_utf8(bytes)?;
        // Limit measured in UTF-16 code units to match Java String.length().
        let units = text.encode_utf16().count();
        if units > MAX_CHARS {
            return Err(CodecError::StringTooLong {
                length: units,
                max: MAX_CHARS,
            });
        }
        Ok(Self(text.to_owned()))
    }

    /// Encodes the string as a `VarInt` byte-length prefix followed by its
    /// UTF-8 bytes. Infallible: the value was bounded at construction.
    pub fn write(&self, buf: &mut impl BufMut) {
        write_length_prefix(buf, self.0.len());
        buf.put_slice(self.0.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::write_var_int;

    /// Builds a wire string with an honest byte-length prefix.
    fn encode(text: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        write_var_int(&mut buf, i32::try_from(text.len()).unwrap());
        buf.extend_from_slice(text.as_bytes());
        buf
    }

    #[test]
    fn new_accepts_within_limit() {
        let s = BoundedString::<5>::new("héllo".to_string()).unwrap();
        assert_eq!(s.as_str(), "héllo");
    }

    #[test]
    fn new_rejects_over_char_limit() {
        // "héllo" is 5 code units but 6 bytes; the limit is on code units.
        assert_eq!(
            BoundedString::<4>::new("héllo".to_string()),
            Err(CodecError::StringTooLong { length: 5, max: 4 })
        );
    }

    #[test]
    fn new_counts_utf16_code_units_not_scalars() {
        // U+1F600 is 1 Unicode scalar but 2 UTF-16 code units (Java counts 2).
        let emoji = "\u{1F600}".to_string();
        assert_eq!(
            BoundedString::<1>::new(emoji.clone()),
            Err(CodecError::StringTooLong { length: 2, max: 1 })
        );
        assert_eq!(
            BoundedString::<2>::new(emoji).unwrap().as_str(),
            "\u{1F600}"
        );
    }

    #[test]
    fn read_counts_utf16_code_units_not_scalars() {
        // The 4-byte prefix clears the <1>::MAX*4 pre-check, so this exercises
        // the post-decode code-unit count specifically.
        let buf = encode("\u{1F600}");
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            BoundedString::<1>::read(&mut reader),
            Err(CodecError::StringTooLong { length: 2, max: 1 })
        );

        let buf = encode("\u{1F600}");
        let mut reader = BoundedReader::new(&buf);
        let s = BoundedString::<2>::read(&mut reader).unwrap();
        assert_eq!(s.as_str(), "\u{1F600}");
    }

    #[test]
    fn max_encoded_bytes_is_varint_domain() {
        // The construction guard rejects byte lengths past the non-negative
        // VarInt domain; only the threshold is unit-testable (a >2 GiB input is
        // not), so pin it here.
        assert_eq!(BoundedString::<8>::max_encoded_bytes(), i32::MAX as usize);
    }

    #[test]
    fn read_happy_path() {
        let buf = encode("hello");
        let mut reader = BoundedReader::new(&buf);
        let s = BoundedString::<16>::read(&mut reader).unwrap();
        assert_eq!(s.as_str(), "hello");
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn read_zero_length_string() {
        let buf = encode("");
        let mut reader = BoundedReader::new(&buf);
        let s = BoundedString::<16>::read(&mut reader).unwrap();
        assert_eq!(s.as_str(), "");
    }

    #[test]
    fn read_multibyte_at_char_boundary() {
        // 3 two-byte chars = 6 bytes; fits a 3-char limit exactly.
        let buf = encode("ßßß");
        let mut reader = BoundedReader::new(&buf);
        let s = BoundedString::<3>::read(&mut reader).unwrap();
        assert_eq!(s.as_str(), "ßßß");
    }

    #[test]
    fn read_rejects_decoded_char_count_over_limit() {
        // 4 ASCII chars (4 bytes) is under the byte pre-check (max 12) but over
        // the 3-char limit.
        let buf = encode("abcd");
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            BoundedString::<3>::read(&mut reader),
            Err(CodecError::StringTooLong { length: 4, max: 3 })
        );
    }

    #[test]
    fn read_rejects_oversized_byte_prefix_before_reading() {
        // Prefix claims 9 bytes; max for a 2-char string is 8. Provide no body
        // at all to prove rejection happens before the read.
        let mut buf = Vec::new();
        write_var_int(&mut buf, 9);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            BoundedString::<2>::read(&mut reader),
            Err(CodecError::StringTooLong { length: 9, max: 8 })
        );
    }

    #[test]
    fn read_rejects_negative_length() {
        let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]; // VarInt -1
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            BoundedString::<16>::read(&mut reader),
            Err(CodecError::NegativeLength { length: -1 })
        );
    }

    #[test]
    fn read_rejects_overlong_length_prefix() {
        // A 6-byte (all continuation) length prefix overruns the VarInt budget;
        // the error must surface through the string decoder.
        let buf = [0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            BoundedString::<16>::read(&mut reader),
            Err(CodecError::VarIntTooLong)
        );
    }

    #[test]
    fn read_truncated_body_is_eof() {
        let mut buf = Vec::new();
        write_var_int(&mut buf, 5);
        buf.extend_from_slice(b"ab"); // only 2 of 5 promised bytes
        let mut reader = BoundedReader::new(&buf);
        assert!(matches!(
            BoundedString::<16>::read(&mut reader),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn read_rejects_invalid_utf8() {
        let mut buf = Vec::new();
        write_var_int(&mut buf, 2);
        buf.extend_from_slice(&[0xFF, 0xFE]); // not valid UTF-8
        let mut reader = BoundedReader::new(&buf);
        assert!(matches!(
            BoundedString::<16>::read(&mut reader),
            Err(CodecError::InvalidUtf8(_))
        ));
    }

    #[test]
    fn write_then_read_round_trip() {
        let original = BoundedString::<16>::new("round trip".to_string()).unwrap();
        let mut buf = Vec::new();
        original.write(&mut buf);
        let mut reader = BoundedReader::new(&buf);
        let decoded = BoundedString::<16>::read(&mut reader).unwrap();
        assert_eq!(decoded.as_str(), "round trip");
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn into_inner_returns_owned() {
        let s = BoundedString::<8>::new("owned".to_string()).unwrap();
        assert_eq!(s.into_inner(), String::from("owned"));
    }
}
