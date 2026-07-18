//! Stable replay gate for the committed bounded framing/decompression seeds.
//!
//! The deep libFuzzer target is opt-in, but these fixed inputs run on stable
//! Rust with every ordinary workspace test.

use std::fs;
use std::path::{Path, PathBuf};

use ferrumc_net::{
    CompressionError, CompressionState, ConnectionLimits, ConnectionState, DecodeError,
    FrameDecodeError, InboundDecoder, InboundPacket,
};

const SEED_COUNT: usize = 24;
const PUSH_ATTEMPTS: usize = 23;
const NEXT_PACKET_ATTEMPTS: usize = 22;
const MAX_SEED_BYTES: usize = 519;
const FRAME_LIMIT: usize = 512;
const BUFFER_LIMIT: usize = 517;
const DECOMPRESSED_LIMIT: usize = 256;
const COMPRESSION_THRESHOLD: usize = 16;

#[derive(Clone, Copy, Debug)]
enum BodySpec {
    Repeated { byte: u8, len: usize },
}

#[derive(Clone, Copy, Debug)]
enum Expected {
    NoSelector,
    NeedMore { buffered: usize },
    Packet(BodySpec),
    BadLengthVarInt,
    NegativeLength,
    FrameTooLarge,
    BufferOverflow,
    BadDataLength,
    NegativeDataLength,
    BelowThreshold,
    UncompressedAtOrAboveThreshold,
    DeclaredTooLarge,
    SizeMismatch,
    Oversized,
    MalformedZlib,
}

impl Expected {
    fn frame_error(self) -> Option<FrameDecodeError> {
        let error = match self {
            Self::BadLengthVarInt => FrameDecodeError::Decode(DecodeError::BadLengthVarInt),
            Self::NegativeLength => {
                FrameDecodeError::Decode(DecodeError::NegativeLength { length: -1 })
            }
            Self::FrameTooLarge => FrameDecodeError::Decode(DecodeError::FrameTooLarge {
                state: ConnectionState::Play,
                length: 513,
                max: FRAME_LIMIT,
            }),
            Self::BadDataLength => FrameDecodeError::Compression(CompressionError::BadDataLength),
            Self::NegativeDataLength => {
                FrameDecodeError::Compression(CompressionError::NegativeDataLength { length: -1 })
            }
            Self::BelowThreshold => {
                FrameDecodeError::Compression(CompressionError::BelowThreshold {
                    declared: 15,
                    threshold: COMPRESSION_THRESHOLD,
                })
            }
            Self::UncompressedAtOrAboveThreshold => {
                FrameDecodeError::Compression(CompressionError::UncompressedAtOrAboveThreshold {
                    actual: 16,
                    threshold: COMPRESSION_THRESHOLD,
                })
            }
            Self::DeclaredTooLarge => {
                FrameDecodeError::Compression(CompressionError::DeclaredTooLarge {
                    declared: 4_096,
                    cap: DECOMPRESSED_LIMIT,
                })
            }
            Self::SizeMismatch => FrameDecodeError::Compression(CompressionError::SizeMismatch {
                declared: 17,
                actual: 16,
            }),
            Self::Oversized => {
                FrameDecodeError::Compression(CompressionError::Oversized { declared: 16 })
            }
            Self::MalformedZlib => FrameDecodeError::Compression(CompressionError::MalformedZlib),
            Self::NoSelector | Self::NeedMore { .. } | Self::Packet(_) | Self::BufferOverflow => {
                return None
            }
        };
        Some(error)
    }
}

#[derive(Default)]
struct Witnesses {
    frame_corruption: bool,
    compression_corruption: bool,
    bomb_rejection: bool,
}

