//! Bounded, deterministic cross-shard messages applied at tick boundaries.
//!
//! Shards produce [`CrossShardIntent`] values without choosing their source,
//! source-local sequence, or production tick. The scheduler assigns those
//! values when it builds a [`CrossShardEnvelope`]. Accepted envelopes always
//! target exactly the tick after they were produced.

#![allow(
    dead_code,
    reason = "the Packet 42 cross-shard carrier is crate-private shadow plumbing without app wiring"
)]

use std::num::NonZeroUsize;

use ferrumc_core::Tick;

use crate::message::GameInput;
use crate::ownership::{ShardId, ShardLifecycleState};

/// A typed operation delivered to another simulation shard.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CrossShardPayload {
    /// Apply an existing simulation input at the destination tick boundary.
    ApplyInput(GameInput),
}

impl CrossShardPayload {
    /// Consumes this payload and returns its destination input.
    pub(crate) fn into_input(self) -> GameInput {
        match self {
            Self::ApplyInput(input) => input,
        }
    }
}

/// A shard-produced request whose source metadata is assigned by the scheduler.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CrossShardIntent {
    destination: ShardId,
    payload: Box<CrossShardPayload>,
}

impl CrossShardIntent {
    /// Creates an intent targeting `destination`.
    ///
    /// The payload is boxed because `GameInput` deliberately carries bounded
    /// fixed-size sign text inline; keeping the owned rejection type pointer-sized
    /// avoids inflating every `Result` that returns a rejected intent.
    pub(crate) fn new(destination: ShardId, payload: CrossShardPayload) -> Self {
        Self {
            destination,
            payload: Box::new(payload),
        }
    }

    /// Returns the intended destination shard.
    pub(crate) const fn destination(&self) -> ShardId {
        self.destination
    }

    /// Consumes this intent into its destination and payload.
    pub(crate) fn into_parts(self) -> (ShardId, CrossShardPayload) {
        (self.destination, *self.payload)
    }
}

/// A scheduler-stamped message for exactly one future tick boundary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CrossShardEnvelope {
    produced_at: Tick,
    apply_at: Tick,
    source: ShardId,
    destination: ShardId,
    source_sequence: usize,
    payload: CrossShardPayload,
}

impl CrossShardEnvelope {
    /// Stamps an intent with scheduler-owned source and tick metadata.
    ///
    /// The original intent is returned intact when `produced_at` has no next
    /// tick. Every successfully constructed envelope therefore carries exactly
    /// `produced_at + 1` as its application boundary.
    pub(crate) fn from_intent(
        produced_at: Tick,
        source: ShardId,
        source_sequence: usize,
        intent: CrossShardIntent,
    ) -> Result<Self, CrossShardIntent> {
        let Some(apply_at) = produced_at.next() else {
            return Err(intent);
        };
        let (destination, payload) = intent.into_parts();
        Ok(Self {
            produced_at,
            apply_at,
            source,
            destination,
            source_sequence,
            payload,
        })
    }

    /// Returns the tick during which the source produced this message.
    pub(crate) const fn produced_at(&self) -> Tick {
        self.produced_at
    }

    /// Returns the exact boundary at which the destination may apply it.
    pub(crate) const fn apply_at(&self) -> Tick {
        self.apply_at
    }

    /// Returns the source shard assigned by the scheduler.
    pub(crate) const fn source(&self) -> ShardId {
        self.source
    }

    /// Returns the destination shard selected by the source intent.
    pub(crate) const fn destination(&self) -> ShardId {
        self.destination
    }

    /// Returns the source-local emission order within `produced_at`.
    pub(crate) const fn source_sequence(&self) -> usize {
        self.source_sequence
    }

    /// Borrows the typed destination payload.
    pub(crate) const fn payload(&self) -> &CrossShardPayload {
        &self.payload
    }

    /// Returns the canonical admission and application ordering key.
    pub(crate) const fn canonical_key(&self) -> (ShardId, ShardId, usize) {
        (self.destination, self.source, self.source_sequence)
    }

