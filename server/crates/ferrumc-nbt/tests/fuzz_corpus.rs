//! Stable replay gate for the committed bounded NBT root-reader fuzz seeds.
//!
//! The deep libFuzzer target is opt-in, but these fixed inputs run on stable
//! Rust with every ordinary workspace test.

use std::fs;
use std::path::{Path, PathBuf};

use ferrumc_codec::CodecError;
use ferrumc_nbt::{
    read_named_root, read_named_root_with_consumed, read_network_root,
    read_network_root_with_consumed, write_named_root, write_network_root, NbtError, NbtLimits,
    NbtTag, Result as NbtResult,
};

const SEED_COUNT: usize = 32;
const MAX_SEED_BYTES: usize = 65;
const ROOT_READER_APIS: usize = 4;
const LIMIT_PROFILES: usize = 2;
const EXPECTED_INVOCATIONS: usize = SEED_COUNT * ROOT_READER_APIS * LIMIT_PROFILES;

#[derive(Clone, Copy, Debug)]
enum RootForm {
    Both,
    Network,
    Named,
}

impl RootForm {
    fn includes_network(self) -> bool {
        matches!(self, Self::Both | Self::Network)
    }

    fn includes_named(self) -> bool {
        matches!(self, Self::Both | Self::Named)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LimitProfile {
    Default,
    Corpus,
}

#[derive(Clone, Copy, Debug)]
struct ExpectedSuccess {
    name: Option<&'static str>,
    entries: usize,
    consumed: usize,
}

#[derive(Clone, Copy, Debug)]
enum ExpectedError {
    UnexpectedEof { needed: usize, remaining: usize },
    TrailingBytes { remaining: usize },
    DepthExceeded { max: usize },
    MaxBytesExceeded { len: usize, max: usize },
    ListTooLong { len: usize, max: usize },
    StringTooLong { len: usize, max: usize },
    UnknownTagType { id: u8 },
    InvalidUtf8,
    NegativeLength { len: i32 },
    MalformedList,
    UnexpectedRootTag { id: u8 },
}

impl ExpectedError {
    fn into_nbt_error(self) -> NbtError {
        match self {
            Self::UnexpectedEof { needed, remaining } => {
                NbtError::Codec(CodecError::UnexpectedEof { needed, remaining })
            }
            Self::TrailingBytes { remaining } => {
                NbtError::Codec(CodecError::TrailingBytes { remaining })
            }
            Self::DepthExceeded { max } => NbtError::DepthExceeded { max },
            Self::MaxBytesExceeded { len, max } => NbtError::MaxBytesExceeded { len, max },
            Self::ListTooLong { len, max } => NbtError::ListTooLong { len, max },
            Self::StringTooLong { len, max } => NbtError::StringTooLong { len, max },
            Self::UnknownTagType { id } => NbtError::UnknownTagType { id },
            Self::InvalidUtf8 => NbtError::InvalidUtf8,
            Self::NegativeLength { len } => NbtError::NegativeLength { len },
            Self::MalformedList => NbtError::MalformedList,
            Self::UnexpectedRootTag { id } => NbtError::UnexpectedRootTag { id },
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ExpectedOutcome {
    Success(ExpectedSuccess),
    Error(ExpectedError),
}

#[derive(Clone, Copy, Debug)]
struct Case {
    file: &'static str,
    bytes: usize,
    form: RootForm,
    whole: ExpectedOutcome,
    consumed: ExpectedOutcome,
}

const fn success(name: Option<&'static str>, entries: usize, consumed: usize) -> ExpectedOutcome {
    ExpectedOutcome::Success(ExpectedSuccess {
        name,
        entries,
        consumed,
    })
}

const fn error(error: ExpectedError) -> ExpectedOutcome {
    ExpectedOutcome::Error(error)
}

const CASES: [Case; SEED_COUNT] = [
    Case {
        file: "00_empty.bin",
        bytes: 0,
        form: RootForm::Both,
        whole: error(ExpectedError::UnexpectedEof {
            needed: 1,
            remaining: 0,
        }),
        consumed: error(ExpectedError::UnexpectedEof {
            needed: 1,
            remaining: 0,
        }),
    },
    Case {
        file: "01_network_empty.bin",
        bytes: 2,
        form: RootForm::Network,
        whole: success(None, 0, 2),
        consumed: success(None, 0, 2),
    },
    Case {
        file: "02_network_scalars.bin",
        bytes: 47,
        form: RootForm::Network,
        whole: success(None, 6, 47),
        consumed: success(None, 6, 47),
    },
    Case {
        file: "03_network_sequences.bin",
        bytes: 55,
        form: RootForm::Network,
        whole: success(None, 5, 55),
        consumed: success(None, 5, 55),
    },
    Case {
        file: "04_network_mutf8_boundary.bin",
        bytes: 15,
        form: RootForm::Network,
        whole: success(None, 1, 15),
        consumed: success(None, 1, 15),
    },
    Case {
        file: "05_network_depth_boundary.bin",
        bytes: 14,
        form: RootForm::Network,
        whole: success(None, 1, 14),
        consumed: success(None, 1, 14),
    },
    Case {
        file: "06_network_bytes_boundary.bin",
        bytes: 64,
        form: RootForm::Network,
        whole: success(None, 15, 64),
        consumed: success(None, 15, 64),
    },
    Case {
        file: "07_network_unexpected_root.bin",
        bytes: 1,
        form: RootForm::Network,
        whole: error(ExpectedError::UnexpectedRootTag { id: 1 }),
        consumed: error(ExpectedError::UnexpectedRootTag { id: 1 }),
    },
    Case {
        file: "08_network_unknown_root.bin",
        bytes: 1,
        form: RootForm::Network,
        whole: error(ExpectedError::UnknownTagType { id: 99 }),
        consumed: error(ExpectedError::UnknownTagType { id: 99 }),
    },
    Case {
        file: "09_network_truncated.bin",
        bytes: 1,
        form: RootForm::Network,
        whole: error(ExpectedError::UnexpectedEof {
            needed: 1,
            remaining: 0,
        }),
        consumed: error(ExpectedError::UnexpectedEof {
            needed: 1,
            remaining: 0,
        }),
    },
    Case {
        file: "10_network_negative_list.bin",
        bytes: 9,
        form: RootForm::Network,
        whole: error(ExpectedError::NegativeLength { len: -1 }),
        consumed: error(ExpectedError::NegativeLength { len: -1 }),
    },
    Case {
        file: "11_network_list_over_limit.bin",
        bytes: 9,
        form: RootForm::Network,
        whole: error(ExpectedError::ListTooLong { len: 5, max: 4 }),
        consumed: error(ExpectedError::ListTooLong { len: 5, max: 4 }),
    },
    Case {
        file: "12_network_invalid_mutf8.bin",
        bytes: 7,
        form: RootForm::Network,
        whole: error(ExpectedError::InvalidUtf8),
        consumed: error(ExpectedError::InvalidUtf8),
    },
    Case {
        file: "13_network_trailing.bin",
        bytes: 3,
        form: RootForm::Network,
        whole: error(ExpectedError::TrailingBytes { remaining: 1 }),
        consumed: success(None, 0, 2),
    },
    Case {
        file: "14_network_depth_over_limit.bin",
        bytes: 13,
        form: RootForm::Network,
        whole: error(ExpectedError::DepthExceeded { max: 4 }),
        consumed: error(ExpectedError::DepthExceeded { max: 4 }),
    },
    Case {
        file: "15_network_bytes_over_limit.bin",
        bytes: 65,
        form: RootForm::Network,
        whole: error(ExpectedError::MaxBytesExceeded { len: 65, max: 64 }),
        consumed: success(None, 15, 64),
    },
    Case {
        file: "16_named_empty.bin",
        bytes: 4,
        form: RootForm::Named,
        whole: success(Some(""), 0, 4),
        consumed: success(Some(""), 0, 4),
    },
    Case {
        file: "17_named_scalars.bin",
        bytes: 50,
        form: RootForm::Named,
        whole: success(Some("r"), 6, 50),
        consumed: success(Some("r"), 6, 50),
    },
    Case {
        file: "18_named_sequences.bin",
        bytes: 57,
        form: RootForm::Named,
        whole: success(Some(""), 5, 57),
        consumed: success(Some(""), 5, 57),
    },
    Case {
        file: "19_named_mutf8_boundary.bin",
        bytes: 17,
        form: RootForm::Named,
        whole: success(Some(""), 1, 17),
        consumed: success(Some(""), 1, 17),
    },
    Case {
        file: "20_named_depth_boundary.bin",
        bytes: 16,
        form: RootForm::Named,
        whole: success(Some(""), 1, 16),
        consumed: success(Some(""), 1, 16),
    },
    Case {
        file: "21_named_bytes_boundary.bin",
        bytes: 64,
        form: RootForm::Named,
        whole: success(Some(""), 15, 64),
        consumed: success(Some(""), 15, 64),
    },
    Case {
        file: "22_named_unexpected_root.bin",
        bytes: 2,
        form: RootForm::Named,
        whole: error(ExpectedError::UnexpectedRootTag { id: 1 }),
        consumed: error(ExpectedError::UnexpectedRootTag { id: 1 }),
    },
    Case {
        file: "23_named_unknown_root.bin",
        bytes: 2,
        form: RootForm::Named,
        whole: error(ExpectedError::UnknownTagType { id: 99 }),
        consumed: error(ExpectedError::UnknownTagType { id: 99 }),
    },
    Case {
        file: "24_named_truncated_name.bin",
        bytes: 2,
        form: RootForm::Named,
        whole: error(ExpectedError::UnexpectedEof {
            needed: 2,
            remaining: 1,
        }),
        consumed: error(ExpectedError::UnexpectedEof {
            needed: 2,
            remaining: 1,
        }),
    },
    Case {
        file: "25_named_nonempty_end_list.bin",
        bytes: 11,
        form: RootForm::Named,
        whole: error(ExpectedError::MalformedList),
        consumed: error(ExpectedError::MalformedList),
    },
    Case {
        file: "26_named_array_over_limit.bin",
        bytes: 10,
        form: RootForm::Named,
        whole: error(ExpectedError::ListTooLong {
            len: 2_147_483_647,
            max: 4,
        }),
        consumed: error(ExpectedError::ListTooLong {
            len: 2_147_483_647,
            max: 4,
        }),
    },
    Case {
        file: "27_named_string_over_limit.bin",
        bytes: 8,
        form: RootForm::Named,
        whole: error(ExpectedError::StringTooLong { len: 9, max: 8 }),
        consumed: error(ExpectedError::StringTooLong { len: 9, max: 8 }),
    },
    Case {
        file: "28_named_unknown_nested.bin",
        bytes: 4,
        form: RootForm::Named,
        whole: error(ExpectedError::UnknownTagType { id: 99 }),
        consumed: error(ExpectedError::UnknownTagType { id: 99 }),
    },
    Case {
        file: "29_named_trailing.bin",
        bytes: 5,
        form: RootForm::Named,
        whole: error(ExpectedError::TrailingBytes { remaining: 1 }),
        consumed: success(Some(""), 0, 4),
    },
    Case {
        file: "30_named_bytes_over_limit.bin",
        bytes: 65,
        form: RootForm::Named,
        whole: error(ExpectedError::MaxBytesExceeded { len: 65, max: 64 }),
        consumed: success(Some(""), 15, 64),
    },
    Case {
        file: "31_named_invalid_root_name.bin",
        bytes: 4,
        form: RootForm::Named,
        whole: error(ExpectedError::InvalidUtf8),
        consumed: error(ExpectedError::InvalidUtf8),
    },
];

fn corpus_limits() -> NbtLimits {
    NbtLimits::default()
        .with_max_depth(4)
        .with_max_bytes(64)
        .with_max_list_len(4)
        .with_max_string_bytes(8)
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/fuzz/ferrumc-nbt/root_readers")
}

fn assert_exact_inventory() -> PathBuf {
    let directory = corpus_dir();
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

    let expected = CASES
        .iter()
        .map(|case| case.file.to_owned())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    directory
}

fn read_seed(directory: &Path, case: Case) -> Vec<u8> {
    let bytes = fs::read(directory.join(case.file)).expect("committed seed must be readable");
    assert_eq!(
        bytes.len(),
        case.bytes,
        "{} changed from its pinned size",
        case.file
    );
    assert!(
        bytes.len() <= MAX_SEED_BYTES,
        "{} exceeds the fixed smoke-test byte bound",
        case.file
    );
    bytes
}

fn compound_len(tag: &NbtTag, seed: &str) -> usize {
    let NbtTag::Compound(compound) = tag else {
        panic!("seed {seed}: root reader returned a non-compound tag");
    };
    compound.len()
}

fn assert_reader_error(error: &NbtError, seed: &str) {
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
            | NbtError::Codec(CodecError::UnexpectedEof { .. } | CodecError::TrailingBytes { .. })
    );
    assert!(
        is_reader_error,
        "seed {seed}: unexpected reader error {error:?}"
    );
}

fn validate_named_whole(result: &NbtResult<(String, NbtTag)>, limits: &NbtLimits, seed: &str) {
    match result {
        Ok((name, tag)) => {
            compound_len(tag, seed);
            assert!(
                write_named_root(name, tag, limits).is_ok(),
                "seed {seed}: reader-created named tree must be writable"
            );
        }
        Err(error) => assert_reader_error(error, seed),
    }
}

fn validate_named_consumed(
    result: &NbtResult<(String, NbtTag, usize)>,
    input_len: usize,
    limits: &NbtLimits,
    seed: &str,
) {
    match result {
        Ok((name, tag, consumed)) => {
            compound_len(tag, seed);
            assert!(*consumed <= input_len, "seed {seed}");
            assert!(*consumed <= limits.max_bytes(), "seed {seed}");
            assert!(
                write_named_root(name, tag, limits).is_ok(),
                "seed {seed}: reader-created named tree must be writable"
            );
        }
        Err(error) => assert_reader_error(error, seed),
    }
}

fn validate_network_whole(result: &NbtResult<NbtTag>, limits: &NbtLimits, seed: &str) {
    match result {
        Ok(tag) => {
            compound_len(tag, seed);
            assert!(
                write_network_root(tag, limits).is_ok(),
                "seed {seed}: reader-created network tree must be writable"
            );
        }
        Err(error) => assert_reader_error(error, seed),
    }
}

fn validate_network_consumed(
    result: &NbtResult<(NbtTag, usize)>,
    input_len: usize,
    limits: &NbtLimits,
    seed: &str,
) {
    match result {
        Ok((tag, consumed)) => {
            compound_len(tag, seed);
            assert!(*consumed <= input_len, "seed {seed}");
            assert!(*consumed <= limits.max_bytes(), "seed {seed}");
            assert!(
                write_network_root(tag, limits).is_ok(),
                "seed {seed}: reader-created network tree must be writable"
            );
        }
        Err(error) => assert_reader_error(error, seed),
    }
}

fn assert_canonical_prefix(encoded: NbtResult<Vec<u8>>, input: &[u8], consumed: usize, seed: &str) {
    assert!(
        consumed <= input.len(),
        "seed {seed}: expected consumed length exceeds the input"
    );
    let encoded =
        encoded.unwrap_or_else(|error| panic!("seed {seed}: matching writer failed: {error:?}"));
    assert_eq!(
        encoded.as_slice(),
        &input[..consumed],
        "seed {seed}: matching writer changed the canonical root bytes"
    );
}

fn assert_named_whole_exact(
    result: &NbtResult<(String, NbtTag)>,
    input: &[u8],
    limits: &NbtLimits,
    expected: ExpectedOutcome,
    seed: &str,
) {
    match (expected, result) {
        (ExpectedOutcome::Success(expected), Ok((name, tag))) => {
            assert_eq!(Some(name.as_str()), expected.name, "seed {seed}");
            assert_eq!(compound_len(tag, seed), expected.entries, "seed {seed}");
            assert_eq!(input.len(), expected.consumed, "seed {seed}");
            assert_canonical_prefix(
                write_named_root(name, tag, limits),
                input,
                expected.consumed,
                seed,
            );
        }
        (ExpectedOutcome::Error(expected), Err(actual)) => {
            assert_eq!(actual, &expected.into_nbt_error(), "seed {seed}");
        }
        _ => panic!("seed {seed}: expected {expected:?}, got {result:?}"),
    }
}

fn assert_named_consumed_exact(
    result: &NbtResult<(String, NbtTag, usize)>,
    input: &[u8],
    limits: &NbtLimits,
    expected: ExpectedOutcome,
    seed: &str,
) {
    match (expected, result) {
        (ExpectedOutcome::Success(expected), Ok((name, tag, consumed))) => {
            assert_eq!(Some(name.as_str()), expected.name, "seed {seed}");
            assert_eq!(compound_len(tag, seed), expected.entries, "seed {seed}");
            assert_eq!(*consumed, expected.consumed, "seed {seed}");
            assert_canonical_prefix(
                write_named_root(name, tag, limits),
                input,
                expected.consumed,
                seed,
            );
        }
        (ExpectedOutcome::Error(expected), Err(actual)) => {
            assert_eq!(actual, &expected.into_nbt_error(), "seed {seed}");
        }
        _ => panic!("seed {seed}: expected {expected:?}, got {result:?}"),
    }
}

fn assert_network_whole_exact(
    result: &NbtResult<NbtTag>,
    input: &[u8],
    limits: &NbtLimits,
    expected: ExpectedOutcome,
    seed: &str,
) {
    match (expected, result) {
        (ExpectedOutcome::Success(expected), Ok(tag)) => {
            assert_eq!(expected.name, None, "seed {seed}");
            assert_eq!(compound_len(tag, seed), expected.entries, "seed {seed}");
            assert_eq!(input.len(), expected.consumed, "seed {seed}");
            assert_canonical_prefix(
                write_network_root(tag, limits),
                input,
                expected.consumed,
                seed,
            );
        }
        (ExpectedOutcome::Error(expected), Err(actual)) => {
            assert_eq!(actual, &expected.into_nbt_error(), "seed {seed}");
        }
        _ => panic!("seed {seed}: expected {expected:?}, got {result:?}"),
    }
}

fn assert_network_consumed_exact(
    result: &NbtResult<(NbtTag, usize)>,
    input: &[u8],
    limits: &NbtLimits,
    expected: ExpectedOutcome,
    seed: &str,
) {
    match (expected, result) {
        (ExpectedOutcome::Success(expected), Ok((tag, consumed))) => {
            assert_eq!(expected.name, None, "seed {seed}");
            assert_eq!(compound_len(tag, seed), expected.entries, "seed {seed}");
            assert_eq!(*consumed, expected.consumed, "seed {seed}");
            assert_canonical_prefix(
                write_network_root(tag, limits),
                input,
                expected.consumed,
                seed,
            );
        }
        (ExpectedOutcome::Error(expected), Err(actual)) => {
            assert_eq!(actual, &expected.into_nbt_error(), "seed {seed}");
        }
        _ => panic!("seed {seed}: expected {expected:?}, got {result:?}"),
    }
}

fn replay_named(
    bytes: &[u8],
    limits: &NbtLimits,
    case: Case,
    exact: bool,
    invocations: &mut usize,
) {
    let whole = read_named_root(bytes, limits);
    *invocations += 1;
    validate_named_whole(&whole, limits, case.file);
    if exact {
        assert_named_whole_exact(&whole, bytes, limits, case.whole, case.file);
    }

    let consumed = read_named_root_with_consumed(bytes, limits);
    *invocations += 1;
    validate_named_consumed(&consumed, bytes.len(), limits, case.file);
    if exact {
        assert_named_consumed_exact(&consumed, bytes, limits, case.consumed, case.file);
        if let (Ok((whole_name, whole_tag)), Ok((consumed_name, consumed_tag, _))) =
            (&whole, &consumed)
        {
            assert_eq!(whole_name, consumed_name, "seed {}", case.file);
            assert_eq!(whole_tag, consumed_tag, "seed {}", case.file);
        }
    }
}

fn replay_network(
    bytes: &[u8],
    limits: &NbtLimits,
    case: Case,
    exact: bool,
    invocations: &mut usize,
) {
    let whole = read_network_root(bytes, limits);
    *invocations += 1;
    validate_network_whole(&whole, limits, case.file);
    if exact {
        assert_network_whole_exact(&whole, bytes, limits, case.whole, case.file);
    }

    let consumed = read_network_root_with_consumed(bytes, limits);
    *invocations += 1;
    validate_network_consumed(&consumed, bytes.len(), limits, case.file);
    if exact {
        assert_network_consumed_exact(&consumed, bytes, limits, case.consumed, case.file);
        if let (Ok(whole_tag), Ok((consumed_tag, _))) = (&whole, &consumed) {
            assert_eq!(whole_tag, consumed_tag, "seed {}", case.file);
        }
    }
}

#[test]
fn root_reader_corpus_replays_with_bounded_typed_outcomes() {
    let directory = assert_exact_inventory();
    let profiles = [
        (LimitProfile::Default, NbtLimits::default()),
        (LimitProfile::Corpus, corpus_limits()),
    ];
    let mut invocations = 0;
    let mut saw_typed_corruption = false;

    for case in CASES {
        let bytes = read_seed(&directory, case);
        for (profile, limits) in profiles {
            let exact_named = profile == LimitProfile::Corpus && case.form.includes_named();
            let exact_network = profile == LimitProfile::Corpus && case.form.includes_network();

            replay_named(&bytes, &limits, case, exact_named, &mut invocations);
            replay_network(&bytes, &limits, case, exact_network, &mut invocations);

            if profile == LimitProfile::Corpus && case.file == "10_network_negative_list.bin" {
                // Both network results were just checked exactly against the
                // typed error rather than merely observed not to panic.
                saw_typed_corruption = true;
            }
        }
    }

    assert_eq!(invocations, EXPECTED_INVOCATIONS);
    assert!(saw_typed_corruption);
}
