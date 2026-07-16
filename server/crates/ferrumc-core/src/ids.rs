//! Identifier newtypes used across the server.
//!
//! Each id is a thin wrapper around a primitive (or a `Uuid`/interned string)
//! that gives it a distinct type, so a [`ConnectionId`] can never be silently
//! used where an [`EntityId`] is expected. Internal representation is hidden
//! behind constructors and accessor methods.

use core::fmt;
use core::str::FromStr;
use std::sync::Arc;

use md5::{Digest, Md5};
use uuid::Uuid;

/// Prefix hashed by vanilla Java Edition for offline-mode player identities.
const OFFLINE_PLAYER_PREFIX: &[u8] = b"OfflinePlayer:";

/// A player's globally unique identity, backed by a [`Uuid`].
///
/// Minecraft identifies players by UUID. For online-mode players this is the
/// Mojang-assigned account UUID; for offline-mode players a stable id is derived
/// from the username via [`PlayerId::offline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlayerId(Uuid);

impl PlayerId {
    /// Wraps an existing [`Uuid`] as a `PlayerId`.
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Generates a fresh, random (version 4) `PlayerId`.
    ///
    /// Useful for tests and for synthesizing identities that are not derived
    /// from a name.
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }

    /// Derives a deterministic `PlayerId` for an offline-mode player from their
    /// `username`.
    ///
    /// Offline-mode servers receive no authoritative Mojang UUID, so this
    /// implements Java Edition's exact
    /// `UUID.nameUUIDFromBytes(("OfflinePlayer:" + username).getBytes(UTF_8))`
    /// semantics: MD5 the literal prefix and username UTF-8 bytes, then stamp
    /// UUID version 3 and the RFC 4122 variant bits.
    ///
    /// The username is case-sensitive and used verbatim. It is not trimmed or
    /// Unicode-normalized, so byte-distinct names remain distinct identities.
    pub fn offline(username: &str) -> Self {
        let mut digest = Md5::new();
        digest.update(OFFLINE_PLAYER_PREFIX);
        digest.update(username.as_bytes());
        let mut bytes: [u8; 16] = digest.finalize().into();
        bytes[6] = (bytes[6] & 0x0f) | 0x30;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }

    /// Returns the underlying [`Uuid`].
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl FromStr for PlayerId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> core::result::Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl fmt::Display for PlayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hyphenated, lowercase canonical form, matching the protocol.
        write!(f, "{}", self.0)
    }
}

impl From<Uuid> for PlayerId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

/// A server-internal handle for a single network connection.
///
/// Unique per accepted connection for the lifetime of the process. It is an
/// opaque counter with no protocol meaning; the allocator that hands these out
/// lives in the networking layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// Wraps a raw counter value as a `ConnectionId`.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw counter value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The protocol-level entity id, a signed 32-bit integer.
///
/// This is the value sent on the wire to reference an entity; it is assigned by
/// the server and is unique within a world for the entity's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EntityId(i32);

impl EntityId {
    /// Wraps a raw network entity id.
    pub const fn new(raw: i32) -> Self {
        Self(raw)
    }

    /// Returns the raw network entity id.
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A server-internal handle for a loaded world (a save / set of dimensions).
///
/// An opaque index assigned when a world is loaded. Human-readable world names
/// are mapped to `WorldId`s by the layer that owns the world registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WorldId(u32);

impl WorldId {
    /// Wraps a raw world index.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw world index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for WorldId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A server-internal handle for a dimension within a world.
///
/// An opaque index into the running dimension registry. Registry keys such as
/// `minecraft:overworld` are mapped to `DimensionId`s by the registry layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DimensionId(u32);

impl DimensionId {
    /// Wraps a raw dimension index.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw dimension index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for DimensionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A plugin's stable string identifier (for example `spawn-protect`).
///
/// Backed by an [`Arc<str>`] so it is cheap to clone and share across event
/// dispatch, registries, and namespaced storage. Equality, ordering, and hashing
/// compare the string contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginId(Arc<str>);

impl PluginId {
    /// Creates a `PluginId` from anything convertible into an [`Arc<str>`]
    /// (such as `&str` or `String`).
    ///
    /// The caller is responsible for supplying a sensible identifier; this
    /// constructor does not validate the character set.
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for PluginId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for PluginId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self(Arc::from(raw)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_id_parse_format_round_trip() {
        let text = "067e6162-3b6f-4ae2-a171-2470b63dff00";
        let id: PlayerId = text.parse().expect("valid uuid");
        assert_eq!(id.to_string(), text);
        assert_eq!(id.as_uuid().to_string(), text);
        // Re-parsing the formatted output yields the same id.
        assert_eq!(text.parse::<PlayerId>().expect("valid uuid"), id);
    }

    #[test]
    fn player_id_rejects_garbage() {
        assert!("not-a-uuid".parse::<PlayerId>().is_err());
        assert!("".parse::<PlayerId>().is_err());
    }

    #[test]
    fn offline_id_is_deterministic_and_versioned() {
        let a = PlayerId::offline("Notch");
        let b = PlayerId::offline("Notch");
        let c = PlayerId::offline("jeb_");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_uuid().get_version_num(), 3);
    }

