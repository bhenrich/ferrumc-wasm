//! [`PlayWriter`]: queues clientbound play packets into bounded per-priority
//! queues and drains them, in strict priority order, into encoded frame batches.
//!
//! The writer is the queuing counterpart to the M08 [`OutboundEncoder`]: callers
//! [`enqueue`](PlayWriter::enqueue) typed [`ClientboundPlayPacket`]s tagged with
//! an [`OutboundPriority`], and [`drain_batch`](PlayWriter::drain_batch) encodes
//! as many as fit a batch — highest priority first — into a single length-
//! delimited byte buffer ready for the socket. It performs no I/O.
//!
//! ## Backpressure and drop policy
//!
//! Each priority has a **bounded** queue (see the `DEFAULT_*_CAPACITY` constants
//! in [`crate::OutboundPriority`]). The simulation enqueues without awaiting, so
//! a full queue cannot block a sim worker; instead the *incoming* packet is
//! dropped (tail-drop) and a per-priority drop counter is incremented. Tail-drop
//! preserves the order of already-queued frames, which matters because play
//! packet order is significant (a block update must not overtake the chunk it
//! sits in). Dropping state/world/cosmetic frames is tolerable — the client
//! recovers via a later update or simply misses a visual effect — but a *full*
//! [`Critical`](crate::OutboundPriority::Critical) queue means the client cannot
//! drain even keep-alives; the caller should escalate that to a
//! [`DisconnectReason::OutboundOverflow`](crate::DisconnectReason).

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};

use ferrumc_proto::generated::play::ClientboundPlayPacket;

use crate::compression::CompressionState;
use crate::error::{EncodeError, FrameEncodeError};
use crate::limits::ConnectionLimits;
use crate::outbound::{OutboundEncoder, OutboundPacket};

use super::metrics::PlayMetrics;
use super::priority::{OutboundPriority, PRIORITY_COUNT};

/// Default soft cap on the on-wire bytes drained into one batch: 64 KiB.
///
/// Matches the networking model's batching threshold. It is a *soft* cap: a
/// single frame larger than the threshold is still emitted on its own (so the
/// queue always makes progress), but the writer stops adding frames once the
/// batch would exceed it.
pub const DEFAULT_BATCH_MAX_BYTES: usize = 64 * 1024;

/// Default cap on the number of frames drained into one batch: 128.
///
/// Bounds the work and latency of a single drain even when frames are tiny.
pub const DEFAULT_BATCH_MAX_FRAMES: usize = 128;

/// The thresholds that bound a single [`drain_batch`](PlayWriter::drain_batch).
///
/// A drain stops at whichever limit is reached first: the cumulative on-wire byte
/// size ([`max_bytes`](Self::max_bytes), a soft cap) or the frame count
/// ([`max_frames`](Self::max_frames), a hard cap). Both are clamped to at least
/// `1` so a batch can always emit a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchLimits {
    max_bytes: usize,
    max_frames: usize,
}

impl BatchLimits {
    /// Builds batch limits, clamping each to at least `1`.
    pub fn new(max_bytes: usize, max_frames: usize) -> Self {
        Self {
            max_bytes: max_bytes.max(1),
            max_frames: max_frames.max(1),
        }
    }

    /// The soft cap on cumulative on-wire batch size, in bytes.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// The hard cap on the number of frames per batch.
    pub fn max_frames(&self) -> usize {
        self.max_frames
    }
}

impl Default for BatchLimits {
    /// [`DEFAULT_BATCH_MAX_BYTES`] and [`DEFAULT_BATCH_MAX_FRAMES`].
    fn default() -> Self {
        Self::new(DEFAULT_BATCH_MAX_BYTES, DEFAULT_BATCH_MAX_FRAMES)
    }
}

/// The result of [`enqueue`](PlayWriter::enqueue)ing a clientbound packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// The packet was appended to its priority queue.
    Enqueued {
        /// The queue depth (packet count) after the append.
        depth: usize,
    },
    /// The priority queue was at capacity; the packet was dropped (tail-drop).
    Dropped {
        /// The priority whose queue overflowed.
        priority: OutboundPriority,
    },
}

