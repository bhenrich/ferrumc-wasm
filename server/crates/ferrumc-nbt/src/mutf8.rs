//! Java Modified UTF-8 (MUTF-8) codec for `TAG_String` payloads.
//!
//! An NBT `TAG_String` is encoded exactly like Java's
//! `DataOutput.writeUTF` / `DataInput.readUTF`: a `u16` byte-length prefix
//! (written and validated by the caller) followed by the string bytes in
//! *Modified* UTF-8. Modified UTF-8 differs from standard UTF-8 in two ways:
//!
//! * `U+0000` is written as the two-byte sequence `0xC0 0x80`, never a bare
//!   `0x00`, so an encoded string never contains an interior NUL byte.
//! * A character outside the Basic Multilingual Plane (`U+10000`..=`U+10FFFF`,
//!   e.g. an emoji) is first split into its UTF-16 surrogate pair, and each
//!   surrogate is then written as its own three-byte group (the CESU-8 form) —
//!   six bytes in total, never the four-byte form standard UTF-8 uses.
//!
//! A real 1.21.8 client decodes `TAG_String` with `readUTF` semantics, so a
//! standard-UTF-8 four-byte lead (`0xF0`..=`0xF4`) for an astral character has
//! no valid Modified UTF-8 form: the client raises `UTFDataFormatException` and
//! disconnects. Encoding here in Modified UTF-8 keeps such strings legible to
//! the client, and decoding here accepts the Modified UTF-8 a client or
//! on-disk NBT produces.

use crate::error::NbtError;

/// Appends the Modified UTF-8 encoding of `text` to `out`.
pub(crate) fn encode(out: &mut Vec<u8>, text: &str) {
    for ch in text.chars() {
        encode_char(out, ch);
    }
}

/// Returns the number of bytes `text` occupies in Modified UTF-8.
///
/// Computed without allocating so the caller can validate the `u16` length
/// prefix before encoding the bytes.
pub(crate) fn encoded_len(text: &str) -> usize {
    text.chars().map(encoded_char_len).sum()
}

/// The Modified UTF-8 byte length of a single character.
fn encoded_char_len(ch: char) -> usize {
    match u32::from(ch) {
        0x0001..=0x007F => 1,
        // NUL is the two-byte 0xC0 0x80 form, the same width as the 0x80..=0x7FF
        // range below.
        0 | 0x0080..=0x07FF => 2,
        0x0800..=0xFFFF => 3,
        _ => 6, // Astral: a surrogate pair, three bytes each.
    }
}

/// Appends the Modified UTF-8 bytes of a single character to `out`.
fn encode_char(out: &mut Vec<u8>, ch: char) {
    let code = u32::from(ch);
    match code {
        0 => out.extend_from_slice(&[0xC0, 0x80]),
        0x0001..=0x007F => out.push(code as u8),
        0x0080..=0x07FF => {
            out.push(0xC0 | ((code >> 6) as u8));
            out.push(0x80 | ((code & 0x3F) as u8));
        }
        0x0800..=0xFFFF => push_three(out, code as u16),
        _ => {
            // Astral plane: split into a UTF-16 surrogate pair and write each
            // surrogate as its own three-byte group.
            let v = code - 0x1_0000;
            let high = 0xD800 + ((v >> 10) as u16);
            let low = 0xDC00 + ((v & 0x3FF) as u16);
            push_three(out, high);
            push_three(out, low);
        }
    }
}

/// Writes one 16-bit code unit as a `1110xxxx 10xxxxxx 10xxxxxx` group.
fn push_three(out: &mut Vec<u8>, unit: u16) {
    out.push(0xE0 | ((unit >> 12) as u8));
    out.push(0x80 | (((unit >> 6) & 0x3F) as u8));
    out.push(0x80 | ((unit & 0x3F) as u8));
}

