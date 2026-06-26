//! A small structured text model for chat, disconnect reasons, and command output.
//!
//! [`TextComponent`] mirrors the shape of Minecraft's JSON text format closely
//! enough to be serialized with serde, but pulls in nothing network-related: it
//! is a plain value type. Styling is applied with a builder-style API, and the
//! unstyled text can be recovered with [`TextComponent::to_plain_string`].

use core::fmt;

/// One of the sixteen named Minecraft text colors.
///
/// Serializes to its lowercase protocol name (for example
/// [`TextColor::DarkBlue`] becomes `dark_blue`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum TextColor {
    /// `black`
    Black,
    /// `dark_blue`
    DarkBlue,
    /// `dark_green`
    DarkGreen,
    /// `dark_aqua`
    DarkAqua,
    /// `dark_red`
    DarkRed,
    /// `dark_purple`
    DarkPurple,
    /// `gold`
    Gold,
    /// `gray`
    Gray,
    /// `dark_gray`
    DarkGray,
    /// `blue`
    Blue,
    /// `green`
    Green,
    /// `aqua`
    Aqua,
    /// `red`
    Red,
    /// `light_purple`
    LightPurple,
    /// `yellow`
    Yellow,
    /// `white`
    White,
}

impl TextColor {
    /// Returns the lowercase protocol name of this color.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::DarkBlue => "dark_blue",
            Self::DarkGreen => "dark_green",
            Self::DarkAqua => "dark_aqua",
            Self::DarkRed => "dark_red",
            Self::DarkPurple => "dark_purple",
            Self::Gold => "gold",
            Self::Gray => "gray",
            Self::DarkGray => "dark_gray",
            Self::Blue => "blue",
            Self::Green => "green",
            Self::Aqua => "aqua",
            Self::Red => "red",
            Self::LightPurple => "light_purple",
            Self::Yellow => "yellow",
            Self::White => "white",
        }
    }
}

impl fmt::Display for TextColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A structured piece of text with optional styling and child components.
///
/// A component carries its own literal `text`, optional color and formatting
/// flags, and a list of child (`extra`) components that inherit nothing here but
/// are appended after this component's text when flattened. Build one with
/// [`TextComponent::text`] and layer styling with the `with_*` methods.
///
/// Unset flags are `None` (meaning "inherit / unspecified"), which keeps the
/// serialized form compact and lets a renderer distinguish "not bold" from
/// "explicitly bold = false".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TextComponent {
    text: String,
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "Option::is_none", default)
    )]
    color: Option<TextColor>,
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "Option::is_none", default)
    )]
    bold: Option<bool>,
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "Option::is_none", default)
    )]
    italic: Option<bool>,
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "Option::is_none", default)
    )]
    underlined: Option<bool>,
    #[cfg_attr(
        feature = "serde",
        serde(skip_serializing_if = "Vec::is_empty", default)
    )]
    extra: Vec<TextComponent>,
}

impl TextComponent {
    /// Creates a plain, unstyled component holding the given text.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            text: content.into(),
            ..Self::default()
        }
    }

    /// Sets the text color.
    #[must_use]
    pub fn with_color(mut self, color: TextColor) -> Self {
        self.color = Some(color);
        self
    }

    /// Sets the bold flag.
    #[must_use]
    pub fn with_bold(mut self, bold: bool) -> Self {
        self.bold = Some(bold);
        self
    }

    /// Sets the italic flag.
    #[must_use]
    pub fn with_italic(mut self, italic: bool) -> Self {
        self.italic = Some(italic);
        self
    }

    /// Sets the underlined flag.
    #[must_use]
    pub fn with_underlined(mut self, underlined: bool) -> Self {
        self.underlined = Some(underlined);
        self
    }

    /// Appends a child component, rendered after this component's text.
    #[must_use]
    pub fn with_child(mut self, child: TextComponent) -> Self {
        self.extra.push(child);
        self
    }

    /// Returns this component's own literal text (excluding children).
    pub fn content(&self) -> &str {
        &self.text
    }

    /// Returns the text color, if one was set.
    pub fn color(&self) -> Option<TextColor> {
        self.color
    }

    /// Returns the bold flag, if one was set.
    pub fn bold(&self) -> Option<bool> {
        self.bold
    }

    /// Returns the italic flag, if one was set.
    pub fn italic(&self) -> Option<bool> {
        self.italic
    }

    /// Returns the underlined flag, if one was set.
    pub fn underlined(&self) -> Option<bool> {
        self.underlined
    }

    /// Returns this component's child components.
    pub fn children(&self) -> &[TextComponent] {
        &self.extra
    }

    /// Renders the component to a plain string, dropping all styling.
    ///
    /// This concatenates this component's text with every descendant's text, in
    /// depth-first order — the same order a client would display them.
    pub fn to_plain_string(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for TextComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)?;
        for child in &self.extra {
            write!(f, "{child}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_round_trips() {
        let c = TextComponent::text("hello");
        assert_eq!(c.content(), "hello");
        assert_eq!(c.to_plain_string(), "hello");
        assert!(c.color().is_none());
        assert!(c.children().is_empty());
    }

    #[test]
    fn builder_sets_style() {
        let c = TextComponent::text("hi")
            .with_color(TextColor::Red)
            .with_bold(true)
            .with_italic(false)
            .with_underlined(true);
        assert_eq!(c.color(), Some(TextColor::Red));
        assert_eq!(c.bold(), Some(true));
        assert_eq!(c.italic(), Some(false));
        assert_eq!(c.underlined(), Some(true));
    }

    #[test]
    fn nested_styled_component_renders_plain() {
        let message = TextComponent::text("[")
            .with_color(TextColor::Gray)
            .with_child(
                TextComponent::text("Server")
                    .with_color(TextColor::Gold)
                    .with_bold(true),
            )
            .with_child(TextComponent::text("] "))
            .with_child(
                TextComponent::text("welcome ")
                    .with_color(TextColor::Green)
                    .with_child(TextComponent::text("Saad").with_underlined(true)),
            );

        assert_eq!(message.to_plain_string(), "[Server] welcome Saad");
    }

    #[test]
    fn color_names_match_protocol() {
        assert_eq!(TextColor::DarkBlue.as_str(), "dark_blue");
        assert_eq!(TextColor::LightPurple.to_string(), "light_purple");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trip_and_omits_unset_fields() {
        let c = TextComponent::text("hi")
            .with_color(TextColor::Aqua)
            .with_child(TextComponent::text("!"));

        let json = serde_json::to_string(&c).expect("serialize");
        // Unset flags are skipped; set fields use protocol names.
        assert!(json.contains("\"text\":\"hi\""));
        assert!(json.contains("\"color\":\"aqua\""));
        assert!(!json.contains("bold"));

        let back: TextComponent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, c);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_deserializes_minimal_object() {
        let back: TextComponent =
            serde_json::from_str("{\"text\":\"x\"}").expect("deserialize minimal");
        assert_eq!(back, TextComponent::text("x"));
    }
}