impl EnqueueOutcome {
    /// `true` when the packet was dropped because its queue was full.
    pub fn is_dropped(self) -> bool {
        matches!(self, Self::Dropped { .. })
    }

    /// `true` when the packet was enqueued.
    pub fn is_enqueued(self) -> bool {
        matches!(self, Self::Enqueued { .. })
    }
}

/// A drained batch of encoded clientbound frames.
///
/// [`bytes`](Self::bytes) is one contiguous buffer of back-to-back length-
/// delimited frames, ready to hand to the socket write side;
/// [`frame_count`](Self::frame_count) is how many frames it holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayBatch {
    bytes: BytesMut,
    frame_count: usize,
}

impl PlayBatch {
    /// The concatenated, length-delimited frame bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The number of frames in the batch.
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// `true` when no frames were drained.
    pub fn is_empty(&self) -> bool {
        self.frame_count == 0
    }

    /// Consumes the batch and returns its owned byte buffer.
    pub fn into_bytes(self) -> BytesMut {
        self.bytes
    }
}

/// Bounded, priority-ordered queue of clientbound play packets with batched
/// draining.
///
/// Construct with [`with_defaults`](Self::with_defaults) for the documented
/// capacities and batch thresholds, or [`new`](Self::new) to override them. See
/// the [module docs](self) for the drop/backpressure policy.
#[derive(Debug)]
pub struct PlayWriter {
    queues: [VecDeque<ClientboundPlayPacket>; PRIORITY_COUNT],
    capacities: [usize; PRIORITY_COUNT],
    batch: BatchLimits,
    encoder: OutboundEncoder,
    metrics: PlayMetrics,
}

impl PlayWriter {
    /// Creates a writer with explicit per-priority `capacities` and batch limits.
    ///
    /// `capacities` is indexed by [`OutboundPriority::index`]; build it with
    /// [`capacities`](Self::capacities_from) if you prefer naming each class.
    pub fn new(
        limits: ConnectionLimits,
        capacities: [usize; PRIORITY_COUNT],
        batch: BatchLimits,
    ) -> Self {
        Self {
            queues: std::array::from_fn(|_| VecDeque::new()),
            capacities,
            batch,
            encoder: OutboundEncoder::new(limits),
            metrics: PlayMetrics::new(),
        }
    }

    /// Creates a writer with the documented default capacities and batch limits.
    pub fn with_defaults(limits: ConnectionLimits) -> Self {
        Self::new(limits, Self::default_capacities(), BatchLimits::default())
    }

    /// The documented default per-priority capacities, indexed by
    /// [`OutboundPriority::index`].
    pub fn default_capacities() -> [usize; PRIORITY_COUNT] {
        Self::capacities_from(
            super::priority::DEFAULT_CRITICAL_CAPACITY,
            super::priority::DEFAULT_STATE_CAPACITY,
            super::priority::DEFAULT_WORLD_CAPACITY,
            super::priority::DEFAULT_COSMETIC_CAPACITY,
        )
    }

    /// Builds a capacity array from per-class values in priority order.
    pub fn capacities_from(
        critical: usize,
        state: usize,
        world: usize,
        cosmetic: usize,
    ) -> [usize; PRIORITY_COUNT] {
        [critical, state, world, cosmetic]
    }

    /// This writer's metrics counters.
    pub fn metrics(&self) -> &PlayMetrics {
        &self.metrics
    }

    /// The batch limits this writer drains under.
    pub fn batch_limits(&self) -> BatchLimits {
        self.batch
    }

    /// The configured capacity of `priority`'s queue.
    pub fn capacity(&self, priority: OutboundPriority) -> usize {
        self.capacities[priority.index()]
    }

    /// The current depth (packet count) of `priority`'s queue.
    pub fn queued_len(&self, priority: OutboundPriority) -> usize {
        self.queues[priority.index()].len()
    }