    #[test]
    fn offline_uuid_matches_java_semantics() {
        let vectors = [
            ("a", "52428a0e-1e30-3cb1-976c-e728b2614047"),
            ("Notch", "b50ad385-829d-3141-a216-7e7d7539ba7f"),
            ("abcdefghijklmnop", "9dc76558-520f-3560-86b0-b5c7c500848b"),
            ("Steve", "5627dd98-e6be-3c21-b8a8-e92344183641"),
            ("steve", "53909932-f794-33c0-9329-948045a4c1ce"),
            ("玩家", "511a6918-f669-3b23-ba72-decf60a9cb59"),
            (" Steve ", "4d53680f-0c07-36b2-8793-d3931967c543"),
            ("é", "7448742a-9f92-3a33-984e-c31c0f97cff3"),
            ("e\u{301}", "931d7ffc-a1a9-35de-94ec-43db5fb39f2b"),
        ];

        for (name, expected) in vectors {
            let actual = PlayerId::offline(name).as_uuid();
            assert_eq!(actual.to_string(), expected, "offline UUID for {name:?}");
            assert_eq!(actual.get_version_num(), 3, "version for {name:?}");
            assert_eq!(
                actual.get_variant(),
                uuid::Variant::RFC4122,
                "variant for {name:?}"
            );
        }

        assert_ne!(PlayerId::offline("Steve"), PlayerId::offline("steve"));
        assert_ne!(PlayerId::offline("Steve"), PlayerId::offline(" Steve "));
        assert_ne!(PlayerId::offline("é"), PlayerId::offline("e\u{301}"));
    }

    #[test]
    fn random_ids_differ() {
        assert_ne!(PlayerId::random(), PlayerId::random());
    }

    #[test]
    fn entity_id_display_and_round_trip() {
        let id = EntityId::new(-42);
        assert_eq!(id.get(), -42);
        assert_eq!(id.to_string(), "-42");
        assert_eq!(EntityId::new(i32::MAX).to_string(), i32::MAX.to_string());
    }

    #[test]
    fn numeric_ids_display() {
        assert_eq!(ConnectionId::new(5).to_string(), "5");
        assert_eq!(ConnectionId::new(5).get(), 5);
        assert_eq!(WorldId::new(0).to_string(), "0");
        assert_eq!(DimensionId::new(7).to_string(), "7");
    }

    #[test]
    fn plugin_id_from_str_and_string() {
        let from_str = PluginId::new("spawn-protect");
        let from_string = PluginId::new(String::from("spawn-protect"));
        assert_eq!(from_str, from_string);
        assert_eq!(from_str.as_str(), "spawn-protect");
        assert_eq!(from_str.to_string(), "spawn-protect");
        // Cloning is cheap (shared Arc) and preserves equality.
        assert_eq!(from_str.clone(), from_str);
    }

    #[test]
    fn plugin_id_allows_empty_string_per_contract() {
        // `PluginId::new` documents that it does not validate the identifier and
        // the caller is responsible for supplying a sensible one. Pin that
        // contract: an empty string is accepted, stored verbatim, and renders
        // empty. If validation is ever added, this test must be revisited.
        let id = PluginId::new("");
        assert_eq!(id.as_str(), "");
        assert_eq!(id.to_string(), "");
        assert_eq!(id, PluginId::new(String::new()));
    }

    #[test]
    fn distinct_ids_keep_their_ordering() {
        let mut v = [
            ConnectionId::new(3),
            ConnectionId::new(1),
            ConnectionId::new(2),
        ];
        v.sort();
        assert_eq!(
            v,
            [
                ConnectionId::new(1),
                ConnectionId::new(2),
                ConnectionId::new(3)
            ]
        );
    }
}
