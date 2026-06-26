//! [`SessionRouter`] and [`PlayerSessionHandle`]: the player<->shard mapping and
//! the message-based bridge between connections and simulation shards.

use std::collections::BTreeMap;

use tokio::sync::mpsc::{self, error::TrySendError};

use ferrumc_core::PlayerId;
use ferrumc_math::{ShardPos, Vec3};
use ferrumc_proto::generated::play::ClientboundPlayPacket;
use ferrumc_sim::{GameInput, GameOutput};

use crate::error::SessionError;
use crate::event::NetEvent;
use crate::translate::{move_shell, play_packet_to_input, shard_for_position, spawn_shell};

/// Default capacity of each shard's input channel.
///
/// Sized to mirror the simulation shard inbox (1024 queued inputs): far above
/// the per-tick volume a well-behaved router produces, so hitting it signals a
/// stall or a flood — exactly when [reject backpressure](SessionError::ShardInboxFull)
/// should engage.
pub const DEFAULT_SHARD_INPUT_CAPACITY: usize = 1024;

/// Default capacity of each player's outbound channel.
///
/// A connection's writer task drains this; a moderate bound absorbs a normal
/// burst of clientbound shells while still capping a backlog. A persistently
/// full queue is the caller's cue to disconnect a client that cannot keep up.
pub const DEFAULT_OUTBOUND_CAPACITY: usize = 256;

/// The router's private per-player record: which shard owns the player and the
/// sending half of their outbound channel.
#[derive(Debug)]
struct SessionEntry {
    shard: ShardPos,
    outbound: mpsc::Sender<ClientboundPlayPacket>,
}

/// A handle to one player's session, returned by
/// [`SessionRouter::join_player`].
///
/// The connection's writer task owns this handle and drains the outbound channel
/// — with [`recv`](Self::recv) (await) or [`try_recv`](Self::try_recv)
/// (non-blocking) — to deliver clientbound packets to the client. The handle
/// also records which [`shard`](Self::shard) the player joined.
///
/// Dropping the handle closes the outbound channel; the router observes this as a
/// [`SessionError::OutboundClosed`] the next time it tries to route an output to
/// the player, and the player should then be disconnected.
#[derive(Debug)]
pub struct PlayerSessionHandle {
    player: PlayerId,
    shard: ShardPos,
    outbound: mpsc::Receiver<ClientboundPlayPacket>,
}

impl PlayerSessionHandle {
    /// The player this session belongs to.
    pub fn player(&self) -> PlayerId {
        self.player
    }

    /// The shard the player was routed to on join.
    pub fn shard(&self) -> ShardPos {
        self.shard
    }

    /// Awaits the next clientbound packet, or `None` once the router has dropped
    /// the session and the channel is drained.
    pub async fn recv(&mut self) -> Option<ClientboundPlayPacket> {
        self.outbound.recv().await
    }

    /// Returns the next queued clientbound packet without waiting, or `None` if
    /// none is ready (the queue is empty or the router has dropped the session).
    pub fn try_recv(&mut self) -> Option<ClientboundPlayPacket> {
        self.outbound.try_recv().ok()
    }
}

