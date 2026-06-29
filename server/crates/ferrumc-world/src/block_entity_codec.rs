//! Compact byte (de)serialization for [`BlockEntity`]s, for the storage layer.
//!
//! The chunk-overlay persistence layer (`ferrumc-storage`) stores each block
//! entity as an opaque, length-bounded payload alongside its [`BlockPos`]; this
//! module is the single place that turns a [`BlockEntity`] into those bytes and
//! back. The world crate owns the format because it owns the block-entity model:
//! storage stays ignorant of sign/chest internals and only bounds the blob, while
//! the *network* NBT a client expects is a separate concern owned by the session
//! layer (see [`crate::block_entity`]).
//!
//! # Layout (all integers big-endian)
//!
//! ```text
//! payload := tag(u8) body
//! tag 0 (Sign):  sign_kind(u8) is_waxed(u8) face(front) face(back)
//! tag 1 (Chest): stack × CHEST_SLOTS          // a fixed count, never file-declared
//!
//! face  := string(color) has_glowing_text(u8) string(line) × SIGN_LINES
//! string:= len(u16) utf8[len]                 // len capped per field on decode
//! stack := present(u8=0 empty | 1 present) [ item_id(i32) count(u8) if present ]
//! ```
//!
//! Counts that drive an allocation are *fixed by the model* (`CHEST_SLOTS`,
//! `SIGN_LINES`), never read from the blob, and every variable-length string is
//! capped before it is read, so a corrupt or hostile payload can never make the
//! decoder allocate without bound. Decode is total: any malformed byte yields a
//! classified [`BlockEntityCodecError`] rather than a panic, so the caller can
//! skip one bad block entity and still load the chunk.
//!
//! ## Deferred: data components
//!
//! A present chest slot persists only its item id and count — the same trusted
//! `(id, count)` pair the player-inventory persistence keeps — and **drops** any
//! data components (custom name, damage, custom NBT, ...). Component persistence
//! is a follow-up; until then a named or damaged item in a chest reloads as the
//! plain item with its count conserved.

use std::num::NonZeroU8;

use ferrumc_items::{ComponentPatch, ItemId, ItemStack};

use crate::block_entity::{
    BlockEntity, ChestInventory, Sign, SignFace, SignKind, CHEST_SLOTS, SIGN_LINES,
};

/// Maximum accepted length, in bytes, of a single serialized [`BlockEntity`]
/// payload.
///
/// The storage layer bounds every persisted block-entity blob by this constant so
/// one record cannot consume unbounded memory. It comfortably exceeds the largest
/// well-formed payload — a sign is two faces of a short color plus four
/// [`MAX_SIGN_LINE_BYTES`]-capped lines (~8 KiB), and a chest is
/// [`CHEST_SLOTS`] fixed-size slots (~0.2 KiB).
pub const MAX_BLOCK_ENTITY_PAYLOAD_LEN: usize = 16 * 1024;

/// Maximum accepted length, in bytes, of one sign text line on decode.
///
/// Vanilla sign lines are short; this generous cap bounds a corrupt/hostile blob
/// without rejecting any real input. Encoding never produces a longer line.
pub const MAX_SIGN_LINE_BYTES: usize = 1024;

/// Maximum accepted length, in bytes, of a sign face's dye-color string on decode.
pub const MAX_SIGN_COLOR_BYTES: usize = 64;

/// Payload tag for a [`BlockEntity::Sign`].
const TAG_SIGN: u8 = 0;
/// Payload tag for a [`BlockEntity::Chest`].
const TAG_CHEST: u8 = 1;

/// Sign-kind tag for [`SignKind::Sign`].
const SIGN_KIND_SIGN: u8 = 0;
/// Sign-kind tag for [`SignKind::Hanging`].
const SIGN_KIND_HANGING: u8 = 1;

/// Slot tag: the slot is empty ([`ItemStack::empty`]); no item bytes follow.
const STACK_EMPTY: u8 = 0;
/// Slot tag: a present stack follows as `item_id(i32) ++ count(u8)`.
const STACK_PRESENT: u8 = 1;

