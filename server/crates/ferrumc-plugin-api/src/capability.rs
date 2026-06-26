//! Plugin capabilities and the manifest that declares them.
//!
//! A plugin declares the [`CapabilityManifest`] it needs in its
//! [`PluginMetadata`](crate::PluginMetadata); the host decides what to grant.
//! Every plugin-facing facade (world reads, mutation intents, command
//! registration, event delivery, permission queries, storage) is gated behind a
//! [`Capability`], so a plugin can only reach what it was granted.

use core::fmt;

use crate::error::CapabilityError;

/// A single permission to use one plugin-facing facade.
///
/// Capabilities are coarse-grained: each one unlocks an entire facade, never a
/// specific resource. Fine-grained authorization (which block, which player)
/// is the simulation's job, not the plugin host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Read the world through a [`WorldView`](crate::WorldView).
    ReadWorld,
    /// Submit mutation intents through a [`CommandSink`](crate::CommandSink).
    SubmitIntents,
    /// Register commands through a [`CommandRegistrar`](crate::CommandRegistrar).
    RegisterCommands,
    /// Receive dispatched events (subscribe and be called back).
    ReceiveEvents,
    /// Query permissions through a [`PermissionApi`](crate::PermissionApi).
    ReadPermissions,
    /// Use the plugin's namespaced
    /// [`PluginStorageApi`](crate::PluginStorageApi).
    Storage,
}

impl Capability {
    /// Every capability, in declaration order.
    ///
    /// Useful for building an all-capabilities manifest or for enumeration in
    /// tooling.
    pub const ALL: [Capability; 6] = [
        Capability::ReadWorld,
        Capability::SubmitIntents,
        Capability::RegisterCommands,
        Capability::ReceiveEvents,
        Capability::ReadPermissions,
        Capability::Storage,
    ];

    /// The single bit this capability occupies inside a [`CapabilityManifest`].
    const fn bit(self) -> u32 {
        match self {
            Capability::ReadWorld => 1 << 0,
            Capability::SubmitIntents => 1 << 1,
            Capability::RegisterCommands => 1 << 2,
            Capability::ReceiveEvents => 1 << 3,
            Capability::ReadPermissions => 1 << 4,
            Capability::Storage => 1 << 5,
        }
    }

    /// Returns the stable, lowercase identifier for this capability.
    pub const fn as_str(self) -> &'static str {
        match self {
            Capability::ReadWorld => "read-world",
            Capability::SubmitIntents => "submit-intents",
            Capability::RegisterCommands => "register-commands",
            Capability::ReceiveEvents => "receive-events",
            Capability::ReadPermissions => "read-permissions",
            Capability::Storage => "storage",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The bit mask covering every defined capability.
const ALL_BITS: u32 = Capability::ReadWorld.bit()
    | Capability::SubmitIntents.bit()
    | Capability::RegisterCommands.bit()
    | Capability::ReceiveEvents.bit()
    | Capability::ReadPermissions.bit()
    | Capability::Storage.bit();

/// An immutable set of [`Capability`] grants.
///
/// A manifest is a small, copyable bit set. Build one from
/// [`CapabilityManifest::empty`] with [`CapabilityManifest::with`], or take
/// [`CapabilityManifest::all`] for a fully-trusted plugin. The internal
/// representation is private; query it with [`CapabilityManifest::grants`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapabilityManifest {
    bits: u32,
}

impl CapabilityManifest {
    /// Returns a manifest granting no capabilities.
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    /// Returns a manifest granting every defined capability.
    pub const fn all() -> Self {
        Self { bits: ALL_BITS }
    }

    /// Returns a copy of this manifest with `capability` granted.
    #[must_use]
    pub const fn with(self, capability: Capability) -> Self {
        Self {
            bits: self.bits | capability.bit(),
        }
    }

    /// Returns a copy of this manifest with `capability` revoked.
    #[must_use]
    pub const fn without(self, capability: Capability) -> Self {
        Self {
            bits: self.bits & !capability.bit(),
        }
    }

    /// Returns whether `capability` is granted by this manifest.
    pub const fn grants(self, capability: Capability) -> bool {
        self.bits & capability.bit() != 0
    }

