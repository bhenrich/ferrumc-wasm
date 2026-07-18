use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_world::{
    pack_motion_blocking_heightmap, BlockStateId, Chunk, ContainerKind, HeightmapKind, PackedArray,
    PalettedContainer, WorldError,
};

const PALETTE_CAPACITY: usize = 4_096;
const DIRECT_WIRE_BITS: u8 = 15;
const MIN_Y: i32 = -64;
const WORLD_HEIGHT: usize = 384;
const COLUMN_COUNT: usize = 16 * 16;
const HEIGHTMAP_BITS: u8 = 9;
const HEIGHTMAP_VALUES_PER_WORD: usize = 7;
const HEIGHTMAP_WORDS: usize = 37;
const TOP_CLEAR_FIXTURE: &str = include_str!("fixtures/heightmap_top_clear.shrunk");

#[derive(Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn below_usize(&mut self, upper: usize) -> usize {
        assert!(upper > 0);
        let upper = u64::try_from(upper).expect("test bound fits u64");
        usize::try_from(self.next_u64() % upper).expect("bounded value fits usize")
    }

    fn below_u32(&mut self, upper: u32) -> u32 {
        assert!(upper > 0);
        u32::try_from(self.next_u64() % u64::from(upper)).expect("bounded value fits u32")
    }
}

#[derive(Clone, Copy, Debug)]
struct PaletteMutation {
    index: usize,
    state: u32,
}

fn palette_mutations(seed: u64, distinct_non_air: u32) -> Vec<PaletteMutation> {
    let mut mutations =
        Vec::with_capacity(usize::try_from(distinct_non_air).expect("small test count") + 512);

    // Insert every requested state in a fixed order. Palette order is historical,
    // so equivalent final dense values reached through another order are not an
    // equivalent determinism input.
    for state in 1..=distinct_non_air {
        mutations.push(PaletteMutation {
            index: usize::try_from(state).expect("small state id"),
            state,
        });
    }

    let mut rng = SplitMix64::new(seed);
    for _ in 0..512 {
        mutations.push(PaletteMutation {
            index: rng.below_usize(PALETTE_CAPACITY),
            state: rng.below_u32(distinct_non_air.saturating_add(1)),
        });
    }
    mutations
}

struct PaletteOracle {
    dense: Vec<u32>,
    palette: Option<Vec<u32>>,
}

impl PaletteOracle {
    fn new() -> Self {
        Self {
            dense: vec![0; PALETTE_CAPACITY],
            palette: Some(vec![0]),
        }
    }

    fn apply(&mut self, mutation: PaletteMutation) {
        assert!(mutation.index < self.dense.len());
        if self.dense[mutation.index] == mutation.state {
            return;
        }

        if let Some(palette) = &mut self.palette {
            let already_known = palette.contains(&mutation.state);
            if !already_known && palette.len() == 256 {
                self.palette = None;
            } else if !already_known {
                palette.push(mutation.state);
            }
        }
        self.dense[mutation.index] = mutation.state;
    }

    fn kind(&self) -> ContainerKind {
        match self.palette.as_deref() {
            Some([_]) => ContainerKind::Single,
            Some(_) => ContainerKind::Indirect,
            None => ContainerKind::Direct,
        }
    }

    fn palette_len(&self) -> Option<usize> {
        self.palette.as_ref().map(Vec::len)
    }

    fn storage_bits(&self) -> u8 {
        match self.palette.as_deref() {
            Some([_]) => 0,
            Some(palette) => indirect_bits(palette.len()),
            None => 32,
        }
    }

    fn wire_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self.palette.as_deref() {
            Some([single]) => {
                out.push(0);
                push_var_u32(&mut out, *single);
            }
            Some(palette) => {
                let bits = indirect_bits(palette.len());
                out.push(bits);
                push_var_u32(
                    &mut out,
                    u32::try_from(palette.len()).expect("palette length fits u32"),
                );
                for &state in palette {
                    push_var_u32(&mut out, state);
                }
                let indices = self
                    .dense
                    .iter()
                    .map(|state| {
                        let index = palette
                            .iter()
                            .position(|candidate| candidate == state)
                            .expect("every dense state remains in the historical palette");
                        u64::try_from(index).expect("palette index fits u64")
                    })
                    .collect::<Vec<_>>();
                push_words(&mut out, &oracle_pack_entries(&indices, bits));
            }
            None => {
                out.push(DIRECT_WIRE_BITS);
                let values = self
                    .dense
                    .iter()
                    .copied()
                    .map(u64::from)
                    .collect::<Vec<_>>();
                push_words(&mut out, &oracle_pack_entries(&values, DIRECT_WIRE_BITS));
            }
        }
        out
    }
}

