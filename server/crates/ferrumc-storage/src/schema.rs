//! Versioning for persisted records.

/// The schema version stamped on a persisted record.
///
/// Every record type ([`crate::ChunkRecord`], [`crate::EntityRecord`],
/// [`crate::PlayerRecord`]) carries one of these so a future backend can detect
/// data written by an older build and migrate or reject it, rather than
/// misreading a changed layout. Storage preserves the value verbatim on
/// save/load; it does not interpret it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    /// Wraps a raw schema version number.
    pub const fn new(version: u32) -> Self {
        Self(version)
    }

    /// Returns the raw schema version number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl core::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_raw_value() {
        let v = SchemaVersion::new(7);
        assert_eq!(v.get(), 7);
        assert_eq!(v.to_string(), "v7");
    }

    #[test]
    fn orders_by_version_number() {
        assert!(SchemaVersion::new(1) < SchemaVersion::new(2));
    }
}
