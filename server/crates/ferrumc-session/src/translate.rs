//! Pure, side-effect-free translation between the network and simulation
//! vocabularies, plus the position->shard routing policy.
//!
//! These functions are the heart of the bridge and are deliberately free of any
//! channel, map, or I/O so they can be unit-tested in isolation:
//!
//! - [`net_event_to_input`] maps a [`NetEvent`] to the simulation
//!   [`GameInput`](GameInput) it represents, if any.
//! - [`output_to_clientbound`] maps a simulation [`GameOutput`](GameOutput) to
//!   the clientbound play-packet *shell* it represents, if any.
//! - [`shard_for_position`] maps a world position to the shard that owns it.
//!
//! The clientbound shells are intentionally minimal: this milestone wires the
//! plumbing, so fields that require state the router does not yet own (server-
//! allocated entity ids, real entity types, teleport-confirm ids) are filled
//! with documented placeholders. Later milestones replace the shells with fully
//! populated packets.

use ferrumc_core::PlayerId;
use ferrumc_math::{BlockPos, ShardPos, Vec3};
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, EntityVelocity, ServerboundPlayPacket, SpawnEntity,
    SynchronizePlayerPosition,
};
use ferrumc_sim::{GameInput, GameOutput};

use crate::event::NetEvent;

/// Placeholder protocol entity id used in the [`SpawnEntity`] shell.
///
/// The real id is a server-allocated [`EntityId`](ferrumc_core::EntityId) that
/// the simulation does not yet expose in its outputs; `0` stands in until it
/// does.
const PLACEHOLDER_ENTITY_ID: i32 = 0;

/// Placeholder entity-type id used in the [`SpawnEntity`] shell.
///
/// Resolving the registry id for the player entity type is a later concern; `0`
/// stands in for this milestone.
const PLACEHOLDER_ENTITY_TYPE: i32 = 0;

/// Translates a [`NetEvent`] into the simulation [`GameInput`] it represents.
///
/// Returns `None` when the event has no simulation effect yet (keep-alives,
/// chat, block interactions are decoded but not modelled this milestone). A
/// movement packet becomes a [`GameInput::PlayerMove`] and a disconnect becomes
/// a [`GameInput::PlayerLeave`].
///
/// This is a pure mapping: it does not consult or mutate the router's state, so
/// a `PlayerMove` it produces for a not-yet-joined player is still the caller's
/// to reject at routing time.
pub fn net_event_to_input(event: &NetEvent) -> Option<GameInput> {
    match event {
        NetEvent::Play { player, packet } => play_packet_to_input(*player, packet),
        NetEvent::Disconnected { player, .. } => Some(GameInput::PlayerLeave { player: *player }),
    }
}

/// Maps a serverbound play `packet` from `player` to a [`GameInput`], if any.
///
/// Both absolute-position packets collapse to a [`GameInput::PlayerMove`]; the
/// rotation is dropped because the simulation only tracks position this
/// milestone.
pub(crate) fn play_packet_to_input(
    player: PlayerId,
    packet: &ServerboundPlayPacket,
) -> Option<GameInput> {
    match packet {
        ServerboundPlayPacket::SetPlayerPosition(p) => Some(GameInput::PlayerMove {
            player,
            position: Vec3::new(p.x(), p.y(), p.z()),
        }),
        ServerboundPlayPacket::SetPlayerPositionAndRotation(p) => Some(GameInput::PlayerMove {
            player,
            position: Vec3::new(p.x(), p.y(), p.z()),
        }),
        // KeepAlive / ChatCommand / PlayerAction / UseItemOn have no simulation
        // input in this milestone.
        _ => None,
    }
}

/// Translates a simulation [`GameOutput`] into the clientbound play-packet shell
/// it represents.
///
/// Returns `None` when the output has no clientbound shell yet: a
/// [`GameOutput::PlayerDespawned`](GameOutput) maps to a remove-entities packet
/// that is not modelled this milestone, and so does any future variant until it
/// is wired. A spawn becomes a [`SpawnEntity`] shell and a move becomes a
/// [`SynchronizePlayerPosition`] shell.
pub fn output_to_clientbound(output: &GameOutput) -> Option<ClientboundPlayPacket> {
    match output {
        GameOutput::PlayerSpawned { player, position } => Some(spawn_shell(*player, *position)),
        GameOutput::PlayerMoved { position, .. } => Some(move_shell(*position)),
        // PlayerDespawned (and any future variant) has no clientbound shell yet.
        _ => None,
    }
}

/// Builds the [`SpawnEntity`] shell for a spawned player at `position`.
///
/// The player's UUID is carried through; the protocol entity id and type are
/// [placeholders](PLACEHOLDER_ENTITY_ID), and orientation/velocity are zeroed.
pub(crate) fn spawn_shell(player: PlayerId, position: Vec3) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SpawnEntity(SpawnEntity::new(
        PLACEHOLDER_ENTITY_ID,
        player.as_uuid(),
        PLACEHOLDER_ENTITY_TYPE,
        position.x,
        position.y,
        position.z,
        0,
        0,
        0,
        0,
        EntityVelocity::new(0, 0, 0),
    ))
}

/// Builds the [`SynchronizePlayerPosition`] shell for a moved player at
/// `position`.
///
/// Teleport id `0`, zero deltas (the position is absolute), and zeroed
/// orientation/flags: enough to convey the new position for this milestone.
pub(crate) fn move_shell(position: Vec3) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SynchronizePlayerPosition(SynchronizePlayerPosition::new(
        0, position.x, position.y, position.z, 0.0, 0.0, 0.0, 0.0, 0.0, 0,
    ))
}