/// Routes network events to simulation shards and simulation outputs back to
/// connections, owning the player<->shard mapping.
///
/// The router is the only component that knows where each player lives. It never
/// holds a [`SimShard`](ferrumc_sim::SimShard), a chunk, a socket, or a database
/// handle: it communicates purely by [`GameInput`]/[`GameOutput`] messages over
/// bounded [`mpsc`] channels.
///
/// # Wiring
///
/// - [`register_shard`](Self::register_shard) creates a shard's bounded input
///   channel and hands back the receiving half for the shard worker to drain.
/// - [`join_player`](Self::join_player) places a player on the shard owning their
///   spawn position, sends a [`GameInput::PlayerJoin`], and returns a
///   [`PlayerSessionHandle`] for the connection.
/// - [`route_event`](Self::route_event) translates a [`NetEvent`] and forwards it
///   to the player's shard.
/// - [`route_output`](Self::route_output) translates a [`GameOutput`] and
///   forwards the clientbound shell to the player's connection.
/// - [`disconnect_player`](Self::disconnect_player) drops the mapping and
///   notifies the shard to despawn the player.
///
/// # Backpressure
///
/// Every channel is bounded and routing uses non-blocking sends, so the router
/// never blocks the tick loop. A full channel surfaces as a classified
/// [`SessionError`] (`ShardInboxFull` / `OutboundFull`) for the caller to act on;
/// the message is rejected, never silently dropped.
///
/// # Single-shard binding (this milestone)
///
/// A player stays bound to the shard they joined for the lifetime of the
/// session: movement routes to that shard even if the new position lies in
/// another shard's region. Cross-shard handoff is a later milestone.
#[derive(Debug)]
pub struct SessionRouter {
    shards: BTreeMap<ShardPos, mpsc::Sender<GameInput>>,
    players: BTreeMap<PlayerId, SessionEntry>,
    shard_input_capacity: usize,
    outbound_capacity: usize,
}

impl SessionRouter {
    /// Creates an empty router with the default channel capacities
    /// ([`DEFAULT_SHARD_INPUT_CAPACITY`] and [`DEFAULT_OUTBOUND_CAPACITY`]).
    pub fn new() -> Self {
        Self::with_capacities(DEFAULT_SHARD_INPUT_CAPACITY, DEFAULT_OUTBOUND_CAPACITY)
    }

    /// Creates an empty router with explicit channel capacities.
    ///
    /// Each capacity is clamped to at least `1`, since a bounded
    /// [`mpsc`] channel cannot have zero capacity.
    pub fn with_capacities(shard_input_capacity: usize, outbound_capacity: usize) -> Self {
        Self {
            shards: BTreeMap::new(),
            players: BTreeMap::new(),
            shard_input_capacity: shard_input_capacity.max(1),
            outbound_capacity: outbound_capacity.max(1),
        }
    }

    /// The configured per-shard input channel capacity.
    pub fn shard_input_capacity(&self) -> usize {
        self.shard_input_capacity
    }

    /// The configured per-player outbound channel capacity.
    pub fn outbound_capacity(&self) -> usize {
        self.outbound_capacity
    }

    /// The number of shards currently registered.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// `true` if a shard is registered at `shard`.
    pub fn is_shard_registered(&self, shard: ShardPos) -> bool {
        self.shards.contains_key(&shard)
    }

    /// The number of players with an active session.
    pub fn player_count(&self) -> usize {
        self.players.len()
    }

    /// `true` if `player` has an active session.
    pub fn is_player_connected(&self, player: PlayerId) -> bool {
        self.players.contains_key(&player)
    }

    /// The shard `player` is bound to, or `None` if they have no session.
    pub fn player_shard(&self, player: PlayerId) -> Option<ShardPos> {
        self.players.get(&player).map(|entry| entry.shard)
    }

    /// Registers a shard at `shard`, returning the receiving half of its bounded
    /// input channel.
    ///
    /// The caller (the shard worker) drains the returned receiver into the
    /// shard's inbox each tick. Registering an already-registered shard replaces
    /// its sender, closing the previous channel — so the previous receiver, if
    /// still held, observes the channel as closed.
    pub fn register_shard(&mut self, shard: ShardPos) -> mpsc::Receiver<GameInput> {
        let (tx, rx) = mpsc::channel(self.shard_input_capacity);
        self.shards.insert(shard, tx);
        rx
    }

