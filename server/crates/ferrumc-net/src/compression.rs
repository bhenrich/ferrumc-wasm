//! Post-`SetCompression` packet compression: [`CompressionState`] and the
//! `zlib` framing transform.
//!
//! Once the server sends `SetCompression` with a threshold, every packet body
//! gains a `data_length` prefix:
//!
//! ```text
//! frame body = [VarInt data_length][ zlib(packet_id + body) | packet_id + body ]
//! ```
//!
//! - `data_length == 0` marks an *uncompressed* packet whose size was below the
//!   threshold; the remainder of the frame is the raw `packet_id + body`.
//! - `data_length != 0` is the *uncompressed* size of a `zlib`-compressed
//!   payload that follows.
//!
//! Before compression is negotiated the frame body is the raw `packet_id + body`
//! with no prefix. [`CompressionState::disabled`] models that and makes
//! [`compress`](CompressionState::compress) and
//! [`decompress`](CompressionState::decompress) pass-throughs, so the framing
//! layer can call them uniformly regardless of negotiation.
//!
//! Decompression is hostile-input hardened:
//!
//! - a declared size below the threshold is a protocol violation;
//! - a declared size above the
//!   [`max_decompressed`](CompressionState::max_decompressed) cap is rejected
//!   *before* a single output byte is allocated (zip-bomb defense);
//! - the inflated output must match the declared size exactly, or the frame is
//!   rejected.
//!
//! Both directions are CPU-bound and synchronous, mirroring the rest of the
//! framing layer; the live (M09) path drives them off the async hot path.

use std::io::Write;

use bytes::{BufMut, BytesMut};
use flate2::write::ZlibEncoder;
use flate2::{Compression, Decompress, FlushDecompress, Status};

use ferrumc_codec::{write_var_int, BoundedReader};

use crate::error::CompressionError;

/// Default cap on a single packet's decompressed size: 2 MiB.
///
/// A `zlib` stream a few kilobytes long can inflate to gigabytes; without a
/// ceiling a malicious peer could exhaust memory with one frame. The decoder
/// refuses any packet whose *declared* uncompressed size exceeds this before
/// allocating the output buffer, matching the decompressed-output cap in the
/// networking model.
pub const DEFAULT_MAX_DECOMPRESSED: usize = 2 * 1024 * 1024;

/// The compression negotiation state of one connection.
///
/// A connection starts [`disabled`](Self::disabled). When the server sends a
/// `SetCompression` packet, the connection adopts the advertised threshold via
/// [`from_threshold`](Self::from_threshold); a negative threshold leaves
/// compression disabled. While enabled, packets at or above the threshold are
/// `zlib`-compressed and smaller packets are sent verbatim behind a zero
/// `data_length` marker.
///
/// The type is `Copy` and holds no buffers, so the framing layer can keep one
/// per connection and pass it by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionState {
    // `None` means compression is not negotiated; `Some(t)` is the active
    // threshold, in bytes.
    threshold: Option<usize>,
    // Hard ceiling on a single packet's decompressed size.
    max_decompressed: usize,
}

impl CompressionState {
    /// Builds a state with an explicit threshold and decompressed-output cap.
    ///
    /// Pass `None` for `threshold` to model a connection on which compression
    /// has not been negotiated. Prefer [`disabled`](Self::disabled),
    /// [`enabled`](Self::enabled), or [`from_threshold`](Self::from_threshold)
    /// unless a deployment needs a non-default cap.
    pub const fn with_cap(threshold: Option<usize>, max_decompressed: usize) -> Self {
        Self {
            threshold,
            max_decompressed,
        }
    }

    /// A state in which compression has not been negotiated, with the default
    /// cap. [`compress`](Self::compress) and [`decompress`](Self::decompress)
    /// are pass-throughs.
    pub const fn disabled() -> Self {
        Self::with_cap(None, DEFAULT_MAX_DECOMPRESSED)
    }

