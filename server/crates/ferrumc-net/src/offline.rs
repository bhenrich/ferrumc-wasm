//! Compatibility access to the canonical offline-mode player UUID.
//!
//! An offline ("cracked") server never contacts Mojang to authenticate a player,
//! so it derives the vanilla Java Edition identity through
//! [`ferrumc_core::PlayerId::offline`]. Keeping this thin UUID-returning adapter
//! preserves the networking crate's existing public surface while ensuring there
//! is only one derivation algorithm in the workspace.

use ferrumc_core::PlayerId;
use uuid::Uuid;

/// Derives a deterministic offline-mode [`Uuid`] for `name`.
///
/// This delegates to [`PlayerId::offline`], whose contract matches Java's
/// `UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes(UTF_8))`
/// semantics exactly. The name is case-sensitive and used verbatim.
#[must_use]
pub fn offline_uuid(name: &str) -> Uuid {
    PlayerId::offline(name).as_uuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegates_to_canonical_player_identity() {
        assert_eq!(offline_uuid("Saad"), PlayerId::offline("Saad").as_uuid());
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
