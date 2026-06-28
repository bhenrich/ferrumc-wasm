//! Clientbound play-phase outbound helpers: traced enqueue, the mandatory-drop
//! overflow backstop, the block-action ack, queue-depth sampling, and the
//! socket flush.

use std::time::Duration;

use tokio::net::TcpStream;

use ferrumc_net::{CompressionState, Criticality, EnqueueOutcome, OutboundPriority, PlayWriter};
use ferrumc_observability::{ConnNetTelemetry, ServerClock, SessionDebug};
use ferrumc_proto::generated::play::{AcknowledgeBlockChange, ClientboundPlayPacket};

use crate::observe;

use super::context::ConnContext;
use super::write_all;

/// Enqueues an `AcknowledgeBlockChange` echoing `sequence`, ending the client's
/// optimistic prediction for that block action.
///
/// Sent as a *mandatory* frame (via [`send_mandatory`]): the ack is precisely the
/// packet that terminates the client's optimistic prediction, so a silent tail-drop
/// at a full outbound queue would strand the predicted (broken, placed, replaced,
/// or no-op) block as a ghost forever. Escalating a dropped ack to an outbound
/// overflow matches both the `Mandatory` criticality the router already tags onto
/// this packet ([`Criticality::for_packet`](ferrumc_net::Criticality::for_packet))
/// and the sim's own block-change rejection path, which forces the same heal-ack
/// mandatory for the same reason.
pub(super) fn ack_sequence(
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    clock: &ServerClock,
    sequence: i32,
) -> anyhow::Result<()> {
    send_mandatory(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::AcknowledgeBlockChange(AcknowledgeBlockChange::new(sequence)),
    )
}

/// Enqueues a mandatory clientbound packet, escalating a tail-drop at a full queue
/// to an outbound overflow (see [`is_mandatory_overflow`]).
///
/// The connection-originated inventory packets (join container content, the
/// creative-slot echo, the click resync) are authoritative state: a silent drop
/// would desync the client's inventory view, so a dropped mandatory frame here is
/// the same fatal condition the keep-alive and router paths already enforce. The
/// block-action heal-ack ([`ack_sequence`]) routes through here for the same
/// reason: dropping it strands the client's optimistic block prediction as a ghost.
pub(super) fn send_mandatory(
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    clock: &ServerClock,
    packet: ClientboundPlayPacket,
) -> anyhow::Result<()> {
    let criticality = Criticality::for_packet(&packet);
    let outcome = enqueue_traced_classified(writer, debug, compression, clock, packet);
    if is_mandatory_overflow(criticality, outcome) {
        return Err(anyhow::anyhow!(
            "outbound overflow: a mandatory inventory packet was dropped at the connection writer"
        ));
    }
    Ok(())
}

/// Enqueues `packet` at its default priority, recording an outbound trace only
/// when it is actually queued.
///
/// Returns the [`EnqueueOutcome`] so the caller can gate per-packet counters
/// (e.g. `ferrumc_chunk_sent_total`) on a real enqueue. A tail-dropped packet
/// (queue at capacity) is neither traced nor counted: the disconnect dump and the
/// send counters then reflect what entered the outbound pipeline rather than
/// intent, so backpressure cannot inflate them.
pub(super) fn enqueue_traced_classified(
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    clock: &ServerClock,
    packet: ClientboundPlayPacket,
) -> EnqueueOutcome {
    // Build the trace before the packet is moved into the queue; recording it is
    // deferred until the enqueue is known to have succeeded.
    let trace = observe::trace_outbound_play(&packet, compression, clock);
    let outcome = writer.enqueue_classified(packet);
    if outcome.is_enqueued() {
        debug.record_outbound(trace);
    }
    outcome
}

/// Whether a Layer-B (connection writer) enqueue `outcome` for a packet of the
/// given `criticality` must escalate to a
/// [`DisconnectReason::OutboundOverflow`](ferrumc_net::DisconnectReason::OutboundOverflow).
///
/// The per-player outbound *channel* (Layer A, the session router) already
/// guarantees mandatory packets are delivered-or-disconnect, but the connection
/// writer ([`PlayWriter`], Layer B) tail-drops a full priority queue silently.
/// This is the backstop that turns a dropped *mandatory* frame — a despawn, spawn,
/// ack, correction, or the keep-alive — into the documented outbound overflow
/// rather than a silent drop that would ghost an entity, leave an invisible body,
/// or strand a prediction. Droppable frames (movement, chunks, chat) may still
/// shed without disconnecting.
///
/// The `criticality` is the one the router tagged onto the packet's
/// [`OutboundMessage`](ferrumc_session::OutboundMessage) envelope at the send
/// site, **not** [`Criticality::for_packet`](ferrumc_net::Criticality::for_packet).
/// So a context-dependent frame is escalated exactly when the router meant it to
/// be: an actor-resync `BlockUpdate` carries `Mandatory` and is escalated here,
/// while the same packet type sent as a viewer broadcast carries `Droppable` and
/// is allowed to shed.
pub(super) fn is_mandatory_overflow(criticality: Criticality, outcome: EnqueueOutcome) -> bool {
    outcome.is_dropped() && matches!(criticality, Criticality::Mandatory)
}