    /// A state with compression enabled at `threshold` bytes and the default
    /// cap.
    pub const fn enabled(threshold: usize) -> Self {
        Self::with_cap(Some(threshold), DEFAULT_MAX_DECOMPRESSED)
    }

    /// Builds a state from a `SetCompression` threshold, using the default cap.
    ///
    /// A negative threshold disables compression, matching the protocol's
    /// convention that `SetCompression` with a negative value turns it off.
    pub fn from_threshold(threshold: i32) -> Self {
        match usize::try_from(threshold) {
            Ok(threshold) => Self::enabled(threshold),
            Err(_) => Self::disabled(),
        }
    }

    /// `true` when compression has been negotiated.
    pub fn is_enabled(&self) -> bool {
        self.threshold.is_some()
    }

    /// The active threshold in bytes, or `None` when compression is disabled.
    pub fn threshold(&self) -> Option<usize> {
        self.threshold
    }

    /// The decompressed-output cap, in bytes.
    pub fn max_decompressed(&self) -> usize {
        self.max_decompressed
    }

    /// Wraps a raw `packet_id + body` into a frame body, appending it to `out`.
    ///
    /// When compression is disabled the packet is appended verbatim. When
    /// enabled, packets below the threshold are written behind a zero
    /// `data_length` marker and larger packets are `zlib`-compressed behind a
    /// `data_length` carrying their uncompressed size.
    ///
    /// Returns [`CompressionError::DeclaredTooLarge`] if a to-be-compressed
    /// packet exceeds [`max_decompressed`](Self::max_decompressed), so the
    /// server never emits a frame a conforming peer would refuse. `out` is left
    /// untouched on any error.
    pub fn compress(&self, packet: &[u8], out: &mut BytesMut) -> Result<(), CompressionError> {
        let Some(threshold) = self.threshold else {
            out.put_slice(packet);
            return Ok(());
        };

        // A `data_length` of 0 is reserved to mark an *uncompressed* packet, so a
        // compressed packet must declare a non-zero size. An empty packet can
        // never satisfy that, so it is always sent uncompressed — even at
        // threshold 0, which would otherwise route it through the compressed
        // branch and emit a 0 `data_length` that `decompress` reads back as the
        // uncompressed marker, corrupting the round trip.
        if packet.len() < threshold || packet.is_empty() {
            // Below threshold (or empty): a zero marker, then the raw packet.
            write_var_int(out, 0);
            out.put_slice(packet);
            return Ok(());
        }

        // At or above threshold: declare the uncompressed size, then the stream.
        // Cap-check before compressing so the cast below is always lossless and
        // we never emit a frame larger than a peer would accept.
        if packet.len() > self.max_decompressed {
            return Err(CompressionError::DeclaredTooLarge {
                declared: packet.len(),
                cap: self.max_decompressed,
            });
        }
        let declared =
            i32::try_from(packet.len()).map_err(|_| CompressionError::DeclaredTooLarge {
                declared: packet.len(),
                cap: self.max_decompressed,
            })?;
        let compressed = deflate(packet)?;
        write_var_int(out, declared);
        out.put_slice(&compressed);
        Ok(())
    }

