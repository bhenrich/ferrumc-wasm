//! A paletted container of block-state ids, Minecraft chunk-section style.

use crate::block_state::BlockStateId;
use crate::error::WorldError;
use crate::packed_array::PackedArray;

/// Minimum bits-per-entry for the indirect (palette-indexed) representation.
///
/// Minecraft clamps block palettes to at least 4 bits even when fewer would
/// suffice, so the smallest indirect palette can address 16 entries.
const MIN_INDIRECT_BITS: u8 = 4;

/// Maximum bits-per-entry before the container abandons the palette and stores
/// raw ids directly. Above 8 bits (a 256-entry palette) the direct
/// representation is used instead.
const MAX_INDIRECT_BITS: u8 = 8;

/// Bits-per-entry for the *internal* direct representation.
///
/// Direct storage holds full [`BlockStateId`] values, so it uses the entire
/// `u32` width. This is wider than vanilla (1.21.8 sizes the direct palette to
/// `ceil(log2(global block-state count))`, which is 15 bits) but is always
/// correct regardless of how high an id climbs, and the direct representation is
/// only reached by sections with more than 256 distinct states.
///
/// The wire format uses the narrower vanilla width: [`PalettedContainer::encode_network`]
/// takes a `direct_wire_bits` argument and repacks a direct container to that
/// width on the way out (15 for block states), so the bytes a client receives
/// match vanilla even though the in-memory layout is 32-bit. A flat world never
/// reaches the direct representation, so this repack is exercised only by tests.
const DIRECT_BITS: u8 = 32;

/// Which internal representation a [`PalettedContainer`] is currently using.
///
/// Exposed for inspection and tests; the container promotes between these
/// automatically as the set of distinct states grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    /// Every entry is the same id; stored as a single value with no array.
    Single,
    /// A palette of distinct ids plus a packed array of palette indices.
    Indirect,
    /// Raw ids packed directly, with no palette.
    Direct,
}

/// Internal storage backing a [`PalettedContainer`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Repr {
    /// All `CAPACITY` entries hold this id.
    Single(BlockStateId),
    /// `indices[i]` selects `palette[indices[i]]`.
    Indirect {
        palette: Vec<BlockStateId>,
        indices: PackedArray,
    },
    /// `data[i]` is the raw [`BlockStateId`] value at `i`.
    Direct(PackedArray),
}

/// A fixed-capacity container of [`BlockStateId`]s with an automatically
/// promoted palette, mirroring how a Minecraft chunk section stores blocks.
///
/// `CAPACITY` is the number of entries (4096 for a 16x16x16 block section). The
/// container starts as a single-value fast path holding [`BlockStateId::AIR`]
/// and promotes its representation as distinct states are added:
///
/// 1. `Single` — one id for every entry, no allocation.
/// 2. `Indirect` — a palette of distinct ids plus a [`PackedArray`] of palette
///    indices. Bits-per-entry grows (clamped to a 4-bit minimum) as the palette
///    grows, up to a 256-entry / 8-bit palette.
/// 3. `Direct` — raw ids packed directly once more than 256 distinct states
///    appear, dropping the palette.
///
/// Indexing is bounds-checked: [`PalettedContainer::get`] returns `None` and
/// [`PalettedContainer::set`] returns [`WorldError::IndexOutOfRange`] for an
/// index `>= CAPACITY`, so no valid call panics. A running count of non-air
/// entries is maintained so [`PalettedContainer::non_air_count`] is O(1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PalettedContainer<const CAPACITY: usize> {
    repr: Repr,
    non_air_count: usize,
}

/// Returns the number of bits needed to address `len` distinct palette indices
/// (`0..len`), or `0` when `len <= 1`.
const fn index_bits_for_len(len: usize) -> u32 {
    if len <= 1 {
        0
    } else {
        usize::BITS - (len - 1).leading_zeros()
    }
}

