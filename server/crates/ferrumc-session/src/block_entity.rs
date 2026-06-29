//! Clientbound builders for sign block entities.
//!
//! This is the one place that turns the simulation's [`Sign`] model into the wire
//! packets a 1.21.8 client expects: a [`BlockEntityData`] carrying the sign's NBT
//! (so the sign renders its text) and an [`OpenSignEditor`] (so a placer gets the
//! editing screen). It lives here, alongside the scoreboard/team NBT builders,
//! because this crate is the only one that depends on both the simulation model
//! (`ferrumc-sim`, which re-exports [`Sign`]) and the protocol/NBT crates.

use ferrumc_core::TextComponent;
use ferrumc_math::BlockPos;
use ferrumc_nbt::{NbtCompound, NbtTag};
use ferrumc_proto::generated::play::{
    BlockEntityData, ClientboundPlayPacket, OpenScreen, OpenSignEditor,
};
use ferrumc_proto::types::BlockPosition;
use ferrumc_sim::{Sign, SignFace};

use crate::text::text_component_to_nbt;

/// Converts a typed [`BlockPos`] into the wire [`BlockPosition`].
fn wire_pos(position: BlockPos) -> BlockPosition {
    BlockPosition::new(position.x(), position.y(), position.z())
}

/// Encodes one sign face into the `front_text`/`back_text` compound a 1.21.8
/// client expects: `has_glowing_text` (byte), `color` (string), and `messages`
/// (a four-element list of text components).
///
/// Each line is emitted as a bare NBT string, which is the shorthand text
/// component a client accepts for plain text; the list always has exactly four
/// entries (blank lines included) so the client reads the fixed-size face.
fn face_nbt(face: &SignFace) -> NbtTag {
    let mut compound = NbtCompound::new();
    compound.push(
        "has_glowing_text",
        NbtTag::Byte(i8::from(face.has_glowing_text())),
    );
    compound.push("color", NbtTag::String(face.color().to_owned()));
    let messages = face
        .lines()
        .iter()
        .map(|line| NbtTag::String(line.clone()))
        .collect();
    compound.push("messages", NbtTag::List(messages));
    NbtTag::Compound(compound)
}

/// Builds the network-NBT compound for a whole sign: `is_waxed` plus its two
/// faces.
fn sign_nbt(sign: &Sign) -> NbtTag {
    let mut root = NbtCompound::new();
    root.push("is_waxed", NbtTag::Byte(i8::from(sign.is_waxed())));
    root.push("front_text", face_nbt(sign.front()));
    root.push("back_text", face_nbt(sign.back()));
    NbtTag::Compound(root)
}

/// Builds the [`BlockEntityData`] packet that renders `sign` at `position`.
///
/// The block-entity-type id is taken from the sign's [`kind`](Sign::kind)
/// (`minecraft:sign` = 7, `minecraft:hanging_sign` = 8), and the payload is the
/// sign's NBT. The router broadcasts this to viewers when a sign's text changes,
/// and the app re-sends it when a player streams the sign's chunk into view.
#[must_use]
pub fn sign_block_entity_data(position: BlockPos, sign: &Sign) -> ClientboundPlayPacket {
    ClientboundPlayPacket::BlockEntityData(BlockEntityData::new(
        wire_pos(position),
        sign.kind().block_entity_type(),
        sign_nbt(sign),
    ))
}

/// Builds the [`OpenSignEditor`] packet that opens the front-face editing screen
/// for the sign at `position`.
///
/// Sent only to the player who just placed the sign; the client replies with a
/// serverbound `UpdateSign` when the player confirms their text.
#[must_use]
pub fn open_sign_editor(position: BlockPos) -> ClientboundPlayPacket {
    // The placer always edits the front face first, matching vanilla.
    ClientboundPlayPacket::OpenSignEditor(OpenSignEditor::new(wire_pos(position), true))
}