impl Witnesses {
    fn observe(&mut self, expected: Expected) {
        match expected {
            Expected::BadLengthVarInt => self.frame_corruption = true,
            Expected::BadDataLength => self.compression_corruption = true,
            Expected::DeclaredTooLarge => self.bomb_rejection = true,
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Case {
    file: &'static str,
    bytes: usize,
    expected: Expected,
}

const CASES: [Case; SEED_COUNT] = [
    Case {
        file: "00_empty.bin",
        bytes: 0,
        expected: Expected::NoSelector,
    },
    Case {
        file: "01_plain_empty_wire.bin",
        bytes: 1,
        expected: Expected::NeedMore { buffered: 0 },
    },
    Case {
        file: "02_plain_truncated_prefix.bin",
        bytes: 2,
        expected: Expected::NeedMore { buffered: 1 },
    },
    Case {
        file: "03_plain_bad_length_varint.bin",
        bytes: 6,
        expected: Expected::BadLengthVarInt,
    },
    Case {
        file: "04_plain_negative_length.bin",
        bytes: 6,
        expected: Expected::NegativeLength,
    },
    Case {
        file: "05_plain_short_body.bin",
        bytes: 3,
        expected: Expected::NeedMore { buffered: 2 },
    },
    Case {
        file: "06_plain_valid_one_byte.bin",
        bytes: 3,
        expected: Expected::Packet(BodySpec::Repeated { byte: 0xab, len: 1 }),
    },
    Case {
        file: "07_plain_exact_frame_limit.bin",
        bytes: 515,
        expected: Expected::Packet(BodySpec::Repeated {
            byte: 0x5a,
            len: FRAME_LIMIT,
        }),
    },
    Case {
        file: "08_plain_frame_over_limit.bin",
        bytes: 516,
        expected: Expected::FrameTooLarge,
    },
    Case {
        file: "09_plain_push_overflow.bin",
        bytes: 519,
        expected: Expected::BufferOverflow,
    },
    Case {
        file: "10_compressed_marker_zero_valid.bin",
        bytes: 4,
        expected: Expected::Packet(BodySpec::Repeated { byte: 0x7f, len: 1 }),
    },
    Case {
        file: "11_compressed_marker_zero_at_threshold.bin",
        bytes: 19,
        expected: Expected::UncompressedAtOrAboveThreshold,
    },
    Case {
        file: "12_compressed_declared_16_valid.bin",
        bytes: 14,
        expected: Expected::Packet(BodySpec::Repeated {
            byte: 0x41,
            len: 16,
        }),
    },
    Case {
        file: "13_compressed_exact_output_limit.bin",
        bytes: 16,
        expected: Expected::Packet(BodySpec::Repeated {
            byte: 0x42,
            len: DECOMPRESSED_LIMIT,
        }),
    },
    Case {
        file: "14_compressed_declared_bomb.bin",
        bytes: 30,
        expected: Expected::DeclaredTooLarge,
    },
    Case {
        file: "15_compressed_corrupt_zlib.bin",
        bytes: 6,
        expected: Expected::MalformedZlib,
    },
    Case {
        file: "16_compressed_size_mismatch.bin",
        bytes: 14,
        expected: Expected::SizeMismatch,
    },
    Case {
        file: "17_compressed_oversized_output.bin",
        bytes: 14,
        expected: Expected::Oversized,
    },
    Case {
        file: "18_compressed_trailing_zlib.bin",
        bytes: 15,
        expected: Expected::MalformedZlib,
    },
    Case {
        file: "19_compressed_bad_data_length.bin",
        bytes: 7,
        expected: Expected::BadDataLength,
    },
    Case {
        file: "20_compressed_negative_data_length.bin",
        bytes: 7,
        expected: Expected::NegativeDataLength,
    },
    Case {
        file: "21_compressed_below_threshold.bin",
        bytes: 14,
        expected: Expected::BelowThreshold,
    },
    Case {
        file: "22_compressed_zero_outer_body.bin",
        bytes: 2,
        expected: Expected::BadDataLength,
    },
    Case {
        file: "23_compressed_truncated_outer.bin",
        bytes: 4,
        expected: Expected::NeedMore { buffered: 3 },
    },
];

fn limits() -> ConnectionLimits {
    ConnectionLimits::new(
        FRAME_LIMIT,
        FRAME_LIMIT,
        FRAME_LIMIT,
        FRAME_LIMIT,
        FRAME_LIMIT,
    )
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/fuzz/ferrumc-net/framing")
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
    assert_eq!(bytes.len(), case.bytes, "seed {} changed size", case.file);
    assert!(
        bytes.len() <= MAX_SEED_BYTES,
        "seed {} exceeds the smoke-test byte bound",
        case.file
    );
    bytes
}

fn assert_body(packet: InboundPacket, spec: BodySpec, seed: &str) {
    let InboundPacket::Play(body) = packet else {
        panic!("seed {seed}: play decode returned a packet for another state");
    };
    match spec {
        BodySpec::Repeated { byte, len } => {
            assert_eq!(body.len(), len, "seed {seed}");
            assert!(body.iter().all(|actual| *actual == byte), "seed {seed}");
        }
    }
}

#[test]
fn framing_corpus_replays_with_bounded_typed_outcomes() {
    let directory = assert_exact_inventory();
    let limits = limits();
    assert_eq!(limits.max_inbound_buffer(), BUFFER_LIMIT);

    let mut pushes = 0;
    let mut next_calls = 0;
    let mut witnesses = Witnesses::default();

    for case in CASES {
        let input = read_seed(&directory, case);
        let Some((&mode, wire)) = input.split_first() else {
            assert!(matches!(case.expected, Expected::NoSelector));
            continue;
        };
        pushes += 1;

        let mut decoder = InboundDecoder::new(limits);
        let push = decoder.push(wire);
        if matches!(case.expected, Expected::BufferOverflow) {
            assert_eq!(
                push,
                Err(DecodeError::BufferOverflow {
                    buffered: 518,
                    max: BUFFER_LIMIT,
                }),
                "seed {}",
                case.file
            );
            assert_eq!(decoder.buffered_len(), 0, "seed {}", case.file);
            continue;
        }
        push.unwrap_or_else(|error| panic!("seed {}: unexpected push error: {error:?}", case.file));

        let compression = if mode == 0 {
            CompressionState::disabled()
        } else {
            CompressionState::with_cap(Some(COMPRESSION_THRESHOLD), DECOMPRESSED_LIMIT)
        };
        next_calls += 1;
        let result = decoder.next_packet_compressed(ConnectionState::Play, &compression);
        match case.expected {
            Expected::NeedMore { buffered } => {
                assert_eq!(result, Ok(None), "seed {}", case.file);
                assert_eq!(decoder.buffered_len(), buffered, "seed {}", case.file);
            }
            Expected::Packet(body) => {
                let packet = result
                    .unwrap_or_else(|error| {
                        panic!("seed {}: unexpected decode error: {error:?}", case.file)
                    })
                    .unwrap_or_else(|| panic!("seed {}: expected a complete packet", case.file));
                assert_eq!(packet.state(), ConnectionState::Play, "seed {}", case.file);
                assert_body(packet, body, case.file);
                assert_eq!(decoder.buffered_len(), 0, "seed {}", case.file);
            }
            Expected::BadLengthVarInt
            | Expected::NegativeLength
            | Expected::FrameTooLarge
            | Expected::BadDataLength
            | Expected::NegativeDataLength
            | Expected::BelowThreshold
            | Expected::UncompressedAtOrAboveThreshold
            | Expected::DeclaredTooLarge
            | Expected::SizeMismatch
            | Expected::Oversized
            | Expected::MalformedZlib => {
                let Some(expected_error) = case.expected.frame_error() else {
                    panic!("seed {}: missing expected frame error", case.file);
                };
                assert_eq!(result, Err(expected_error), "seed {}", case.file);
                witnesses.observe(case.expected);
            }
            Expected::NoSelector | Expected::BufferOverflow => {
                panic!("seed {}: outcome was handled before decode", case.file)
            }
        }
        assert!(decoder.buffered_len() <= BUFFER_LIMIT, "seed {}", case.file);
    }

    assert_eq!(pushes, PUSH_ATTEMPTS);
    assert_eq!(next_calls, NEXT_PACKET_ATTEMPTS);
    assert!(witnesses.frame_corruption);
    assert!(witnesses.compression_corruption);
    assert!(witnesses.bomb_rejection);
}