/// Enqueues `packet` at an explicit priority, recording an outbound trace only
/// when it is actually queued (see [`enqueue_traced_classified`] for the
/// drop-vs-trace policy).
pub(super) fn enqueue_traced(
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    clock: &ServerClock,
    priority: OutboundPriority,
    packet: ClientboundPlayPacket,
) -> EnqueueOutcome {
    let trace = observe::trace_outbound_play(&packet, compression, clock);
    let outcome = writer.enqueue(priority, packet);
    if outcome.is_enqueued() {
        debug.record_outbound(trace);
    }
    outcome
}

/// Samples the writer's outbound queue depth into the per-session dump and the
/// `ferrumc_session_outbound_queue_len{session}` aggregate gauge, and republishes
/// this connection's network telemetry into the shared hub for the live snapshot.
///
/// Called at each flush boundary — not per packet — so publishing a fresh
/// telemetry snapshot here is off the hot path. The per-connection counters and
/// packet-name tallies are read straight from the writer's metrics and the
/// session debug recorder (which already funnels every traced packet), so no new
/// per-packet bookkeeping is added; the hub merges these into the per-tick
/// `ServerSnapshot` and prunes the session when the connection disconnects.
///
/// `over_budget` is the serverbound packet budget's running over-budget tally for
/// this connection (the reader's counter, not the writer's), surfaced per player
/// so the dashboard reflects a throttled/flooding client.
pub(super) fn observe_queue_len(
    debug: &mut SessionDebug,
    ctx: &ConnContext,
    writer: &PlayWriter,
    over_budget: u64,
) {
    let depth = writer.total_queued();
    debug.observe_outbound_queue_len(depth);
    ctx.metrics.observe_outbound_queue_len(depth);

    let metrics = writer.metrics();
    let mut dropped = [0u64; 4];
    for priority in OutboundPriority::ALL {
        dropped[priority.index()] = metrics.dropped(priority);
    }
    ctx.net_telemetry.publish(ConnNetTelemetry {
        session: debug.session().to_owned(),
        frames_in: debug.inbound_frames(),
        bytes_in: debug.inbound_bytes(),
        frames_out: metrics.frames_encoded(),
        bytes_out: metrics.bytes_out(),
        over_budget,
        dropped,
        queue_depth: depth as u64,
        inbound: debug.inbound_tally().clone(),
        outbound: debug.outbound_tally().clone(),
    });
}

/// Drains the writer into back-to-back batches and writes each to the socket.
pub(super) async fn flush_writer(
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    compression: &CompressionState,
    io_timeout: Duration,
) -> anyhow::Result<()> {
    loop {
        let batch = writer.drain_batch(compression)?;
        if batch.is_empty() {
            break;
        }
        write_all(stream, batch.bytes(), io_timeout).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn mandatory_layer_b_drop_escalates_droppable_does_not() {
        use ferrumc_net::{Criticality, EnqueueOutcome, OutboundPriority};

        use super::is_mandatory_overflow;

        let dropped = EnqueueOutcome::Dropped {
            priority: OutboundPriority::State,
        };
        let enqueued = EnqueueOutcome::Enqueued { depth: 1 };

        // A dropped mandatory frame is an outbound overflow; a dropped droppable
        // frame is tolerated, and a successfully enqueued frame never escalates.
        assert!(is_mandatory_overflow(Criticality::Mandatory, dropped));
        assert!(!is_mandatory_overflow(Criticality::Droppable, dropped));
        assert!(!is_mandatory_overflow(Criticality::Mandatory, enqueued));
        assert!(!is_mandatory_overflow(Criticality::Droppable, enqueued));
    }

    #[test]
    fn actor_resync_envelope_escalates_at_layer_b_despite_droppable_type() {
        // Acceptance 5a: the actor-resync `BlockUpdate` rides a (Mandatory, State)
        // envelope, so Layer B escalates a dropped resync to an outbound overflow —
        // never silently dropped while its ack survives — even though the packet
        // TYPE defaults to (Droppable, World). This is the seam the envelope closes.
        use ferrumc_net::{Criticality, EnqueueOutcome, OutboundPriority};
        use ferrumc_proto::generated::play::{BlockUpdate, ClientboundPlayPacket};
        use ferrumc_proto::types::BlockPosition;
        use ferrumc_session::OutboundMessage;

        use super::is_mandatory_overflow;

        let resync = OutboundMessage::new(
            ClientboundPlayPacket::BlockUpdate(BlockUpdate::new(BlockPosition::new(8, 63, 8), 1)),
            Criticality::Mandatory,
            OutboundPriority::State,
        );

        // The carried criticality is Mandatory while the type default is Droppable:
        // re-inferring from the type (the old Layer-B bug) would mis-drop the resync.
        assert_eq!(resync.criticality(), Criticality::Mandatory);
        assert_eq!(
            Criticality::for_packet(resync.packet()),
            Criticality::Droppable
        );

        // With the carried criticality, a dropped resync at a full State queue is an
        // outbound overflow (disconnect), not a silent drop.
        let dropped_state = EnqueueOutcome::Dropped {
            priority: OutboundPriority::State,
        };
        assert!(is_mandatory_overflow(resync.criticality(), dropped_state));
    }
}
