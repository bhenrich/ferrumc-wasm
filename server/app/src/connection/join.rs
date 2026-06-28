//! The two-stage join kit: framing + position sync first (flushed), then the
//! spawn-area chunk columns (flushed), in the order a real client needs to leave
//! the loading screen.

use tokio::net::TcpStream;
use tokio::sync::oneshot;

use ferrumc_core::PlayerId;
use ferrumc_math::{ChunkPos, Vec3};
use ferrumc_net::{CompressionState, PlayWriter};
use ferrumc_observability::SessionDebug;
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, ClientboundSetHeldItem, Commands, GameEvent, PlayerAbilities,
    SetCenterChunk, SetContainerContent, SetDefaultSpawnPosition,
};

use crate::driver::SimCommand;
use crate::inventory::{PlayerInventory, WINDOW_ID};

use super::context::ConnContext;
use super::outbound::{enqueue_traced_classified, flush_writer, send_mandatory};
use super::{spawn_sync, GAME_EVENT_CHANGE_GAMEMODE, JOIN_TELEPORT_ID};

/// `Game Event` reason `13`: "level chunks load start". Sent right after
/// `JoinGame` to tell the client the spawn chunks are on their way; without it
/// the client never leaves the "Loading terrain" screen.
const GAME_EVENT_LEVEL_CHUNKS_LOAD_START: u8 = 13;

/// Player Abilities `flags` bits sent to a creative client on join: invulnerable
/// (`0x01`) | allow flying (`0x04`) | instabuild/creative (`0x08`). Flying itself
/// (`0x02`) is left off so the player starts grounded but may take off.
const CREATIVE_ABILITY_FLAGS: i8 = 0x01 | 0x04 | 0x08;

/// Flying speed sent in Player Abilities (the vanilla creative default).
const ABILITY_FLYING_SPEED: f32 = 0.05;

/// Walking speed (field-of-view modifier base) sent in Player Abilities.
const ABILITY_WALKING_SPEED: f32 = 0.1;