    /// The total number of packets queued across all priorities.
    pub fn total_queued(&self) -> usize {
        self.queues.iter().map(VecDeque::len).sum()
    }

    /// Enqueues `packet` at `priority`, applying the tail-drop policy when the
    /// queue is full.
    ///
    /// Returns [`EnqueueOutcome::Enqueued`] with the new depth, or
    /// [`EnqueueOutcome::Dropped`] (incrementing the priority's drop counter)
    /// when the queue is at capacity. See the [module docs](self) for why drops
    /// are preferred to blocking, and when a drop should be escalated to a
    /// disconnect.
    pub fn enqueue(
        &mut self,
        priority: OutboundPriority,
        packet: ClientboundPlayPacket,
    ) -> EnqueueOutcome {
        let idx = priority.index();
        if self.queues[idx].len() >= self.capacities[idx] {
            self.metrics.record_drop(priority);
            return EnqueueOutcome::Dropped { priority };
        }
        self.queues[idx].push_back(packet);
        EnqueueOutcome::Enqueued {
            depth: self.queues[idx].len(),
        }
    }

    /// Enqueues `packet` at its default priority
    /// ([`OutboundPriority::for_packet`]).
    pub fn enqueue_classified(&mut self, packet: ClientboundPlayPacket) -> EnqueueOutcome {
        let priority = OutboundPriority::for_packet(&packet);
        self.enqueue(priority, packet)
    }

    /// Drains queued packets — highest priority first — into one encoded
    /// [`PlayBatch`], stopping at the configured [`BatchLimits`].
    ///
    /// Each packet is encoded through `compression` (pass
    /// [`CompressionState::disabled`] before negotiation). The drain visits
    /// priorities in strict order and never sends a lower-priority frame ahead of
    /// a still-queued higher-priority one: if the next frame would push the batch
    /// past the soft byte cap (and the batch is non-empty), the drain stops and
    /// leaves the rest queued for the next call.
    ///
    /// Returns a [`FrameEncodeError`] if a packet fails to encode — a server-side
    /// fault. The offending packet remains at the front of its queue; the
    /// connection layer treats an encode failure as fatal and closes the socket,
    /// so this does not retry-loop.
    pub fn drain_batch(
        &mut self,
        compression: &CompressionState,
    ) -> Result<PlayBatch, FrameEncodeError> {
        let mut out = BytesMut::new();
        let mut frame_count = 0usize;

        'outer: for priority in OutboundPriority::ALL {
            let idx = priority.index();
            loop {
                if frame_count >= self.batch.max_frames {
                    break 'outer;
                }
                let Some(front) = self.queues[idx].front() else {
                    break;
                };

                // Encode into a scratch buffer first so the size is known before
                // committing the frame to the batch (the front packet is not
                // popped until it is accepted).
                let body = encode_play_body(front)?;
                let mut scratch = BytesMut::new();
                self.encoder.encode_compressed(
                    &OutboundPacket::Play(body),
                    &mut scratch,
                    compression,
                )?;

                // Soft byte cap: never displace a still-queued higher-priority
                // frame, but always emit at least one frame so the queue drains.
                if frame_count > 0 && out.len() + scratch.len() > self.batch.max_bytes {
                    break 'outer;
                }

                let scratch_len = scratch.len();
                out.extend_from_slice(&scratch);
                self.queues[idx].pop_front();
                frame_count += 1;
                self.metrics.record_frame_encoded(scratch_len);
            }
        }

        if frame_count > 0 {
            self.metrics.record_batch();
        }
        Ok(PlayBatch {
            bytes: out,
            frame_count,
        })
    }
}

/// Serializes a clientbound play packet's `id + fields` body into owned bytes.
///
/// A proto encode failure (e.g. an NBT encode error) is surfaced as a
/// [`FrameEncodeError`] — a server-side fault, since outbound data is the
/// server's own.
fn encode_play_body(packet: &ClientboundPlayPacket) -> Result<Bytes, FrameEncodeError> {
    let mut body = BytesMut::new();
    packet
        .encode(&mut body)
        .map_err(EncodeError::from)
        .map_err(FrameEncodeError::from)?;
    Ok(body.freeze())
}