    /// Joins `player` at `position`, routing them to the owning shard.
    ///
    /// Determines the shard from `position`, sends a [`GameInput::PlayerJoin`] to
    /// it, records the mapping, and returns a [`PlayerSessionHandle`] carrying the
    /// player's outbound channel.
    ///
    /// # Errors
    ///
    /// - [`SessionError::UnknownShard`] if no shard owns `position`.
    /// - [`SessionError::DuplicatePlayer`] if `player` already has a session.
    /// - [`SessionError::ShardInboxFull`] / [`SessionError::ShardClosed`] if the
    ///   join could not be delivered to the shard.
    ///
    /// On any error nothing is registered, so the join can be retried cleanly.
    pub fn join_player(
        &mut self,
        player: PlayerId,
        position: Vec3,
    ) -> Result<PlayerSessionHandle, SessionError> {
        let shard = shard_for_position(position);
        if !self.shards.contains_key(&shard) {
            return Err(SessionError::UnknownShard { shard });
        }
        if self.players.contains_key(&player) {
            return Err(SessionError::DuplicatePlayer { player });
        }

        // Notify the shard before recording anything: if the join cannot be
        // delivered, the player must not be left half-registered.
        self.send_to_shard(shard, GameInput::PlayerJoin { player, position })?;

        let (tx, rx) = mpsc::channel(self.outbound_capacity);
        self.players.insert(
            player,
            SessionEntry {
                shard,
                outbound: tx,
            },
        );
        Ok(PlayerSessionHandle {
            player,
            shard,
            outbound: rx,
        })
    }

    /// Translates and routes a [`NetEvent`] to the player's shard.
    ///
    /// A [`NetEvent::Disconnected`] runs the full [disconnect
    /// cleanup](Self::disconnect_player). A [`NetEvent::Play`] is translated with
    /// [`net_event_to_input`](crate::net_event_to_input); events with no
    /// simulation effect (keep-alives and the like) are a no-op `Ok(())`.
    ///
    /// # Errors
    ///
    /// - [`SessionError::UnknownPlayer`] if the event targets a player with no
    ///   session.
    /// - [`SessionError::ShardInboxFull`] / [`SessionError::ShardClosed`] if the
    ///   input could not be delivered to the shard.
    pub fn route_event(&mut self, event: &NetEvent) -> Result<(), SessionError> {
        match event {
            NetEvent::Disconnected { player, .. } => self.disconnect_player(*player).map(|_| ()),
            NetEvent::Play { player, packet } => match play_packet_to_input(*player, packet) {
                Some(input) => self.route_input(*player, input),
                None => Ok(()),
            },
        }
    }

    /// Translates and routes a simulation [`GameOutput`] to the player's
    /// connection.
    ///
    /// Outputs with no clientbound shell this milestone (a despawn, or a future
    /// variant) are a no-op `Ok(())`.
    ///
    /// # Errors
    ///
    /// - [`SessionError::UnknownPlayer`] if the output targets a player with no
    ///   session.
    /// - [`SessionError::OutboundFull`] / [`SessionError::OutboundClosed`] if the
    ///   packet could not be delivered to the connection.
    pub fn route_output(&self, output: &GameOutput) -> Result<(), SessionError> {
        match output {
            GameOutput::PlayerSpawned { player, position } => {
                self.send_outbound(*player, spawn_shell(*player, *position))
            }
            GameOutput::PlayerMoved { player, position } => {
                self.send_outbound(*player, move_shell(*position))
            }
            // No clientbound shell for a despawn (or future variant) yet.
            _ => Ok(()),
        }
    }

    /// Disconnects `player`: drops the player<->shard mapping and notifies the
    /// shard to despawn them. Returns the shard the player was on.
    ///
    /// The mapping is removed first — cleanup is the priority — and only then is
    /// the despawn [`GameInput::PlayerLeave`] sent on a best-effort basis. A send
    /// failure is surfaced so the caller knows the despawn notice was lost, but
    /// the mapping stays removed regardless.
    ///
    /// # Errors
    ///
    /// - [`SessionError::UnknownPlayer`] if `player` had no session.
    /// - [`SessionError::ShardInboxFull`] / [`SessionError::ShardClosed`] if the
    ///   despawn could not be delivered (mapping already removed).
    pub fn disconnect_player(&mut self, player: PlayerId) -> Result<ShardPos, SessionError> {
        let entry = self
            .players
            .remove(&player)
            .ok_or(SessionError::UnknownPlayer { player })?;
        self.send_to_shard(entry.shard, GameInput::PlayerLeave { player })?;
        Ok(entry.shard)
    }