/// Returns the [`ShardPos`] of the simulation shard that owns `position`.
///
/// This is the router's placement policy: a world position floors to its
/// [`BlockPos`], then to its [`ChunkPos`](ferrumc_math::ChunkPos), then to the
/// 8x8-chunk [`ShardPos`]. Flooring (not truncation) keeps negative coordinates
/// on the correct shard, matching the simulation's chunk ownership.
pub fn shard_for_position(position: Vec3) -> ShardPos {
    let block = BlockPos::new(
        floor_to_i32(position.x),
        floor_to_i32(position.y),
        floor_to_i32(position.z),
    );
    block.to_chunk_pos().to_shard_pos()
}

/// Floors an `f64` coordinate to the block grid as an `i32`.
///
/// Uses [`f64::floor`] so negatives round toward negative infinity (block
/// `x = -0.5` is block `-1`), matching the integer coordinate spaces' floor
/// division. Non-finite inputs are clamped to the `i32` range by the cast, which
/// is acceptable: positions are bounded by the networking layer before reaching
/// here.
fn floor_to_i32(value: f64) -> i32 {
    value.floor() as i32
}

#[cfg(test)]
mod tests {
    // Shell coordinates are exact, representable values copied verbatim from the
    // input, so exact float comparison is intentional here.
    #![allow(clippy::float_cmp)]

    use ferrumc_net::DisconnectReason;
    use ferrumc_proto::generated::play::{
        ServerboundKeepAlive, SetPlayerPosition, SetPlayerPositionAndRotation,
    };

    use super::*;

    fn player() -> PlayerId {
        PlayerId::offline("steve")
    }

    #[test]
    fn position_packet_becomes_player_move() {
        let event = NetEvent::play(
            player(),
            ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(1.0, 64.0, -2.0, 0)),
        );
        assert_eq!(
            net_event_to_input(&event),
            Some(GameInput::PlayerMove {
                player: player(),
                position: Vec3::new(1.0, 64.0, -2.0),
            })
        );
    }

    #[test]
    fn position_and_rotation_packet_becomes_player_move() {
        let event = NetEvent::play(
            player(),
            ServerboundPlayPacket::SetPlayerPositionAndRotation(SetPlayerPositionAndRotation::new(
                3.5, 70.0, 8.5, 90.0, 0.0, 1,
            )),
        );
        assert_eq!(
            net_event_to_input(&event),
            Some(GameInput::PlayerMove {
                player: player(),
                position: Vec3::new(3.5, 70.0, 8.5),
            })
        );
    }

    #[test]
    fn keep_alive_has_no_input() {
        let event = NetEvent::play(
            player(),
            ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(7)),
        );
        assert_eq!(net_event_to_input(&event), None);
    }

    #[test]
    fn disconnect_becomes_player_leave() {
        let event = NetEvent::disconnected(player(), DisconnectReason::Kicked);
        assert_eq!(
            net_event_to_input(&event),
            Some(GameInput::PlayerLeave { player: player() })
        );
    }

    #[test]
    fn spawn_output_becomes_spawn_entity_shell() {
        let out = GameOutput::PlayerSpawned {
            player: player(),
            position: Vec3::new(10.0, 65.0, -4.0),
        };
        let Some(ClientboundPlayPacket::SpawnEntity(spawn)) = output_to_clientbound(&out) else {
            panic!("expected a SpawnEntity shell");
        };
        assert_eq!(spawn.entity_uuid(), player().as_uuid());
        assert_eq!((spawn.x(), spawn.y(), spawn.z()), (10.0, 65.0, -4.0));
        assert_eq!(spawn.entity_id(), PLACEHOLDER_ENTITY_ID);
    }

    #[test]
    fn move_output_becomes_synchronize_position_shell() {
        let out = GameOutput::PlayerMoved {
            player: player(),
            position: Vec3::new(-1.0, 64.0, 1.0),
        };
        let Some(ClientboundPlayPacket::SynchronizePlayerPosition(sync)) =
            output_to_clientbound(&out)
        else {
            panic!("expected a SynchronizePlayerPosition shell");
        };
        assert_eq!((sync.x(), sync.y(), sync.z()), (-1.0, 64.0, 1.0));
        assert_eq!(sync.teleport_id(), 0);
    }

    #[test]
    fn despawn_output_has_no_shell() {
        let out = GameOutput::PlayerDespawned { player: player() };
        assert_eq!(output_to_clientbound(&out), None);
    }

    #[test]
    fn position_floors_to_shard() {
        // Chunk 0 (blocks 0..16) -> shard 0; chunk -1 (block -1) -> shard -1.
        assert_eq!(shard_for_position(Vec3::ZERO), ShardPos::new(0, 0));
        assert_eq!(
            shard_for_position(Vec3::new(-0.5, 64.0, -0.5)),
            ShardPos::new(-1, -1)
        );
        // 8 chunks * 16 blocks = 128 blocks per shard edge.
        assert_eq!(
            shard_for_position(Vec3::new(128.0, 64.0, 0.0)),
            ShardPos::new(1, 0)
        );
        assert_eq!(
            shard_for_position(Vec3::new(127.0, 64.0, 0.0)),
            ShardPos::new(0, 0)
        );
    }
}
