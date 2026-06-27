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
use ferrumc_math::{BlockPos, ChunkPos, Direction, ShardPos, Vec3};
use ferrumc_proto::generated::play::{
    AcknowledgeBlockChange, BlockUpdate, ClientboundPlayPacket, EntityTeleport, EntityVelocity,
    PlayerAction, PlayerInfoUpdate, RemoveEntities, RemovePlayerInfo, ServerboundPlayPacket,
    SpawnEntity, SynchronizePlayerPosition, UpdateEntityPosition, UseItemOn,
};
use ferrumc_proto::types::BlockPosition;
use ferrumc_sim::{BlockStateId, GameInput, GameOutput};

use crate::event::NetEvent;

/// Placeholder protocol entity id used in the [`SpawnEntity`] shell built by
/// [`output_to_clientbound`].
///
/// The router proper allocates a real, per-player network id when it broadcasts
/// a remote player to viewers (see [`entity_spawn_shell`]); this constant only
/// backs the standalone, state-free [`output_to_clientbound`] mapping that has no
/// router to consult.
const PLACEHOLDER_ENTITY_ID: i32 = 0;

/// Entity-type id used for a player in a [`SpawnEntity`] shell.
///
/// `minecraft:player` is numeric entity-type id `149` in 1.21.8 / protocol 772.
/// Entity-type ids are not carried in `protocol.json` (it only describes packet
/// and field shapes), so the registry data is the ground truth; this value is
/// verified against two independent sources pinned at protocol 772:
/// `ref/assets/extracted/entities.json` (`data["player"]["id"] == 149`) and
/// `ref/assets/data/registries.json`
/// (`["minecraft:entity_type"]["entries"]["minecraft:player"]["protocol_id"] == 149`).
/// Sanity: the registry holds 151 types (ids `0..=150`) with `148 =
/// zombified_piglin`, `149 = player`, `150 = fishing_bobber`; id `0` is
/// `acacia_boat`, which is why the previous placeholder of `0` spawned a boat.
const PLAYER_ENTITY_TYPE: i32 = 149;

/// The vanilla "Add Player" player-list action bit.
const ADD_PLAYER: u8 = 0x01;

/// The vanilla "Update Listed" player-list action bit: when set, the per-player
/// body carries a trailing `Listed` boolean and the client shows the player in
/// the tab list.
const UPDATE_LISTED: u8 = 0x08;

/// The `action` byte a [`player_info_add`] packet carries: "Add Player" plus
/// "Update Listed" (`0x01 | 0x08 == 0x09`).
///
/// The `action` field of a Player Info Update is an 8-bit `EnumSet`; with these
/// two bits set the per-player body is `name` + empty `properties` array (from
/// Add Player) followed by a `Listed` boolean (from Update Listed). The trailing
/// boolean is mandatory: once `0x08` is asserted a real client reads it after the
/// properties array, so omitting it desynchronizes the decoder.
pub const PLAYER_INFO_ADD: u8 = ADD_PLAYER | UPDATE_LISTED;

/// The `PlayerAction` status meaning "start destroying block".
///
/// This server advertises creative mode, where a block is destroyed the instant
/// digging starts, so this is the status the break path keys on. Other statuses
/// (cancel/finish digging, item drops, swap-hand, ...) have no simulation effect
/// this milestone.
const START_DESTROY_BLOCK: i32 = 0;

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
/// Both absolute-position packets collapse to a [`GameInput::PlayerMove`] (the
/// rotation is dropped because the simulation only tracks position this
/// milestone). A dig-start `PlayerAction` becomes a [`GameInput::BlockBreak`]
/// and a `UseItemOn` becomes a [`GameInput::BlockPlace`]; both carry a typed
/// target [`BlockPos`].
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
        ServerboundPlayPacket::PlayerAction(p) => block_break_input(player, p),
        ServerboundPlayPacket::UseItemOn(p) => block_place_input(player, p),
        // KeepAlive / ChatCommand have no simulation input in this milestone.
        _ => None,
    }
}