    /// Recovers the raw `packet_id + body` from a frame body.
    ///
    /// When compression is disabled the frame body is returned as the raw
    /// packet. When enabled, the `data_length` prefix selects the path: a zero
    /// declares an uncompressed remainder, and any other value is the declared
    /// uncompressed size of the `zlib` stream that follows.
    ///
    /// The validation order is: a malformed or negative `data_length` is
    /// rejected first; a non-zero declared size below the threshold is a
    /// protocol violation; a declared size above
    /// [`max_decompressed`](Self::max_decompressed) is rejected before any
    /// allocation; and the inflated output must equal the declared size with no
    /// trailing bytes.
    pub fn decompress(&self, frame: &[u8]) -> Result<Vec<u8>, CompressionError> {
        let Some(threshold) = self.threshold else {
            // Compression not negotiated: the frame body is the raw packet.
            return Ok(frame.to_vec());
        };

        let mut reader = BoundedReader::new(frame);
        // The whole frame is present, so a truncated or overlong prefix is a
        // malformed data-length, never "need more".
        let declared = reader
            .read_var_int()
            .map_err(|_| CompressionError::BadDataLength)?;
        // `position` never exceeds the slice length, so the range is always valid.
        let payload = frame.get(reader.position()..).unwrap_or_default();

        if declared == 0 {
            // Below-threshold marker: the remainder is the raw, uncompressed
            // packet. Its size is already bounded by the frame cap upstream.
            return Ok(payload.to_vec());
        }

        let declared = usize::try_from(declared)
            .map_err(|_| CompressionError::NegativeDataLength { length: declared })?;
        if declared < threshold {
            return Err(CompressionError::BelowThreshold {
                declared,
                threshold,
            });
        }
        if declared > self.max_decompressed {
            return Err(CompressionError::DeclaredTooLarge {
                declared,
                cap: self.max_decompressed,
            });
        }

        inflate(payload, declared)
    }
}

/// Compresses `packet` into a standalone `zlib` stream.
///
/// The `Vec` writer grows as needed and never performs real I/O, so the inner
/// `io::Result` is effectively infallible; any error is surfaced as
/// [`CompressionError::MalformedZlib`] rather than panicking.
fn deflate(packet: &[u8]) -> Result<Vec<u8>, CompressionError> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(packet)
        .map_err(|_| CompressionError::MalformedZlib)?;
    encoder
        .finish()
        .map_err(|_| CompressionError::MalformedZlib)
}