    /// Returns payload-independent identity for stale-boundary detection.
    ///
    /// Payloads contain floating-point values, so value equality is not
    /// reflexive for every valid bit pattern (notably IEEE NaNs). Scheduler
    /// commit identity therefore uses only immutable, scheduler-stamped
    /// metadata.
    const fn commit_identity(&self) -> (Tick, Tick, ShardId, ShardId, usize) {
        (
            self.produced_at,
            self.apply_at,
            self.source,
            self.destination,
            self.source_sequence,
        )
    }
}

/// Why a cross-shard envelope was not admitted for future application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrossShardRejectionReason {
    /// The bounded queue retained its existing entries and rejected this new one.
    QueueFull {
        /// Configured logical envelope capacity.
        capacity: usize,
    },
    /// A shard attempted to route an input back to itself.
    SameShard,
    /// The destination did not exist in the scheduler directory.
    UnknownDestination,
    /// The destination existed but could not accept new cross-shard work.
    DestinationNotActive {
        /// Lifecycle state observed during scheduler validation.
        state: ShardLifecycleState,
    },
    /// The production tick was the maximum representable tick.
    NoNextTick,
}

/// A rejected message retaining ownership of its complete envelope.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CrossShardRejection {
    reason: CrossShardRejectionReason,
    envelope: CrossShardEnvelope,
}

impl CrossShardRejection {
    /// Couples an intact rejected envelope to its classified reason.
    pub(crate) const fn new(
        reason: CrossShardRejectionReason,
        envelope: CrossShardEnvelope,
    ) -> Self {
        Self { reason, envelope }
    }

    /// Returns the classified rejection reason.
    pub(crate) const fn reason(&self) -> CrossShardRejectionReason {
        self.reason
    }

    /// Borrows the intact rejected envelope.
    pub(crate) const fn envelope(&self) -> &CrossShardEnvelope {
        &self.envelope
    }

    /// Consumes the rejection and returns the intact envelope.
    pub(crate) fn into_envelope(self) -> CrossShardEnvelope {
        self.envelope
    }
}

/// A non-mutating snapshot of the messages ready for one exact boundary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedBoundary {
    apply_at: Tick,
    envelopes: Vec<CrossShardEnvelope>,
}

impl PreparedBoundary {
    /// Returns the boundary represented by this snapshot.
    pub(crate) const fn apply_at(&self) -> Tick {
        self.apply_at
    }

    /// Borrows ready envelopes in canonical order.
    pub(crate) fn envelopes(&self) -> &[CrossShardEnvelope] {
        &self.envelopes
    }

    /// Returns the number of ready envelopes.
    pub(crate) const fn len(&self) -> usize {
        self.envelopes.len()
    }

    /// Returns whether no envelope is ready at this boundary.
    pub(crate) const fn is_empty(&self) -> bool {
        self.envelopes.is_empty()
    }
}

/// A logically bounded queue of accepted future-boundary envelopes.
///
/// Admission never evicts an existing entry. A completed tick's candidate
/// envelopes are sorted by destination, source, and source-local sequence before
/// capacity is assigned; overflow therefore has a deterministic winner set
/// independent of worker completion order.
#[derive(Debug)]
pub(crate) struct CrossShardQueue {
    capacity: usize,
    pending: Vec<CrossShardEnvelope>,
}

impl CrossShardQueue {
    /// Creates an empty queue with a non-zero logical capacity.
    pub(crate) const fn new(capacity: NonZeroUsize) -> Self {
        Self {
            capacity: capacity.get(),
            pending: Vec::new(),
        }
    }

    /// Returns the configured logical envelope capacity.
    pub(crate) const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of accepted envelopes awaiting commit.
    pub(crate) const fn len(&self) -> usize {
        self.pending.len()
    }

    /// Returns whether no envelope awaits a future boundary.
    pub(crate) const fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Returns whether an admitted envelope still targets `destination`.
    ///
    /// Draining shards use this to remain runnable until work admitted while
    /// they were active reaches its promised boundary.
    pub(crate) fn has_destination(&self, destination: ShardId) -> bool {
        self.pending
            .iter()
            .any(|envelope| envelope.destination() == destination)
    }

