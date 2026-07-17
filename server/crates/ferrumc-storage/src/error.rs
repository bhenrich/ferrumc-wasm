//! The classifying error type for the storage layer.

use ferrumc_core::ServerError;

use crate::JournalBatchId;

/// A classified storage failure.
///
/// Every variant names *why* an operation failed so callers can react
/// programmatically instead of parsing message strings. Storage methods on the
/// public traits surface [`ferrumc_core::ServerError`]; this type is the
/// crate-internal classification that converts into it through
/// [`From<StorageError> for ServerError`](ServerError). Constructors such as
/// [`crate::StorageKey::new`] and [`crate::EntityRecord::new`] return it
/// directly so callers can match on the precise reason.
///
/// The enum is `#[non_exhaustive]`, so new variants may be added later and
/// downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// A plugin storage key exceeded the maximum accepted byte length and was
    /// rejected before being stored, to bound work done on untrusted input.
    #[error("storage key too long: {len} bytes (maximum {max})")]
    KeyTooLong {
        /// Length of the rejected key, in bytes.
        len: usize,
        /// Maximum accepted key length, in bytes.
        max: usize,
    },

    /// A plugin storage value exceeded the maximum accepted byte length and was
    /// rejected before being stored.
    #[error("storage value too large: {len} bytes (maximum {max})")]
    ValueTooLarge {
        /// Length of the rejected value, in bytes.
        len: usize,
        /// Maximum accepted value length, in bytes.
        max: usize,
    },

    /// A record's serialized payload exceeded the maximum accepted byte length
    /// and was rejected when the record was constructed.
    #[error("record payload too large: {len} bytes (maximum {max})")]
    RecordTooLarge {
        /// Length of the rejected payload, in bytes.
        len: usize,
        /// Maximum accepted payload length, in bytes.
        max: usize,
    },

    /// A batched save request carried more items than the backend accepts in one
    /// call and was rejected before any item was processed.
    #[error("save batch too large: {len} items (maximum {max})")]
    BatchTooLarge {
        /// Number of items in the rejected batch.
        len: usize,
        /// Maximum accepted number of items per batch.
        max: usize,
    },

    /// The durable block-mutation journal has no sequence IDs left for the
    /// requested batch. The append is rejected before any record is written.
    #[error(
        "mutation journal sequence exhausted after id {last_id} while allocating {requested} records"
    )]
    JournalSequenceExhausted {
        /// Greatest sequence ID that was already durably allocated.
        last_id: u64,
        /// Number of records the rejected append attempted to allocate.
        requested: usize,
    },

    /// A supposedly fresh journal sequence ID already existed on disk.
    ///
    /// This indicates corrupt or inconsistent sequence metadata. The whole
    /// append transaction is rejected rather than replacing history.
    #[error("mutation journal sequence id {id} already exists")]
    JournalIdCollision {
        /// Durable sequence ID that unexpectedly already existed.
        id: u64,
    },

    /// An idempotency token was reused with a different normalized mutation
    /// payload than the batch it already identifies.
    #[error("mutation journal batch id {batch_id} was reused with a different payload")]
    JournalBatchConflict {
        /// Reused idempotency token.
        batch_id: JournalBatchId,
    },

    /// A persisted journal receipt did not have the required fixed-width encoding.
    #[error(
        "malformed mutation journal receipt for batch {batch_id}: {len} bytes (expected {expected})"
    )]
    MalformedJournalReceipt {
        /// Batch whose receipt was malformed.
        batch_id: JournalBatchId,
        /// Length of the persisted receipt.
        len: usize,
        /// Required fixed-width receipt length.
        expected: usize,
    },

    /// A persisted journal receipt described an impossible or over-limit range.
    #[error(
        "invalid mutation journal receipt range for batch {batch_id}: first={first_id}, count={count}"
    )]
    InvalidJournalReceiptRange {
        /// Batch whose receipt was inconsistent.
        batch_id: JournalBatchId,
        /// First durable ID encoded by the receipt.
        first_id: u64,
        /// Number of records encoded by the receipt.
        count: u64,
    },

    /// A receipt referenced a journal record that was not present.
    #[error("mutation journal receipt for batch {batch_id} references missing record {id}")]
    JournalReceiptMissingRecord {
        /// Batch whose committed range was incomplete.
        batch_id: JournalBatchId,
        /// Missing durable journal sequence ID.
        id: u64,
    },

    /// A persisted mutation-journal key did not have the required eight-byte
    /// big-endian representation.
    #[error("malformed mutation journal key: {len} bytes (expected 8)")]
    MalformedJournalKey {
        /// Length of the malformed durable key, in bytes.
        len: usize,
    },

    /// The storage backend failed in an unexpected way (for example, an internal
    /// lock was poisoned by a panic in another thread). The payload describes
    /// the failure for diagnostics.
    #[error("storage backend failure: {0}")]
    Backend(String),
}

