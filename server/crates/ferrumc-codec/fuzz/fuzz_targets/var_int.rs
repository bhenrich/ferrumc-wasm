#![no_main]
#![forbid(unsafe_code)]

use ferrumc_codec::{BoundedReader, CodecError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let mut raw_reader = BoundedReader::new(input);
    let raw = raw_reader.read_var_int();
    assert!(matches!(
        raw,
        Ok(_) | Err(CodecError::VarIntTooLong | CodecError::UnexpectedEof { .. })
    ));
    assert!(raw_reader.position() <= input.len());
    assert!(raw_reader.position() <= 5);

    let mut length_reader = BoundedReader::new(input);
    let length = length_reader.read_var_int_len();
    assert!(matches!(
        length,
        Ok(_)
            | Err(CodecError::VarIntTooLong
                | CodecError::UnexpectedEof { .. }
                | CodecError::NegativeLength { .. })
    ));
    assert!(length_reader.position() <= input.len());
    assert!(length_reader.position() <= 5);
});