/// Maps a serverbound `PlayerAction` to a [`GameInput::BlockBreak`], if it is a
/// dig-start.
///
/// Only the [`START_DESTROY_BLOCK`] status breaks a block (creative insta-mine,
/// the mode this server advertises); every other status yields `None`. The wire
/// block position is converted to a typed [`BlockPos`].
fn block_break_input(player: PlayerId, packet: &PlayerAction) -> Option<GameInput> {
    if packet.status() != START_DESTROY_BLOCK {
        return None;
    }
    Some(GameInput::BlockBreak {
        player,
        position: block_pos(packet.location()),
        // The client stamps each block action with a sequence; carry it so an
        // accepted break can be acknowledged back to this player.
        sequence: packet.sequence(),
    })
}

/// Maps a serverbound `UseItemOn` to a [`GameInput::BlockPlace`] at the block
/// adjacent to the clicked face.
///
/// The target is the clicked block stepped one block along the clicked face, so
/// a place never overwrites the block clicked on (matching vanilla). A face
/// index outside the canonical `0..6` range is malformed and yields `None`. The
/// block actually placed is the simulation's fixed default this milestone, so it
/// is not carried here.
fn block_place_input(player: PlayerId, packet: &UseItemOn) -> Option<GameInput> {
    let face = face_from_index(packet.direction())?;
    Some(GameInput::BlockPlace {
        player,
        position: block_pos(packet.location()).offset(face),
        // The client stamps each block action with a sequence; carry it so an
        // accepted place can be acknowledged back to this player.
        sequence: packet.sequence(),
    })
}

/// Converts a wire [`BlockPosition`] into a typed [`BlockPos`].
fn block_pos(pos: BlockPosition) -> BlockPos {
    BlockPos::new(pos.x(), pos.y(), pos.z())
}

/// Maps a protocol face index to a [`Direction`], or `None` if it is out of the
/// canonical `0..6` range.
///
/// The protocol encodes faces in Minecraft's canonical order, which is exactly
/// [`Direction::ALL`], so the index is a direct lookup into it.
fn face_from_index(index: i32) -> Option<Direction> {
    let index = usize::try_from(index).ok()?;
    Direction::ALL.get(index).copied()
}

/// Translates a simulation [`GameOutput`] into the clientbound play-packet shell
/// it represents.
///
/// Returns `None` when the output has no clientbound shell yet: a
/// [`GameOutput::PlayerDespawned`](GameOutput) maps to a remove-entities packet
/// that is not modelled this milestone, and so does any future variant until it
/// is wired. A spawn becomes a [`SpawnEntity`] shell; a move and a position
/// correction both become a [`SynchronizePlayerPosition`] shell (a correction is
/// just an authoritative resync to the player's last accepted position); a block
/// change becomes a [`BlockUpdate`].
///
/// This is the state-free standalone mapping: the [`SynchronizePlayerPosition`]
/// shell self-teleports the moved actor, which is only meaningful as a
/// position correction. The live send path (`SessionRouter::route_output`) does
/// **not** route [`GameOutput::PlayerMoved`] through here — it broadcasts the move
/// to other viewers with the relative/teleport movement shells below — so the
/// `PlayerMoved` arm here is exercised only by this module's unit tests.
pub fn output_to_clientbound(output: &GameOutput) -> Option<ClientboundPlayPacket> {
    match output {
        GameOutput::PlayerSpawned { player, position } => Some(entity_spawn_shell(
            PLACEHOLDER_ENTITY_ID,
            *player,
            *position,
        )),
        GameOutput::PlayerMoved { position, .. }
        | GameOutput::PlayerPositionCorrected { position, .. } => Some(move_shell(*position)),
        GameOutput::BlockChanged {
            position, state, ..
        } => Some(block_update_shell(*position, *state)),
        // PlayerDespawned, BlockChangeRejected (a targeted resync the router
        // addresses to the actor, not a standalone shell), and any future variant
        // have no clientbound shell here yet.
        _ => None,
    }
}