fn indirect_bits(palette_len: usize) -> u8 {
    assert!(palette_len >= 2);
    let needed = usize::BITS - (palette_len - 1).leading_zeros();
    u8::try_from(needed)
        .expect("palette bit width fits u8")
        .max(4)
}

fn push_var_u32(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = u8::try_from(value & 0x7f).expect("masked VarInt byte fits u8");
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn oracle_pack_entries(values: &[u64], bits: u8) -> Vec<u64> {
    assert!((1..=64).contains(&bits));
    let bits = usize::from(bits);
    let values_per_word = 64 / bits;
    let word_count = values.len().div_ceil(values_per_word);
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let mut words = vec![0u64; word_count];
    for (index, &value) in values.iter().enumerate() {
        assert_eq!(value & !mask, 0, "oracle value must fit its width");
        let word_index = index / values_per_word;
        let shift = (index % values_per_word) * bits;
        words[word_index] |= value << shift;
    }
    words
}

fn push_words(out: &mut Vec<u8>, words: &[u64]) {
    for word in words {
        out.extend_from_slice(&word.to_be_bytes());
    }
}

struct ClientPalette {
    bits: u8,
    palette: Option<Vec<u32>>,
    values: Vec<u32>,
    consumed: usize,
}

fn client_read_palette(bytes: &[u8], entries: usize) -> ClientPalette {
    let mut cursor = 0usize;
    let bits = read_byte(bytes, &mut cursor);
    if bits == 0 {
        let state = read_var_u32(bytes, &mut cursor);
        return ClientPalette {
            bits,
            palette: Some(vec![state]),
            values: vec![state; entries],
            consumed: cursor,
        };
    }

    let palette = if (4..=8).contains(&bits) {
        let len = usize::try_from(read_var_u32(bytes, &mut cursor))
            .expect("client palette length fits usize");
        assert!((2..=256).contains(&len));
        Some(
            (0..len)
                .map(|_| read_var_u32(bytes, &mut cursor))
                .collect::<Vec<_>>(),
        )
    } else {
        assert_eq!(bits, DIRECT_WIRE_BITS);
        None
    };

    let values_per_word = 64 / usize::from(bits);
    let word_count = entries.div_ceil(values_per_word);
    let mut words = Vec::with_capacity(word_count);
    for _ in 0..word_count {
        let end = cursor
            .checked_add(8)
            .expect("client cursor cannot overflow");
        let raw = bytes
            .get(cursor..end)
            .expect("encoded palette contains every computed client long");
        words.push(u64::from_be_bytes(
            raw.try_into().expect("client long is exactly eight bytes"),
        ));
        cursor = end;
    }

    let mask = (1u64 << bits) - 1;
    let values = (0..entries)
        .map(|index| {
            let word = words[index / values_per_word];
            let shift = (index % values_per_word) * usize::from(bits);
            let raw = (word >> shift) & mask;
            match &palette {
                Some(entries) => {
                    let palette_index = usize::try_from(raw).expect("palette index fits usize");
                    *entries
                        .get(palette_index)
                        .expect("wire index names an existing palette entry")
                }
                None => u32::try_from(raw).expect("direct block state fits u32"),
            }
        })
        .collect();

    ClientPalette {
        bits,
        palette,
        values,
        consumed: cursor,
    }
}

fn read_byte(bytes: &[u8], cursor: &mut usize) -> u8 {
    let byte = *bytes.get(*cursor).expect("client byte is present");
    *cursor += 1;
    byte
}

fn read_var_u32(bytes: &[u8], cursor: &mut usize) -> u32 {
    let mut value = 0u32;
    for byte_index in 0..5 {
        let byte = read_byte(bytes, cursor);
        value |= u32::from(byte & 0x7f) << (byte_index * 7);
        if byte & 0x80 == 0 {
            return value;
        }
    }
    panic!("test encoder emitted an overlong VarInt");
}

#[test]
fn palette_identical_replay_matches_dense_oracle_and_client_decoder() {
    let distinct_cases = [0u32, 1, 15, 16, 31, 32, 63, 64, 127, 128, 255, 256];
    for (case_index, distinct) in distinct_cases.into_iter().enumerate() {
        let seed = 0xA076_1D64_78BD_642F_u64
            .wrapping_add(u64::try_from(case_index).expect("small case index") * 0x100_0000_01B3);
        let mutations = palette_mutations(seed, distinct);
        let mut first = PalettedContainer::<PALETTE_CAPACITY>::new();
        let mut second = PalettedContainer::<PALETTE_CAPACITY>::new();
        let mut oracle = PaletteOracle::new();

        for mutation in mutations {
            first
                .set(mutation.index, BlockStateId::new(mutation.state))
                .expect("generated palette mutation is valid");
            second
                .set(mutation.index, BlockStateId::new(mutation.state))
                .expect("identical generated palette mutation is valid");
            oracle.apply(mutation);
            assert_eq!(
                first.get(mutation.index),
                Some(BlockStateId::new(oracle.dense[mutation.index]))
            );
        }

        assert_eq!(first, second, "distinct case {distinct}");
        assert_eq!(first.kind(), oracle.kind(), "distinct case {distinct}");
        assert_eq!(
            first.palette_len(),
            oracle.palette_len(),
            "distinct case {distinct}"
        );
        assert_eq!(
            first.bits_per_entry(),
            oracle.storage_bits(),
            "distinct case {distinct}"
        );
        let expected_non_air = oracle.dense.iter().filter(|state| **state != 0).count();
        assert_eq!(first.non_air_count(), expected_non_air);
        assert_eq!(first.air_count(), PALETTE_CAPACITY - expected_non_air);
        for (index, expected) in oracle.dense.iter().copied().enumerate() {
            assert_eq!(
                first.get(index),
                Some(BlockStateId::new(expected)),
                "distinct case {distinct}, index {index}"
            );
        }

        let mut first_bytes = Vec::new();
        first
            .encode_network(&mut first_bytes, DIRECT_WIRE_BITS)
            .expect("generated ids fit the direct wire width");
        let mut second_bytes = Vec::new();
        second
            .encode_network(&mut second_bytes, DIRECT_WIRE_BITS)
            .expect("identical generated ids fit the direct wire width");
        assert_eq!(first_bytes, second_bytes, "distinct case {distinct}");
        assert_eq!(
            first_bytes,
            oracle.wire_bytes(),
            "exact wire bytes, distinct case {distinct}"
        );

        let decoded = client_read_palette(&first_bytes, PALETTE_CAPACITY);
        let expected_wire_bits = if oracle.kind() == ContainerKind::Direct {
            DIRECT_WIRE_BITS
        } else {
            oracle.storage_bits()
        };
        assert_eq!(decoded.bits, expected_wire_bits);
        assert_eq!(decoded.palette.as_deref(), oracle.palette.as_deref());
        assert_eq!(decoded.values, oracle.dense);
        assert_eq!(decoded.consumed, first_bytes.len());
    }
}

#[test]
fn palette_representation_thresholds_are_exact() {
    let checkpoints = [
        (1u32, ContainerKind::Indirect, Some(2usize), 4u8),
        (15, ContainerKind::Indirect, Some(16), 4),
        (16, ContainerKind::Indirect, Some(17), 5),
        (31, ContainerKind::Indirect, Some(32), 5),
        (32, ContainerKind::Indirect, Some(33), 6),
        (63, ContainerKind::Indirect, Some(64), 6),
        (64, ContainerKind::Indirect, Some(65), 7),
        (127, ContainerKind::Indirect, Some(128), 7),
        (128, ContainerKind::Indirect, Some(129), 8),
        (255, ContainerKind::Indirect, Some(256), 8),
        (256, ContainerKind::Direct, None, 32),
    ];
    let mut container = PalettedContainer::<PALETTE_CAPACITY>::new();
    assert_eq!(container.kind(), ContainerKind::Single);
    assert_eq!(container.palette_len(), Some(1));
    assert_eq!(container.bits_per_entry(), 0);

    let mut checkpoint = 0usize;
    for state in 1..=256u32 {
        container
            .set(
                usize::try_from(state).expect("small state index"),
                BlockStateId::new(state),
            )
            .expect("threshold mutation is valid");
        if checkpoints[checkpoint].0 == state {
            let (_, kind, palette_len, bits) = checkpoints[checkpoint];
            assert_eq!(container.kind(), kind, "state {state}");
            assert_eq!(container.palette_len(), palette_len, "state {state}");
            assert_eq!(container.bits_per_entry(), bits, "state {state}");
            checkpoint += 1;
        }
    }
    assert_eq!(checkpoint, checkpoints.len());
}

#[test]
fn invalid_palette_packed_and_chunk_operations_are_typed_and_atomic() {
    let mut container = PalettedContainer::<PALETTE_CAPACITY>::new();
    for state in 1..=20u32 {
        container
            .set(
                usize::try_from(state).expect("small state index"),
                BlockStateId::new(state),
            )
            .expect("setup mutation is valid");
    }
    for index in [PALETTE_CAPACITY, PALETTE_CAPACITY + 17, usize::MAX] {
        let before = container.clone();
        assert_eq!(
            container.set(index, BlockStateId::new(99)),
            Err(WorldError::IndexOutOfRange {
                index,
                capacity: PALETTE_CAPACITY,
            })
        );
        assert_eq!(container, before, "invalid palette index {index}");
    }

    assert_eq!(
        PackedArray::new(0, 8),
        Err(WorldError::InvalidBitsPerEntry { bits: 0 })
    );
    assert_eq!(
        PackedArray::new(65, 8),
        Err(WorldError::InvalidBitsPerEntry { bits: 65 })
    );
    let mut packed = PackedArray::new(5, 19).expect("valid setup width");
    packed.set(0, 31).expect("maximum five-bit value fits");
    packed.set(18, 7).expect("last index is valid");

    let before = packed.clone();
    assert_eq!(
        packed.set(19, 1),
        Err(WorldError::PackedIndexOutOfRange { index: 19, len: 19 })
    );
    assert_eq!(packed, before);

    let before = packed.clone();
    assert_eq!(
        packed.set(18, 32),
        Err(WorldError::ValueTooWide { value: 32, bits: 5 })
    );
    assert_eq!(packed, before);

    let before = packed.clone();
    assert_eq!(
        packed.resized(4),
        Err(WorldError::ValueTooWide { value: 31, bits: 4 })
    );
    assert_eq!(packed, before);

    let chunk_pos = ChunkPos::new(-2, 3);
    let mut chunk = Chunk::new(chunk_pos);
    let valid = BlockPos::new(-32, 0, 48);
    chunk
        .set_block(valid, BlockStateId::new(1))
        .expect("setup block belongs to the chunk");
    for pos in [
        BlockPos::new(-32, MIN_Y - 1, 48),
        BlockPos::new(
            -32,
            MIN_Y + i32::try_from(WORLD_HEIGHT).expect("height fits i32"),
            48,
        ),
        BlockPos::new(-33, 0, 48),
    ] {
        let before = chunk.clone();
        assert_eq!(
            chunk.set_block(pos, BlockStateId::new(2)),
            Err(WorldError::BlockOutsideChunk { pos })
        );
        assert_eq!(chunk, before, "invalid chunk edit at {pos:?}");
    }
}

#[derive(Clone, Copy, Debug)]
struct BlockMutation {
    x: u8,
    z: u8,
    y: i32,
    state: u32,
}

fn heightmap_mutations(seed: u64) -> Vec<BlockMutation> {
    let mut rng = SplitMix64::new(seed);
    let mut mutations = Vec::with_capacity(COLUMN_COUNT * 3 + 768);

    // Give every column a lower block, a higher block, and a clear of the higher
    // block. This forces the implementation to rescan below a removed maximum.
    for z in 0..16u8 {
        for x in 0..16u8 {
            let lower_offset = rng.below_usize(WORLD_HEIGHT - 1);
            let remaining_above = WORLD_HEIGHT - lower_offset - 1;
            let upper_offset = lower_offset + 1 + rng.below_usize(remaining_above);
            let lower_y = MIN_Y + i32::try_from(lower_offset).expect("height offset fits i32");
            let upper_y = MIN_Y + i32::try_from(upper_offset).expect("height offset fits i32");
            mutations.push(BlockMutation {
                x,
                z,
                y: lower_y,
                state: 1 + rng.below_u32(64),
            });
            mutations.push(BlockMutation {
                x,
                z,
                y: upper_y,
                state: 1 + rng.below_u32(64),
            });
            mutations.push(BlockMutation {
                x,
                z,
                y: upper_y,
                state: 0,
            });
        }
    }

    for _ in 0..768 {
        let state = if rng.below_u32(4) == 0 {
            0
        } else {
            1 + rng.below_u32(96)
        };
        mutations.push(BlockMutation {
            x: u8::try_from(rng.below_usize(16)).expect("local x fits u8"),
            z: u8::try_from(rng.below_usize(16)).expect("local z fits u8"),
            y: MIN_Y
                + i32::try_from(rng.below_usize(WORLD_HEIGHT)).expect("height offset fits i32"),
            state,
        });
    }
    mutations
}

struct DenseChunkOracle {
    blocks: Vec<u32>,
}

impl DenseChunkOracle {
    fn new() -> Self {
        Self {
            blocks: vec![0; COLUMN_COUNT * WORLD_HEIGHT],
        }
    }

    fn apply(&mut self, mutation: BlockMutation) {
        let index = dense_block_index(mutation);
        self.blocks[index] = mutation.state;
    }

    fn heights(&self) -> [Option<i32>; COLUMN_COUNT] {
        std::array::from_fn(|column| {
            let offset = (0..WORLD_HEIGHT)
                .rev()
                .find(|offset| self.blocks[*offset * COLUMN_COUNT + column] != 0)?;
            Some(MIN_Y + i32::try_from(offset).expect("height offset fits i32"))
        })
    }

    fn packed_heightmap(&self) -> Vec<i64> {
        let values = self
            .heights()
            .into_iter()
            .map(|height| match height {
                Some(y) => u64::try_from(y - MIN_Y + 1).expect("height value is positive"),
                None => 0,
            })
            .collect::<Vec<_>>();
        let words = oracle_pack_entries(&values, HEIGHTMAP_BITS);
        assert_eq!(words.len(), HEIGHTMAP_WORDS);
        words
            .into_iter()
            .map(|word| i64::from_ne_bytes(word.to_ne_bytes()))
            .collect()
    }
}

fn dense_block_index(mutation: BlockMutation) -> usize {
    assert!(mutation.x < 16 && mutation.z < 16);
    let y_offset = usize::try_from(mutation.y - MIN_Y).expect("generated height is at least MIN_Y");
    assert!(y_offset < WORLD_HEIGHT);
    let column = usize::from(mutation.z) * 16 + usize::from(mutation.x);
    y_offset * COLUMN_COUNT + column
}

fn absolute_pos(chunk_pos: ChunkPos, mutation: BlockMutation) -> BlockPos {
    let origin = chunk_pos.origin_block(mutation.y);
    BlockPos::new(
        origin.x() + i32::from(mutation.x),
        mutation.y,
        origin.z() + i32::from(mutation.z),
    )
}

fn apply_mutation(chunk_pos: ChunkPos, chunk: &mut Chunk, mutation: BlockMutation) {
    chunk
        .set_block(
            absolute_pos(chunk_pos, mutation),
            BlockStateId::new(mutation.state),
        )
        .expect("generated block mutation belongs to the chunk");
}

fn unpack_heightmap_like_client(longs: &[i64]) -> [u16; COLUMN_COUNT] {
    assert_eq!(longs.len(), HEIGHTMAP_WORDS);
    for (word_index, &long) in longs.iter().enumerate() {
        let word = u64::from_ne_bytes(long.to_ne_bytes());
        let remaining = COLUMN_COUNT.saturating_sub(word_index * HEIGHTMAP_VALUES_PER_WORD);
        let used = remaining.min(HEIGHTMAP_VALUES_PER_WORD);
        let used_bits = used * usize::from(HEIGHTMAP_BITS);
        assert_eq!(
            word >> used_bits,
            0,
            "word {word_index} has non-zero padding bits"
        );
    }

    std::array::from_fn(|column| {
        let word_index = column / HEIGHTMAP_VALUES_PER_WORD;
        let slot = column % HEIGHTMAP_VALUES_PER_WORD;
        let word = u64::from_ne_bytes(longs[word_index].to_ne_bytes());
        let value = (word >> (slot * usize::from(HEIGHTMAP_BITS))) & 0x1ff;
        u16::try_from(value).expect("nine-bit height value fits u16")
    })
}

fn assert_heightmaps_match_oracle(chunk: &Chunk, oracle: &DenseChunkOracle) {
    let expected = oracle.heights();
    let surface = chunk.heightmap(HeightmapKind::WorldSurface);
    let motion = chunk.heightmap(HeightmapKind::MotionBlocking);
    for z in 0..16u8 {
        for x in 0..16u8 {
            let column = usize::from(z) * 16 + usize::from(x);
            assert_eq!(surface.height(x, z), expected[column], "column ({x},{z})");
            assert_eq!(motion.height(x, z), expected[column], "column ({x},{z})");
        }
    }

    let actual = pack_motion_blocking_heightmap(chunk).expect("generated heights fit nine bits");
    assert_eq!(actual.len(), HEIGHTMAP_WORDS);
    assert_eq!(actual, oracle.packed_heightmap());
    let unpacked = unpack_heightmap_like_client(&actual);
    for (column, value) in unpacked.into_iter().enumerate() {
        let expected_value = match expected[column] {
            Some(y) => u16::try_from(y - MIN_Y + 1).expect("height value fits u16"),
            None => 0,
        };
        assert_eq!(value, expected_value, "packed column {column}");
    }
}

#[test]
fn heightmap_twin_replay_matches_dense_oracle_and_independent_unpack() {
    let seeds = [
        0x243F_6A88_85A3_08D3,
        0x1319_8A2E_0370_7344,
        0xA409_3822_299F_31D0,
        0x082E_FA98_EC4E_6C89,
    ];
    for seed in seeds {
        let chunk_pos = ChunkPos::new(-3, 5);
        let mutations = heightmap_mutations(seed);
        let mut first = Chunk::new(chunk_pos);
        let mut second = Chunk::new(chunk_pos);
        let mut oracle = DenseChunkOracle::new();

        // Both chunks receive the exact same ordered history. Reordering equal
        // final writes is not a valid twin input because section palette order is
        // itself historical and participates in Chunk equality/wire bytes.
        for mutation in mutations {
            apply_mutation(chunk_pos, &mut first, mutation);
            apply_mutation(chunk_pos, &mut second, mutation);
            oracle.apply(mutation);
        }

        assert_eq!(first, second, "seed {seed:#018x}");
        assert_heightmaps_match_oracle(&first, &oracle);
        assert_heightmaps_match_oracle(&second, &oracle);
        assert_eq!(
            pack_motion_blocking_heightmap(&first),
            pack_motion_blocking_heightmap(&second)
        );
    }
}

fn parse_top_clear_fixture() -> Vec<BlockMutation> {
    TOP_CLEAR_FIXTURE
        .lines()
        .map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let x = fields
                .next()
                .expect("fixture x")
                .parse::<u8>()
                .expect("fixture x is u8");
            let z = fields
                .next()
                .expect("fixture z")
                .parse::<u8>()
                .expect("fixture z is u8");
            let y = fields
                .next()
                .expect("fixture y")
                .parse::<i32>()
                .expect("fixture y is i32");
            let state = fields
                .next()
                .expect("fixture state")
                .parse::<u32>()
                .expect("fixture state is u32");
            assert!(fields.next().is_none(), "fixture row has four fields");
            BlockMutation { x, z, y, state }
        })
        .collect()
}