/// Builds the clientbound [`OpenScreen`] packet that opens a container GUI.
///
/// `window_id` is the server-assigned `ContainerID` the client echoes on every
/// subsequent Click Container / Close Container; `window_type` is the
/// `minecraft:menu` registry id (e.g. `2` = `generic_9x3`, a single chest);
/// `title` is the window title, encoded as a network-NBT text component the same
/// way [`system_chat`](crate::system_chat) encodes its content.
///
/// The app pairs this with a `SetContainerContent` carrying the window's slots so
/// the freshly opened screen renders the container's contents.
#[must_use]
pub fn open_screen(
    window_id: i32,
    window_type: i32,
    title: &TextComponent,
) -> ClientboundPlayPacket {
    ClientboundPlayPacket::OpenScreen(OpenScreen::new(
        window_id,
        window_type,
        text_component_to_nbt(title),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_nbt::{read_network_root, write_network_root, NbtLimits};
    use ferrumc_sim::SignKind;

    /// Reads the messages list of a face compound as owned strings.
    fn messages(face: &NbtTag) -> Vec<String> {
        let NbtTag::Compound(compound) = face else {
            panic!("face is not a compound");
        };
        let Some(NbtTag::List(items)) = compound.get("messages") else {
            panic!("face has no messages list");
        };
        items
            .iter()
            .map(|tag| match tag {
                NbtTag::String(s) => s.clone(),
                other => panic!("message is not a string: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn sign_block_entity_data_carries_position_type_and_text() {
        let mut sign = Sign::new(SignKind::Sign);
        sign.set_face_lines(
            true,
            [
                "line one".to_owned(),
                String::new(),
                "line three".to_owned(),
                String::new(),
            ],
        );
        let position = BlockPos::new(1, 64, -3);

        let ClientboundPlayPacket::BlockEntityData(packet) =
            sign_block_entity_data(position, &sign)
        else {
            panic!("expected a BlockEntityData");
        };
        let loc = packet.location();
        assert_eq!((loc.x(), loc.y(), loc.z()), (1, 64, -3));
        // A standing/wall sign is block-entity type 7.
        assert_eq!(packet.block_entity_type(), 7);

        // The NBT round-trips through the network encoder and carries the text.
        let bytes = write_network_root(packet.data(), &NbtLimits::default()).expect("encode nbt");
        let tag = read_network_root(&bytes, &NbtLimits::default()).expect("decode nbt");
        let NbtTag::Compound(root) = &tag else {
            panic!("root is not a compound");
        };
        assert_eq!(root.get("is_waxed"), Some(&NbtTag::Byte(0)));
        let front = root.get("front_text").expect("front_text present");
        assert_eq!(
            messages(front),
            vec![
                "line one".to_owned(),
                String::new(),
                "line three".to_owned(),
                String::new(),
            ]
        );
        // The back face is present and blank (four empty strings).
        let back = root.get("back_text").expect("back_text present");
        assert_eq!(messages(back), vec![String::new(); 4]);
    }

    #[test]
    fn hanging_sign_uses_block_entity_type_eight() {
        let sign = Sign::new(SignKind::Hanging);
        let ClientboundPlayPacket::BlockEntityData(packet) =
            sign_block_entity_data(BlockPos::new(0, 0, 0), &sign)
        else {
            panic!("expected a BlockEntityData");
        };
        assert_eq!(packet.block_entity_type(), 8);
    }

    #[test]
    fn open_sign_editor_targets_the_front_face() {
        let ClientboundPlayPacket::OpenSignEditor(packet) =
            open_sign_editor(BlockPos::new(5, 70, 5))
        else {
            panic!("expected an OpenSignEditor");
        };
        let loc = packet.location();
        assert_eq!((loc.x(), loc.y(), loc.z()), (5, 70, 5));
        assert!(packet.is_front_text());
    }

    #[test]
    fn open_screen_carries_window_id_type_and_title() {
        let title = TextComponent::text("Chest");
        let ClientboundPlayPacket::OpenScreen(packet) = open_screen(3, 2, &title) else {
            panic!("expected an OpenScreen");
        };
        assert_eq!(packet.window_id(), 3);
        // generic_9x3 (single chest) is menu-registry id 2.
        assert_eq!(packet.window_type(), 2);
        // The title encodes as a network-NBT text component ({ text: "Chest" }),
        // and survives a network-root round trip.
        let NbtTag::Compound(root) = packet.title() else {
            panic!("title is not a compound");
        };
        assert_eq!(root.get("text"), Some(&NbtTag::String("Chest".to_owned())));
        let bytes = write_network_root(packet.title(), &NbtLimits::default()).expect("encode");
        assert_eq!(
            &read_network_root(&bytes, &NbtLimits::default()).expect("decode"),
            packet.title()
        );
    }
}