/// Decodes a Modified UTF-8 byte slice into a [`String`].
///
/// Returns [`NbtError::InvalidUtf8`] for any sequence `readUTF` would reject: an
/// invalid lead byte, a missing or malformed continuation byte, a multi-byte
/// group that runs off the end of the slice, or a surrogate code unit that is
/// not part of a valid high/low pair.
pub(crate) fn decode(bytes: &[u8]) -> Result<String, NbtError> {
    // Each decoded UTF-16 code unit consumes at least one input byte, so the
    // already-bounded input length is a safe (over-)estimate of the capacity.
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&lead) = bytes.get(index) {
        if lead & 0x80 == 0 {
            // 0xxxxxxx: a single byte (readUTF also accepts a bare 0x00 here).
            units.push(u16::from(lead));
            index += 1;
        } else if lead & 0xE0 == 0xC0 {
            // 110xxxxx 10xxxxxx
            let b = continuation(bytes, index + 1)?;
            units.push((u16::from(lead & 0x1F) << 6) | u16::from(b & 0x3F));
            index += 2;
        } else if lead & 0xF0 == 0xE0 {
            // 1110xxxx 10xxxxxx 10xxxxxx
            let b = continuation(bytes, index + 1)?;
            let c = continuation(bytes, index + 2)?;
            units.push(
                (u16::from(lead & 0x0F) << 12) | (u16::from(b & 0x3F) << 6) | u16::from(c & 0x3F),
            );
            index += 3;
        } else {
            // A bare continuation byte (10xxxxxx) or a standard-UTF-8 four-byte
            // lead (0xF0..) — neither is valid Modified UTF-8.
            return Err(NbtError::InvalidUtf8);
        }
    }

    // Combine surrogate pairs into scalar values; reject any unpaired surrogate.
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| NbtError::InvalidUtf8)
}

/// Returns the byte at `index`, requiring it to be a `10xxxxxx` continuation.
fn continuation(bytes: &[u8], index: usize) -> Result<u8, NbtError> {
    match bytes.get(index) {
        Some(&b) if b & 0xC0 == 0x80 => Ok(b),
        _ => Err(NbtError::InvalidUtf8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips `text` through encode/decode and returns the encoded bytes.
    fn round_trip(text: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        encode(&mut bytes, text);
        assert_eq!(encoded_len(text), bytes.len(), "encoded_len must agree");
        assert_eq!(decode(&bytes).expect("decode"), text);
        bytes
    }

    #[test]
    fn ascii_is_byte_identical_to_standard_utf8() {
        let bytes = round_trip("Hello, world!");
        assert_eq!(bytes, b"Hello, world!");
    }

    #[test]
    fn nul_uses_the_two_byte_form() {
        // A bare 0x00 must never appear; NUL is 0xC0 0x80.
        let bytes = round_trip("a\0b");
        assert_eq!(bytes, [b'a', 0xC0, 0x80, b'b']);
    }

    #[test]
    fn two_and_three_byte_bmp_chars_round_trip() {
        // U+00A7 (section sign) is two bytes; U+20AC (euro) is three.
        let bytes = round_trip("\u{00A7}\u{20AC}");
        assert_eq!(bytes, [0xC2, 0xA7, 0xE2, 0x82, 0xAC]);
    }

    #[test]
    fn astral_char_is_a_six_byte_surrogate_pair_not_standard_utf8() {
        // U+1F600 (grinning face). Standard UTF-8 would be the four-byte
        // F0 9F 98 80; Modified UTF-8 is the six-byte CESU-8 surrogate pair.
        let bytes = round_trip("\u{1F600}");
        assert_eq!(bytes, [0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]);
        // The four-byte lead a client cannot read must be absent.
        assert!(
            !bytes.iter().any(|&b| (0xF0..=0xF4).contains(&b)),
            "no standard-UTF-8 astral lead byte may appear"
        );
    }

    #[test]
    fn mixed_ascii_bmp_and_astral_round_trips() {
        round_trip("hi \u{00A7}\u{20AC} \u{1F600}\u{1F680} bye");
    }

    #[test]
    fn standard_utf8_four_byte_lead_is_rejected() {
        // The standard-UTF-8 encoding of U+1F600 is not valid Modified UTF-8.
        assert_eq!(
            decode(&[0xF0, 0x9F, 0x98, 0x80]),
            Err(NbtError::InvalidUtf8)
        );
    }

    #[test]
    fn truncated_multibyte_group_is_rejected() {
        // A three-byte lead with only one of its two continuation bytes.
        assert_eq!(decode(&[0xE2, 0x82]), Err(NbtError::InvalidUtf8));
        // A two-byte lead at the very end of the slice.
        assert_eq!(decode(&[0xC2]), Err(NbtError::InvalidUtf8));
    }

    #[test]
    fn bad_continuation_byte_is_rejected() {
        // 0xC2 must be followed by a 10xxxxxx byte; 0x41 ('A') is not.
        assert_eq!(decode(&[0xC2, 0x41]), Err(NbtError::InvalidUtf8));
    }

    #[test]
    fn lone_surrogate_is_rejected() {
        // A single high surrogate (U+D83D) with no following low surrogate.
        assert_eq!(decode(&[0xED, 0xA0, 0xBD]), Err(NbtError::InvalidUtf8));
    }

    #[test]
    fn bare_continuation_lead_is_rejected() {
        assert_eq!(decode(&[0x80]), Err(NbtError::InvalidUtf8));
    }
}
