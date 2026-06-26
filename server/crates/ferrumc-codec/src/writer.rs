//! Encoders for `VarInt`/`VarLong` and the internal length-prefix helper.
//!
//! The write side is generic over [`bytes::BufMut`], so callers can encode
//! straight into a `BytesMut`, a `Vec<u8>`, or any other buffer without an
//! intermediate allocation.

use bytes::BufMut;

use crate::{CONTINUE_BIT, SEGMENT_BITS};

/// Encodes `value` as a Minecraft `VarInt` (1–5 bytes) into `buf`.
///
/// Negative values are encoded over their full two's-complement bit pattern and
/// therefore always occupy 5 bytes, matching the protocol.
pub fn write_var_int(buf: &mut impl BufMut, value: i32) {
    // Reinterpret as unsigned for logical shifting; sign-extension would emit
    // the wrong bytes.
    let mut bits = value as u32;
    let segment = u32::from(SEGMENT_BITS);
    loop {
        if bits & !segment == 0 {
            buf.put_u8(bits as u8);
            return;
        }
        buf.put_u8(((bits & segment) as u8) | CONTINUE_BIT);
        bits >>= 7;
    }
}

/// Encodes `value` as a Minecraft `VarLong` (1–10 bytes) into `buf`.
pub fn write_var_long(buf: &mut impl BufMut, value: i64) {
    let mut bits = value as u64;
    let segment = u64::from(SEGMENT_BITS);
    loop {
        if bits & !segment == 0 {
            buf.put_u8(bits as u8);
            return;
        }
        buf.put_u8(((bits & segment) as u8) | CONTINUE_BIT);
        bits >>= 7;
    }
}

/// Encodes a non-negative length as a `VarInt` prefix.
///
/// Lengths are conceptually unsigned, so we encode straight from `usize` and
/// never go through a signed cast. Callers only ever pass already-bounded
/// lengths, so the result never exceeds the 5-byte `VarInt` budget.
pub(crate) fn write_length_prefix(buf: &mut impl BufMut, mut len: usize) {
    let segment = usize::from(SEGMENT_BITS);
    loop {
        if len & !segment == 0 {
            buf.put_u8(len as u8);
            return;
        }
        buf.put_u8(((len & segment) as u8) | CONTINUE_BIT);
        len >>= 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::BoundedReader;

    fn round_trip_var_int(value: i32) {
        let mut buf = Vec::new();
        write_var_int(&mut buf, value);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(reader.read_var_int().unwrap(), value);
        assert_eq!(reader.remaining(), 0);
    }

    fn round_trip_var_long(value: i64) {
        let mut buf = Vec::new();
        write_var_long(&mut buf, value);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(reader.read_var_long().unwrap(), value);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn var_int_known_encodings() {
        let mut buf = Vec::new();
        write_var_int(&mut buf, 0);
        assert_eq!(buf, [0x00]);

        buf.clear();
        write_var_int(&mut buf, 128);
        assert_eq!(buf, [0x80, 0x01]);

        buf.clear();
        write_var_int(&mut buf, 25565);
        assert_eq!(buf, [0xDD, 0xC7, 0x01]);

        buf.clear();
        write_var_int(&mut buf, -1);
        assert_eq!(buf, [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
    }

    #[test]
    fn var_int_round_trips_boundaries() {
        for value in [0, 1, -1, 127, 128, 255, i32::MIN, i32::MAX, 2_147_483_646] {
            round_trip_var_int(value);
        }
    }

    #[test]
    fn var_int_never_exceeds_five_bytes() {
        for value in [i32::MIN, i32::MAX, -1, 0] {
            let mut buf = Vec::new();
            write_var_int(&mut buf, value);
            assert!(buf.len() <= 5, "value={value} len={}", buf.len());
        }
    }

    #[test]
    fn var_long_known_encodings() {
        let mut buf = Vec::new();
        write_var_long(&mut buf, 128);
        assert_eq!(buf, [0x80, 0x01]);

        buf.clear();
        write_var_long(&mut buf, -1);
        assert_eq!(
            buf,
            [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]
        );
    }

    #[test]
    fn var_long_round_trips_boundaries() {
        for value in [
            0i64,
            1,
            -1,
            127,
            128,
            i64::MIN,
            i64::MAX,
            i64::from(i32::MIN),
        ] {
            round_trip_var_long(value);
        }
    }

    #[test]
    fn var_long_never_exceeds_ten_bytes() {
        for value in [i64::MIN, i64::MAX, -1, 0] {
            let mut buf = Vec::new();
            write_var_long(&mut buf, value);
            assert!(buf.len() <= 10, "value={value} len={}", buf.len());
        }
    }
}