/// A failure decoding a persisted [`BlockEntity`] payload.
///
/// Each variant *classifies* the corruption so the caller can log it and skip the
/// one offending block entity rather than failing the whole chunk load. The enum
/// is `#[non_exhaustive]`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BlockEntityCodecError {
    /// The payload ended before a field could be read in full.
    #[error("block-entity payload truncated")]
    Truncated,

    /// The leading payload tag named no known block-entity type.
    #[error("unknown block-entity tag {0}")]
    UnknownTag(u8),

    /// The sign-kind tag named no known [`SignKind`].
    #[error("unknown sign-kind tag {0}")]
    UnknownSignKind(u8),

    /// A chest slot's leading tag was neither empty nor present.
    #[error("unknown chest-slot tag {0}")]
    BadStackTag(u8),

    /// A length-prefixed string declared more bytes than its field allows.
    #[error("string length {len} exceeds maximum {max}")]
    StringTooLong {
        /// The declared length.
        len: usize,
        /// The field's cap.
        max: usize,
    },

    /// A string's bytes were not valid UTF-8.
    #[error("block-entity string is not valid UTF-8")]
    InvalidUtf8,

    /// A present chest slot named an item id absent from the registry.
    #[error("unknown item id {0} in persisted chest slot")]
    UnknownItem(i32),

    /// A present chest slot declared a zero count (a present stack is `>= 1`).
    #[error("present chest slot has a zero count")]
    ZeroCount,

    /// Bytes remained after a complete payload was decoded.
    #[error("trailing bytes after block-entity payload")]
    TrailingBytes,
}

/// Appends the serialized form of `entity` to `out`.
///
/// The encoding is exhaustive over the (crate-private-to-extend) [`BlockEntity`]
/// variants: adding a new variant is a compile error here until it is given a
/// payload, so a block-entity type can never be silently dropped from
/// persistence. Output for any real block entity is bounded well under
/// [`MAX_BLOCK_ENTITY_PAYLOAD_LEN`].
pub fn encode_block_entity(entity: &BlockEntity, out: &mut Vec<u8>) {
    match entity {
        BlockEntity::Sign(sign) => {
            out.push(TAG_SIGN);
            out.push(sign_kind_tag(sign.kind()));
            out.push(u8::from(sign.is_waxed()));
            encode_face(sign.front(), out);
            encode_face(sign.back(), out);
        }
        BlockEntity::Chest(chest) => {
            out.push(TAG_CHEST);
            // A fixed CHEST_SLOTS slots, so the decoder never reads a count.
            for slot in chest.slots() {
                encode_stack(slot, out);
            }
        }
    }
}

/// Decodes a complete [`BlockEntity`] payload, rejecting trailing bytes.
///
/// # Errors
///
/// Returns a [`BlockEntityCodecError`] for any malformed input (truncation,
/// unknown tag, oversized or non-UTF-8 string, unknown item, zero count, or
/// trailing bytes). Decoding never panics, so the caller can skip one corrupt
/// block entity and still reconstruct the rest of the chunk.
pub fn decode_block_entity(bytes: &[u8]) -> Result<BlockEntity, BlockEntityCodecError> {
    let mut reader = PayloadReader::new(bytes);
    let entity = match reader.read_u8()? {
        TAG_SIGN => {
            let kind = sign_kind_from_tag(reader.read_u8()?)?;
            let is_waxed = reader.read_u8()? != 0;
            let front = decode_face(&mut reader)?;
            let back = decode_face(&mut reader)?;
            BlockEntity::Sign(Sign::from_parts(kind, is_waxed, front, back))
        }
        TAG_CHEST => {
            let mut chest = ChestInventory::new();
            for index in 0..CHEST_SLOTS {
                let stack = decode_stack(&mut reader)?;
                // `index < CHEST_SLOTS`, so the slot is always in range; the guard
                // keeps the decoder panic-free regardless.
                if let Some(slot) = chest.slot_mut(index) {
                    *slot = stack;
                }
            }
            BlockEntity::Chest(chest)
        }
        other => return Err(BlockEntityCodecError::UnknownTag(other)),
    };
    if reader.remaining() != 0 {
        return Err(BlockEntityCodecError::TrailingBytes);
    }
    Ok(entity)
}

/// Maps a [`SignKind`] to its wire tag.
fn sign_kind_tag(kind: SignKind) -> u8 {
    match kind {
        SignKind::Sign => SIGN_KIND_SIGN,
        SignKind::Hanging => SIGN_KIND_HANGING,
    }
}

/// Recovers a [`SignKind`] from its wire tag.
fn sign_kind_from_tag(tag: u8) -> Result<SignKind, BlockEntityCodecError> {
    match tag {
        SIGN_KIND_SIGN => Ok(SignKind::Sign),
        SIGN_KIND_HANGING => Ok(SignKind::Hanging),
        other => Err(BlockEntityCodecError::UnknownSignKind(other)),
    }
}

