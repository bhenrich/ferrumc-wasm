//! [`BoundedReader`]: a forward-only cursor over a byte slice that refuses to
//! read past the end of its input.

use crate::error::{CodecError, Result};
use crate::{CONTINUE_BIT, MAX_VAR_INT_BYTES, MAX_VAR_LONG_BYTES, SEGMENT_BITS};

/// A bounds-checked, forward-only reader over a borrowed byte slice.
///
/// Every read first verifies that enough bytes remain; a read that would run
/// off the end returns [`CodecError::UnexpectedEof`] instead of panicking. This
/// is the type downstream decoders (e.g. `ferrumc-nbt`) build on, so it never
/// trusts a length it was handed.
///
/// Multi-byte integers are read big-endian, matching the Minecraft Java
/// protocol. [`read_bytes`](Self::read_bytes) borrows directly from the
/// underlying slice for zero-copy decoding.
///
/// ```
/// use ferrumc_codec::BoundedReader;
///
/// let data = [0x80u8, 0x01]; // VarInt encoding of 128
/// let mut reader = BoundedReader::new(&data);
/// assert_eq!(reader.read_var_int().ok(), Some(128));
/// assert_eq!(reader.remaining(), 0);
/// ```
pub struct BoundedReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BoundedReader<'a> {
    /// Creates a reader positioned at the start of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The number of bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        // Invariant: `pos` never advances past `data.len()`, so this never wraps.
        self.data.len() - self.pos
    }

    /// `true` when no unread bytes remain.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The number of bytes consumed so far, measured from the start of input.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Borrows exactly `len` bytes from the input, advancing the cursor.
    ///
    /// Returns [`CodecError::UnexpectedEof`] if fewer than `len` bytes remain.
    /// The returned slice borrows the reader's backing buffer, so no copy is
    /// made.
    pub fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        self.take(len)
    }

    /// Reads a single unsigned byte.
    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Reads a single signed byte.
    pub fn read_i8(&mut self) -> Result<i8> {
        Ok(i8::from_ne_bytes([self.read_u8()?]))
    }

    /// Reads a big-endian unsigned 16-bit integer.
    pub fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.read_array()?))
    }

    /// Reads a big-endian signed 16-bit integer.
    pub fn read_i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.read_array()?))
    }

    /// Reads a big-endian unsigned 32-bit integer.
    pub fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.read_array()?))
    }

    /// Reads a big-endian signed 32-bit integer.
    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.read_array()?))
    }

    /// Reads a big-endian unsigned 64-bit integer.
    pub fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.read_array()?))
    }

    /// Reads a big-endian signed 64-bit integer.
    pub fn read_i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.read_array()?))
    }

    /// Reads a big-endian IEEE-754 32-bit float.
    pub fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.read_array()?))
    }

    /// Reads a big-endian IEEE-754 64-bit float.
    pub fn read_f64(&mut self) -> Result<f64> {
        Ok(f64::from_be_bytes(self.read_array()?))
    }

    /// Reads a raw Minecraft `VarInt` (signed 32-bit, LEB128-style).
    ///
    /// Negative values legitimately encode as the full 5 bytes. Anything that
    /// fails to terminate within 5 bytes is rejected with
    /// [`CodecError::VarIntTooLong`]. To use a `VarInt` as a length, prefer
    /// [`read_var_int_len`](Self::read_var_int_len), which rejects negatives.
    ///
    /// Overlong-but-within-5-byte encodings are accepted (matching the
    /// Notchian client): any bits of the final byte that fall outside the
    /// 32-bit value are silently discarded.
    pub fn read_var_int(&mut self) -> Result<i32> {
        let mut value: u32 = 0;
        let mut shift: u32 = 0;
        for _ in 0..MAX_VAR_INT_BYTES {
            let byte = self.read_u8()?;
            value |= u32::from(byte & SEGMENT_BITS) << shift;
            if byte & CONTINUE_BIT == 0 {
                // Reinterpret the bit pattern as i32; `as` here would trip
                // clippy::cast_possible_wrap, so go through the byte buffer.
                return Ok(i32::from_ne_bytes(value.to_ne_bytes()));
            }
            shift += 7;
        }
        Err(CodecError::VarIntTooLong)
    }

    /// Reads a raw Minecraft `VarLong` (signed 64-bit, LEB128-style).
    ///
    /// Rejects anything that fails to terminate within 10 bytes with
    /// [`CodecError::VarLongTooLong`].
    pub fn read_var_long(&mut self) -> Result<i64> {
        let mut value: u64 = 0;
        let mut shift: u32 = 0;
        for _ in 0..MAX_VAR_LONG_BYTES {
            let byte = self.read_u8()?;
            value |= u64::from(byte & SEGMENT_BITS) << shift;
            if byte & CONTINUE_BIT == 0 {
                return Ok(i64::from_ne_bytes(value.to_ne_bytes()));
            }
            shift += 7;
        }
        Err(CodecError::VarLongTooLong)
    }

    /// Reads a `VarInt` and validates it as a non-negative length.
    ///
    /// Returns [`CodecError::NegativeLength`] for negative values. This is the
    /// length-validated counterpart to [`read_var_int`](Self::read_var_int).
    pub fn read_var_int_len(&mut self) -> Result<usize> {
        let value = self.read_var_int()?;
        usize::try_from(value).map_err(|_| CodecError::NegativeLength { length: value })
    }

    /// Strict end-of-input check: `Ok(())` only if everything was consumed.
    ///
    /// Returns [`CodecError::TrailingBytes`] when unread bytes remain. Use this
    /// after decoding a self-delimiting message to reject junk on the wire.
    pub fn finish(&self) -> Result<()> {
        match self.remaining() {
            0 => Ok(()),
            remaining => Err(CodecError::TrailingBytes { remaining }),
        }
    }

    /// Consumes `len` bytes, enforcing the remaining-bytes bound ourselves
    /// rather than relying on slice indexing to panic.
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        // `checked_add` guards against a hostile `len` overflowing `pos`.
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.data.len())
            .ok_or(CodecError::UnexpectedEof {
                needed: len,
                remaining: self.remaining(),
            })?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Reads a fixed-size array of `N` bytes for the integer/float readers.
    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut buf = [0u8; N];
        buf.copy_from_slice(self.take(N)?);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fixed_width_big_endian() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_u16().unwrap(), 0x0102);
        assert_eq!(reader.read_i16().unwrap(), 0x0304);
        assert_eq!(reader.read_u32().unwrap(), 0x0506_0708);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn read_u8_and_i8_round_trip() {
        let data = [0x00, 0xFF, 0x80, 0x7F];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_u8().unwrap(), 0x00);
        assert_eq!(reader.read_u8().unwrap(), 0xFF);
        assert_eq!(reader.read_i8().unwrap(), -128);
        assert_eq!(reader.read_i8().unwrap(), 127);
    }

    #[test]
    fn reads_floats() {
        let mut data = Vec::new();
        data.extend_from_slice(&1.5f32.to_be_bytes());
        data.extend_from_slice(&(-2.25f64).to_be_bytes());
        let mut reader = BoundedReader::new(&data);
        // Compare bit patterns to dodge clippy::float_cmp; the decode is exact.
        assert_eq!(reader.read_f32().unwrap().to_bits(), 1.5f32.to_bits());
        assert_eq!(reader.read_f64().unwrap().to_bits(), (-2.25f64).to_bits());
    }

    #[test]
    fn read_bytes_borrows_and_advances() {
        let data = [0xAA, 0xBB, 0xCC];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_bytes(2).unwrap(), &[0xAA, 0xBB]);
        assert_eq!(reader.position(), 2);
        assert_eq!(reader.remaining(), 1);
    }

    #[test]
    fn read_bytes_zero_length_is_empty_slice() {
        let data = [0x01];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_bytes(0).unwrap(), &[] as &[u8]);
        assert_eq!(reader.remaining(), 1);
    }

    #[test]
    fn read_bytes_past_end_is_eof() {
        let data = [0x01, 0x02];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(
            reader.read_bytes(3),
            Err(CodecError::UnexpectedEof {
                needed: 3,
                remaining: 2
            })
        );
    }

    #[test]
    fn read_bytes_rejects_overflowing_length() {
        let data = [0x01];
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_bytes(usize::MAX),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn fixed_width_read_truncated_is_eof() {
        let data = [0x01];
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_u32(),
            Err(CodecError::UnexpectedEof { .. })
        ));
        // The failed read must not have advanced the cursor.
        assert_eq!(reader.remaining(), 1);
    }

    #[test]
    fn read_u64_truncated_is_eof() {
        let data = [0x01, 0x02, 0x03]; // only 3 of 8 bytes
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_u64(),
            Err(CodecError::UnexpectedEof { .. })
        ));
        assert_eq!(reader.remaining(), 3);
    }

    #[test]
    fn read_i64_truncated_is_eof() {
        let data = [0xFF, 0xFF, 0xFF, 0xFF]; // only 4 of 8 bytes
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_i64(),
            Err(CodecError::UnexpectedEof { .. })
        ));
        assert_eq!(reader.remaining(), 4);
    }

    #[test]
    fn read_f64_truncated_is_eof() {
        let data = [0x00, 0x00, 0x00, 0x00, 0x00]; // only 5 of 8 bytes
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_f64(),
            Err(CodecError::UnexpectedEof { .. })
        ));
        assert_eq!(reader.remaining(), 5);
    }

    #[test]
    fn read_on_empty_is_eof() {
        let data: [u8; 0] = [];
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_u8(),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn var_int_happy_values() {
        let cases: &[(&[u8], i32)] = &[
            (&[0x00], 0),
            (&[0x01], 1),
            (&[0x02], 2),
            (&[0x7F], 127),
            (&[0x80, 0x01], 128),
            (&[0xFF, 0x01], 255),
            (&[0xDD, 0xC7, 0x01], 25565),
            (&[0xFF, 0xFF, 0xFF, 0xFF, 0x07], 2_147_483_647),
            (&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F], -1),
            (&[0x80, 0x80, 0x80, 0x80, 0x08], i32::MIN),
        ];
        for (bytes, expected) in cases {
            let mut reader = BoundedReader::new(bytes);
            assert_eq!(reader.read_var_int().unwrap(), *expected, "bytes={bytes:?}");
            assert_eq!(reader.remaining(), 0);
        }
    }

    #[test]
    fn var_int_overlong_within_five_bytes_is_accepted() {
        // Setting the unused high bits of the 5th byte is technically
        // "overlong", but Notchian clients accept it: the extra bits are simply
        // shifted out of the 32-bit value. Pin that lenient behavior — this
        // still decodes to -1, identical to the canonical [.., 0x0F] encoding.
        let data = [0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_var_int().unwrap(), -1);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn var_int_six_bytes_is_too_long() {
        // Five continuation bytes followed by a terminator: never terminates
        // within the 5-byte budget.
        let data = [0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_var_int(), Err(CodecError::VarIntTooLong));
    }

    #[test]
    fn var_int_truncated_continuation_is_eof() {
        // Continuation bit set but the input ends.
        let data = [0x80];
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_var_int(),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn var_int_empty_is_eof() {
        let data: [u8; 0] = [];
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_var_int(),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn var_int_len_rejects_negative() {
        let data = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]; // -1
        let mut reader = BoundedReader::new(&data);
        assert_eq!(
            reader.read_var_int_len(),
            Err(CodecError::NegativeLength { length: -1 })
        );
    }

    #[test]
    fn var_int_len_accepts_non_negative() {
        let data = [0x80, 0x01]; // 128
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_var_int_len().unwrap(), 128);
    }

    #[test]
    fn var_long_happy_values() {
        let cases: &[(&[u8], i64)] = &[
            (&[0x00], 0),
            (&[0x01], 1),
            (&[0x7F], 127),
            (&[0x80, 0x01], 128),
            (&[0xFF, 0xFF, 0xFF, 0xFF, 0x07], 2_147_483_647),
            (
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F],
                9_223_372_036_854_775_807,
            ),
            (
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01],
                -1,
            ),
            (
                &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01],
                i64::MIN,
            ),
        ];
        for (bytes, expected) in cases {
            let mut reader = BoundedReader::new(bytes);
            assert_eq!(
                reader.read_var_long().unwrap(),
                *expected,
                "bytes={bytes:?}"
            );
            assert_eq!(reader.remaining(), 0);
        }
    }

    #[test]
    fn var_long_eleven_bytes_is_too_long() {
        let data = [
            0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00,
        ];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_var_long(), Err(CodecError::VarLongTooLong));
    }

    #[test]
    fn var_long_truncated_continuation_is_eof() {
        let data = [0x80, 0x80];
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_var_long(),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn var_long_empty_is_eof() {
        let data: [u8; 0] = [];
        let mut reader = BoundedReader::new(&data);
        assert!(matches!(
            reader.read_var_long(),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn finish_ok_when_drained() {
        let data = [0x01];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_u8().unwrap(), 0x01);
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn finish_reports_trailing_bytes() {
        let data = [0x01, 0x02, 0x03];
        let mut reader = BoundedReader::new(&data);
        assert_eq!(reader.read_u8().unwrap(), 0x01);
        assert_eq!(
            reader.finish(),
            Err(CodecError::TrailingBytes { remaining: 2 })
        );
    }
}
