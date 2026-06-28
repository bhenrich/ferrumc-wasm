//! [`TextComponent`] -> network-NBT encoding and the clientbound [`SystemChat`]
//! builder.
//!
//! A 1.21.8 client expects the `content` of a System Chat Message (clientbound
//! `0x72`) as a text component encoded in **network NBT** — an anonymous-root
//! (unnamed) compound, the 1.20.3+ form — not as a legacy JSON string. This
//! module bridges the polished [`TextComponent`] value type (which owns no
//! network concepts) and the generated [`SystemChat`] packet (whose `content`
//! field is a [`ferrumc_nbt::NbtTag`] written with
//! [`ferrumc_nbt::write_network_root`]).
//!
//! This crate is the right home for the encoder: it is the only one that depends
//! on both [`ferrumc_core`] (which owns [`TextComponent`]) and [`ferrumc_proto`]
//! (which owns [`SystemChat`]). `ferrumc-proto` does not depend on `ferrumc-core`,
//! so the encoder cannot live there without a new cross-crate edge.

use ferrumc_core::TextComponent;
use ferrumc_nbt::{NbtCompound, NbtTag};
use ferrumc_proto::generated::play::{ClientboundPlayPacket, SystemChat};

/// Encodes a [`TextComponent`] tree into the network-NBT [`NbtTag`] a 1.21.8
/// client accepts as a text component.
///
/// The root is always a `TAG_Compound` carrying the literal `text`, the optional
/// `color` (its lowercase protocol name), the optional boolean styles `bold` /
/// `italic` / `underlined` (each a `TAG_Byte` of `0` or `1`, the NBT boolean
/// encoding — *not* a `TAG_String` `"true"`/`"false"`, which a real client
/// silently drops), and, when present, the child components under `extra` as a
/// `TAG_List` of the same compound form. Unset style flags are omitted so the
/// compound stays compact and the client inherits them.
///
/// The compound root is mandatory: [`ferrumc_nbt::write_network_root`] requires a
/// `TAG_Compound` root, and `{ text: "..." }` is the canonical component form.
///
/// Visible to the rest of the crate so the [`crate::presentation`] title and
/// action-bar builders encode their text the exact same way `SystemChat` does —
/// the single source of truth for the `TextComponent` -> network-NBT path.
pub(crate) fn text_component_to_nbt(component: &TextComponent) -> NbtTag {
    let mut root = NbtCompound::new();
    root.push("text", NbtTag::String(component.content().to_owned()));
    if let Some(color) = component.color() {
        root.push("color", NbtTag::String(color.as_str().to_owned()));
    }
    // Minecraft text-component booleans are TAG_Byte 0/1, never TAG_String.
    if let Some(bold) = component.bold() {
        root.push("bold", NbtTag::Byte(i8::from(bold)));
    }
    if let Some(italic) = component.italic() {
        root.push("italic", NbtTag::Byte(i8::from(italic)));
    }
    if let Some(underlined) = component.underlined() {
        root.push("underlined", NbtTag::Byte(i8::from(underlined)));
    }
    let children = component.children();
    if !children.is_empty() {
        root.push(
            "extra",
            NbtTag::List(children.iter().map(text_component_to_nbt).collect()),
        );
    }
    NbtTag::Compound(root)
}

/// Builds the clientbound [`SystemChat`] packet carrying `component`.
///
/// `overlay = true` renders the message above the hotbar (the action bar);
/// `overlay = false` renders it in the chat box. The component is encoded as
/// network NBT (see [`text_component_to_nbt`]). System chat is unsigned, so this
/// is the carrier for command feedback, relayed player chat, and any other
/// server-authored message — none of which need the 1.19 signing apparatus.
///
/// The returned packet is ready to enqueue on a player's outbound channel (the
/// generated encoder writes the NBT with [`ferrumc_nbt::write_network_root`]).
pub fn system_chat(component: &TextComponent, overlay: bool) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SystemChat(SystemChat::new(text_component_to_nbt(component), overlay))
}

#[cfg(test)]
mod tests {
    use ferrumc_core::TextColor;
    use ferrumc_nbt::{read_network_root, write_network_root, NbtLimits};

    use super::*;

    /// A styled component with a nested child, the shape command feedback and
    /// relayed chat produce.
    fn styled_component() -> TextComponent {
        TextComponent::text("<Saad> ")
            .with_color(TextColor::Yellow)
            .with_bold(true)
            .with_italic(false)
            .with_child(TextComponent::text("hello").with_color(TextColor::Green))
    }