#[test]
fn heightmap_top_clear_shrunk_fixture_is_pinned() {
    assert_eq!(TOP_CLEAR_FIXTURE, "0 0 -64 1\n0 0 -63 2\n0 0 -63 0\n");
    let mutations = parse_top_clear_fixture();
    assert_eq!(mutations.len(), 3);

    let chunk_pos = ChunkPos::new(-1, -1);
    let mut first = Chunk::new(chunk_pos);
    let mut second = Chunk::new(chunk_pos);
    let mut oracle = DenseChunkOracle::new();
    let expected_steps = [Some(-64), Some(-63), Some(-64)];

    for (mutation, expected_height) in mutations.into_iter().zip(expected_steps) {
        apply_mutation(chunk_pos, &mut first, mutation);
        apply_mutation(chunk_pos, &mut second, mutation);
        oracle.apply(mutation);
        assert_eq!(
            first.heightmap(HeightmapKind::MotionBlocking).height(0, 0),
            expected_height
        );
        assert_eq!(first, second);
    }

    assert_heightmaps_match_oracle(&first, &oracle);
    let packed = pack_motion_blocking_heightmap(&first).expect("fixture height fits");
    let unpacked = unpack_heightmap_like_client(&packed);
    assert_eq!(unpacked[0], 1);
    assert!(unpacked[1..].iter().all(|value| *value == 0));
}
