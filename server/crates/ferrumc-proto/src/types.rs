//! Hand-written wire value types referenced by the generated packet codecs.
//!
//! These are protocol-level value types that need bespoke (de)serialization the
//! declarative spec grammar cannot express. They live here rather than in
//! `ferrumc-math` because `ferrumc-proto` deliberately does not depend on the
//! math crate; a later session/world bridge converts between this wire form and
//! the typed `ferrumc_math::BlockPos`.

use bytes::BufMut;
use ferrumc_codec::{BoundedReader, CodecError};

/// A block position as carried on the wire: a single big-endian `i64` packing a
/// 26-bit signed X, a 26-bit signed Z, and a 12-bit signed Y (`x << 38 | z << 12
/// | y`).
///
/// The components are exposed only through accessors so the packed wire layout
/// stays an implementation detail. Encodable values are bounded by the field
/// widths: X and Z to `-33_554_432..=33_554_431`, Y to `-2048..=2047`; values
/// outside those ranges are truncated to the low bits when packed (matching the
/// Notchian wire behavior), so callers should keep coordinates in range for a
/// lossless round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockPosition {
    x: i32,
    y: i32,
    z: i32,
}

impl BlockPosition {
    /// Bit mask selecting the 26-bit X / Z fields.
    const XZ_MASK: i64 = 0x3FF_FFFF;
    /// Bit mask selecting the 12-bit Y field.
    const Y_MASK: i64 = 0xFFF;

    /// Creates a block position from its component coordinates.
    pub fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// Returns the X coordinate.
    pub fn x(&self) -> i32 {
        self.x
    }

    /// Returns the Y coordinate.
    pub fn y(&self) -> i32 {
        self.y
    }

    /// Returns the Z coordinate.
    pub fn z(&self) -> i32 {
        self.z
    }

    /// Decodes a packed-`i64` block position from `reader`.
    pub fn read(reader: &mut BoundedReader<'_>) -> Result<Self, CodecError> {
        Ok(Self::from_packed(reader.read_i64()?))
    }

    /// Encodes this block position as a single big-endian packed `i64`.
    pub fn write(&self, buf: &mut impl BufMut) {
        buf.put_i64(self.to_packed());
    }

    /// Packs the components into the 26/26/12-bit wire layout.
    fn to_packed(self) -> i64 {
        let x = i64::from(self.x) & Self::XZ_MASK;
        let z = i64::from(self.z) & Self::XZ_MASK;
        let y = i64::from(self.y) & Self::Y_MASK;
        (x << 38) | (z << 12) | y
    }

    /// Unpacks a wire `i64`, sign-extending each field back to an `i32`.
    // Each shifted field provably fits in an i32 (26- and 12-bit signed ranges),
    // so the narrowing casts are intentional and lossless here.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "packed fields are 26/12-bit signed values that fit an i32"
    )]
    fn from_packed(value: i64) -> Self {
        // Arithmetic shifts sign-extend each field from its high bit.
        let x = (value >> 38) as i32;
        let y = (value << 52 >> 52) as i32;
        let z = (value << 26 >> 38) as i32;
        Self { x, y, z }
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use ferrumc_codec::BoundedReader;

    use super::BlockPosition;

    #[test]
    fn round_trips_through_the_wire() {
        for pos in [
            BlockPosition::new(0, 0, 0),
            BlockPosition::new(1, 2, 3),
            BlockPosition::new(-1, -1, -1),
            BlockPosition::new(33_554_431, 2047, 33_554_431),
            BlockPosition::new(-33_554_432, -2048, -33_554_432),
        ] {
            let mut buf = BytesMut::new();
            pos.write(&mut buf);
            assert_eq!(buf.len(), 8, "a position is exactly one i64");
            let mut reader = BoundedReader::new(&buf);
            assert_eq!(BlockPosition::read(&mut reader).unwrap(), pos);
            assert_eq!(reader.remaining(), 0);
        }
    }

    #[test]
    fn matches_known_packed_encoding() {
        // x=1, y=2, z=3 packs as (1 << 38) | (3 << 12) | 2.
        let pos = BlockPosition::new(1, 2, 3);
        let mut buf = BytesMut::new();
        pos.write(&mut buf);
        let expected: i64 = (1i64 << 38) | (3i64 << 12) | 2;
        assert_eq!(&buf[..], &expected.to_be_bytes());
    }

    #[test]
    fn read_truncated_is_error() {
        let buf = [0u8; 4]; // half an i64
        let mut reader = BoundedReader::new(&buf);
        assert!(BlockPosition::read(&mut reader).is_err());
    }
}