    /// Returns whether this manifest grants no capabilities.
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Returns the raw bitset backing this manifest.
    ///
    /// The bit layout is the stable representation carried across the plugin C
    /// ABI (see [`crate::abi`]); pair it with [`CapabilityManifest::from_bits_truncate`].
    pub const fn bits(self) -> u32 {
        self.bits
    }

    /// Builds a manifest from a raw bitset, discarding any bits that do not
    /// correspond to a defined [`Capability`].
    ///
    /// Used by the host to interpret the capability bitset a dynamically-loaded
    /// plugin declares across the C ABI: unknown bits are dropped rather than
    /// trusted, so a malformed or future-versioned plugin can never conjure a
    /// capability the host does not know about.
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self {
            bits: bits & ALL_BITS,
        }
    }

    /// Returns the number of capabilities granted.
    pub const fn len(self) -> u32 {
        self.bits.count_ones()
    }

    /// Returns whether every capability granted by `other` is also granted by
    /// `self`.
    pub const fn contains_all(self, other: Self) -> bool {
        self.bits & other.bits == other.bits
    }

    /// Returns `Ok(())` if `capability` is granted, or a [`CapabilityError`]
    /// naming it otherwise.
    pub const fn require(self, capability: Capability) -> Result<(), CapabilityError> {
        if self.grants(capability) {
            Ok(())
        } else {
            Err(CapabilityError::missing(capability))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_grants_nothing() {
        let manifest = CapabilityManifest::empty();
        assert!(manifest.is_empty());
        assert_eq!(manifest.len(), 0);
        for cap in Capability::ALL {
            assert!(!manifest.grants(cap));
        }
    }

    #[test]
    fn all_grants_everything() {
        let manifest = CapabilityManifest::all();
        assert!(!manifest.is_empty());
        assert_eq!(manifest.len(), Capability::ALL.len() as u32);
        for cap in Capability::ALL {
            assert!(manifest.grants(cap));
        }
    }

    #[test]
    fn with_and_without_toggle_single_bits() {
        let manifest = CapabilityManifest::empty()
            .with(Capability::Storage)
            .with(Capability::ReadWorld);
        assert!(manifest.grants(Capability::Storage));
        assert!(manifest.grants(Capability::ReadWorld));
        assert!(!manifest.grants(Capability::SubmitIntents));

        let revoked = manifest.without(Capability::Storage);
        assert!(!revoked.grants(Capability::Storage));
        assert!(revoked.grants(Capability::ReadWorld));
    }

    #[test]
    fn require_reports_missing_capability() {
        let manifest = CapabilityManifest::empty().with(Capability::ReadWorld);
        assert!(manifest.require(Capability::ReadWorld).is_ok());
        let err = manifest
            .require(Capability::Storage)
            .expect_err("storage is not granted");
        assert_eq!(err.capability(), Capability::Storage);
    }

    #[test]
    fn contains_all_is_subset_check() {
        let full = CapabilityManifest::all();
        let some = CapabilityManifest::empty()
            .with(Capability::ReadWorld)
            .with(Capability::Storage);
        assert!(full.contains_all(some));
        assert!(!some.contains_all(full));
        assert!(some.contains_all(some));
    }

    #[test]
    fn bits_round_trip_through_from_bits() {
        let manifest = CapabilityManifest::empty()
            .with(Capability::ReadWorld)
            .with(Capability::Storage);
        let restored = CapabilityManifest::from_bits_truncate(manifest.bits());
        assert_eq!(manifest, restored);
        assert_eq!(CapabilityManifest::all().bits(), ALL_BITS);
    }

    #[test]
    fn from_bits_truncate_drops_unknown_bits() {
        // Every high bit beyond the defined capabilities must be discarded.
        let garbage = CapabilityManifest::from_bits_truncate(0xFFFF_FFFF);
        assert_eq!(garbage, CapabilityManifest::all());
        assert_eq!(
            CapabilityManifest::from_bits_truncate(0),
            CapabilityManifest::empty()
        );
    }

    #[test]
    fn display_is_stable_identifier() {
        assert_eq!(Capability::ReadWorld.to_string(), "read-world");
        assert_eq!(Capability::Storage.as_str(), "storage");
    }
}