/// Sends the join kit in the order a real client needs to leave the loading
/// screen: `JoinGame`, `GameEvent(13)`, `SetCenterChunk`, `SetDefaultSpawnPosition`,
/// the permission-filtered `Commands` graph, a non-zero `SynchronizePlayerPosition`,
/// then the spawn-area chunks.
///
/// The spawn-area chunks are fetched LIVE from the resident shard chunks at join
/// time (a [`SimCommand::StreamChunks`] round-trip), not replayed from a cached
/// snapshot, so an edit a previous session placed in a spawn chunk is reflected on
/// this (re)join. The round-trip also gives this connection a player ticket on each
/// spawn column, released on disconnect via the normal `ReleaseChunks` path.
///
/// The position sync goes out *before* the chunks so the client's spawn point is
/// fixed first: the loading-screen gate releases on the chunk that contains the
/// player's position, and sending the sync first guarantees that chunk is among
/// the spawn-area column packets that follow, regardless of where spawn lands.
///
/// The sequence is flushed in two stages because the [`PlayWriter`] drains by
/// priority (State before World): flushing the framing-and-position packets, then
/// the chunks, guarantees the position sync lands ahead of the chunk column
/// rather than being reordered after it.
///
/// # Errors
///
/// Returns an error if any stage fails to encode or write to the socket.
#[allow(clippy::too_many_arguments)] // one cohesive step: framing + self player-info + I/O + trace state
#[allow(clippy::too_many_lines)] // one join sequence: framing, abilities, inventory, chunks
pub(super) async fn send_join_kit(
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    compression: &CompressionState,
    ctx: &ConnContext,
    debug: &mut SessionDebug,
    player: PlayerId,
    name: &str,
    position: Vec3,
    yaw: f32,
    pitch: f32,
    is_returning: bool,
    inventory: &PlayerInventory,
) -> anyhow::Result<()> {
    let kit = &ctx.join_kit;
    let clock = &ctx.clock;

    // Stage 1: enter play, cue the client to expect spawn chunks, fix the world
    // spawn, and teleport the player in — all before any chunk is sent.
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::JoinGame(kit.join_game().clone()),
    );
    // Put the local player on their own tab list: a Player Info Update "Add
    // Player" for themselves. Other players' entries arrive from the session
    // router's join-visibility broadcast; this is the one entry the router cannot
    // send (a player is not in their own viewer set).
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ferrumc_session::player_info_add(player, name),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::GameEvent(GameEvent::new(GAME_EVENT_LEVEL_CHUNKS_LOAD_START, 0.0)),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SetCenterChunk(SetCenterChunk::new(
            kit.spawn_chunk().x(),
            kit.spawn_chunk().z(),
        )),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SetDefaultSpawnPosition(SetDefaultSpawnPosition::new(
            kit.spawn_block(),
            0.0,
        )),
    );
    // Declare the command graph so the client renders `/spawn` and `/gamemode` as
    // valid (not red) and offers autocomplete for them. The graph is filtered to
    // this player's permission level AND their granted permission nodes, so a
    // non-operator never receives the level-gated `/gamemode` subtree and a player
    // without a permission-gated command's node never receives that command.
    let allowed = |node: &str| ctx.policy.permissions().is_allowed(player, node);
    let command_body = ctx
        .policy
        .command_tree()
        .encode_commands_body(ctx.policy.permission_level(player), &allowed);
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::Commands(Commands::new(command_body)),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SynchronizePlayerPosition(spawn_sync(
            JOIN_TELEPORT_ID,
            position,
            yaw,
            pitch,
        )),
    );
    // Player Abilities: tell a creative client it may fly and instabuild, so the
    // flight + creative-reach UX matches the creative mode JoinGame advertised.
    send_mandatory(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::PlayerAbilities(PlayerAbilities::new(
            CREATIVE_ABILITY_FLAGS,
            ABILITY_FLYING_SPEED,
            ABILITY_WALKING_SPEED,
        )),
    )?;
    // Initialize window 0 with the full 46-slot inventory (the starter kit in the
    // hotbar) and an empty cursor. Mandatory: a dropped container-content leaves the
    // client's inventory view desynced.
    let container_payload = inventory
        .container_content_payload()
        .map_err(|err| anyhow::anyhow!("encoding join container content: {err}"))?;
    send_mandatory(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SetContainerContent(SetContainerContent::new(
            WINDOW_ID,
            inventory.state_id(),
            container_payload,
        )),
    )?;

    // For a RETURNING player, restore the saved game mode and held hotbar slot. The
    // JoinGame above always advertises creative and the client defaults to hotbar
    // slot 0, so a fresh joiner needs neither packet; a returning one is corrected
    // here. The GameEvent (reason 3 = change game mode) switches the client's mode to
    // the inventory's authoritative mirror, and `ClientboundSetHeldItem` moves the
    // selector to the restored slot so the held item matches the broadcast equipment.
    if is_returning {
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            clock,
            ClientboundPlayPacket::GameEvent(GameEvent::new(
                GAME_EVENT_CHANGE_GAMEMODE,
                f32::from(inventory.game_mode().as_id()),
            )),
        );
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            clock,
            ClientboundPlayPacket::ClientboundSetHeldItem(ClientboundSetHeldItem::new(i32::from(
                inventory.selected(),
            ))),
        );
    }

    flush_writer(writer, stream, compression, ctx.io_timeout).await?;

    // Stage 2: the spawn-area chunk column packets (includes the player's chunk),
    // fetched LIVE from the resident shard chunks rather than replayed from a
    // cached snapshot. A `StreamChunks` round-trip acquires a player ticket on each
    // spawn column and builds its packet from the current chunk state, so a block a
    // previous session placed in a spawn chunk is present on this (re)join. The
    // whole batch is still sent up-front here (not re-paced through the streaming
    // pump) so the loading screen releases as before.
    let spawn_positions: Vec<ChunkPos> = kit.chunk_positions().collect();
    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.commands
        .send(SimCommand::StreamChunks {
            load: spawn_positions,
            unload: Vec::new(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
    let chunks = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver dropped the spawn chunk reply"))?;
    for streamed in chunks {
        let outcome = enqueue_traced_classified(
            writer,
            debug,
            compression,
            clock,
            ClientboundPlayPacket::ChunkDataAndLight(streamed.chunk),
        );
        // Count only chunks that actually entered the queue; a tail-dropped chunk
        // never reaches the wire and must not inflate the counter.
        if outcome.is_enqueued() {
            ctx.metrics.incr_chunk_sent(1);
        }
        // Render any signs in the column right after it, so a (re)joining player
        // sees text a previous session left on spawn-area signs.
        for block_entity in streamed.block_entities {
            enqueue_traced_classified(writer, debug, compression, clock, block_entity);
        }
    }
    flush_writer(writer, stream, compression, ctx.io_timeout).await
}
