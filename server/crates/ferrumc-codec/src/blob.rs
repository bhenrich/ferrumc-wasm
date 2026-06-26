//! [`BoundedBytes`]: a length-prefixed byte blob with an upper size bound.

use bytes::BufMut;

use crate::error::{CodecError, Result};
use crate::reader::BoundedReader;
use crate::writer::write_length_prefix;

/// A length-prefixed byte blob capped at `MAX_BYTES` bytes.
///
/// On the wire this is a `VarInt` byte-length prefix followed by that many raw
/// bytes. Decoding validates the declared length against both `MAX_BYTES` and
/// the bytes actually available **before** allocating, so a hostile prefix can
/// never trick the decoder into reserving a huge buffer.
///
/// ```
/// use ferrumc_codec::BoundedBytes;
///
/// let blob = BoundedBytes::<8>::new(vec![1, 2, 3]);
/// assert!(blob.is_ok());
/// assert!(BoundedBytes::<2>::new(vec![1, 2, 3]).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedBytes<const MAX_BYTES: usize>(Vec<u8>);

impl<const MAX_BYTES: usize> BoundedBytes<MAX_BYTES> {
    /// The largest blob this wrapper can encode: the tighter of `MAX_BYTES` and
    /// the non-negative `VarInt` length domain (`i32::MAX`). The second bound
    /// guarantees [`write`](Self::write) can never emit a length prefix that
    /// would decode back as a negative length.
    const fn max_encodable() -> usize {
        let varint_max = i32::MAX as usize;
        if MAX_BYTES < varint_max {
            MAX_BYTES
        } else {
            varint_max
        }
    }

    /// Wraps owned bytes, rejecting them if longer than the encodable maximum
    /// (see [`max_encodable`](Self::max_encodable)).
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        let cap = Self::max_encodable();
        if bytes.len() > cap {
            return Err(CodecError::BytesTooLong {
                length: bytes.len(),
                max: cap,
            });
        }
        Ok(Self(bytes))
    }

    /// Borrows the blob contents.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the wrapper, returning the owned [`Vec<u8>`].
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }

    /// Decodes a length-prefixed byte blob from `reader`.
    ///
    /// Returns [`CodecError::NegativeLength`] for a negative prefix,
    /// [`CodecError::BytesTooLong`] if the declared length exceeds the encodable
    /// maximum, and [`CodecError::UnexpectedEof`] if fewer bytes are available
    /// than declared.
    pub fn read(reader: &mut BoundedReader<'_>) -> Result<Self> {
        let len = reader.read_var_int_len()?;
        // Bound-check the declared length against our cap *before* allocating.
        let cap = Self::max_encodable();
        if len > cap {
            return Err(CodecError::BytesTooLong {
                length: len,
                max: cap,
            });
        }
        // `read_bytes` then confirms the bytes are actually present before we
        // copy, so the `to_vec` allocation is bounded by both the cap and the
        // real input length.
        let bytes = reader.read_bytes(len)?;
        Ok(Self(bytes.to_vec()))
    }

    /// Encodes the blob as a `VarInt` byte-length prefix followed by its bytes.
    /// Infallible: the length was bounded at construction.
    pub fn write(&self, buf: &mut impl BufMut) {
        write_length_prefix(buf, self.0.len());
        buf.put_slice(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::write_var_int;

    /// Builds a wire blob with an honest length prefix.
    fn encode(bytes: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_var_int(&mut buf, i32::try_from(bytes.len()).unwrap());
        buf.extend_from_slice(bytes);
        buf
    }

    #[test]
    fn new_accepts_within_limit() {
        let blob = BoundedBytes::<4>::new(vec![1, 2, 3]).unwrap();
        assert_eq!(blob.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn new_accepts_exactly_at_limit() {
        let blob = BoundedBytes::<3>::new(vec![1, 2, 3]).unwrap();
        assert_eq!(blob.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn new_rejects_over_limit() {
        assert_eq!(
            BoundedBytes::<2>::new(vec![1, 2, 3]),
            Err(CodecError::BytesTooLong { length: 3, max: 2 })
        );
    }

    #[test]
    fn max_encodable_caps_at_varint_domain() {
        // A sane cap is used verbatim; an absurd cap is clamped to the
        // non-negative VarInt domain so the length prefix can't read back
        // negative. (Triggering the clamp needs a >2 GiB buffer, which isn't
        // unit-testable, so we pin the threshold itself.)
        assert_eq!(BoundedBytes::<4>::max_encodable(), 4);
        assert_eq!(
            BoundedBytes::<{ usize::MAX }>::max_encodable(),
            i32::MAX as usize
        );
    }

    #[test]
    fn read_happy_path() {
        let buf = encode(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let mut reader = BoundedReader::new(&buf);
        let blob = BoundedBytes::<16>::read(&mut reader).unwrap();
        assert_eq!(blob.as_slice(), &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn read_zero_length_blob() {
        let buf = encode(&[]);
        let mut reader = BoundedReader::new(&buf);
        let blob = BoundedBytes::<16>::read(&mut reader).unwrap();
        assert!(blob.as_slice().is_empty());
    }

    #[test]
    fn read_at_exact_limit() {
        let buf = encode(&[1, 2, 3, 4]);
        let mut reader = BoundedReader::new(&buf);
        let blob = BoundedBytes::<4>::read(&mut reader).unwrap();
        assert_eq!(blob.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn read_rejects_over_limit_before_allocating() {
        // Prefix claims 5 bytes, cap is 4. No body provided: rejection must
        // happen on the length check, not the read.
        let mut buf = Vec::new();
        write_var_int(&mut buf, 5);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            BoundedBytes::<4>::read(&mut reader),
            Err(CodecError::BytesTooLong { length: 5, max: 4 })
        );
    }

    #[test]
    fn read_rejects_negative_length() {
        let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]; // VarInt -1
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            BoundedBytes::<16>::read(&mut reader),
            Err(CodecError::NegativeLength { length: -1 })
        );
    }

    #[test]
    fn read_rejects_overlong_length_prefix() {
        // A 6-byte (all continuation) length prefix overruns the VarInt budget;
        // the error must surface through the blob decoder.
        let buf = [0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            BoundedBytes::<16>::read(&mut reader),
            Err(CodecError::VarIntTooLong)
        );
    }

    #[test]
    fn read_truncated_body_is_eof() {
        let mut buf = Vec::new();
        write_var_int(&mut buf, 10);
        buf.extend_from_slice(&[1, 2, 3]); // only 3 of 10 promised bytes
        let mut reader = BoundedReader::new(&buf);
        assert!(matches!(
            BoundedBytes::<16>::read(&mut reader),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn write_then_read_round_trip() {
        let original = BoundedBytes::<16>::new(vec![9, 8, 7, 6, 5]).unwrap();
        let mut buf = Vec::new();
        original.write(&mut buf);
        let mut reader = BoundedReader::new(&buf);
        let decoded = BoundedBytes::<16>::read(&mut reader).unwrap();
        assert_eq!(decoded.as_slice(), &[9, 8, 7, 6, 5]);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn into_inner_returns_owned() {
        let blob = BoundedBytes::<8>::new(vec![1, 2]).unwrap();
        assert_eq!(blob.into_inner(), vec![1, 2]);
    }
}