/// Builds the [`SpawnEntity`] shell that makes `player` visible to a viewer at
/// `position`, tagged with the server-allocated network `entity_id`.
///
/// The player's UUID is carried through; the type is [`PLAYER_ENTITY_TYPE`] (so
/// the client renders a player model and skin) and orientation/velocity are
/// zeroed. This is sent on the player's first appearance to a viewer; subsequent
/// moves are conveyed with the relative/teleport movement shells below, never a
/// re-sent spawn (which a real client ignores for an already-known entity).
pub(crate) fn entity_spawn_shell(
    entity_id: i32,
    player: PlayerId,
    position: Vec3,
) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SpawnEntity(SpawnEntity::new(
        entity_id,
        player.as_uuid(),
        PLAYER_ENTITY_TYPE,
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

/// Builds the [`PlayerInfoUpdate`] "Add Player" packet that adds `player`
/// (rendered with `name`) to a viewer's tab list.
///
/// The generated proto exposes [`PlayerInfoUpdate`] as an opaque
/// `(action, entries)` shell, so this hand-encodes the per-action body. With
/// `action = `[`PLAYER_INFO_ADD`]` (Add Player | Update Listed)` the body is a
/// `VarInt` entry count of `1`, then for the one player: the 16-byte UUID, the
/// `name` (a `VarInt` length prefix then its UTF-8 bytes), an empty `properties`
/// array (`VarInt` count `0`), and finally the `Listed` boolean (`1`/true).
///
/// The trailing `Listed` byte is required: the Update Listed bit (`0x08`) is set,
/// so a real client reads a boolean after the properties array. Omitting it — or
/// stopping the body after the UUID — leaves the client decoding past the end of
/// the buffer, which is the Netty `DecoderException` that disconnects both
/// players when a second one becomes visible.
///
/// Exported so the app can also send a player their *own* entry on join, putting
/// the local player on their tab list.
pub fn player_info_add(player: PlayerId, name: &str) -> ClientboundPlayPacket {
    let uuid = player.as_uuid();
    let name_bytes = name.as_bytes();
    let mut entries = Vec::with_capacity(1 + 16 + 1 + name_bytes.len() + 1 + 1);
    // VarInt entry count: exactly one player.
    ferrumc_codec::write_var_int(&mut entries, 1);
    // Per-player entry: UUID first, then the Add Player + Update Listed fields in
    // ascending action-bit order.
    entries.extend_from_slice(uuid.as_bytes());
    // Add Player (bit 0x01): name (VarInt length + UTF-8 bytes) then properties.
    // The name is a bounded username (<= 16 code units), far below `i32::MAX`; the
    // fallback only keeps the conversion panic-free.
    let name_len = i32::try_from(name_bytes.len()).unwrap_or(i32::MAX);
    ferrumc_codec::write_var_int(&mut entries, name_len);
    entries.extend_from_slice(name_bytes);
    // Properties: empty array (no signed skin/cape this milestone).
    ferrumc_codec::write_var_int(&mut entries, 0);
    // Update Listed (bit 0x08): show the player in the tab list.
    entries.push(1);
    ClientboundPlayPacket::PlayerInfoUpdate(PlayerInfoUpdate::new(PLAYER_INFO_ADD, entries))
}

/// Builds the [`RemovePlayerInfo`] packet (clientbound `0x3E`) that drops
/// `player` from a viewer's tab list.
///
/// This is the dedicated Player Info Remove packet — a prefixed array of UUIDs —
/// not a [`PlayerInfoUpdate`] action. A real client has no "remove" action on the
/// `0x3F` packet, so faking a removal there is a no-op; only this `0x3E` packet
/// actually clears the entry.
pub(crate) fn player_info_remove(player: PlayerId) -> ClientboundPlayPacket {
    ClientboundPlayPacket::RemovePlayerInfo(RemovePlayerInfo::new(vec![player.as_uuid()]))
}

/// Converts a position delta in blocks to the relative-move fixed-point format
/// (`delta * 4096`, i.e. 12 fractional bits) as an `i16`.
///
/// Only valid for moves up to ~8 blocks per axis (`8 * 4096` overflows an
/// `i16`); larger jumps must use [`entity_teleport_shell`] instead. The cast
/// saturates, so an out-of-range delta clamps rather than wrapping.
fn delta_to_fixed_point(delta: f64) -> i16 {
    (delta * 4096.0) as i16
}

/// Builds an [`EntityTeleport`] shell repositioning `entity_id` absolutely to
/// `position`.
///
/// Used for jumps larger than a relative move can encode (>8 blocks). Velocity
/// and orientation are zeroed: rotation is not threaded through the simulation
/// this milestone, so remote players face north and carry no head turn.
pub(crate) fn entity_teleport_shell(entity_id: i32, position: Vec3) -> ClientboundPlayPacket {
    ClientboundPlayPacket::EntityTeleport(EntityTeleport::new(
        entity_id, position.x, position.y, position.z, 0.0, 0.0, 0.0, 0.0, 0.0, true,
    ))
}

/// Builds an [`UpdateEntityPosition`] shell moving `entity_id` by the relative
/// delta `(dx, dy, dz)` blocks.
///
/// The deltas are encoded in the 1/4096-block fixed-point wire format. Callers
/// must only use this for moves within ~8 blocks per axis (the router selects
/// [`entity_teleport_shell`] for larger jumps).
pub(crate) fn update_entity_position_shell(
    entity_id: i32,
    dx: f64,
    dy: f64,
    dz: f64,
) -> ClientboundPlayPacket {
    ClientboundPlayPacket::UpdateEntityPosition(UpdateEntityPosition::new(
        entity_id,
        delta_to_fixed_point(dx),
        delta_to_fixed_point(dy),
        delta_to_fixed_point(dz),
        true,
    ))
}

/// Builds a [`RemoveEntities`] shell despawning every id in `entity_ids` on the
/// client.
pub(crate) fn remove_entities_shell(entity_ids: &[i32]) -> ClientboundPlayPacket {
    ClientboundPlayPacket::RemoveEntities(RemoveEntities::new(entity_ids.to_vec()))
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

/// Builds the [`BlockUpdate`] packet announcing `state` at `position`.
///
/// This is the clientbound carrier for an accepted break or place: the router
/// broadcasts it to every viewer within view distance of the changed block.
pub(crate) fn block_update_shell(position: BlockPos, state: BlockStateId) -> ClientboundPlayPacket {
    // Block-state ids are small protocol constants far below `i32::MAX`, so this
    // conversion never saturates; the fallback only keeps the path panic-free.
    let block_state = i32::try_from(state.as_u32()).unwrap_or(i32::MAX);
    ClientboundPlayPacket::BlockUpdate(BlockUpdate::new(
        BlockPosition::new(position.x(), position.y(), position.z()),
        block_state,
    ))
}

/// Builds the [`AcknowledgeBlockChange`] packet echoing `sequence`.
///
/// Sent to the acting player alone after an accepted break/place so its
/// optimistic, client-side prediction for that block action is confirmed and the
/// client stops waiting on it. A rejected edit is healed with a
/// [`block_update_shell`] resync instead, never an ack.
pub(crate) fn ack_shell(sequence: i32) -> ClientboundPlayPacket {
    ClientboundPlayPacket::AcknowledgeBlockChange(AcknowledgeBlockChange::new(sequence))
}

/// Returns the [`ShardPos`] of the simulation shard that owns `position`.
///
/// This is the router's placement policy: a world position floors to its
/// [`BlockPos`], then to its [`ChunkPos`](ferrumc_math::ChunkPos), then to the
/// 8x8-chunk [`ShardPos`]. Flooring (not truncation) keeps negative coordinates
/// on the correct shard, matching the simulation's chunk ownership.
pub fn shard_for_position(position: Vec3) -> ShardPos {
    chunk_for_position(position).to_shard_pos()
}

/// Returns the [`ChunkPos`] the world `position` falls in.
///
/// Floors each coordinate to the block grid (so negatives stay on the correct
/// chunk) and coarsens to the chunk column. This is the unit the router measures
/// view distance in — viewer/subject visibility is a chunk-distance test.
pub(crate) fn chunk_for_position(position: Vec3) -> ChunkPos {
    BlockPos::new(
        floor_to_i32(position.x),
        floor_to_i32(position.y),
        floor_to_i32(position.z),
    )
    .to_chunk_pos()
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
    fn dig_start_player_action_becomes_block_break() {
        let event = NetEvent::play(
            player(),
            ServerboundPlayPacket::PlayerAction(PlayerAction::new(
                START_DESTROY_BLOCK,
                BlockPosition::new(8, 63, 8),
                1,
                42,
            )),
        );
        // The packet's sequence (42) must flow through to the BlockBreak input.
        assert_eq!(
            net_event_to_input(&event),
            Some(GameInput::BlockBreak {
                player: player(),
                position: BlockPos::new(8, 63, 8),
                sequence: 42,
            })
        );
    }

    #[test]
    fn non_dig_start_player_action_has_no_input() {
        // Status 1 (cancel digging) is not a break in this milestone.
        let event = NetEvent::play(
            player(),
            ServerboundPlayPacket::PlayerAction(PlayerAction::new(
                1,
                BlockPosition::new(8, 63, 8),
                1,
                0,
            )),
        );
        assert_eq!(net_event_to_input(&event), None);
    }

    #[test]
    fn use_item_on_places_against_the_clicked_face() {
        // Click the top face (Up = index 1) of block (8, 63, 8): the place
        // target is the block one step up, (8, 64, 8).
        let event = NetEvent::play(
            player(),
            ServerboundPlayPacket::UseItemOn(UseItemOn::new(
                0,
                BlockPosition::new(8, 63, 8),
                1,
                0.5,
                1.0,
                0.5,
                false,
                false,
                99,
            )),
        );
        // The packet's sequence (99) must flow through to the BlockPlace input.
        assert_eq!(
            net_event_to_input(&event),
            Some(GameInput::BlockPlace {
                player: player(),
                position: BlockPos::new(8, 64, 8),
                sequence: 99,
            })
        );
    }

    #[test]
    fn use_item_on_with_out_of_range_face_has_no_input() {
        // Only face indices 0..6 are valid; 6 is malformed.
        let event = NetEvent::play(
            player(),
            ServerboundPlayPacket::UseItemOn(UseItemOn::new(
                0,
                BlockPosition::new(8, 63, 8),
                6,
                0.5,
                0.5,
                0.5,
                false,
                false,
                0,
            )),
        );
        assert_eq!(net_event_to_input(&event), None);
    }

    #[test]
    fn block_changed_output_becomes_block_update() {
        let out = GameOutput::BlockChanged {
            position: BlockPos::new(8, 63, 8),
            state: BlockStateId::AIR,
            sequence: 7,
            cause: ferrumc_sim::MutationCause::PlayerCreative { player: player() },
        };
        let Some(ClientboundPlayPacket::BlockUpdate(update)) = output_to_clientbound(&out) else {
            panic!("expected a BlockUpdate shell");
        };
        let loc = update.location();
        assert_eq!((loc.x(), loc.y(), loc.z()), (8, 63, 8));
        assert_eq!(update.block_state(), 0);
    }

    #[test]
    fn ack_shell_carries_the_sequence() {
        let ClientboundPlayPacket::AcknowledgeBlockChange(ack) = ack_shell(123) else {
            panic!("expected an AcknowledgeBlockChange");
        };
        assert_eq!(ack.sequence(), 123);
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
        // A remote player must spawn as minecraft:player (149), not a boat (0).
        assert_eq!(spawn.entity_type(), PLAYER_ENTITY_TYPE);
        assert_eq!(spawn.entity_type(), 149);
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
    fn correction_output_becomes_synchronize_position_shell() {
        let out = GameOutput::PlayerPositionCorrected {
            player: player(),
            position: Vec3::new(8.0, 64.0, 8.0),
        };
        let Some(ClientboundPlayPacket::SynchronizePlayerPosition(sync)) =
            output_to_clientbound(&out)
        else {
            panic!("expected a SynchronizePlayerPosition shell");
        };
        assert_eq!((sync.x(), sync.y(), sync.z()), (8.0, 64.0, 8.0));
    }

    #[test]
    fn despawn_output_has_no_shell() {
        let out = GameOutput::PlayerDespawned { player: player() };
        assert_eq!(output_to_clientbound(&out), None);
    }

    #[test]
    fn entity_spawn_shell_carries_id_uuid_and_position() {
        let p = player();
        let ClientboundPlayPacket::SpawnEntity(spawn) =
            entity_spawn_shell(7, p, Vec3::new(2.0, 3.0, 4.0))
        else {
            panic!("expected a SpawnEntity");
        };
        assert_eq!(spawn.entity_id(), 7);
        assert_eq!(spawn.entity_uuid(), p.as_uuid());
        assert_eq!((spawn.x(), spawn.y(), spawn.z()), (2.0, 3.0, 4.0));
        // The spawned entity type is minecraft:player (149) for 1.21.8 / proto 772.
        assert_eq!(spawn.entity_type(), 149);
    }

    #[test]
    fn player_info_add_uses_the_add_and_listed_action() {
        let p = player();
        let ClientboundPlayPacket::PlayerInfoUpdate(info) = player_info_add(p, "steve") else {
            panic!("expected a PlayerInfoUpdate");
        };
        // Action is Add Player | Update Listed (0x09).
        assert_eq!(info.action(), PLAYER_INFO_ADD);
        assert_eq!(info.action(), 0x09);
        // Count byte, then the UUID — the prefix the visibility tests key on.
        assert_eq!(info.entries()[0], 1);
        assert_eq!(&info.entries()[1..17], p.as_uuid().as_bytes());
    }

    /// Strict byte-level assertion of the "Add Player" body. This is the test the
    /// original truncated encoder slipped past (a lenient fake client accepted a
    /// body that ended after the UUID): asserting the exact name length, empty
    /// properties array, and trailing `Listed` boolean makes any truncation fail.
    #[test]
    fn player_info_add_encodes_the_exact_body_bytes() {
        let p = player();
        let ClientboundPlayPacket::PlayerInfoUpdate(info) = player_info_add(p, "Steve") else {
            panic!("expected a PlayerInfoUpdate");
        };
        assert_eq!(info.action(), 0x09);

        let mut expected = Vec::new();
        expected.push(0x01); // VarInt entry count = 1
        expected.extend_from_slice(p.as_uuid().as_bytes()); // 16-byte UUID
        expected.push(0x05); // VarInt name length = 5
        expected.extend_from_slice(b"Steve"); // name bytes
        expected.push(0x00); // VarInt properties count = 0
        expected.push(0x01); // Listed boolean = true

        assert_eq!(info.entries(), &expected[..]);
        // 1 + 16 + 1 + 5 + 1 + 1 = 25 bytes: a truncated body would be shorter.
        assert_eq!(info.entries().len(), 25);
    }

    #[test]
    fn player_info_remove_builds_a_dedicated_remove_packet() {
        let p = player();
        // Removal is the dedicated 0x3E packet, not a PlayerInfoUpdate action.
        let ClientboundPlayPacket::RemovePlayerInfo(remove) = player_info_remove(p) else {
            panic!("expected a RemovePlayerInfo");
        };
        assert_eq!(remove.players(), [p.as_uuid()].as_slice());
    }

    #[test]
    fn update_entity_position_encodes_fixed_point_deltas() {
        let ClientboundPlayPacket::UpdateEntityPosition(rel) =
            update_entity_position_shell(7, 1.0, -0.5, 2.0)
        else {
            panic!("expected an UpdateEntityPosition");
        };
        assert_eq!(rel.entity_id(), 7);
        // delta * 4096: 1.0 -> 4096, -0.5 -> -2048, 2.0 -> 8192.
        assert_eq!(rel.delta_x(), 4096);
        assert_eq!(rel.delta_y(), -2048);
        assert_eq!(rel.delta_z(), 8192);
    }

    #[test]
    fn entity_teleport_carries_absolute_position() {
        let ClientboundPlayPacket::EntityTeleport(tp) =
            entity_teleport_shell(9, Vec3::new(-1.0, 64.0, 20.0))
        else {
            panic!("expected an EntityTeleport");
        };
        assert_eq!(tp.entity_id(), 9);
        assert_eq!((tp.x(), tp.y(), tp.z()), (-1.0, 64.0, 20.0));
    }

    #[test]
    fn remove_entities_carries_the_ids() {
        let ClientboundPlayPacket::RemoveEntities(remove) = remove_entities_shell(&[2, 5]) else {
            panic!("expected a RemoveEntities");
        };
        assert_eq!(remove.entity_ids(), [2, 5].as_slice());
    }

    #[test]
    fn chunk_for_position_floors_to_chunk() {
        use ferrumc_math::ChunkPos;
        assert_eq!(
            chunk_for_position(Vec3::new(8.0, 64.0, 8.0)),
            ChunkPos::new(0, 0)
        );
        assert_eq!(
            chunk_for_position(Vec3::new(-0.5, 64.0, 20.0)),
            ChunkPos::new(-1, 1)
        );
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