    #[test]
    fn encodes_text_color_styles_and_children() {
        let tag = text_component_to_nbt(&styled_component());
        let NbtTag::Compound(root) = &tag else {
            panic!("a component must encode to a compound root");
        };
        assert_eq!(
            root.get("text"),
            Some(&NbtTag::String("<Saad> ".to_owned()))
        );
        assert_eq!(
            root.get("color"),
            Some(&NbtTag::String("yellow".to_owned()))
        );
        // Booleans are TAG_Byte 0/1.
        assert_eq!(root.get("bold"), Some(&NbtTag::Byte(1)));
        assert_eq!(root.get("italic"), Some(&NbtTag::Byte(0)));
        // An unset flag is omitted entirely (the client inherits it).
        assert_eq!(root.get("underlined"), None);

        let Some(NbtTag::List(extra)) = root.get("extra") else {
            panic!("children must encode under `extra` as a list");
        };
        assert_eq!(extra.len(), 1);
        let NbtTag::Compound(child) = &extra[0] else {
            panic!("a child must be a compound");
        };
        assert_eq!(child.get("text"), Some(&NbtTag::String("hello".to_owned())));
        assert_eq!(
            child.get("color"),
            Some(&NbtTag::String("green".to_owned()))
        );
    }

    #[test]
    fn network_nbt_round_trips_byte_faithfully() {
        // Encode the component to the exact wire bytes the packet carries, then
        // read them back with the matching network-root reader and assert the
        // structure survives — i.e. the anonymous-root form is well-formed.
        let tag = text_component_to_nbt(&styled_component());
        let bytes = write_network_root(&tag, &NbtLimits::default()).expect("encode network NBT");
        // First byte is the compound type tag (10) with NO root name following it.
        assert_eq!(bytes[0], 10, "network root must start with TAG_Compound");
        let parsed = read_network_root(&bytes, &NbtLimits::default()).expect("decode network NBT");
        assert_eq!(parsed, tag);
    }

    #[test]
    fn emoji_component_encodes_modified_utf8_not_standard() {
        // A player typing an emoji is the case that disconnected real clients:
        // the component's `text` must reach the wire as Modified UTF-8 (a
        // six-byte surrogate pair), never the four-byte standard UTF-8 form whose
        // 0xF0 lead a 1.21.8 client's NBT reader rejects with
        // UTFDataFormatException — which, broadcast via SystemChat, would drop
        // every recipient.
        let component = TextComponent::text("gg \u{1F600}");
        let tag = text_component_to_nbt(&component);
        let bytes = write_network_root(&tag, &NbtLimits::default()).expect("encode network NBT");
        assert!(
            !bytes.iter().any(|&b| (0xF0..=0xF4).contains(&b)),
            "the emoji must not be written as a four-byte standard UTF-8 sequence"
        );
        // The exact CESU-8 surrogate pair for U+1F600 must appear verbatim.
        let surrogate_pair = [0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80];
        assert!(
            bytes
                .windows(surrogate_pair.len())
                .any(|w| w == surrogate_pair),
            "expected the Modified UTF-8 surrogate pair for the emoji"
        );
        // It still decodes back to the same text through the matching reader.
        let parsed = read_network_root(&bytes, &NbtLimits::default()).expect("decode network NBT");
        let NbtTag::Compound(root) = parsed else {
            panic!("a component must encode to a compound root");
        };
        assert_eq!(
            root.get("text"),
            Some(&NbtTag::String("gg \u{1F600}".to_owned()))
        );
    }

    #[test]
    fn system_chat_sets_content_and_overlay_bit() {
        let component = TextComponent::text("on the action bar");

        let ClientboundPlayPacket::SystemChat(chat) = system_chat(&component, true) else {
            panic!("system_chat must build a SystemChat packet");
        };
        assert!(chat.overlay(), "overlay = true is the action-bar bit");
        let NbtTag::Compound(root) = chat.content() else {
            panic!("content must be a compound");
        };
        assert_eq!(
            root.get("text"),
            Some(&NbtTag::String("on the action bar".to_owned()))
        );

        // overlay = false targets the chat box.
        let ClientboundPlayPacket::SystemChat(chat) = system_chat(&component, false) else {
            panic!("system_chat must build a SystemChat packet");
        };
        assert!(!chat.overlay(), "overlay = false is the chat box");
    }
}
