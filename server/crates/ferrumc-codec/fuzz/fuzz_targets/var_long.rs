#![no_main]
#![forbid(unsafe_code)]

use ferrumc_codec::{BoundedReader, CodecError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let mut reader = BoundedReader::new(input);
    let result = reader.read_var_long();
    assert!(matches!(
        result,
        Ok(_) | Err(CodecError::VarLongTooLong | CodecError::UnexpectedEof { .. })
    ));
    assert!(reader.position() <= input.len());
    assert!(reader.position() <= 10);
});
