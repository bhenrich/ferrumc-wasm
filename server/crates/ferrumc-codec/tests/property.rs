//! Property-based and corpus malformed-input tests for the bounded codec
//! primitives.
//!
//! The per-module `#[cfg(test)]` suites pin specific malformed encodings; this
//! file asserts the crate-wide invariants hold for *arbitrary* input. For every
//! primitive and any byte string, decoding must:
//!
//! * never panic, hang, or overflow,
//! * return a classified [`CodecError`] rather than an opaque failure,
//! * never advance the cursor past the input, and
//! * never reserve a buffer from an attacker-declared length before the bytes
//!   are proven present.
//!
//! For round-trippable values the encoders produce, `decode(encode(v)) == v`.

use ferrumc_codec::{
    write_var_int, write_var_long, BoundedBytes, BoundedReader, BoundedString, CodecError,
    FrameLengthReader,
};
use proptest::prelude::*;

/// Code-unit cap used by the `BoundedString` round-trip strategies.
const STR_CAP: usize = 64;
/// Byte cap used by the `BoundedBytes` round-trip strategies.
const BYTES_CAP: usize = 256;

proptest! {
    /// A raw `VarInt` decode is total: any bytes yield a clean `Result`, a
    /// success consumes at most five bytes, and the cursor never overruns.
    #[test]
    fn var_int_decode_is_total(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let mut reader = BoundedReader::new(&bytes);
        match reader.read_var_int() {
            Ok(_) => {
                prop_assert!(reader.position() <= 5);
                prop_assert!(reader.position() <= bytes.len());
            }
            Err(e) => prop_assert!(
                matches!(e, CodecError::VarIntTooLong | CodecError::UnexpectedEof { .. }),
                "unexpected var_int error: {e:?}"
            ),
        }
    }

    /// A raw `VarLong` decode is total and bounded to ten bytes.
    #[test]
    fn var_long_decode_is_total(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let mut reader = BoundedReader::new(&bytes);
        match reader.read_var_long() {
            Ok(_) => {
                prop_assert!(reader.position() <= 10);
                prop_assert!(reader.position() <= bytes.len());
            }
            Err(e) => prop_assert!(
                matches!(e, CodecError::VarLongTooLong | CodecError::UnexpectedEof { .. }),
                "unexpected var_long error: {e:?}"
            ),
        }
    }

    /// `read_var_int_len` is total and only ever yields a non-negative length.
    #[test]
    fn var_int_len_is_total(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let mut reader = BoundedReader::new(&bytes);
        match reader.read_var_int_len() {
            Ok(len) => prop_assert!(i32::try_from(len).is_ok()),
            Err(e) => prop_assert!(
                matches!(
                    e,
                    CodecError::VarIntTooLong
                        | CodecError::UnexpectedEof { .. }
                        | CodecError::NegativeLength { .. }
                ),
                "unexpected var_int_len error: {e:?}"
            ),
        }
    }

    /// Every `i32` survives an encode→decode round trip with nothing left over.
    #[test]
    fn var_int_round_trips(value in any::<i32>()) {
        let mut buf = Vec::new();
        write_var_int(&mut buf, value);
        prop_assert!(buf.len() <= 5);
        let mut reader = BoundedReader::new(&buf);
        prop_assert_eq!(reader.read_var_int().unwrap(), value);
        prop_assert_eq!(reader.remaining(), 0);
    }

    /// Every `i64` survives an encode→decode round trip with nothing left over.
    #[test]
    fn var_long_round_trips(value in any::<i64>()) {
        let mut buf = Vec::new();
        write_var_long(&mut buf, value);
        prop_assert!(buf.len() <= 10);
        let mut reader = BoundedReader::new(&buf);
        prop_assert_eq!(reader.read_var_long().unwrap(), value);
        prop_assert_eq!(reader.remaining(), 0);
    }

    /// An arbitrary sequence of reads over arbitrary bytes never panics and
    /// never advances the cursor past the end of the input.
    #[test]
    fn mixed_reads_never_overrun(
        bytes in prop::collection::vec(any::<u8>(), 0..128),
        ops in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut reader = BoundedReader::new(&bytes);
        for op in ops {
            // Each arm ignores errors; only the absence of a panic and the
            // bound invariant matter here.
            match op % 9 {
                0 => drop(reader.read_u8()),
                1 => drop(reader.read_i16()),
                2 => drop(reader.read_u32()),
                3 => drop(reader.read_i64()),
                4 => drop(reader.read_f32()),
                5 => drop(reader.read_f64()),
                6 => drop(reader.read_var_int()),
                7 => drop(reader.read_var_long()),
                _ => drop(reader.read_bytes(usize::from(op))),
            }
            prop_assert!(reader.position() <= bytes.len());
            prop_assert!(reader.remaining() <= bytes.len());
        }
    }

    /// `BoundedString::read` is total: any bytes yield a clean `Result`, and a
    /// success never exceeds the code-unit cap nor overruns the input.
    #[test]
    fn bounded_string_decode_is_total(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let mut reader = BoundedReader::new(&bytes);
        match BoundedString::<32>::read(&mut reader) {
            Ok(s) => {
                prop_assert!(s.as_str().encode_utf16().count() <= 32);
                prop_assert!(reader.position() <= bytes.len());
            }
            Err(e) => prop_assert!(
                matches!(
                    e,
                    CodecError::VarIntTooLong
                        | CodecError::UnexpectedEof { .. }
                        | CodecError::NegativeLength { .. }
                        | CodecError::StringTooLong { .. }
                        | CodecError::InvalidUtf8(_)
                ),
                "unexpected string error: {e:?}"
            ),
        }
    }

    /// Any in-bounds string round-trips through `new`→`write`→`read`.
    #[test]
    fn bounded_string_round_trips(text in "[a-zA-Z0-9 _]{0,32}") {
        let original = BoundedString::<STR_CAP>::new(text.clone()).unwrap();
        let mut buf = Vec::new();
        original.write(&mut buf);
        let mut reader = BoundedReader::new(&buf);
        let decoded = BoundedString::<STR_CAP>::read(&mut reader).unwrap();
        prop_assert_eq!(decoded.as_str(), text.as_str());
        prop_assert_eq!(reader.remaining(), 0);
    }

    /// Unicode (including astral) strings round-trip, and the code-unit count is
    /// preserved across the wire form.
    #[test]
    fn bounded_string_unicode_round_trips(
        chars in prop::collection::vec(any::<char>(), 0..16),
    ) {
        let text: String = chars.into_iter().collect();
        // 16 scalars cost at most 32 UTF-16 code units, comfortably under STR_CAP.
        let original = BoundedString::<STR_CAP>::new(text.clone()).unwrap();
        let mut buf = Vec::new();
        original.write(&mut buf);
        let mut reader = BoundedReader::new(&buf);
        let decoded = BoundedString::<STR_CAP>::read(&mut reader).unwrap();
        prop_assert_eq!(decoded.as_str(), text.as_str());
        prop_assert_eq!(reader.remaining(), 0);
    }

    /// `BoundedBytes::read` is total: any bytes yield a clean `Result`, and a
    /// success never exceeds the cap nor overruns the input.
    #[test]
    fn bounded_bytes_decode_is_total(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let mut reader = BoundedReader::new(&bytes);
        match BoundedBytes::<64>::read(&mut reader) {
            Ok(blob) => {
                prop_assert!(blob.as_slice().len() <= 64);
                prop_assert!(reader.position() <= bytes.len());
            }
            Err(e) => prop_assert!(
                matches!(
                    e,
                    CodecError::VarIntTooLong
                        | CodecError::UnexpectedEof { .. }
                        | CodecError::NegativeLength { .. }
                        | CodecError::BytesTooLong { .. }
                ),
                "unexpected bytes error: {e:?}"
            ),
        }
    }

    /// Any in-bounds blob round-trips through `new`→`write`→`read`.
    #[test]
    fn bounded_bytes_round_trips(data in prop::collection::vec(any::<u8>(), 0..BYTES_CAP)) {
        let original = BoundedBytes::<BYTES_CAP>::new(data.clone()).unwrap();
        let mut buf = Vec::new();
        original.write(&mut buf);
        let mut reader = BoundedReader::new(&buf);
        let decoded = BoundedBytes::<BYTES_CAP>::read(&mut reader).unwrap();
        prop_assert_eq!(decoded.as_slice(), data.as_slice());
        prop_assert_eq!(reader.remaining(), 0);
    }

    /// `FrameLengthReader` is total for an arbitrary cap and arbitrary bytes; a
    /// success never exceeds the cap.
    #[test]
    fn frame_length_is_total(
        cap in any::<usize>(),
        bytes in prop::collection::vec(any::<u8>(), 0..16),
    ) {
        let cfg = FrameLengthReader::new(cap);
        let mut reader = BoundedReader::new(&bytes);
        match cfg.read_length(&mut reader) {
            Ok(len) => prop_assert!(len <= cap),
            Err(e) => prop_assert!(
                matches!(
                    e,
                    CodecError::VarIntTooLong
                        | CodecError::UnexpectedEof { .. }
                        | CodecError::NegativeLength { .. }
                        | CodecError::FrameTooLarge { .. }
                ),
                "unexpected frame error: {e:?}"
            ),
        }
    }

    /// A declared body length far larger than what is present must be rejected
    /// (cap or EOF) — never used to size a pre-read allocation. The test
    /// completing at all proves no multi-gigabyte reservation was attempted.
    #[test]
    fn huge_declared_lengths_do_not_pre_allocate(declared in (1_i32..=i32::MAX)) {
        let mut buf = Vec::new();
        write_var_int(&mut buf, declared);
        // No body follows the prefix in any of the three cases below.

        let mut reader = BoundedReader::new(&buf);
        prop_assert!(BoundedString::<256>::read(&mut reader).is_err());

        let mut reader = BoundedReader::new(&buf);
        prop_assert!(BoundedBytes::<256>::read(&mut reader).is_err());

        // `read_bytes` borrows, so even an enormous cap fails on EOF rather than
        // reserving: the declared length is never trusted ahead of the bytes.
        let mut reader = BoundedReader::new(&buf);
        let result = BoundedBytes::<{ i32::MAX as usize }>::read(&mut reader);
        prop_assert!(
            matches!(
                result,
                Err(CodecError::UnexpectedEof { .. } | CodecError::BytesTooLong { .. })
            ),
            "expected EOF or BytesTooLong, got {result:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Hand-crafted regression corpus: specific malformed encodings that must be
// rejected on the exact classified error, kept as plain `#[test]` cases.
// ---------------------------------------------------------------------------

/// A six-byte all-continuation `VarInt` overruns the 5-byte budget.
#[test]
fn var_int_six_continuation_bytes_is_too_long() {
    let buf = [0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
    let mut reader = BoundedReader::new(&buf);
    assert_eq!(reader.read_var_int(), Err(CodecError::VarIntTooLong));
}

/// An eleven-byte all-continuation `VarLong` overruns the 10-byte budget.
#[test]
fn var_long_eleven_continuation_bytes_is_too_long() {
    let buf = [
        0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00,
    ];
    let mut reader = BoundedReader::new(&buf);
    assert_eq!(reader.read_var_long(), Err(CodecError::VarLongTooLong));
}

/// The maximal `VarInt` value (`i32::MIN`, encoded with the high segment bits
/// set) decodes without overflow — guarding the `value << 28` step.
#[test]
fn var_int_max_shift_does_not_overflow() {
    // 0xFF on the fifth byte sets bits that fall outside the 32-bit value; they
    // are discarded rather than overflowing the shift.
    let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
    let mut reader = BoundedReader::new(&buf);
    assert_eq!(reader.read_var_int().unwrap(), -1);
}

/// `BoundedString` rejects a prefix declaring `i32::MAX` bytes on the cap check,
/// long before any allocation.
#[test]
fn bounded_string_rejects_max_prefix_on_cap() {
    let mut buf = Vec::new();
    write_var_int(&mut buf, i32::MAX);
    let mut reader = BoundedReader::new(&buf);
    assert!(matches!(
        BoundedString::<256>::read(&mut reader),
        Err(CodecError::StringTooLong { .. })
    ));
}

/// A declared blob length within an enormous cap but with no body present fails
/// on EOF — proving the length is never used to reserve memory up front.
#[test]
fn bounded_bytes_huge_cap_empty_body_is_eof_not_alloc() {
    let mut buf = Vec::new();
    write_var_int(&mut buf, 1_000_000);
    let mut reader = BoundedReader::new(&buf);
    assert!(matches!(
        BoundedBytes::<{ i32::MAX as usize }>::read(&mut reader),
        Err(CodecError::UnexpectedEof { .. })
    ));
}