/// Appends one sign face: color string, glow flag, then its four lines.
fn encode_face(face: &SignFace, out: &mut Vec<u8>) {
    encode_str(face.color(), out);
    out.push(u8::from(face.has_glowing_text()));
    for line in face.lines() {
        encode_str(line, out);
    }
}

/// Decodes one sign face (the inverse of [`encode_face`]).
fn decode_face(reader: &mut PayloadReader) -> Result<SignFace, BlockEntityCodecError> {
    let color = decode_str(reader, MAX_SIGN_COLOR_BYTES)?;
    let has_glowing_text = reader.read_u8()? != 0;
    let mut lines: [String; SIGN_LINES] = std::array::from_fn(|_| String::new());
    for line in &mut lines {
        *line = decode_str(reader, MAX_SIGN_LINE_BYTES)?;
    }
    Ok(SignFace::from_parts(lines, color, has_glowing_text))
}

/// Appends a `len(u16) ++ utf8` string. Encoding never exceeds `u16::MAX` bytes
/// for any real input (sign lines/colors are short); a pathological longer value
/// is truncated rather than corrupting the stream.
fn encode_str(text: &str, out: &mut Vec<u8>) {
    let bytes = text.as_bytes();
    let len = bytes.len().min(usize::from(u16::MAX));
    // `len <= u16::MAX`, so the cast is lossless.
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&bytes[..len]);
}

/// Decodes a `len(u16) ++ utf8` string, rejecting a length above `max` (before
/// any allocation) or non-UTF-8 bytes.
fn decode_str(reader: &mut PayloadReader, max: usize) -> Result<String, BlockEntityCodecError> {
    let len = usize::from(reader.read_u16()?);
    if len > max {
        return Err(BlockEntityCodecError::StringTooLong { len, max });
    }
    let bytes = reader.take(len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| BlockEntityCodecError::InvalidUtf8)
}

/// Appends one chest slot. An empty slot is a single tag byte; a present slot adds
/// `item_id(i32) ++ count(u8)`. Data components are deferred (see the module docs).
fn encode_stack(stack: &ItemStack, out: &mut Vec<u8>) {
    match stack.item() {
        Some(item) => {
            out.push(STACK_PRESENT);
            out.extend_from_slice(&item.id().to_be_bytes());
            out.push(stack.count());
        }
        None => out.push(STACK_EMPTY),
    }
}

/// Decodes one chest slot (the inverse of [`encode_stack`]).
///
/// The `(item_id, count)` round-trips exactly, so a chest's contents are conserved
/// with no duplication or loss. An unknown item id or a zero count on a present
/// slot is rejected so a corrupt blob cannot fabricate an invalid stack.
fn decode_stack(reader: &mut PayloadReader) -> Result<ItemStack, BlockEntityCodecError> {
    match reader.read_u8()? {
        STACK_EMPTY => Ok(ItemStack::empty()),
        STACK_PRESENT => {
            let raw = reader.read_i32()?;
            let item = ItemId::new(raw).ok_or(BlockEntityCodecError::UnknownItem(raw))?;
            let count =
                NonZeroU8::new(reader.read_u8()?).ok_or(BlockEntityCodecError::ZeroCount)?;
            Ok(ItemStack::new(item, count, ComponentPatch::empty()))
        }
        other => Err(BlockEntityCodecError::BadStackTag(other)),
    }
}