#[cfg(test)]
mod tests {
    use ferrumc_proto::generated::play::ClientboundKeepAlive;

    use super::*;

    /// A clientbound `KeepAlive` packet (the simplest typed play packet).
    fn keep_alive(id: i64) -> ClientboundPlayPacket {
        ClientboundPlayPacket::ClientboundKeepAlive(ClientboundKeepAlive::new(id))
    }

    /// A writer with the given per-class capacities and the given batch limits.
    fn writer(caps: [usize; PRIORITY_COUNT], batch: BatchLimits) -> PlayWriter {
        PlayWriter::new(ConnectionLimits::default(), caps, batch)
    }

    #[test]
    fn enqueue_reports_depth_and_drains_in_order() {
        let mut w = PlayWriter::with_defaults(ConnectionLimits::default());
        assert_eq!(
            w.enqueue(OutboundPriority::World, keep_alive(1)),
            EnqueueOutcome::Enqueued { depth: 1 }
        );
        assert_eq!(
            w.enqueue(OutboundPriority::World, keep_alive(2)),
            EnqueueOutcome::Enqueued { depth: 2 }
        );
        assert_eq!(w.total_queued(), 2);
    }

    #[test]
    fn full_queue_drops_the_incoming_packet() {
        // Capacity 2 on the critical queue.
        let mut w = writer(
            PlayWriter::capacities_from(2, 2, 2, 2),
            BatchLimits::default(),
        );
        assert!(w
            .enqueue(OutboundPriority::Critical, keep_alive(1))
            .is_enqueued());
        assert!(w
            .enqueue(OutboundPriority::Critical, keep_alive(2))
            .is_enqueued());
        // The third overflows: tail-dropped, queue unchanged, counter incremented.
        let outcome = w.enqueue(OutboundPriority::Critical, keep_alive(3));
        assert_eq!(
            outcome,
            EnqueueOutcome::Dropped {
                priority: OutboundPriority::Critical
            }
        );
        assert_eq!(w.queued_len(OutboundPriority::Critical), 2);
        assert_eq!(w.metrics().dropped(OutboundPriority::Critical), 1);
        assert_eq!(w.metrics().dropped_total(), 1);
    }

    #[test]
    fn drain_empties_critical_before_lower_priorities() {
        // One frame per drain so the depletion order is observable.
        let mut w = writer(
            PlayWriter::capacities_from(8, 8, 8, 8),
            BatchLimits::new(64 * 1024, 1),
        );
        // Enqueue out of priority order on purpose.
        w.enqueue(OutboundPriority::World, keep_alive(1));
        w.enqueue(OutboundPriority::Cosmetic, keep_alive(2));
        w.enqueue(OutboundPriority::Critical, keep_alive(3));
        w.enqueue(OutboundPriority::State, keep_alive(4));

        let disabled = CompressionState::disabled();

        // 1st drain pulls the Critical frame.
        let b1 = w.drain_batch(&disabled).unwrap();
        assert_eq!(b1.frame_count(), 1);
        assert_eq!(w.queued_len(OutboundPriority::Critical), 0);
        assert_eq!(w.queued_len(OutboundPriority::State), 1);

        // 2nd drain pulls State.
        w.drain_batch(&disabled).unwrap();
        assert_eq!(w.queued_len(OutboundPriority::State), 0);
        assert_eq!(w.queued_len(OutboundPriority::World), 1);

        // 3rd drain pulls World, leaving only Cosmetic.
        w.drain_batch(&disabled).unwrap();
        assert_eq!(w.queued_len(OutboundPriority::World), 0);
        assert_eq!(w.queued_len(OutboundPriority::Cosmetic), 1);

        // 4th drain pulls Cosmetic last.
        w.drain_batch(&disabled).unwrap();
        assert_eq!(w.total_queued(), 0);
        assert_eq!(w.metrics().batches_flushed(), 4);
    }