    /// Admits one completed tick's envelopes in canonical order.
    ///
    /// The scheduler validates destination existence and lifecycle before this
    /// call. This queue additionally rejects same-shard routing, a completed tick
    /// with no successor, and capacity overflow. Existing entries are never
    /// displaced; rejected envelopes are returned intact.
    pub(crate) fn admit_completed_tick(
        &mut self,
        completed_tick: Tick,
        mut envelopes: Vec<CrossShardEnvelope>,
    ) -> Vec<CrossShardRejection> {
        envelopes.sort_by_key(CrossShardEnvelope::canonical_key);
        let next_boundary = completed_tick.next();
        let mut rejections = Vec::new();

        for envelope in envelopes {
            let reason = if next_boundary.is_none() {
                Some(CrossShardRejectionReason::NoNextTick)
            } else if envelope.source() == envelope.destination() {
                Some(CrossShardRejectionReason::SameShard)
            } else if self.pending.len() >= self.capacity {
                Some(CrossShardRejectionReason::QueueFull {
                    capacity: self.capacity,
                })
            } else {
                None
            };

            if let Some(reason) = reason {
                rejections.push(CrossShardRejection::new(reason, envelope));
            } else {
                self.pending.push(envelope);
            }
        }

        self.pending.sort_by_key(|envelope| {
            (
                envelope.apply_at(),
                envelope.destination(),
                envelope.source(),
                envelope.source_sequence(),
            )
        });
        rejections
    }

    /// Snapshots canonical messages for `apply_at` without removing them.
    ///
    /// The bounded clone lets the scheduler validate and stage every destination
    /// input before advancing its coordinator. If that preparation fails, the
    /// queue remains byte-for-byte unchanged.
    pub(crate) fn prepare_boundary(&self, apply_at: Tick) -> PreparedBoundary {
        let envelopes = self
            .pending
            .iter()
            .filter(|envelope| envelope.apply_at() == apply_at)
            .cloned()
            .collect();
        PreparedBoundary {
            apply_at,
            envelopes,
        }
    }

