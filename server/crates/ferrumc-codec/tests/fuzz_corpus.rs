//! Stable replay gate for the committed `VarInt` and `VarLong` fuzz seeds.
//!
//! The deep libFuzzer targets are opt-in, but these fixed inputs run on stable
//! Rust with every ordinary workspace test.

use std::fs;
use std::path::{Path, PathBuf};

use ferrumc_codec::{BoundedReader, CodecError};

const SEEDS_PER_TARGET: usize = 12;
const MAX_SEED_BYTES: usize = 10;

#[derive(Clone, Copy, Debug)]
enum Expected<T> {
    Value(T),
    UnexpectedEof,
    TooLong,
}

const VAR_INT_CASES: [(&str, Expected<i32>); SEEDS_PER_TARGET] = [
    ("00_empty.bin", Expected::UnexpectedEof),
    ("01_zero.bin", Expected::Value(0)),
    ("02_127.bin", Expected::Value(127)),
    ("03_128.bin", Expected::Value(128)),
    ("04_max.bin", Expected::Value(i32::MAX)),
    ("05_min.bin", Expected::Value(i32::MIN)),
    ("06_negative_one.bin", Expected::Value(-1)),
    ("07_noncanonical_zero.bin", Expected::Value(0)),
    ("08_unused_high_bits.bin", Expected::Value(-1)),
    ("09_trailing.bin", Expected::Value(1)),
    ("10_truncated.bin", Expected::UnexpectedEof),
    ("11_too_long.bin", Expected::TooLong),
];

const VAR_LONG_CASES: [(&str, Expected<i64>); SEEDS_PER_TARGET] = [
    ("00_empty.bin", Expected::UnexpectedEof),
    ("01_zero.bin", Expected::Value(0)),
    ("02_127.bin", Expected::Value(127)),
    ("03_128.bin", Expected::Value(128)),
    ("04_max.bin", Expected::Value(i64::MAX)),
    ("05_min.bin", Expected::Value(i64::MIN)),
    ("06_negative_one.bin", Expected::Value(-1)),
    ("07_noncanonical_zero.bin", Expected::Value(0)),
    ("08_unused_high_bits.bin", Expected::Value(-1)),
    ("09_trailing.bin", Expected::Value(1)),
    ("10_truncated.bin", Expected::UnexpectedEof),
    ("11_too_long.bin", Expected::TooLong),
];

fn corpus_dir(target: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures/fuzz/ferrumc-codec")
        .join(target)
}

fn assert_exact_inventory<T>(target: &str, cases: &[(&str, Expected<T>)]) -> PathBuf {
    let directory = corpus_dir(target);
    let mut actual = fs::read_dir(&directory)
        .expect("committed corpus directory must exist")
        .map(|entry| {
            let entry = entry.expect("committed corpus entry must be readable");
            assert!(
                entry
                    .file_type()
                    .expect("committed corpus entry type must be readable")
                    .is_file(),
                "corpus entries must be regular files: {}",
                entry.path().display()
            );
            entry
                .file_name()
                .into_string()
                .expect("corpus filenames must be UTF-8")
        })
        .collect::<Vec<_>>();
    actual.sort_unstable();

    let expected = cases
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    directory
}

fn read_seed(directory: &Path, name: &str) -> Vec<u8> {
    let bytes = fs::read(directory.join(name)).expect("committed seed must be readable");
    assert!(
        bytes.len() <= MAX_SEED_BYTES,
        "{name} exceeds the fixed smoke-test byte bound"
    );
    bytes
}

#[test]
fn var_int_corpus_replays_with_typed_corrupt_seed() {
    let directory = assert_exact_inventory("var_int", &VAR_INT_CASES);
    let mut saw_typed_corruption = false;

    for (name, expected) in VAR_INT_CASES {
        let bytes = read_seed(&directory, name);
        let mut reader = BoundedReader::new(&bytes);
        let result = reader.read_var_int();
        match expected {
            Expected::Value(value) => assert_eq!(result, Ok(value), "seed {name}"),
            Expected::UnexpectedEof => assert!(
                matches!(result, Err(CodecError::UnexpectedEof { .. })),
                "seed {name}: {result:?}"
            ),
            Expected::TooLong => {
                assert_eq!(result, Err(CodecError::VarIntTooLong), "seed {name}");
                saw_typed_corruption = true;
            }
        }
        assert!(reader.position() <= bytes.len(), "seed {name}");
        assert!(reader.position() <= 5, "seed {name}");

        let mut length_reader = BoundedReader::new(&bytes);
        let length_result = length_reader.read_var_int_len();
        match expected {
            Expected::Value(value) if value < 0 => assert_eq!(
                length_result,
                Err(CodecError::NegativeLength { length: value }),
                "seed {name}"
            ),
            Expected::Value(value) => assert_eq!(
                length_result,
                Ok(usize::try_from(value).expect("non-negative i32 fits usize")),
                "seed {name}"
            ),
            Expected::UnexpectedEof => assert!(
                matches!(length_result, Err(CodecError::UnexpectedEof { .. })),
                "seed {name}: {length_result:?}"
            ),
            Expected::TooLong => {
                assert_eq!(length_result, Err(CodecError::VarIntTooLong), "seed {name}");
            }
        }
        assert!(length_reader.position() <= bytes.len(), "seed {name}");
        assert!(length_reader.position() <= 5, "seed {name}");
    }

    assert!(saw_typed_corruption);
}

#[test]
fn var_long_corpus_replays_with_typed_corrupt_seed() {
    let directory = assert_exact_inventory("var_long", &VAR_LONG_CASES);
    let mut saw_typed_corruption = false;

    for (name, expected) in VAR_LONG_CASES {
        let bytes = read_seed(&directory, name);
        let mut reader = BoundedReader::new(&bytes);
        let result = reader.read_var_long();
        match expected {
            Expected::Value(value) => assert_eq!(result, Ok(value), "seed {name}"),
            Expected::UnexpectedEof => assert!(
                matches!(result, Err(CodecError::UnexpectedEof { .. })),
                "seed {name}: {result:?}"
            ),
            Expected::TooLong => {
                assert_eq!(result, Err(CodecError::VarLongTooLong), "seed {name}");
                saw_typed_corruption = true;
            }
        }
        assert!(reader.position() <= bytes.len(), "seed {name}");
        assert!(reader.position() <= 10, "seed {name}");
    }

    assert!(saw_typed_corruption);
}