/// Inflates `compressed` into exactly `declared` bytes, rejecting any mismatch.
///
/// The output buffer is sized to `declared` (already bounded by the cap), so a
/// stream that expands beyond its declared size simply fills the buffer and is
/// rejected as [`CompressionError::Oversized`] rather than over-allocating.
fn inflate(compressed: &[u8], declared: usize) -> Result<Vec<u8>, CompressionError> {
    let mut decompressor = Decompress::new(true);
    let mut out = vec![0u8; declared];
    let status = decompressor
        .decompress(compressed, &mut out, FlushDecompress::Finish)
        .map_err(|_| CompressionError::MalformedZlib)?;
    // `total_out` never exceeds the `declared`-sized buffer.
    let produced = usize::try_from(decompressor.total_out()).unwrap_or(declared);

    match status {
        Status::StreamEnd => {
            if produced != declared {
                // The buffer caps output at `declared`, so a short read here
                // means the stream genuinely decoded to fewer bytes.
                return Err(CompressionError::SizeMismatch {
                    declared,
                    actual: produced,
                });
            }
            let consumed = usize::try_from(decompressor.total_in()).unwrap_or(compressed.len());
            if consumed != compressed.len() {
                // Junk after a complete zlib stream inside the frame.
                return Err(CompressionError::MalformedZlib);
            }
            Ok(out)
        }
        // `Finish` did not complete. A full output buffer means the stream has
        // more to emit than it declared; otherwise the input ran out early.
        Status::Ok | Status::BufError => {
            if produced >= declared {
                Err(CompressionError::Oversized { declared })
            } else {
                Err(CompressionError::MalformedZlib)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DisconnectClass;

    /// Builds a compressed-format frame body with an arbitrary declared size.
    fn frame(declared: i32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_var_int(&mut out, declared);
        out.extend_from_slice(payload);
        out
    }

    /// Produces a standalone zlib stream for `data` (the on-wire payload form).
    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn uncompressed_below_threshold_roundtrip() {
        let state = CompressionState::enabled(128);
        let packet = b"\x00small packet body".to_vec();
        let mut out = BytesMut::new();
        state.compress(&packet, &mut out).unwrap();
        // A zero data-length marker precedes the raw, unchanged packet.
        assert_eq!(out[0], 0x00);
        assert_eq!(&out[1..], packet.as_slice());
        assert_eq!(state.decompress(&out).unwrap(), packet);
    }

    #[test]
    fn compressed_above_threshold_roundtrip() {
        let state = CompressionState::enabled(8);
        let packet = vec![0xABu8; 4096];
        let mut out = BytesMut::new();
        state.compress(&packet, &mut out).unwrap();
        // A non-zero declared size precedes a stream smaller than the input.
        assert_ne!(out[0], 0x00);
        assert!(out.len() < packet.len(), "payload must actually compress");
        assert_eq!(state.decompress(&out).unwrap(), packet);
    }

    #[test]
    fn at_threshold_exactly_is_compressed_and_roundtrips() {
        // A packet whose size equals the threshold must be compressed, not sent
        // behind the uncompressed marker.
        let state = CompressionState::enabled(64);
        let packet = vec![0x7Au8; 64];
        let mut out = BytesMut::new();
        state.compress(&packet, &mut out).unwrap();
        assert_ne!(out[0], 0x00);
        assert_eq!(state.decompress(&out).unwrap(), packet);
    }

    #[test]
    fn compressed_below_threshold_is_rejected() {
        // A non-zero declared size under the threshold is a protocol violation:
        // the client should have sent this packet uncompressed.
        let state = CompressionState::enabled(128);
        let body = frame(50, &zlib(&[0u8; 50]));
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(
            err,
            CompressionError::BelowThreshold {
                declared: 50,
                threshold: 128,
            }
        );
        assert_eq!(err.disconnect_class(), DisconnectClass::ProtocolViolation);
    }

    #[test]
    fn declared_size_over_cap_is_rejected_before_allocation() {
        let state = CompressionState::with_cap(Some(0), 1024);
        let body = frame(4096, &zlib(&[0u8; 4096]));
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(
            err,
            CompressionError::DeclaredTooLarge {
                declared: 4096,
                cap: 1024,
            }
        );
        assert_eq!(err.disconnect_class(), DisconnectClass::FrameTooLarge);
    }

    #[test]
    fn zip_bomb_declared_size_is_capped() {
        // 4 MiB of zeros compresses to a few KiB. A 1 MiB cap rejects the
        // honestly-declared size before any large allocation, proving the cap —
        // not the on-wire size — is the OOM defense.
        let huge = vec![0u8; 4 * 1024 * 1024];
        let producer = CompressionState::with_cap(Some(0), 8 * 1024 * 1024);
        let mut bomb = BytesMut::new();
        producer.compress(&huge, &mut bomb).unwrap();
        assert!(bomb.len() < 64 * 1024, "the bomb is tiny on the wire");

        let victim = CompressionState::with_cap(Some(0), 1024 * 1024);
        let err = victim.decompress(&bomb).unwrap_err();
        assert!(matches!(err, CompressionError::DeclaredTooLarge { .. }));
    }

    #[test]
    fn malformed_zlib_is_rejected() {
        // A plausible declared size but a payload that is not a zlib stream.
        let state = CompressionState::enabled(0);
        let body = frame(100, &[0xFF, 0x00, 0x13, 0x37, 0x42]);
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(err, CompressionError::MalformedZlib);
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn truncated_zlib_is_rejected() {
        // A valid stream cut short cannot inflate to the declared size.
        let state = CompressionState::enabled(0);
        let stream = zlib(&[0x42u8; 200]);
        let truncated = &stream[..stream.len() / 2];
        let body = frame(200, truncated);
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(err, CompressionError::MalformedZlib);
    }

    #[test]
    fn declared_size_undersized_mismatch_is_rejected() {
        // The stream decodes to fewer bytes than declared.
        let state = CompressionState::enabled(0);
        let body = frame(250, &zlib(&[0x55u8; 200]));
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(
            err,
            CompressionError::SizeMismatch {
                declared: 250,
                actual: 200,
            }
        );
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn declared_size_oversized_mismatch_is_rejected() {
        // The stream decodes to more bytes than declared; decompression stops at
        // the declared size and rejects rather than over-allocating.
        let state = CompressionState::enabled(0);
        let body = frame(150, &zlib(&[0x55u8; 200]));
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(err, CompressionError::Oversized { declared: 150 });
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn trailing_bytes_after_stream_are_rejected() {
        // A complete zlib stream followed by junk inside the same frame.
        let state = CompressionState::enabled(0);
        let mut payload = zlib(&[0x11u8; 64]);
        payload.extend_from_slice(&[0xDE, 0xAD]);
        let body = frame(64, &payload);
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(err, CompressionError::MalformedZlib);
    }

    #[test]
    fn negative_declared_size_is_rejected() {
        let state = CompressionState::enabled(0);
        let body = frame(-1, &[]);
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(err, CompressionError::NegativeDataLength { length: -1 });
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn malformed_data_length_prefix_is_rejected() {
        // Six continuation bytes never terminate the VarInt budget.
        let state = CompressionState::enabled(0);
        let body = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x00];
        let err = state.decompress(&body).unwrap_err();
        assert_eq!(err, CompressionError::BadDataLength);
        assert_eq!(err.disconnect_class(), DisconnectClass::Malformed);
    }

    #[test]
    fn empty_enabled_frame_is_malformed() {
        // An enabled frame must carry at least a data-length prefix.
        let state = CompressionState::enabled(0);
        let err = state.decompress(&[]).unwrap_err();
        assert_eq!(err, CompressionError::BadDataLength);
    }

    #[test]
    fn disabled_state_is_pass_through() {
        let state = CompressionState::disabled();
        assert!(!state.is_enabled());
        assert_eq!(state.threshold(), None);
        let packet = b"raw packet id and body".to_vec();
        let mut out = BytesMut::new();
        state.compress(&packet, &mut out).unwrap();
        // No data-length prefix is added when compression is not negotiated.
        assert_eq!(&out[..], packet.as_slice());
        assert_eq!(state.decompress(&packet).unwrap(), packet);
    }

    #[test]
    fn from_threshold_maps_negative_to_disabled() {
        assert!(!CompressionState::from_threshold(-1).is_enabled());
        let state = CompressionState::from_threshold(256);
        assert!(state.is_enabled());
        assert_eq!(state.threshold(), Some(256));
        assert_eq!(state.max_decompressed(), DEFAULT_MAX_DECOMPRESSED);
    }

    #[test]
    fn compress_rejects_packet_over_cap() {
        let state = CompressionState::with_cap(Some(0), 16);
        let packet = vec![0u8; 64];
        let mut out = BytesMut::new();
        let err = state.compress(&packet, &mut out).unwrap_err();
        assert_eq!(
            err,
            CompressionError::DeclaredTooLarge {
                declared: 64,
                cap: 16,
            }
        );
        assert!(out.is_empty(), "output must be untouched on failure");
    }

    #[test]
    fn zero_byte_uncompressed_packet_roundtrips() {
        // An empty packet is below any positive threshold and round-trips as a
        // bare zero marker.
        let state = CompressionState::enabled(8);
        let mut out = BytesMut::new();
        state.compress(&[], &mut out).unwrap();
        assert_eq!(&out[..], &[0x00]);
        assert_eq!(state.decompress(&out).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn zero_byte_packet_at_threshold_zero_roundtrips() {
        // Regression: at threshold 0 the compressed branch would otherwise catch
        // an empty packet and emit a 0 `data_length` (the uncompressed marker)
        // followed by a zlib stream, which decompress reads back as the raw body.
        // An empty packet must be sent uncompressed regardless of threshold.
        let state = CompressionState::enabled(0);
        let mut out = BytesMut::new();
        state.compress(&[], &mut out).unwrap();
        assert_eq!(&out[..], &[0x00]);
        assert_eq!(state.decompress(&out).unwrap(), Vec::<u8>::new());
    }
}
