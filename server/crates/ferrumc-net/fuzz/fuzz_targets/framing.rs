#![no_main]
#![forbid(unsafe_code)]

use ferrumc_net::{
    CompressionState, ConnectionLimits, ConnectionState, DecodeError, InboundDecoder, InboundPacket,
};
use libfuzzer_sys::fuzz_target;

const MAX_FUZZ_INPUT: usize = 1_024;
const FRAME_LIMIT: usize = 512;
const DECOMPRESSED_LIMIT: usize = 256;
const COMPRESSION_THRESHOLD: usize = 16;

fn limits() -> ConnectionLimits {
    ConnectionLimits::new(
        FRAME_LIMIT,
        FRAME_LIMIT,
        FRAME_LIMIT,
        FRAME_LIMIT,
        FRAME_LIMIT,
    )
}

fuzz_target!(|input: &[u8]| {
    if input.len() > MAX_FUZZ_INPUT {
        return;
    }
    let Some((&mode, wire)) = input.split_first() else {
        return;
    };

    let limits = limits();
    let mut decoder = InboundDecoder::new(limits);
    if let Err(error) = decoder.push(wire) {
        assert!(matches!(error, DecodeError::BufferOverflow { .. }));
        assert_eq!(decoder.buffered_len(), 0);
        return;
    }

    let compression = if mode == 0 {
        CompressionState::disabled()
    } else {
        CompressionState::with_cap(Some(COMPRESSION_THRESHOLD), DECOMPRESSED_LIMIT)
    };
    if let Ok(Some(packet)) = decoder.next_packet_compressed(ConnectionState::Play, &compression) {
        assert_eq!(packet.state(), ConnectionState::Play);
        if let InboundPacket::Play(body) = packet {
            let output_limit = if mode == 0 {
                FRAME_LIMIT
            } else {
                DECOMPRESSED_LIMIT
            };
            assert!(body.len() <= output_limit);
        }
    }
    assert!(decoder.buffered_len() <= limits.max_inbound_buffer());
});