/// Appends `value` to `out` as a Minecraft `VarInt` (1–5 bytes).
///
/// Only non-negative values are encoded here (palette ids and palette lengths),
/// for which the unsigned 7-bit grouping is identical to the protocol's signed
/// `VarInt`. The high bit of each byte is the continuation flag. Shared with the
/// chunk-blob assembler in [`crate::network`], which writes the single-valued
/// biome container the same way.
pub(crate) fn write_var_u32(out: &mut Vec<u8>, mut value: u32) {
    loop {
        // Mask to the low 7 bits before narrowing so the cast can never truncate.
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends each packed `u64` word to `out` as a big-endian 8-byte long.
///
/// The chunk-section wire format writes the backing words verbatim (the bit
/// pattern is what the client unpacks), so this is a straight big-endian dump
/// with no length prefix.
fn write_packed_longs(out: &mut Vec<u8>, words: &[u64]) {
    for &word in words {
        out.extend_from_slice(&word.to_be_bytes());
    }
}

impl<const CAPACITY: usize> PalettedContainer<CAPACITY> {
    /// Creates a container with every entry set to [`BlockStateId::AIR`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            repr: Repr::Single(BlockStateId::AIR),
            non_air_count: 0,
        }
    }

    /// Creates a container with every entry set to `state`.
    #[must_use]
    pub fn filled(state: BlockStateId) -> Self {
        Self {
            repr: Repr::Single(state),
            non_air_count: if state.is_air() { 0 } else { CAPACITY },
        }
    }

    /// Returns which internal representation is currently in use.
    #[must_use]
    pub const fn kind(&self) -> ContainerKind {
        match self.repr {
            Repr::Single(_) => ContainerKind::Single,
            Repr::Indirect { .. } => ContainerKind::Indirect,
            Repr::Direct(_) => ContainerKind::Direct,
        }
    }

    /// Returns the palette length: `Some(1)` for the single-value fast path,
    /// `Some(n)` for an indirect palette of `n` ids, and `None` for direct
    /// storage (which has no palette).
    #[must_use]
    pub fn palette_len(&self) -> Option<usize> {
        match &self.repr {
            Repr::Single(_) => Some(1),
            Repr::Indirect { palette, .. } => Some(palette.len()),
            Repr::Direct(_) => None,
        }
    }

    /// Returns the current bits-per-entry: `0` for the single-value fast path,
    /// the palette-index width for indirect storage, and the direct width for
    /// direct storage.
    #[must_use]
    pub fn bits_per_entry(&self) -> u8 {
        match &self.repr {
            Repr::Single(_) => 0,
            Repr::Indirect { indices, .. } => indices.bits_per_entry(),
            Repr::Direct(data) => data.bits_per_entry(),
        }
    }

    /// Returns the number of entries whose id is not [`BlockStateId::AIR`].
    #[must_use]
    pub const fn non_air_count(&self) -> usize {
        self.non_air_count
    }

    /// Returns the number of entries whose id is [`BlockStateId::AIR`].
    #[must_use]
    pub const fn air_count(&self) -> usize {
        CAPACITY.saturating_sub(self.non_air_count)
    }

    /// Returns the id at `index`, or `None` if `index >= CAPACITY`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<BlockStateId> {
        if index >= CAPACITY {
            return None;
        }
        Some(self.value_at(index))
    }

    /// Sets the id at `index`.
    ///
    /// Returns [`WorldError::IndexOutOfRange`] if `index >= CAPACITY`. Promotes
    /// the internal representation as needed so any [`BlockStateId`] can be
    /// stored.
    pub fn set(&mut self, index: usize, state: BlockStateId) -> Result<(), WorldError> {
        if index >= CAPACITY {
            return Err(WorldError::IndexOutOfRange {
                index,
                capacity: CAPACITY,
            });
        }
        let old = self.value_at(index);
        if old == state {
            return Ok(());
        }

        // Fast paths that mutate the existing representation in place. Returns
        // `true` when the write is done, `false` when a promotion is required.
        let handled = match &mut self.repr {
            Repr::Single(_) => false,
            Repr::Direct(data) => {
                data.set(index, u64::from(state.as_u32()))?;
                true
            }
            Repr::Indirect { palette, indices } => {
                if let Some(palette_index) = palette.iter().position(|&p| p == state) {
                    indices.set(index, palette_index as u64)?;
                    true
                } else {
                    let needed = index_bits_for_len(palette.len() + 1);
                    if needed <= u32::from(MAX_INDIRECT_BITS) {
                        let target_bits = needed.max(u32::from(MIN_INDIRECT_BITS)) as u8;
                        if target_bits > indices.bits_per_entry() {
                            *indices = indices.resized(target_bits)?;
                        }
                        let palette_index = palette.len() as u64;
                        palette.push(state);
                        indices.set(index, palette_index)?;
                        true
                    } else {
                        // Palette would exceed the indirect ceiling: promote.
                        false
                    }
                }
            }
        };

        if !handled {
            // Representation transitions. The replacement is built fully before
            // assigning so no borrow of `self.repr` is held across the write.
            let new_repr = match &self.repr {
                Repr::Single(_) => {
                    let mut indices = PackedArray::new(MIN_INDIRECT_BITS, CAPACITY)?;
                    // Index 0 maps to `old` (the former single value); the new
                    // entry takes palette slot 1.
                    indices.set(index, 1)?;
                    Some(Repr::Indirect {
                        palette: vec![old, state],
                        indices,
                    })
                }
                Repr::Indirect { palette, indices } => {
                    let mut data = PackedArray::new(DIRECT_BITS, CAPACITY)?;
                    for i in 0..CAPACITY {
                        let palette_index = indices.get(i).unwrap_or(0) as usize;
                        let raw = palette.get(palette_index).copied().unwrap_or_default();
                        data.set(i, u64::from(raw.as_u32()))?;
                    }
                    data.set(index, u64::from(state.as_u32()))?;
                    Some(Repr::Direct(data))
                }
                Repr::Direct(_) => None,
            };
            if let Some(repr) = new_repr {
                self.repr = repr;
            }
        }

        self.adjust_count(old, state);
        Ok(())
    }

    /// Appends this container's chunk-section wire encoding to `out`.
    ///
    /// The layout matches the Minecraft 1.21.5+ `PalettedContainer`: a
    /// `bits_per_entry` byte, then the palette, then the packed long array — with
    /// **no length prefix** on the long array (its length is recomputed by the
    /// client from `bits_per_entry` and the fixed `CAPACITY`).
    ///
    /// - [`ContainerKind::Single`]: `bits_per_entry = 0`, one `VarInt` palette
    ///   value, and no long array.
    /// - [`ContainerKind::Indirect`]: `bits_per_entry` is the palette-index width,
    ///   followed by a `VarInt` palette length, that many `VarInt` ids, then the
    ///   non-spanning long array ([`PackedArray::words`]).
    /// - [`ContainerKind::Direct`]: `bits_per_entry = direct_wire_bits`, no
    ///   palette, then the long array repacked to `direct_wire_bits`.
    ///
    /// `direct_wire_bits` is the bits-per-entry the direct representation uses on
    /// the wire (15 for block states in 1.21.8). It is ignored for the single and
    /// indirect forms, which a flat world is the only thing that emits.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::ValueTooWide`] if the container is direct and holds
    /// an id that does not fit in `direct_wire_bits` bits. Real block-state ids
    /// fit in 15 bits, so this cannot happen for genuine chunk data.
    pub fn encode_network(
        &self,
        out: &mut Vec<u8>,
        direct_wire_bits: u8,
    ) -> Result<(), WorldError> {
        match &self.repr {
            Repr::Single(state) => {
                out.push(0);
                write_var_u32(out, state.as_u32());
            }
            Repr::Indirect { palette, indices } => {
                out.push(indices.bits_per_entry());
                // Palette length and ids are small non-negative values.
                write_var_u32(out, u32::try_from(palette.len()).unwrap_or(u32::MAX));
                for id in palette {
                    write_var_u32(out, id.as_u32());
                }
                write_packed_longs(out, indices.words());
            }
            Repr::Direct(data) => {
                out.push(direct_wire_bits);
                // The in-memory layout is 32-bit for lossless storage; the wire
                // uses the narrower vanilla width, so repack before emitting.
                let wire = data.resized(direct_wire_bits)?;
                write_packed_longs(out, wire.words());
            }
        }
        Ok(())
    }

    /// Returns the id at `index`, assuming `index < CAPACITY`. The bounds-
    /// checked callers ([`Self::get`], [`Self::set`]) guarantee this; the
    /// fallbacks below keep it panic-free even if that ever fails to hold.
    fn value_at(&self, index: usize) -> BlockStateId {
        match &self.repr {
            Repr::Single(state) => *state,
            Repr::Indirect { palette, indices } => {
                let palette_index = indices.get(index).unwrap_or(0) as usize;
                palette.get(palette_index).copied().unwrap_or_default()
            }
            Repr::Direct(data) => BlockStateId::new(data.get(index).unwrap_or(0) as u32),
        }
    }

    /// Updates the running non-air count for a single entry changing from `old`
    /// to `new`.
    fn adjust_count(&mut self, old: BlockStateId, new: BlockStateId) {
        match (old.is_air(), new.is_air()) {
            (true, false) => self.non_air_count += 1,
            (false, true) => self.non_air_count = self.non_air_count.saturating_sub(1),
            _ => {}
        }
    }
}

