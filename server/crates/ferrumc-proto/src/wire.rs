//! Hand-written wire primitives the generated packet codecs call.
//!
//! `ferrumc-codec` covers `VarInt`/`VarLong`, bounded strings/blobs, and
//! fixed-width *reads*, but it has no helpers for UUIDs, booleans, fixed-width
//! *writes*, or prefixed-array length prefixes. This module fills exactly those
//! gaps so the generated code stays tiny and byte-stable, calling only
//! `wire::*`, `BoundedReader`/`BoundedString`, and `ferrumc_nbt`.
//!
//! Everything here is `pub(crate)`: the generated modules are the only callers.
//! Multi-byte integers are big-endian, matching the Minecraft Java protocol.

use bytes::BufMut;
use ferrumc_codec::{BoundedReader, CodecError};
use uuid::Uuid;

/// Reads a UUID as 16 big-endian bytes (two `u64` halves, high then low).
pub(crate) fn read_uuid(reader: &mut BoundedReader<'_>) -> Result<Uuid, CodecError> {
    let high = reader.read_u64()?;
    let low = reader.read_u64()?;
    Ok(Uuid::from_u64_pair(high, low))
}

/// Writes a UUID as 16 big-endian bytes.
pub(crate) fn write_uuid(buf: &mut impl BufMut, value: Uuid) {
    buf.put_u128(value.as_u128());
}

/// Reads a boolean: a single byte, `0` is `false` and any non-zero is `true`
/// (the Notchian client only ever sends `0`/`1`).
pub(crate) fn read_bool(reader: &mut BoundedReader<'_>) -> Result<bool, CodecError> {
    Ok(reader.read_u8()? != 0)
}

/// Writes a boolean as a single `0`/`1` byte.
pub(crate) fn write_bool(buf: &mut impl BufMut, value: bool) {
    buf.put_u8(u8::from(value));
}

/// Writes a single unsigned byte.
pub(crate) fn write_u8(buf: &mut impl BufMut, value: u8) {
    buf.put_u8(value);
}

/// Writes a single signed byte.
pub(crate) fn write_i8(buf: &mut impl BufMut, value: i8) {
    buf.put_i8(value);
}

/// Writes a big-endian unsigned 16-bit integer.
pub(crate) fn write_u16(buf: &mut impl BufMut, value: u16) {
    buf.put_u16(value);
}

/// Writes a big-endian signed 64-bit integer.
pub(crate) fn write_i64(buf: &mut impl BufMut, value: i64) {
    buf.put_i64(value);
}

/// Writes raw bytes verbatim (used for inline, self-delimiting payloads such as
/// network-form NBT).
pub(crate) fn write_raw(buf: &mut impl BufMut, bytes: &[u8]) {
    buf.put_slice(bytes);
}

/// Reads a prefixed-array length: a non-negative `VarInt` count.
///
/// Rejects a negative prefix via [`CodecError::NegativeLength`]. The caller must
/// bound any pre-allocation against the bytes actually remaining (the generated
/// code caps `Vec::with_capacity` at `reader.remaining()`), so a hostile count
/// cannot drive a large reservation; an over-long count simply runs the element
/// loop into [`CodecError::UnexpectedEof`].
pub(crate) fn read_prefixed_len(reader: &mut BoundedReader<'_>) -> Result<usize, CodecError> {
    reader.read_var_int_len()
}

/// Writes a prefixed-array length as a `VarInt`, encoding straight from `usize`
/// so the count never passes through a signed cast.
pub(crate) fn write_prefixed_len(buf: &mut impl BufMut, mut len: usize) {
    const SEGMENT: usize = 0x7F;
    const CONTINUE: u8 = 0x80;
    loop {
        if len & !SEGMENT == 0 {
            buf.put_u8(len as u8);
            return;
        }
        buf.put_u8(((len & SEGMENT) as u8) | CONTINUE);
        len >>= 7;
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use ferrumc_codec::BoundedReader;
    use uuid::Uuid;

    use super::{
        read_bool, read_prefixed_len, read_uuid, write_bool, write_i64, write_i8,
        write_prefixed_len, write_raw, write_u16, write_u8, write_uuid,
    };

    #[test]
    fn uuid_round_trips_big_endian() {
        let value = Uuid::from_u128(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
        let mut buf = BytesMut::new();
        write_uuid(&mut buf, value);
        assert_eq!(buf.len(), 16);
        // High-then-low big-endian layout.
        assert_eq!(&buf[..8], &0x0011_2233_4455_6677u64.to_be_bytes());
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(read_uuid(&mut reader).unwrap(), value);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn uuid_read_truncated_is_eof() {
        let buf = [0u8; 8]; // only half a UUID
        let mut reader = BoundedReader::new(&buf);
        assert!(read_uuid(&mut reader).is_err());
    }

    #[test]
    fn bool_round_trips_and_reads_nonzero_as_true() {
        let mut buf = BytesMut::new();
        write_bool(&mut buf, true);
        write_bool(&mut buf, false);
        assert_eq!(&buf[..], &[1, 0]);

        let raw = [0u8, 1, 2, 255];
        let mut reader = BoundedReader::new(&raw);
        assert!(!read_bool(&mut reader).unwrap());
        assert!(read_bool(&mut reader).unwrap());
        assert!(read_bool(&mut reader).unwrap()); // 2 -> true
        assert!(read_bool(&mut reader).unwrap()); // 255 -> true
    }

    #[test]
    fn bool_read_empty_is_eof() {
        let buf: [u8; 0] = [];
        let mut reader = BoundedReader::new(&buf);
        assert!(read_bool(&mut reader).is_err());
    }

    #[test]
    fn fixed_width_writes_are_big_endian() {
        let mut buf = BytesMut::new();
        write_u8(&mut buf, 0xAB);
        write_i8(&mut buf, -2);
        write_u16(&mut buf, 0x1234);
        write_i64(&mut buf, -1);
        // 0xAB, then -2 as two's complement (0xFE).
        let mut expected = vec![0xABu8, 0xFE];
        expected.extend_from_slice(&0x1234u16.to_be_bytes());
        expected.extend_from_slice(&(-1i64).to_be_bytes());
        assert_eq!(&buf[..], &expected[..]);
    }

    #[test]
    fn write_raw_appends_verbatim() {
        let mut buf = BytesMut::new();
        write_raw(&mut buf, &[1, 2, 3]);
        assert_eq!(&buf[..], &[1, 2, 3]);
    }

    #[test]
    fn prefixed_len_round_trips() {
        for len in [0usize, 1, 127, 128, 300, 100_000] {
            let mut buf = BytesMut::new();
            write_prefixed_len(&mut buf, len);
            let mut reader = BoundedReader::new(&buf);
            assert_eq!(read_prefixed_len(&mut reader).unwrap(), len);
            assert_eq!(reader.remaining(), 0);
        }
    }

    #[test]
    fn prefixed_len_rejects_negative_prefix() {
        // VarInt -1 is a negative length.
        let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F];
        let mut reader = BoundedReader::new(&buf);
        assert!(read_prefixed_len(&mut reader).is_err());
    }

    #[test]
    fn prefixed_len_rejects_bad_varint() {
        // Six continuation bytes never terminate within the VarInt budget.
        let buf = [0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        let mut reader = BoundedReader::new(&buf);
        assert!(read_prefixed_len(&mut reader).is_err());
    }
}
