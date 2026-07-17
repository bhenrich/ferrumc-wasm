//! Durable idempotency identities and receipts for mutation-journal batches.

use core::fmt;
use core::ops::RangeInclusive;

use uuid::Uuid;

/// Identifies one logical mutation-journal batch across retries.
///
/// Callers generate one ID for a logical batch and retain it until the append
/// result is known. Reusing the ID with the same normalized mutation payload is
/// idempotent; reusing it for another payload is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JournalBatchId(Uuid);

impl JournalBatchId {
    /// Generates a random ID for a new logical journal batch.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    /// Builds an ID from its stable 16-byte representation.
    ///
    /// This constructor makes deterministic schedules and persisted protocols
    /// independent of UUID text formatting.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes))
    }

    /// Returns the stable 16-byte representation.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    /// Consumes the ID and returns its stable 16-byte representation.
    #[must_use]
    pub fn into_bytes(self) -> [u8; 16] {
        self.0.into_bytes()
    }
}

impl fmt::Display for JournalBatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Proves the durable sequence range assigned to one journal batch.
///
/// A receipt is returned only after the journal records, sequence metadata, and
/// the receipt itself have committed atomically. Replaying its
/// [`JournalBatchId`] returns the same value without appending again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalAppendReceipt {
    batch_id: JournalBatchId,
    first_id: Option<u64>,
    last_id: Option<u64>,
    len: usize,
}

impl JournalAppendReceipt {
    pub(crate) fn from_range(
        batch_id: JournalBatchId,
        range: Option<(u64, u64)>,
        len: usize,
    ) -> Self {
        let (first_id, last_id) =
            range.map_or((None, None), |(first, last)| (Some(first), Some(last)));
        Self {
            batch_id,
            first_id,
            last_id,
            len,
        }
    }

    /// Returns the logical batch ID this receipt acknowledges.
    #[must_use]
    pub fn batch_id(self) -> JournalBatchId {
        self.batch_id
    }

    /// Returns the first durable journal sequence ID, or `None` for an empty batch.
    #[must_use]
    pub fn first_id(self) -> Option<u64> {
        self.first_id
    }

    /// Returns the last durable journal sequence ID, or `None` for an empty batch.
    #[must_use]
    pub fn last_id(self) -> Option<u64> {
        self.last_id
    }

    /// Returns the number of journal records acknowledged by this receipt.
    #[must_use]
    pub fn len(self) -> usize {
        self.len
    }

    /// Returns whether this receipt acknowledges an empty batch.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    pub(crate) fn id_range(self) -> Option<RangeInclusive<u64>> {
        self.first_id
            .zip(self.last_id)
            .map(|(first, last)| first..=last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_id_bytes_round_trip() {
        let bytes = [0x5a; 16];
        let id = JournalBatchId::from_bytes(bytes);
        assert_eq!(id.as_bytes(), &bytes);
        assert_eq!(id.into_bytes(), bytes);
    }

    #[test]
    fn receipt_exposes_nonempty_and_empty_ranges() {
        let id = JournalBatchId::from_bytes([1; 16]);
        let receipt = JournalAppendReceipt::from_range(id, Some((4, 6)), 3);
        assert_eq!(receipt.batch_id(), id);
        assert_eq!(receipt.first_id(), Some(4));
        assert_eq!(receipt.last_id(), Some(6));
        assert_eq!(receipt.len(), 3);
        assert!(!receipt.is_empty());

        let empty = JournalAppendReceipt::from_range(id, None, 0);
        assert_eq!(empty.first_id(), None);
        assert_eq!(empty.last_id(), None);
        assert!(empty.is_empty());
    }
}