    /// Routes an already-translated input to the player's bound shard.
    fn route_input(&self, player: PlayerId, input: GameInput) -> Result<(), SessionError> {
        let entry = self
            .players
            .get(&player)
            .ok_or(SessionError::UnknownPlayer { player })?;
        self.send_to_shard(entry.shard, input)
    }

    /// Non-blocking send of `input` to `shard`'s input channel.
    fn send_to_shard(&self, shard: ShardPos, input: GameInput) -> Result<(), SessionError> {
        let sender = self
            .shards
            .get(&shard)
            .ok_or(SessionError::UnknownShard { shard })?;
        sender.try_send(input).map_err(|err| match err {
            TrySendError::Full(_) => SessionError::ShardInboxFull { shard },
            TrySendError::Closed(_) => SessionError::ShardClosed { shard },
        })
    }

    /// Non-blocking send of `packet` to `player`'s outbound channel.
    fn send_outbound(
        &self,
        player: PlayerId,
        packet: ClientboundPlayPacket,
    ) -> Result<(), SessionError> {
        let entry = self
            .players
            .get(&player)
            .ok_or(SessionError::UnknownPlayer { player })?;
        entry.outbound.try_send(packet).map_err(|err| match err {
            TrySendError::Full(_) => SessionError::OutboundFull { player },
            TrySendError::Closed(_) => SessionError::OutboundClosed { player },
        })
    }
}