    /// Removes exactly the envelopes represented by an unchanged preparation.
    ///
    /// Returns `false` and removes nothing if messages for that boundary changed
    /// after preparation. The scheduler calls this only after the coordinator
    /// successfully advances to [`PreparedBoundary::apply_at`].
    pub(crate) fn commit_boundary(&mut self, prepared: &PreparedBoundary) -> bool {
        let unchanged = self
            .pending
            .iter()
            .filter(|envelope| envelope.apply_at() == prepared.apply_at())
            .map(CrossShardEnvelope::commit_identity)
            .eq(prepared
                .envelopes()
                .iter()
                .map(CrossShardEnvelope::commit_identity));
        if !unchanged {
            return false;
        }

        self.pending
            .retain(|envelope| envelope.apply_at() != prepared.apply_at());
        true
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use ferrumc_core::{DimensionId, PlayerId, Tick, WorldId};
    use ferrumc_math::ShardPos;

    use super::{
        CrossShardEnvelope, CrossShardIntent, CrossShardPayload, CrossShardQueue,
        CrossShardRejectionReason,
    };
    use crate::{GameInput, ShardId};

    fn shard(x: i32) -> ShardId {
        ShardId::try_new(WorldId::new(0), DimensionId::new(0), ShardPos::new(x, 0))
            .expect("valid test shard")
    }

    fn intent(destination: ShardId, player: &str) -> CrossShardIntent {
        CrossShardIntent::new(
            destination,
            CrossShardPayload::ApplyInput(GameInput::PlayerLeave {
                player: PlayerId::offline(player),
            }),
        )
    }

    fn envelope(
        produced_at: Tick,
        source: ShardId,
        destination: ShardId,
        source_sequence: usize,
        player: &str,
    ) -> CrossShardEnvelope {
        CrossShardEnvelope::from_intent(
            produced_at,
            source,
            source_sequence,
            intent(destination, player),
        )
        .expect("test tick has a successor")
    }

    #[test]
    fn envelope_applies_at_exactly_the_next_tick_and_prepare_is_nonmutating() {
        let source = shard(0);
        let destination = shard(1);
        let produced_at = Tick::new(41);
        let first_envelope = envelope(produced_at, source, destination, 0, "next-tick");

        assert_eq!(first_envelope.produced_at(), produced_at);
        assert_eq!(first_envelope.apply_at(), Tick::new(42));
        assert_eq!(first_envelope.source(), source);
        assert_eq!(first_envelope.destination(), destination);

        let mut queue = CrossShardQueue::new(NonZeroUsize::new(2).expect("nonzero test capacity"));
        assert!(queue
            .admit_completed_tick(produced_at, vec![first_envelope.clone()])
            .is_empty());
        assert_eq!(queue.len(), 1);
        assert!(queue.prepare_boundary(produced_at).is_empty());

        let prepared = queue.prepare_boundary(Tick::new(42));
        assert_eq!(prepared.envelopes(), std::slice::from_ref(&first_envelope));
        assert_eq!(queue.len(), 1, "prepare must not remove the message");
        let admitted_after_prepare = envelope(produced_at, source, destination, 1, "late");
        assert!(queue
            .admit_completed_tick(produced_at, vec![admitted_after_prepare.clone()])
            .is_empty());
        assert!(
            !queue.commit_boundary(&prepared),
            "a stale preparation must not remove a changed boundary"
        );
        assert_eq!(queue.len(), 2);

        let refreshed = queue.prepare_boundary(Tick::new(42));
        assert_eq!(
            refreshed.envelopes(),
            &[first_envelope, admitted_after_prepare]
        );
        assert!(queue.commit_boundary(&refreshed));
        assert!(queue.is_empty());

        let overflow_intent = intent(destination, "overflow");
        assert_eq!(
            CrossShardEnvelope::from_intent(
                Tick::new(u64::MAX),
                source,
                1,
                overflow_intent.clone(),
            ),
            Err(overflow_intent),
        );
    }

    #[test]
    fn capacity_and_same_shard_rejections_return_the_intact_envelope() {
        let source_a = shard(0);
        let source_b = shard(1);
        let destination = shard(2);
        let capacity = NonZeroUsize::new(2).expect("nonzero capacity");
        let produced_at = Tick::new(7);
        let rejected = envelope(produced_at, source_b, destination, 0, "rejected");
        let mut queue = CrossShardQueue::new(capacity);

        let rejections = queue.admit_completed_tick(
            produced_at,
            vec![
                rejected.clone(),
                envelope(produced_at, source_a, destination, 1, "accepted-second"),
                envelope(produced_at, source_a, destination, 0, "accepted-first"),
            ],
        );

        assert_eq!(queue.capacity(), 2);
        assert_eq!(queue.len(), 2);
        assert_eq!(rejections.len(), 1);
        assert_eq!(
            rejections[0].reason(),
            CrossShardRejectionReason::QueueFull { capacity: 2 }
        );
        assert_eq!(rejections[0].envelope(), &rejected);
        assert_eq!(rejections[0].clone().into_envelope(), rejected);

        let same_shard = envelope(produced_at, source_a, source_a, 2, "same-shard");
        let same_shard_rejections =
            queue.admit_completed_tick(produced_at, vec![same_shard.clone()]);
        assert_eq!(same_shard_rejections.len(), 1);
        assert_eq!(
            same_shard_rejections[0].reason(),
            CrossShardRejectionReason::SameShard
        );
        assert_eq!(same_shard_rejections[0].envelope(), &same_shard);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn boundary_order_is_destination_then_source_then_source_sequence() {
        let source_a = shard(0);
        let source_b = shard(1);
        let destination_a = shard(2);
        let destination_b = shard(3);
        let produced_at = Tick::new(15);
        let mut queue = CrossShardQueue::new(NonZeroUsize::new(4).expect("nonzero capacity"));

        assert!(queue
            .admit_completed_tick(
                produced_at,
                vec![
                    envelope(produced_at, source_b, destination_b, 0, "fourth"),
                    envelope(produced_at, source_b, destination_a, 0, "third"),
                    envelope(produced_at, source_a, destination_a, 1, "second"),
                    envelope(produced_at, source_a, destination_a, 0, "first"),
                ],
            )
            .is_empty());

        let prepared = queue.prepare_boundary(Tick::new(16));
        let order: Vec<_> = prepared
            .envelopes()
            .iter()
            .map(CrossShardEnvelope::canonical_key)
            .collect();
        assert_eq!(
            order,
            vec![
                (destination_a, source_a, 0),
                (destination_a, source_a, 1),
                (destination_a, source_b, 0),
                (destination_b, source_b, 0),
            ]
        );
    }
}
