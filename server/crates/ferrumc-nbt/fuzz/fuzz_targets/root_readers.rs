#![no_main]
#![forbid(unsafe_code)]

use ferrumc_nbt::{
    read_named_root, read_named_root_with_consumed, read_network_root,
    read_network_root_with_consumed, write_named_root, write_network_root, NbtError, NbtLimits,
    NbtTag,
};
use libfuzzer_sys::fuzz_target;

fn corpus_limits() -> NbtLimits {
    NbtLimits::default()
        .with_max_depth(4)
        .with_max_bytes(64)
        .with_max_list_len(4)
        .with_max_string_bytes(8)
}

fn assert_compound(tag: &NbtTag) {
    assert!(matches!(tag, NbtTag::Compound(_)));
}

fn assert_reader_error(error: &NbtError) {
    let is_reader_error = matches!(
        error,
        NbtError::DepthExceeded { .. }
            | NbtError::MaxBytesExceeded { .. }
            | NbtError::ListTooLong { .. }
            | NbtError::StringTooLong { .. }
            | NbtError::UnknownTagType { .. }
            | NbtError::InvalidUtf8
            | NbtError::NegativeLength { .. }
            | NbtError::MalformedList
            | NbtError::UnexpectedRootTag { .. }
            | NbtError::Codec(_)
    );
    assert!(is_reader_error, "unexpected reader result: {error:?}");
}

fn exercise_named(input: &[u8], limits: &NbtLimits) {
    match read_named_root(input, limits) {
        Ok((name, tag)) => {
            assert_compound(&tag);
            assert!(write_named_root(&name, &tag, limits).is_ok());
        }
        Err(error) => assert_reader_error(&error),
    }

    match read_named_root_with_consumed(input, limits) {
        Ok((name, tag, consumed)) => {
            assert_compound(&tag);
            assert!(consumed <= input.len());
            assert!(consumed <= limits.max_bytes());
            assert!(write_named_root(&name, &tag, limits).is_ok());
        }
        Err(error) => assert_reader_error(&error),
    }
}

fn exercise_network(input: &[u8], limits: &NbtLimits) {
    match read_network_root(input, limits) {
        Ok(tag) => {
            assert_compound(&tag);
            assert!(write_network_root(&tag, limits).is_ok());
        }
        Err(error) => assert_reader_error(&error),
    }

    match read_network_root_with_consumed(input, limits) {
        Ok((tag, consumed)) => {
            assert_compound(&tag);
            assert!(consumed <= input.len());
            assert!(consumed <= limits.max_bytes());
            assert!(write_network_root(&tag, limits).is_ok());
        }
        Err(error) => assert_reader_error(&error),
    }
}

fn exercise(input: &[u8], limits: &NbtLimits) {
    exercise_named(input, limits);
    exercise_network(input, limits);
}

fuzz_target!(|input: &[u8]| {
    exercise(input, &NbtLimits::default());
    exercise(input, &corpus_limits());
});
