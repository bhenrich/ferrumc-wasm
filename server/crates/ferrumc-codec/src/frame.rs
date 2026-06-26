//! [`FrameLengthReader`]: reads a `VarInt` frame-length prefix against a cap.

use crate::error::{CodecError, Result};
use crate::reader::BoundedReader;

/// Reads the `VarInt` length prefix of a length-delimited frame, enforcing a
/// configurable maximum.
///
/// The Minecraft protocol prefixes each packet frame with its length as a
/// `VarInt`. A hostile peer can advertise an enormous frame to force a large
/// buffer allocation, so the length is validated against `max_frame_size`
/// before the caller ever reserves space for the body.
///
/// ```
/// use ferrumc_codec::{BoundedReader, FrameLengthReader};
///
/// let reader_cfg = FrameLengthReader::new(512 * 1024);
/// let data = [0x05u8]; // a 5-byte frame
/// let mut reader = BoundedReader::new(&data);
/// assert_eq!(reader_cfg.read_length(&mut reader).ok(), Some(5));
/// ```
pub struct FrameLengthReader {
    max_frame_size: usize,
}

impl FrameLengthReader {
    /// Creates a reader that rejects frames larger than `max_frame_size` bytes.
    pub fn new(max_frame_size: usize) -> Self {
        Self { max_frame_size }
    }

    /// The configured maximum frame size, in bytes.
    pub fn max_frame_size(&self) -> usize {
        self.max_frame_size
    }

    /// Reads and validates a frame-length prefix.
    ///
    /// Returns [`CodecError::NegativeLength`] for a negative prefix,
    /// [`CodecError::FrameTooLarge`] if the declared length exceeds the cap,
    /// [`CodecError::VarIntTooLong`] for a malformed prefix, and
    /// [`CodecError::UnexpectedEof`] if the prefix is truncated.
    pub fn read_length(&self, reader: &mut BoundedReader<'_>) -> Result<usize> {
        let len = reader.read_var_int_len()?;
        if len > self.max_frame_size {
            return Err(CodecError::FrameTooLarge {
                length: len,
                max: self.max_frame_size,
            });
        }
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::write_var_int;

    fn prefix(len: i32) -> Vec<u8> {
        let mut buf = Vec::new();
        write_var_int(&mut buf, len);
        buf
    }

    #[test]
    fn reads_length_under_cap() {
        let cfg = FrameLengthReader::new(1024);
        let buf = prefix(300);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(cfg.read_length(&mut reader).unwrap(), 300);
    }

    #[test]
    fn accepts_zero_length_frame() {
        // An empty frame (length 0) is valid — e.g. a keepalive-style packet.
        let cfg = FrameLengthReader::new(512);
        let buf = prefix(0);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(cfg.read_length(&mut reader).unwrap(), 0);
    }

    #[test]
    fn reads_length_exactly_at_cap() {
        let cfg = FrameLengthReader::new(512);
        let buf = prefix(512);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(cfg.read_length(&mut reader).unwrap(), 512);
    }

    #[test]
    fn rejects_length_over_cap() {
        let cfg = FrameLengthReader::new(512);
        let buf = prefix(513);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            cfg.read_length(&mut reader),
            Err(CodecError::FrameTooLarge {
                length: 513,
                max: 512
            })
        );
    }

    #[test]
    fn rejects_negative_length() {
        let cfg = FrameLengthReader::new(512);
        let buf = prefix(-1);
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(
            cfg.read_length(&mut reader),
            Err(CodecError::NegativeLength { length: -1 })
        );
    }

    #[test]
    fn rejects_malformed_prefix() {
        let cfg = FrameLengthReader::new(512);
        // Six continuation bytes: never terminates within the VarInt budget.
        let buf = [0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        let mut reader = BoundedReader::new(&buf);
        assert_eq!(cfg.read_length(&mut reader), Err(CodecError::VarIntTooLong));
    }

    #[test]
    fn truncated_prefix_is_eof() {
        let cfg = FrameLengthReader::new(512);
        let buf = [0x80]; // continuation set, input ends
        let mut reader = BoundedReader::new(&buf);
        assert!(matches!(
            cfg.read_length(&mut reader),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn max_frame_size_is_exposed() {
        let cfg = FrameLengthReader::new(4096);
        assert_eq!(cfg.max_frame_size(), 4096);
    }
}