impl<const CAPACITY: usize> Default for PalettedContainer<CAPACITY> {
    /// A default container is entirely [`BlockStateId::AIR`].
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{ContainerKind, PalettedContainer, MAX_INDIRECT_BITS};
    use crate::block_state::BlockStateId;
    use crate::error::WorldError;

    const CAP: usize = 4096;
    type Container = PalettedContainer<CAP>;

    #[test]
    fn default_is_all_air() {
        let c = Container::default();
        assert_eq!(c.kind(), ContainerKind::Single);
        assert_eq!(c.non_air_count(), 0);
        assert_eq!(c.air_count(), CAP);
        assert_eq!(c.palette_len(), Some(1));
        assert_eq!(c.bits_per_entry(), 0);
        for i in 0..CAP {
            assert_eq!(c.get(i), Some(BlockStateId::AIR));
        }
    }

    #[test]
    fn filled_non_air_counts_full() {
        let c = Container::filled(BlockStateId::new(1));
        assert_eq!(c.non_air_count(), CAP);
        assert_eq!(c.air_count(), 0);
        assert_eq!(c.get(0), Some(BlockStateId::new(1)));
    }

    #[test]
    fn single_to_indirect_promotion() {
        let mut c = Container::new();
        c.set(0, BlockStateId::new(1)).unwrap();
        assert_eq!(c.kind(), ContainerKind::Indirect);
        // Palette holds the former air plus the new state.
        assert_eq!(c.palette_len(), Some(2));
        assert_eq!(c.bits_per_entry(), 4);
        assert_eq!(c.get(0), Some(BlockStateId::new(1)));
        assert_eq!(c.get(1), Some(BlockStateId::AIR));
        assert_eq!(c.non_air_count(), 1);
    }

