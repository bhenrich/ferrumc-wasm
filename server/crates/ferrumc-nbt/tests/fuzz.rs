//! Property-based and corpus malformed-input tests for the NBT reader.
//!
//! The reader's `#[cfg(test)]` suite pins specific malformed encodings;
//! `roundtrip.rs` pins encode→decode identity for well-formed trees. This file
//! covers the adversarial direction: for *arbitrary* bytes the reader must
//!
//! * never panic, overflow, or recurse without bound,
//! * always terminate (every step consumes input from a bounded reader),
//! * reject an attacker-declared array/list/string length on the cap *before*
//!   allocating from it, and
//! * only ever produce a tree the writer can re-encode.
//!
//! All four public entry points are exercised: the whole-slice
//! [`read_named_root`]/[`read_network_root`] and the embedded
//! `*_with_consumed` variants.

use ferrumc_nbt::{
    read_named_root, read_named_root_with_consumed, read_network_root,
    read_network_root_with_consumed, write_network_root, NbtLimits,
};
use proptest::prelude::*;

/// Runs every reader entry point over `bytes` under `limits`, asserting none of
/// them panic. Returns nothing — the absence of a panic is the assertion.
fn drive_all_readers(bytes: &[u8], limits: &NbtLimits) {
    let _ = read_named_root(bytes, limits);
    let _ = read_network_root(bytes, limits);
    let _ = read_named_root_with_consumed(bytes, limits);
    let _ = read_network_root_with_consumed(bytes, limits);
}

/// Tight limits that still admit small valid trees while keeping every cap
/// easy to trip — exercises the limit-rejection paths far more often than the
/// defaults would on small inputs.
fn tight_limits() -> NbtLimits {
    NbtLimits::default()
        .with_max_depth(8)
        .with_max_bytes(4096)
        .with_max_list_len(64)
        .with_max_string_bytes(128)
}

proptest! {
    /// Raw arbitrary bytes never panic any reader, under default or tight limits.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        drive_all_readers(&bytes, &NbtLimits::default());
        drive_all_readers(&bytes, &tight_limits());
    }

    /// Bytes that begin with a valid network-root header (`0x0A`) drive the
    /// compound parser far deeper than random bytes ever would, exercising the
    /// type-dispatch, name, list, array, and string paths with fuzzed payloads.
    #[test]
    fn network_root_prefixed_bytes_never_panic(
        body in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let mut bytes = vec![0x0A];
        bytes.extend_from_slice(&body);
        drive_all_readers(&bytes, &NbtLimits::default());
        drive_all_readers(&bytes, &tight_limits());
    }

    /// Bytes that begin with a valid named-root header (`0x0A` + empty name)
    /// drive the named-compound parser the same way.
    #[test]
    fn named_root_prefixed_bytes_never_panic(
        body in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let mut bytes = vec![0x0A, 0x00, 0x00];
        bytes.extend_from_slice(&body);
        drive_all_readers(&bytes, &NbtLimits::default());
        drive_all_readers(&bytes, &tight_limits());
    }

    /// Any tree the whole-slice reader accepts must be re-encodable by the
    /// writer (the writer's domain covers the reader's range). Re-decoding the
    /// re-encoded bytes must also succeed. Byte- and value-equality are *not*
    /// asserted: arbitrary input can decode to `NaN` floats (never `==`
    /// themselves) and to non-canonical Modified UTF-8 (a bare `0x00` re-encodes
    /// as `0xC0 0x80`), so only structural acceptance is a total invariant.
    #[test]
    fn reader_range_is_within_writer_domain(
        body in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        let mut bytes = vec![0x0A];
        bytes.extend_from_slice(&body);
        if let Ok(tag) = read_network_root(&bytes, &NbtLimits::default()) {
            let reencoded = write_network_root(&tag, &NbtLimits::default())
                .expect("a tree the reader accepted must re-encode");
            prop_assert!(read_network_root(&reencoded, &NbtLimits::default()).is_ok());
        }
    }

    /// An array/list whose declared element count exceeds `max_list_len` is
    /// rejected on the cap with no body present — the count is never used to
    /// reserve memory. The case completing proves no huge reservation occurred.
    #[test]
    fn huge_declared_sequence_length_is_capped(
        // 7 = ByteArray, 9 = List, 11 = IntArray, 12 = LongArray.
        tag_id in prop::sample::select(vec![7u8, 9u8, 11u8, 12u8]),
        // Lower bound is one past the default ~1 MiB list cap (asserted below).
        declared in 1_048_577_i32..=i32::MAX,
    ) {
        prop_assert!(
            usize::try_from(declared).unwrap() > NbtLimits::DEFAULT_MAX_LIST_LEN,
            "declared count must exceed the default list cap"
        );
        let mut body = vec![tag_id, 0x00, 0x01, b'x']; // entry: type, name "x"
        if tag_id == 9 {
            body.push(0x01); // a List also declares an element type (Byte) first
        }
        body.extend_from_slice(&declared.to_be_bytes()); // i32 length, no payload
        let bytes = {
            let mut b = vec![0x0A];
            b.extend_from_slice(&body);
            b
        };
        // Default max_list_len is ~1M, far below the declared count, so the cap
        // fires before any element is read.
        prop_assert!(read_network_root(&bytes, &NbtLimits::default()).is_err());
    }
}