impl StorageError {
    /// Builds a [`StorageError::Backend`] describing an unexpected backend
    /// failure.
    pub fn backend(detail: impl Into<String>) -> Self {
        Self::Backend(detail.into())
    }
}

impl From<StorageError> for ServerError {
    fn from(err: StorageError) -> Self {
        let message = err.to_string();
        match err {
            // Every bound rejection is a capacity-class failure: the request
            // asked for more than a bounded resource permits.
            StorageError::KeyTooLong { .. }
            | StorageError::ValueTooLarge { .. }
            | StorageError::RecordTooLarge { .. }
            | StorageError::BatchTooLarge { .. }
            | StorageError::JournalSequenceExhausted { .. } => Self::capacity(message),
            // A backend failure is an internal invariant violation.
            StorageError::JournalIdCollision { .. }
            | StorageError::MalformedJournalReceipt { .. }
            | StorageError::InvalidJournalReceiptRange { .. }
            | StorageError::JournalReceiptMissingRecord { .. }
            | StorageError::MalformedJournalKey { .. }
            | StorageError::Backend(_) => Self::internal(message),
            StorageError::JournalBatchConflict { .. } => Self::invalid_state(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_errors_map_to_capacity() {
        for err in [
            StorageError::KeyTooLong { len: 9, max: 8 },
            StorageError::ValueTooLarge { len: 9, max: 8 },
            StorageError::RecordTooLarge { len: 9, max: 8 },
            StorageError::BatchTooLarge { len: 9, max: 8 },
            StorageError::JournalSequenceExhausted {
                last_id: u64::MAX,
                requested: 1,
            },
        ] {
            let converted: ServerError = err.into();
            assert!(matches!(converted, ServerError::Capacity(_)));
        }
    }

    #[test]
    fn backend_error_maps_to_internal() {
        let err = StorageError::backend("lock poisoned");
        assert_eq!(err.to_string(), "storage backend failure: lock poisoned");
        let converted: ServerError = err.into();
        assert!(matches!(converted, ServerError::Internal { .. }));
    }

    #[test]
    fn journal_collision_maps_to_internal() {
        let err = StorageError::JournalIdCollision { id: 7 };
        let converted: ServerError = err.into();
        assert!(matches!(converted, ServerError::Internal { .. }));
    }

    #[test]
    fn journal_batch_conflict_maps_to_invalid_state() {
        let err = StorageError::JournalBatchConflict {
            batch_id: JournalBatchId::from_bytes([7; 16]),
        };
        let converted: ServerError = err.into();
        assert!(matches!(converted, ServerError::InvalidState(_)));
    }

    #[test]
    fn malformed_journal_key_maps_to_internal() {
        let err = StorageError::MalformedJournalKey { len: 7 };
        let converted: ServerError = err.into();
        assert!(matches!(converted, ServerError::Internal { .. }));
    }
}
