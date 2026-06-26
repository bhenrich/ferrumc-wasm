//! Deterministic offline-mode player UUID derivation.
//!
//! An offline ("cracked") server never contacts Mojang to authenticate a player,
//! so it must mint each username a stable UUID on its own. The only hard
//! requirement is determinism: the same name must always map to the same UUID,
//! within a run and across restarts, so player data keyed by UUID stays
//! consistent.

use sha1::{Digest, Sha1};
use uuid::Uuid;

/// The prefix vanilla prepends to the username before hashing, kept so this
/// scheme is at least seeded identically to vanilla's offline UUID.
const OFFLINE_SEED_PREFIX: &[u8] = b"OfflinePlayer:";

/// Derives a deterministic offline-mode [`Uuid`] for `name`.
///
/// The UUID is the SHA-1 digest of `"OfflinePlayer:" + name` (UTF-8), truncated
/// to its first 16 bytes, with the UUID version nibble set to `3` and the
/// RFC 4122 variant bits set. Stamping version `3` keeps the result recognizable
/// as a name-derived, offline-style UUID (vanilla offline UUIDs are version 3)
/// rather than a random (version 4) account UUID.
///
/// # Not byte-identical to vanilla
///
/// Vanilla computes `UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes(UTF_8))`,
/// which is an **MD5** digest (a true version-3 UUID). This crate has no MD5
/// dependency and deliberately does not add one, so it substitutes SHA-1 over
/// the same seed. The output is therefore deterministic and offline-shaped, but
/// **NOT** the same bytes a vanilla server would assign to the same name. To
/// match vanilla exactly you would have to implement (or depend on) raw MD5 and
/// hash with that instead — see the milestone notes.
///
/// # Stability
///
/// Both the seed prefix and the digest are fixed, so this mapping is stable for
/// the lifetime of the crate. Changing either would re-key every offline
/// player's stored data and is a breaking change.
#[must_use]
pub fn offline_uuid(name: &str) -> Uuid {
    let mut hasher = Sha1::new();
    hasher.update(OFFLINE_SEED_PREFIX);
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 16];
    // SHA-1 produces 20 bytes; the first 16 fill the UUID, the rest are dropped.
    bytes.copy_from_slice(&digest[..16]);
    // Version 3 (name-based): clear the high nibble of byte 6 and set it to 3.
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    // RFC 4122 variant: clear the top two bits of byte 8 and set them to `10`.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic() {
        assert_eq!(offline_uuid("Saad"), offline_uuid("Saad"));
    }

    #[test]
    fn distinct_names_yield_distinct_uuids() {
        assert_ne!(offline_uuid("Saad"), offline_uuid("Notch"));
    }

    #[test]
    fn stamps_version_three_and_rfc4122_variant() {
        let uuid = offline_uuid("Saad");
        // Version nibble lives in the high nibble of byte 6.
        assert_eq!(uuid.as_bytes()[6] >> 4, 0x3, "version must be 3");
        // RFC 4122 variant is the top two bits of byte 8 being `10`.
        assert_eq!(uuid.as_bytes()[8] >> 6, 0b10, "variant must be RFC 4122");
        assert_eq!(uuid.get_version_num(), 3);
    }

    #[test]
    fn empty_name_is_handled() {
        // An empty username is degenerate but must still produce a stable value.
        assert_eq!(offline_uuid(""), offline_uuid(""));
    }

    #[test]
    fn is_not_the_nil_uuid() {
        assert_ne!(offline_uuid("Saad"), Uuid::nil());
    }
}