    #[test]
    fn batch_stops_at_the_frame_count_threshold() {
        let mut w = writer(
            PlayWriter::capacities_from(16, 16, 16, 16),
            BatchLimits::new(64 * 1024, 2),
        );
        for i in 0..5 {
            w.enqueue(OutboundPriority::Critical, keep_alive(i));
        }
        let batch = w.drain_batch(&CompressionState::disabled()).unwrap();
        assert_eq!(batch.frame_count(), 2, "frame-count threshold");
        assert_eq!(
            w.queued_len(OutboundPriority::Critical),
            3,
            "rest left queued"
        );
        assert_eq!(w.metrics().frames_encoded(), 2);
    }

    #[test]
    fn batch_stops_at_the_byte_threshold() {
        // Each KeepAlive frame is 10 bytes on the wire (1 prefix + 1 id + 8 i64).
        let mut w = writer(
            PlayWriter::capacities_from(16, 16, 16, 16),
            BatchLimits::new(25, 128),
        );
        for i in 0..5 {
            w.enqueue(OutboundPriority::Critical, keep_alive(i));
        }
        let batch = w.drain_batch(&CompressionState::disabled()).unwrap();
        // Frame 1 (10) + frame 2 (20) fit; frame 3 (30) exceeds 25 and stops it.
        assert_eq!(batch.frame_count(), 2);
        assert_eq!(batch.bytes().len(), 20);
    }

    #[test]
    fn a_single_oversized_frame_is_still_emitted() {
        // A 1-byte soft cap is below any real frame, but the first frame must
        // still go out so the queue can make progress.
        let mut w = writer(
            PlayWriter::capacities_from(4, 4, 4, 4),
            BatchLimits::new(1, 128),
        );
        w.enqueue(OutboundPriority::Critical, keep_alive(1));
        let batch = w.drain_batch(&CompressionState::disabled()).unwrap();
        assert_eq!(batch.frame_count(), 1);
        assert!(batch.bytes().len() > 1);
    }

    #[test]
    fn draining_an_empty_writer_yields_an_empty_batch() {
        let mut w = PlayWriter::with_defaults(ConnectionLimits::default());
        let batch = w.drain_batch(&CompressionState::disabled()).unwrap();
        assert!(batch.is_empty());
        assert_eq!(batch.frame_count(), 0);
        // An empty drain is not counted as a flushed batch.
        assert_eq!(w.metrics().batches_flushed(), 0);
    }

    #[test]
    fn enqueue_classified_routes_keep_alive_to_critical() {
        let mut w = PlayWriter::with_defaults(ConnectionLimits::default());
        w.enqueue_classified(keep_alive(1));
        assert_eq!(w.queued_len(OutboundPriority::Critical), 1);
    }

    #[test]
    fn drained_bytes_round_trip_through_the_decoder() {
        use crate::inbound::{decode_inbound_frame, DecodeOutcome, InboundPacket};
        use crate::state::ConnectionState;

        let mut w = PlayWriter::with_defaults(ConnectionLimits::default());
        w.enqueue(OutboundPriority::Critical, keep_alive(0x4242));
        let batch = w.drain_batch(&CompressionState::disabled()).unwrap();

        // The drained frame decodes as a raw play frame whose body is the
        // KeepAlive id + payload.
        let outcome = decode_inbound_frame(
            batch.bytes(),
            ConnectionState::Play,
            &ConnectionLimits::default(),
        )
        .unwrap();
        let DecodeOutcome::Decoded { packet, consumed } = outcome else {
            panic!("expected a decoded frame");
        };
        assert_eq!(consumed, batch.bytes().len());
        let InboundPacket::Play(body) = packet else {
            panic!("expected a raw play body");
        };
        // The framed body is the clientbound KeepAlive id followed by its i64
        // payload, proving the typed packet survived encoding and framing.
        assert_eq!(body[0], ClientboundKeepAlive::PACKET_ID as u8);
        assert_eq!(body.len(), 1 + 8);
    }
}