impl Default for SessionRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    // Decoded shell coordinates are exact, representable values, so exact float
    // comparison is intentional in these assertions.
    #![allow(clippy::float_cmp)]

    use ferrumc_net::DisconnectReason;
    use ferrumc_proto::generated::play::{ServerboundKeepAlive, ServerboundPlayPacket};

    use super::*;

    fn player(name: &str) -> PlayerId {
        PlayerId::offline(name)
    }

    /// Spawn position inside shard (0, 0): chunk 0, well within blocks 0..128.
    fn spawn_pos() -> Vec3 {
        Vec3::new(8.0, 64.0, 8.0)
    }

    #[test]
    fn join_routes_player_to_a_shard() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("alice");

        let handle = router.join_player(p, spawn_pos()).expect("join");
        assert_eq!(handle.player(), p);
        assert_eq!(handle.shard(), ShardPos::new(0, 0));

        // The mapping records the player on the shard.
        assert!(router.is_player_connected(p));
        assert_eq!(router.player_shard(p), Some(ShardPos::new(0, 0)));
        assert_eq!(router.player_count(), 1);

        // The shard's inbox received exactly the PlayerJoin input.
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin {
                player: p,
                position: spawn_pos(),
            })
        );
    }

    #[test]
    fn join_without_a_shard_is_rejected() {
        let mut router = SessionRouter::new();
        let err = router
            .join_player(player("bob"), spawn_pos())
            .expect_err("no shard registered");
        assert_eq!(
            err,
            SessionError::UnknownShard {
                shard: ShardPos::new(0, 0)
            }
        );
        assert_eq!(router.player_count(), 0);
    }

    #[test]
    fn duplicate_join_is_rejected_without_touching_state() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("carol");

        let _handle = router.join_player(p, spawn_pos()).expect("first join");
        let _ = inbox.try_recv();

        let err = router
            .join_player(p, spawn_pos())
            .expect_err("already joined");
        assert_eq!(err, SessionError::DuplicatePlayer { player: p });
        // No second PlayerJoin was sent.
        assert!(inbox.try_recv().is_err());
    }

    #[test]
    fn movement_event_routes_to_the_single_shard_as_player_move() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("dave");
        let _handle = router.join_player(p, spawn_pos()).expect("join");
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin {
                player: p,
                position: spawn_pos(),
            })
        );

        // A movement packet far inside the same shard's region.
        let move_event = NetEvent::play(
            p,
            ServerboundPlayPacket::SetPlayerPosition(
                ferrumc_proto::generated::play::SetPlayerPosition::new(20.0, 64.0, 20.0, 0),
            ),
        );
        router.route_event(&move_event).expect("route move");
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerMove {
                player: p,
                position: Vec3::new(20.0, 64.0, 20.0),
            })
        );
    }

    #[test]
    fn keep_alive_event_is_a_no_op() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("erin");
        let _handle = router.join_player(p, spawn_pos()).expect("join");
        let _ = inbox.try_recv();

        router
            .route_event(&NetEvent::play(
                p,
                ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(1)),
            ))
            .expect("route keep-alive");
        // Nothing reached the shard.
        assert!(inbox.try_recv().is_err());
    }

    #[test]
    fn play_event_for_unknown_player_is_rejected() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("ghost");
        let err = router
            .route_event(&NetEvent::play(
                p,
                ServerboundPlayPacket::SetPlayerPosition(
                    ferrumc_proto::generated::play::SetPlayerPosition::new(0.0, 0.0, 0.0, 0),
                ),
            ))
            .expect_err("no session");
        assert_eq!(err, SessionError::UnknownPlayer { player: p });
    }

    #[test]
    fn output_becomes_the_right_outbound_packet() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("frank");
        let mut handle = router.join_player(p, spawn_pos()).expect("join");

        router
            .route_output(&GameOutput::PlayerSpawned {
                player: p,
                position: Vec3::new(1.0, 2.0, 3.0),
            })
            .expect("route spawn");
        let ClientboundPlayPacket::SpawnEntity(spawn) = handle.try_recv().expect("a spawn packet")
        else {
            panic!("expected a SpawnEntity packet");
        };
        assert_eq!(spawn.entity_uuid(), p.as_uuid());
        assert_eq!((spawn.x(), spawn.y(), spawn.z()), (1.0, 2.0, 3.0));

        router
            .route_output(&GameOutput::PlayerMoved {
                player: p,
                position: Vec3::new(4.0, 5.0, 6.0),
            })
            .expect("route move");
        let ClientboundPlayPacket::SynchronizePlayerPosition(sync) =
            handle.try_recv().expect("a sync packet")
        else {
            panic!("expected a SynchronizePlayerPosition packet");
        };
        assert_eq!((sync.x(), sync.y(), sync.z()), (4.0, 5.0, 6.0));
    }

    #[test]
    fn despawn_output_sends_nothing() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("grace");
        let mut handle = router.join_player(p, spawn_pos()).expect("join");

        router
            .route_output(&GameOutput::PlayerDespawned { player: p })
            .expect("route despawn");
        assert!(handle.try_recv().is_none());
    }

    #[test]
    fn output_for_unknown_player_is_rejected() {
        let router = SessionRouter::new();
        let err = router
            .route_output(&GameOutput::PlayerMoved {
                player: player("nobody"),
                position: Vec3::ZERO,
            })
            .expect_err("no session");
        assert_eq!(
            err,
            SessionError::UnknownPlayer {
                player: player("nobody")
            }
        );
    }

    #[test]
    fn disconnect_cleans_up_the_mapping_and_despawns() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("heidi");
        let _handle = router.join_player(p, spawn_pos()).expect("join");
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin {
                player: p,
                position: spawn_pos(),
            })
        );
        assert!(router.is_player_connected(p));

        let shard = router.disconnect_player(p).expect("disconnect");
        assert_eq!(shard, ShardPos::new(0, 0));

        // Mapping is gone.
        assert!(!router.is_player_connected(p));
        assert_eq!(router.player_shard(p), None);
        assert_eq!(router.player_count(), 0);

        // The shard was told to despawn the player.
        assert_eq!(inbox.try_recv(), Ok(GameInput::PlayerLeave { player: p }));
    }

    #[test]
    fn disconnect_event_routes_through_cleanup() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("ivan");
        let _handle = router.join_player(p, spawn_pos()).expect("join");
        let _ = inbox.try_recv();

        router
            .route_event(&NetEvent::disconnected(p, DisconnectReason::ServerShutdown))
            .expect("route disconnect");
        assert!(!router.is_player_connected(p));
        assert_eq!(inbox.try_recv(), Ok(GameInput::PlayerLeave { player: p }));
    }

    #[test]
    fn disconnect_unknown_player_is_rejected() {
        let mut router = SessionRouter::new();
        let err = router
            .disconnect_player(player("nobody"))
            .expect_err("no session");
        assert_eq!(
            err,
            SessionError::UnknownPlayer {
                player: player("nobody")
            }
        );
    }

    #[test]
    fn shard_inbox_full_is_classified_reject_backpressure() {
        // Capacity 1: the join fills the channel, the next input is rejected.
        let mut router = SessionRouter::with_capacities(1, 16);
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("judy");
        let _handle = router.join_player(p, spawn_pos()).expect("join");

        // The channel now holds the join; a movement input overflows it.
        let err = router
            .route_event(&NetEvent::play(
                p,
                ServerboundPlayPacket::SetPlayerPosition(
                    ferrumc_proto::generated::play::SetPlayerPosition::new(1.0, 1.0, 1.0, 0),
                ),
            ))
            .expect_err("inbox full");
        assert_eq!(
            err,
            SessionError::ShardInboxFull {
                shard: ShardPos::new(0, 0)
            }
        );
    }

    #[test]
    fn outbound_full_is_classified_reject_backpressure() {
        let mut router = SessionRouter::with_capacities(16, 1);
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("kate");
        let _handle = router.join_player(p, spawn_pos()).expect("join");

        // First output fills the capacity-1 outbound channel.
        router
            .route_output(&GameOutput::PlayerMoved {
                player: p,
                position: Vec3::ZERO,
            })
            .expect("first output fits");
        // Second overflows it.
        let err = router
            .route_output(&GameOutput::PlayerMoved {
                player: p,
                position: Vec3::new(1.0, 1.0, 1.0),
            })
            .expect_err("outbound full");
        assert_eq!(err, SessionError::OutboundFull { player: p });
    }

    #[test]
    fn dropped_handle_closes_the_outbound_channel() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("leo");
        let handle = router.join_player(p, spawn_pos()).expect("join");
        drop(handle);

        let err = router
            .route_output(&GameOutput::PlayerMoved {
                player: p,
                position: Vec3::ZERO,
            })
            .expect_err("channel closed");
        assert_eq!(err, SessionError::OutboundClosed { player: p });
    }

    #[test]
    fn defaults_match_documented_capacities() {
        let router = SessionRouter::default();
        assert_eq!(router.shard_input_capacity(), DEFAULT_SHARD_INPUT_CAPACITY);
        assert_eq!(router.outbound_capacity(), DEFAULT_OUTBOUND_CAPACITY);
        assert_eq!(router.shard_count(), 0);
        assert_eq!(router.player_count(), 0);
    }

    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let router = SessionRouter::with_capacities(0, 0);
        assert_eq!(router.shard_input_capacity(), 1);
        assert_eq!(router.outbound_capacity(), 1);
    }

    #[tokio::test]
    async fn handle_recv_awaits_a_routed_output() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("mallory");
        let mut handle = router.join_player(p, spawn_pos()).expect("join");

        router
            .route_output(&GameOutput::PlayerMoved {
                player: p,
                position: Vec3::new(7.0, 8.0, 9.0),
            })
            .expect("route move");

        let packet = handle.recv().await.expect("a packet");
        let ClientboundPlayPacket::SynchronizePlayerPosition(sync) = packet else {
            panic!("expected a SynchronizePlayerPosition packet");
        };
        assert_eq!((sync.x(), sync.y(), sync.z()), (7.0, 8.0, 9.0));
    }
}
