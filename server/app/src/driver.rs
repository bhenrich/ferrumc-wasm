//! The single-owner simulation/session driver task.
//!
//! One task owns the [`SessionRouter`] and the single [`SimShard`], so neither
//! is shared behind a lock. Connection tasks reach it only through a bounded
//! [`SimCommand`] channel:
//!
//! ```text
//!   connection --SimCommand::Join-->  driver  -- router.join_player --> shard inbox
//!   connection --SimCommand::Event--> driver  -- router.route_event --> shard inbox
//!                                        |
//!                                   (every tick) drain inbox -> run_tick ->
//!                                   route_output -> player outbound channels
//! ```
//!
//! The driver advances the shard on a fixed interval with **no catch-up**
//! ([`MissedTickBehavior::Skip`]), matching the project's overload rule, and
//! never blocks: channel sends are non-blocking inside the router, and a full
//! inbox defers inputs rather than stalling.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;

use ferrumc_core::PlayerId;
use ferrumc_math::Vec3;
use ferrumc_session::{NetEvent, PlayerSessionHandle, SessionError, SessionRouter};
use ferrumc_sim::{GameInput, SimShard};

/// A request from a connection task to the simulation/session driver.
///
/// The enum is the only way a connection influences simulation state; it carries
/// no sockets and no shard handles.
pub(crate) enum SimCommand {
    /// Place a player at `position` and hand back their session handle.
    Join {
        /// The joining player's identity.
        player: PlayerId,
        /// The world-space position to join at.
        position: Vec3,
        /// One-shot channel the driver replies on with the new session handle (or
        /// a classified routing error).
        reply: oneshot::Sender<Result<PlayerSessionHandle, SessionError>>,
    },
    /// Route a translated network event (a play packet or a disconnect) to the
    /// player's shard.
    Event(NetEvent),
}

/// Runs the driver loop until `shutdown` flips or every command sender drops.
///
/// Owns `router`, `shard`, the shard input receiver `shard_rx` (drained each
/// tick), and the `commands` channel. The loop prioritises shutdown, then a due
/// tick, then a pending command, so a command flood can never starve the
/// simulation.
pub(crate) async fn run(
    mut router: SessionRouter,
    mut shard: SimShard,
    mut shard_rx: mpsc::Receiver<GameInput>,
    mut commands: mpsc::Receiver<SimCommand>,
    tick_period: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(tick_period);
    // Lag must not trigger catch-up ticks: skip missed deadlines instead.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = ticker.tick() => run_tick(&mut router, &mut shard, &mut shard_rx),
            maybe_command = commands.recv() => match maybe_command {
                Some(command) => handle_command(&mut router, command),
                None => break,
            },
        }
    }
}

/// Applies one command against the router.
fn handle_command(router: &mut SessionRouter, command: SimCommand) {
    match command {
        SimCommand::Join {
            player,
            position,
            reply,
        } => {
            let result = router.join_player(player, position);
            // The connection task may have already gone away; a failed reply send
            // means the join handle is simply discarded.
            let _ = reply.send(result);
        }
        SimCommand::Event(event) => {
            if let Err(err) = router.route_event(&event) {
                tracing::trace!(%err, "dropping network event");
            }
        }
    }
}

/// Drains queued inputs into the shard, advances one tick, and routes outputs.
fn run_tick(
    router: &mut SessionRouter,
    shard: &mut SimShard,
    shard_rx: &mut mpsc::Receiver<GameInput>,
) {
    // Move everything the router queued since the last tick into the inbox; a
    // full inbox stops the drain (reject backpressure) and retries next tick.
    while let Ok(input) = shard_rx.try_recv() {
        if let Err(err) = shard.enqueue(input) {
            tracing::warn!(%err, "shard inbox full; deferring inputs to next tick");
            break;
        }
    }

    let outputs = shard.run_tick();
    // Routing an output may fan out to many viewers; any whose connection has
    // closed are returned so we can schedule a clean despawn for each.
    let mut closed = Vec::new();
    for output in &outputs {
        closed.extend(router.route_output(output));
    }
    for player in closed {
        let _ = router.disconnect_player(player);
    }
}