/// A panic-free, length-checked cursor over a block-entity payload.
///
/// Every read validates that enough bytes remain; an underrun returns
/// [`BlockEntityCodecError::Truncated`] rather than panicking.
struct PayloadReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PayloadReader<'a> {
    /// Wraps `data` at offset zero.
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Returns the next `n` bytes, advancing the cursor, or
    /// [`BlockEntityCodecError::Truncated`] if fewer than `n` remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8], BlockEntityCodecError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(BlockEntityCodecError::Truncated)?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or(BlockEntityCodecError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// Reads one byte.
    fn read_u8(&mut self) -> Result<u8, BlockEntityCodecError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a big-endian `u16`.
    fn read_u16(&mut self) -> Result<u16, BlockEntityCodecError> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| BlockEntityCodecError::Truncated)?;
        Ok(u16::from_be_bytes(bytes))
    }

    /// Reads a big-endian `i32`.
    fn read_i32(&mut self) -> Result<i32, BlockEntityCodecError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| BlockEntityCodecError::Truncated)?;
        Ok(i32::from_be_bytes(bytes))
    }

    /// Returns the number of unread bytes remaining.
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips `entity` through encode/decode and asserts equality.
    fn round_trip(entity: &BlockEntity) {
        let mut bytes = Vec::new();
        encode_block_entity(entity, &mut bytes);
        assert!(
            bytes.len() <= MAX_BLOCK_ENTITY_PAYLOAD_LEN,
            "payload must stay within the storage bound",
        );
        let decoded = decode_block_entity(&bytes).expect("decode");
        assert_eq!(&decoded, entity);
    }

    fn stack(id: i32, count: u8) -> ItemStack {
        ItemStack::new(
            ItemId::new(id).expect("known item"),
            NonZeroU8::new(count).expect("non-zero"),
            ComponentPatch::empty(),
        )
    }

    #[test]
    fn blank_sign_round_trips() {
        round_trip(&BlockEntity::Sign(Sign::new(SignKind::Sign)));
        round_trip(&BlockEntity::Sign(Sign::new(SignKind::Hanging)));
    }

    #[test]
    fn sign_with_text_on_both_faces_round_trips() {
        let mut sign = Sign::new(SignKind::Sign);
        sign.set_face_lines(
            true,
            [
                "front 1".to_owned(),
                "front 2".to_owned(),
                String::new(),
                "front 4".to_owned(),
            ],
        );
        sign.set_face_lines(
            false,
            [
                "back A".to_owned(),
                String::new(),
                "back C".to_owned(),
                "back D".to_owned(),
            ],
        );
        round_trip(&BlockEntity::Sign(sign));
    }

    #[test]
    fn sign_preserves_kind_waxed_color_and_glow() {
        // Build a fully-styled hanging, waxed sign via the persisted-parts seam so
        // the round-trip proves color/glow/waxed survive, not just line text.
        let front = SignFace::from_parts(
            [
                "hello".to_owned(),
                "world".to_owned(),
                String::new(),
                "!".to_owned(),
            ],
            "red".to_owned(),
            true,
        );
        let back = SignFace::from_parts(
            std::array::from_fn(|_| String::new()),
            "lime".to_owned(),
            false,
        );
        let sign = Sign::from_parts(SignKind::Hanging, true, front, back);
        let mut bytes = Vec::new();
        encode_block_entity(&BlockEntity::Sign(sign.clone()), &mut bytes);
        let BlockEntity::Sign(decoded) = decode_block_entity(&bytes).expect("decode") else {
            panic!("decoded a non-sign");
        };
        assert_eq!(decoded.kind(), SignKind::Hanging);
        assert!(decoded.is_waxed());
        assert_eq!(decoded.front().color(), "red");
        assert!(decoded.front().has_glowing_text());
        assert_eq!(decoded.front().lines()[0], "hello");
        assert_eq!(decoded.back().color(), "lime");
        assert!(!decoded.back().has_glowing_text());
        assert_eq!(&BlockEntity::Sign(decoded), &BlockEntity::Sign(sign));
    }

    #[test]
    fn empty_chest_round_trips() {
        round_trip(&BlockEntity::Chest(ChestInventory::new()));
    }

    #[test]
    fn chest_with_items_round_trips_and_conserves_counts() {
        let mut chest = ChestInventory::new();
        *chest.slot_mut(0).expect("slot 0") = stack(1, 64); // stone, full stack
        *chest.slot_mut(13).expect("slot 13") = stack(1, 1); // single stone
        *chest.slot_mut(CHEST_SLOTS - 1).expect("last slot") = stack(195, 7); // glass

        let total_before: u32 = chest.slots().iter().map(|s| u32::from(s.count())).sum();

        let mut bytes = Vec::new();
        encode_block_entity(&BlockEntity::Chest(chest.clone()), &mut bytes);
        let BlockEntity::Chest(decoded) = decode_block_entity(&bytes).expect("decode") else {
            panic!("decoded a non-chest");
        };
        let total_after: u32 = decoded.slots().iter().map(|s| u32::from(s.count())).sum();
        assert_eq!(total_before, total_after, "counts must be conserved");
        assert_eq!(decoded.slot(0), Some(&stack(1, 64)));
        assert_eq!(decoded.slot(13), Some(&stack(1, 1)));
        assert_eq!(decoded.slot(CHEST_SLOTS - 1), Some(&stack(195, 7)));
        assert_eq!(&BlockEntity::Chest(decoded), &BlockEntity::Chest(chest));
    }

    #[test]
    fn chest_drops_data_components_but_keeps_id_and_count() {
        use ferrumc_items::ComponentValue;
        let mut chest = ChestInventory::new();
        let named = ItemStack::new(
            ItemId::from_name("diamond_sword").expect("known"),
            NonZeroU8::new(1).expect("non-zero"),
            ComponentPatch::new(vec![ComponentValue::Damage(5)], vec![]),
        );
        *chest.slot_mut(2).expect("slot 2") = named;

        let mut bytes = Vec::new();
        encode_block_entity(&BlockEntity::Chest(chest), &mut bytes);
        let BlockEntity::Chest(decoded) = decode_block_entity(&bytes).expect("decode") else {
            panic!("decoded a non-chest");
        };
        let slot = decoded.slot(2).expect("slot 2");
        assert_eq!(
            slot.item().map(ItemId::id),
            ItemId::from_name("diamond_sword").map(ItemId::id)
        );
        assert_eq!(slot.count(), 1);
        // Components were deferred, so the reloaded stack carries an empty patch.
        assert!(slot.components().is_empty());
    }

    #[test]
    fn decode_rejects_empty_input() {
        assert_eq!(
            decode_block_entity(&[]),
            Err(BlockEntityCodecError::Truncated)
        );
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        assert_eq!(
            decode_block_entity(&[200]),
            Err(BlockEntityCodecError::UnknownTag(200))
        );
    }

    #[test]
    fn decode_rejects_unknown_sign_kind() {
        // tag=Sign, sign_kind=99
        assert_eq!(
            decode_block_entity(&[TAG_SIGN, 99]),
            Err(BlockEntityCodecError::UnknownSignKind(99))
        );
    }

    #[test]
    fn decode_rejects_truncated_sign() {
        // tag=Sign, kind=Sign, waxed=0, then truncated mid-face.
        assert_eq!(
            decode_block_entity(&[TAG_SIGN, SIGN_KIND_SIGN, 0]),
            Err(BlockEntityCodecError::Truncated)
        );
    }

    #[test]
    fn decode_rejects_oversized_string() {
        // tag=Sign, kind=Sign, waxed=0, color length declares > MAX_SIGN_COLOR_BYTES.
        let mut bytes = vec![TAG_SIGN, SIGN_KIND_SIGN, 0];
        bytes.extend_from_slice(&((MAX_SIGN_COLOR_BYTES as u16) + 1).to_be_bytes());
        assert!(matches!(
            decode_block_entity(&bytes),
            Err(BlockEntityCodecError::StringTooLong { .. })
        ));
    }

    #[test]
    fn decode_rejects_invalid_utf8_color() {
        // tag=Sign, kind=Sign, waxed=0, color len=1, then a lone continuation byte.
        let bytes = vec![TAG_SIGN, SIGN_KIND_SIGN, 0, 0, 1, 0xFF];
        assert_eq!(
            decode_block_entity(&bytes),
            Err(BlockEntityCodecError::InvalidUtf8)
        );
    }

    #[test]
    fn decode_rejects_unknown_item_in_chest() {
        // tag=Chest, slot 0 present with an item id that is not in the registry.
        let mut bytes = vec![TAG_CHEST, STACK_PRESENT];
        bytes.extend_from_slice(&(-1i32).to_be_bytes());
        bytes.push(1);
        assert_eq!(
            decode_block_entity(&bytes),
            Err(BlockEntityCodecError::UnknownItem(-1))
        );
    }

    #[test]
    fn decode_rejects_zero_count_present_slot() {
        let mut bytes = vec![TAG_CHEST, STACK_PRESENT];
        bytes.extend_from_slice(&1i32.to_be_bytes()); // stone
        bytes.push(0); // zero count
        assert_eq!(
            decode_block_entity(&bytes),
            Err(BlockEntityCodecError::ZeroCount)
        );
    }

    #[test]
    fn decode_rejects_bad_chest_slot_tag() {
        assert_eq!(
            decode_block_entity(&[TAG_CHEST, 7]),
            Err(BlockEntityCodecError::BadStackTag(7))
        );
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut bytes = Vec::new();
        encode_block_entity(&BlockEntity::Sign(Sign::new(SignKind::Sign)), &mut bytes);
        bytes.push(0xAB);
        assert_eq!(
            decode_block_entity(&bytes),
            Err(BlockEntityCodecError::TrailingBytes)
        );
    }
}