    #[test]
    fn setting_same_value_is_a_no_op() {
        let mut c = Container::new();
        c.set(0, BlockStateId::AIR).unwrap();
        assert_eq!(c.kind(), ContainerKind::Single);
        assert_eq!(c.non_air_count(), 0);

        let mut c = Container::filled(BlockStateId::new(5));
        c.set(7, BlockStateId::new(5)).unwrap();
        assert_eq!(c.kind(), ContainerKind::Single);
        assert_eq!(c.non_air_count(), CAP);
    }

    #[test]
    fn bits_per_entry_grows_at_palette_boundaries() {
        let mut c = Container::new();
        // Add distinct non-air states. Palette = air + N distinct.
        // 15 distinct -> palette 16 -> 4 bits.
        for i in 1..=15u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.palette_len(), Some(16));
        assert_eq!(c.bits_per_entry(), 4);
        // 16 distinct -> palette 17 -> 5 bits.
        c.set(16, BlockStateId::new(16)).unwrap();
        assert_eq!(c.palette_len(), Some(17));
        assert_eq!(c.bits_per_entry(), 5);
        assert_eq!(c.kind(), ContainerKind::Indirect);
    }

    #[test]
    fn promotes_to_direct_past_the_indirect_ceiling() {
        let mut c = Container::new();
        // 256 distinct non-air ids -> palette would be air + 256 = 257 entries,
        // needing 9 index bits, which exceeds the 8-bit indirect ceiling.
        for i in 1..=256u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.kind(), ContainerKind::Direct);
        assert_eq!(c.palette_len(), None);
        assert_eq!(c.bits_per_entry(), 32);
        assert_eq!(c.non_air_count(), 256);
        // Every stored value survives the transition.
        for i in 1..=256u32 {
            assert_eq!(c.get(i as usize), Some(BlockStateId::new(i)));
        }
        // Untouched entries are still air.
        assert_eq!(c.get(0), Some(BlockStateId::AIR));
        assert_eq!(c.get(257), Some(BlockStateId::AIR));
    }

    #[test]
    fn direct_storage_accepts_high_ids() {
        // Drive the container to direct, then store an id far beyond any palette.
        let mut c = Container::new();
        for i in 1..=256u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.kind(), ContainerKind::Direct);
        let high = BlockStateId::new(1_000_000);
        c.set(500, high).unwrap();
        assert_eq!(c.get(500), Some(high));
    }

    #[test]
    fn round_trip_across_representations() {
        let mut c = Container::new();
        let probe = 1234usize;
        // Single -> Indirect.
        c.set(probe, BlockStateId::new(7)).unwrap();
        assert_eq!(c.get(probe), Some(BlockStateId::new(7)));
        // Grow within indirect.
        for i in 1..=20u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.kind(), ContainerKind::Indirect);
        assert_eq!(c.get(probe), Some(BlockStateId::new(7)));
        // Overwrite the probe with an existing palette entry, then a new one.
        c.set(probe, BlockStateId::new(3)).unwrap();
        assert_eq!(c.get(probe), Some(BlockStateId::new(3)));
        // Push into direct and confirm the probe still reads back.
        for i in 1..=256u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.kind(), ContainerKind::Direct);
        assert_eq!(c.get(probe), Some(BlockStateId::new(3)));
    }

    #[test]
    fn air_counting_tracks_transitions() {
        let mut c = Container::new();
        assert_eq!(c.non_air_count(), 0);
        // Place a non-air block.
        c.set(0, BlockStateId::new(1)).unwrap();
        assert_eq!(c.non_air_count(), 1);
        // Replace it with a different non-air block: count unchanged.
        c.set(0, BlockStateId::new(2)).unwrap();
        assert_eq!(c.non_air_count(), 1);
        // Clear it back to air: count drops.
        c.set(0, BlockStateId::AIR).unwrap();
        assert_eq!(c.non_air_count(), 0);
        assert_eq!(c.air_count(), CAP);
    }

    #[test]
    fn air_counting_survives_direct_promotion() {
        let mut c = Container::new();
        for i in 1..=256u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.kind(), ContainerKind::Direct);
        assert_eq!(c.non_air_count(), 256);
        // Clearing entries decrements through the direct representation too.
        c.set(1, BlockStateId::AIR).unwrap();
        assert_eq!(c.non_air_count(), 255);
        assert_eq!(c.air_count(), CAP - 255);
    }

    #[test]
    fn out_of_range_index_is_handled() {
        let mut c = Container::new();
        assert_eq!(c.get(CAP), None);
        assert_eq!(c.get(CAP + 100), None);
        assert!(matches!(
            c.set(CAP, BlockStateId::new(1)),
            Err(WorldError::IndexOutOfRange {
                index,
                capacity
            }) if index == CAP && capacity == CAP
        ));
    }

    /// Bits-per-entry the direct representation uses on the wire for block
    /// states in 1.21.8 (`ceil(log2(global block-state count))`).
    const DIRECT_WIRE_BITS: u8 = 15;

    #[test]
    fn encode_single_air_is_bpe_zero_one_varint_no_longs() {
        let c = Container::new();
        let mut out = Vec::new();
        c.encode_network(&mut out, DIRECT_WIRE_BITS).unwrap();
        // bits_per_entry = 0, then the single palette value (air = 0), no longs.
        assert_eq!(out, vec![0x00, 0x00]);
    }

    #[test]
    fn encode_single_non_air_value() {
        let c = Container::filled(BlockStateId::new(5));
        let mut out = Vec::new();
        c.encode_network(&mut out, DIRECT_WIRE_BITS).unwrap();
        assert_eq!(out, vec![0x00, 0x05]);
    }

    #[test]
    fn encode_indirect_layout_and_long_count() {
        // One stone block over an otherwise-air section: palette [air, stone],
        // clamped to 4 bits per entry.
        let mut c = Container::new();
        c.set(0, BlockStateId::new(1)).unwrap();
        assert_eq!(c.kind(), ContainerKind::Indirect);
        assert_eq!(c.bits_per_entry(), 4);

        let mut out = Vec::new();
        c.encode_network(&mut out, DIRECT_WIRE_BITS).unwrap();

        // Header: bpe = 4, palette length = 2, ids [air=0, stone=1].
        assert_eq!(out[0], 4);
        assert_eq!(out[1], 2);
        assert_eq!(out[2], 0);
        assert_eq!(out[3], 1);

        // 4 bits/entry -> 16 entries per long -> ceil(4096 / 16) = 256 longs.
        let long_bytes = out.len() - 4;
        assert_eq!(long_bytes % 8, 0);
        assert_eq!(long_bytes / 8, 256);

        // Block index 0 maps to palette index 1 (stone) in the low nibble of the
        // first long; every other entry is palette index 0.
        assert_eq!(&out[4..12], &[0, 0, 0, 0, 0, 0, 0, 1]);
        assert!(out[12..].iter().all(|&b| b == 0));
    }

    #[test]
    fn encode_direct_uses_wire_bits_and_no_palette() {
        // Drive the container to direct (more than 256 distinct ids).
        let mut c = Container::new();
        for i in 1..=256u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.kind(), ContainerKind::Direct);

        let mut out = Vec::new();
        c.encode_network(&mut out, DIRECT_WIRE_BITS).unwrap();

        // bits_per_entry is the *wire* width (15), not the internal 32.
        assert_eq!(out[0], DIRECT_WIRE_BITS);
        // No palette is written for direct; 15 bits -> 4 entries per long ->
        // ceil(4096 / 4) = 1024 longs, and nothing else.
        let long_bytes = out.len() - 1;
        assert_eq!(long_bytes, 1024 * 8);
    }

    #[test]
    fn encode_direct_rejects_ids_wider_than_wire_bits() {
        let mut c = Container::new();
        for i in 1..=256u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.kind(), ContainerKind::Direct);
        // An id beyond 15 bits cannot be repacked to the block wire width.
        c.set(500, BlockStateId::new(40_000)).unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            c.encode_network(&mut out, DIRECT_WIRE_BITS),
            Err(WorldError::ValueTooWide { .. })
        ));
    }

    #[test]
    fn write_var_u32_known_encodings() {
        use super::write_var_u32;
        let cases: &[(u32, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (300, &[0xAC, 0x02]),
            (25_565, &[0xDD, 0xC7, 0x01]),
        ];
        for &(value, expected) in cases {
            let mut out = Vec::new();
            write_var_u32(&mut out, value);
            assert_eq!(out, expected, "value={value}");
        }
    }

    #[test]
    fn indirect_ceiling_constant_is_respected() {
        // Guard the promotion threshold: an 8-bit palette is the largest
        // indirect palette (256 entries) before direct storage takes over.
        assert_eq!(MAX_INDIRECT_BITS, 8);
        let mut c = Container::new();
        // air + 255 distinct = 256 palette entries -> still indirect at 8 bits.
        for i in 1..=255u32 {
            c.set(i as usize, BlockStateId::new(i)).unwrap();
        }
        assert_eq!(c.kind(), ContainerKind::Indirect);
        assert_eq!(c.palette_len(), Some(256));
        assert_eq!(c.bits_per_entry(), 8);
    }
}