// ---------------------------------------------------------------------------
// Hand-crafted regression corpus.
// ---------------------------------------------------------------------------

/// `[0x0A][body...]` — wraps a compound body in a network (nameless) root.
fn network_root(body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x0A];
    bytes.extend_from_slice(body);
    bytes
}

/// A `ByteArray` declaring `i32::MAX` elements with no payload is rejected on
/// the list-length cap, never used to size an allocation.
#[test]
fn byte_array_max_length_is_capped_not_allocated() {
    let mut body = vec![0x07, 0x00, 0x01, b'a']; // ByteArray "a"
    body.extend_from_slice(&i32::MAX.to_be_bytes());
    let bytes = network_root(&body);
    assert!(read_network_root(&bytes, &NbtLimits::default()).is_err());
}

/// An `IntArray` declaring `i32::MAX` elements with no payload is likewise
/// capped before allocation.
#[test]
fn int_array_max_length_is_capped_not_allocated() {
    let mut body = vec![0x0B, 0x00, 0x01, b'a']; // IntArray "a"
    body.extend_from_slice(&i32::MAX.to_be_bytes());
    let bytes = network_root(&body);
    assert!(read_network_root(&bytes, &NbtLimits::default()).is_err());
}

/// A `List` declaring `i32::MAX` `Int` elements with no payload is capped
/// before any element is read.
#[test]
fn list_max_length_is_capped_not_allocated() {
    let mut body = vec![0x09, 0x00, 0x01, b'l', 0x03]; // List "l" of Int
    body.extend_from_slice(&i32::MAX.to_be_bytes());
    let bytes = network_root(&body);
    assert!(read_network_root(&bytes, &NbtLimits::default()).is_err());
}

/// Nesting far past `max_depth` returns [`NbtError::DepthExceeded`] rather than
/// overflowing the stack. Each `0x0A 0x00 0x00` opens a deeper compound entry.
#[test]
fn pathological_nesting_is_depth_exceeded_not_stack_overflow() {
    // Network root type, then 4096 nested compound entries — well past the
    // default 512 depth cap. Each `0x0A 0x00 0x00` opens a deeper compound.
    let mut bytes = vec![0x0A];
    for _ in 0..4096 {
        bytes.extend_from_slice(&[0x0A, 0x00, 0x00]);
    }
    // The depth cap fires while descending, so the truncated tail is never
    // reached and the reader returns an error instead of recursing unbounded.
    let result = read_network_root(&bytes, &NbtLimits::default());
    assert!(
        result.is_err(),
        "deep nesting must be rejected, got {result:?}"
    );
}

/// A string declaring its full `u16` length but supplying no bytes fails on EOF,
/// never reserving the declared length up front.
#[test]
fn string_declared_length_without_body_is_eof() {
    let mut body = vec![0x08, 0x00, 0x01, b's']; // String "s"
    body.extend_from_slice(&u16::MAX.to_be_bytes()); // declares 65535 bytes, none follow
    let bytes = network_root(&body);
    assert!(read_network_root(&bytes, &NbtLimits::default()).is_err());
}
