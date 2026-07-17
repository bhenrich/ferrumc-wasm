//! Per-packet serverbound play handlers: chat/commands, inventory, the block
//! break/place plugin-intent boundary and its single rejection funnel, and tab
//! completion.

use ferrumc_codec::{BoundedReader, BoundedString};
use ferrumc_command::{CommandSource, CommandTree};
use ferrumc_core::{GameMode, PlayerId, TextColor, TextComponent};
use ferrumc_items::{left_click_exchange, ItemStack, UntrustedItemStack};
use ferrumc_math::{BlockPos, Direction, Vec3, WorldIntent};
use ferrumc_net::{CompressionState, ConnectionState, DecodeError, PlayWriter};
use ferrumc_observability::{PacketState, SessionDebug};
use ferrumc_plugin_api::{
    BlockBreakAttempt, BlockPlaceAttempt, ChatAttempt, InteractAttempt, InteractHand,
    InteractTarget, PluginEventDecision,
};
use ferrumc_plugin_host::ResolvedDecision;
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, CloseContainer, CommandSuggestionMatch, GameEvent,
    ServerboundPlayPacket, ServerboundSetHeldItem, SetContainerContent, SetContainerSlot,
    SetCreativeSlot, SetPlayerPosition, TabCompleteResponse, UpdateSign, UseItemOn, WindowClick,
};
use ferrumc_proto::ProtoError;
use ferrumc_session::{
    net_event_to_input, open_screen, use_item_on_block, use_item_on_face, use_item_on_target,
    NetEvent,
};
use ferrumc_sim::{BlockStateId, GameInput};
use tokio::sync::oneshot;

use crate::command::{parse_gamemode, GAMEMODE_COMMAND, SPAWN_COMMAND};
use crate::driver::SimCommand;
use crate::inventory::{PlayerInventory, SLOT_COUNT, WINDOW_ID};
use crate::observe;
use crate::player_data::is_valid_player_position;
use crate::plugins::PermissionFacade;
use crate::window::{OpenContainer, WindowSlot, WindowState, GENERIC_9X3_TYPE};

use super::chunk_stream::{mirror_server_teleport, ChunkStream};
use super::context::ConnContext;
use super::outbound::{ack_sequence, enqueue_traced_classified, send_mandatory};
use super::rate_limiter::ChatRateLimiter;
use super::{send_sim_command_accepted, spawn_sync, GAME_EVENT_CHANGE_GAMEMODE, JOIN_TELEPORT_ID};

/// Last serverbound Play packet id in protocol 772.
///
/// The protocol's ids are contiguous from `0x00` through `0x41`. `FerrumC` models
/// only the subset it handles, so an in-range generated-enum miss is compatible
/// unmodelled traffic; an id outside this range is a protocol violation.
const MAX_PROTOCOL_772_SERVERBOUND_PLAY_ID: i32 = 0x41;

/// One decoded frame body plus its validated nested creative-item payload.
struct DecodedPlayBody {
    packet: ServerboundPlayPacket,
    creative_item: Option<ItemStack>,
}

/// Result of classifying a complete protocol-772 Play frame body.
enum PlayBodyDecode {
    /// A packet `FerrumC` models and can dispatch.
    Modelled(DecodedPlayBody),
    /// A protocol-valid id omitted from `FerrumC`'s deliberately partial enum.
    Unmodelled { id: i32 },
}

/// Handles one decoded serverbound play-frame body.
///
/// Protocol-valid but unmodelled packets are ignored because the generated slice
/// intentionally covers only what `FerrumC` handles. A malformed modelled packet,
/// an impossible protocol-772 id, or trailing outer/nested bytes is a classified
/// fatal decode error. Teleport confirmation and Keep Alive are decoded strictly
/// but otherwise ignored. A `ChatCommand` is dispatched locally; every other
/// modelled packet is forwarded to the simulation unless policy vetoes it.
/// Movement is validated atomically and admitted to the bounded shard route
/// before it may update any connection-local mirror.
#[allow(clippy::too_many_arguments)] // one play step: framing + policy + trace state
#[allow(clippy::too_many_lines)] // one dispatch: chat, inventory, place, movement fallthrough
pub(super) async fn handle_play_body(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    chat_limiter: &mut ChatRateLimiter,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    player_yaw: &mut f32,
    player_pitch: &mut f32,
    body: &[u8],
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let decoded = match decode_play_body(body) {
        Ok(PlayBodyDecode::Modelled(decoded)) => decoded,
        Ok(PlayBodyDecode::Unmodelled { id }) => {
            // A conforming 1.21.8 client can send packets the deliberately partial
            // generated enum does not model. The complete raw frame has already
            // been consumed, so skip only this body and keep the next frame intact.
            ctx.metrics
                .record_packet_decode_error(PacketState::Play, "unknown_play");
            tracing::trace!(
                packet_id = id,
                "ignoring unmodelled protocol-772 Play packet"
            );
            return Ok(());
        }
        Err(err) => {
            ctx.metrics.record_packet_decode_error(
                PacketState::Play,
                observe::body_decode_error_label(&err),
            );
            debug.dump("play_packet_decode_error");
            return Err(err.into());
        }
    };
    let packet = decoded.packet;
    let creative_item = decoded.creative_item;

    // A movement packet is one atomic hostile observation. Reject it before even
    // tracing it as accepted when any carried coordinate or look component is
    // unsafe: a valid position paired with NaN yaw (or an invalid position paired
    // with finite look) must not partially update a mirror, call a plugin, route
    // to the shard, or trigger chunk work.
    let movement = match validated_movement(&packet) {
        MovementValidation::NotMovement => None,
        MovementValidation::Valid(movement) => Some(movement),
        MovementValidation::Invalid => return Ok(()),
    };

    // Record the inbound play trace with the exact frame-body size.
    debug.record_inbound(observe::trace_inbound_play(
        &packet,
        body.len(),
        compression,
        &ctx.clock,
    ));

    if let Some(movement) = movement {
        // The router must own the exact movement before any connection-local
        // state advances. A full/closed shard route returns a classified failure
        // (and may terminate the overloaded session), leaving the stream centre,
        // plugin baseline, and leave-save candidate untouched.
        send_sim_command_accepted(
            ctx,
            SimCommand::Event {
                event: NetEvent::play(player, packet),
                acceptance: None,
            },
        )
        .await?;

        if let Some(position) = movement.position {
            chunk_stream.observe(position);
            // Fire the observe-only PlayerMove plugin event only for a validated,
            // admitted move, throttled to block granularity. Movement cannot be
            // vetoed through this surface; only emitted intents are routed.
            if let Some((from, to)) = chunk_stream.block_crossing(position) {
                let perms = PermissionFacade::new(ctx.policy.permissions());
                let intents =
                    ctx.block_events
                        .player_move(player, from, to, Some(position), &perms);
                route_emitted_intents(ctx, player, writer, 0, intents, debug, compression).await?;
            }
        }
        if let Some(yaw) = movement.yaw {
            *player_yaw = yaw;
        }
        if let Some(pitch) = movement.pitch {
            *player_pitch = pitch;
        }
        return Ok(());
    }

    match &packet {
        ServerboundPlayPacket::ChatCommand(command) => {
            let command = command.command().as_str().to_owned();
            return handle_command(
                ctx,
                player,
                name,
                writer,
                chunk_stream,
                inventory,
                player_yaw,
                player_pitch,
                &command,
                debug,
                compression,
            )
            .await;
        }
        ServerboundPlayPacket::ChatMessage(chat) => {
            // Rate-limit at the SOURCE before relaying: one spammer must not fan
            // spam into every recipient's bounded outbound channel and starve legit
            // packets. The per-connection token bucket is driven by the server tick
            // (connection task, allowed a non-deterministic clock). Over budget ->
            // drop the line (logged, not relayed); the sender is not disconnected
            // for a transient burst.
            if !chat_limiter.try_consume(ctx.clock.now()) {
                tracing::debug!(player = name, "dropping over-budget chat line");
                return Ok(());
            }
            // Consult the loaded plugins at the chat intent boundary (off the tick,
            // under the host mutex, no lock across an `.await`) BEFORE broadcasting.
            // A Deny drops the line (it is never relayed) and shows the sender the
            // reason, if any; a chat filter rides this path. The plugins see the raw
            // message the player typed and the sender's last known position.
            let perms = PermissionFacade::new(ctx.policy.permissions());
            let (chat_decision, chat_intents) = ctx.block_events.before_chat(
                &ChatAttempt::new(player, chat.message().as_str()),
                chunk_stream.last_position(),
                &perms,
            );
            if let PluginEventDecision::Deny { message } = chat_decision {
                // `chat_intents` is empty on a Deny (the dispatcher drops them so a
                // dropped message cannot still trigger a side effect); routing it is
                // a no-op kept for symmetry with the Allow path.
                deliver_deny_message(ctx, writer, debug, compression, message);
                return route_emitted_intents(
                    ctx,
                    player,
                    writer,
                    0,
                    chat_intents,
                    debug,
                    compression,
                )
                .await;
            }
            // Allowed: route any intents the plugins emitted, then broadcast.
            route_emitted_intents(ctx, player, writer, 0, chat_intents, debug, compression).await?;
            // Relay unsigned player chat to everyone as a system message: format it
            // "<name> message" and hand it to the driver, the only owner of every
            // player's outbound channel. enforces_secure_chat = false, so no 1.19
            // signing apparatus is needed (the signature tail was decoded into the
            // ignored `rest` field). The relay reaches the sender via its own
            // session outbound channel, so it is NOT also enqueued on the writer.
            //
            // Strip legacy section-sign (§, U+00A7) codes from the untrusted
            // message first: a client still interprets `§k`/`§l`/§<colour> inside
            // a component's `text`, so leaving them in would let a player inject
            // colour/obfuscation formatting into the relayed line. The name is
            // not user-controlled (usernames are `[A-Za-z0-9_]`), so only the
            // message body needs sanitising.
            let sanitized = chat.message().as_str().replace('\u{00A7}', "");
            let line = format!("<{name}> {sanitized}");
            let content = TextComponent::text(line);
            return ctx
                .commands
                .send(SimCommand::BroadcastSystemChat {
                    content,
                    overlay: false,
                })
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"));
        }
        // A tab-complete request is answered locally from the command tree; it
        // never touches the simulation.
        ServerboundPlayPacket::TabCompleteRequest(req) => {
            handle_tab_complete(
                ctx,
                player,
                writer,
                req.transaction_id(),
                req.text().as_str(),
                debug,
                compression,
            );
            return Ok(());
        }
        // Place-with-held-item: resolve the held hotbar stack to a block-state and
        // route a place carrying it (or just ack, on empty hand / non-placeable /
        // veto). Handled here, not via the generic NetEvent path, because the place
        // needs the inventory the session layer cannot see.
        ServerboundPlayPacket::UseItemOn(p) => {
            return handle_use_item_on(
                ctx,
                player,
                writer,
                inventory,
                window_state,
                p,
                *player_yaw,
                chunk_stream.last_position(),
                debug,
                compression,
            )
            .await;
        }
        // Update Sign: the player confirmed text in the sign editor. Route it to
        // the block's owning shard, which validates and stores it (net never writes
        // the world directly) and broadcasts the rendered sign to viewers.
        ServerboundPlayPacket::UpdateSign(p) => {
            return handle_update_sign(ctx, player, p).await;
        }
        // Set Creative Slot (untrusted): validate the hostile item bytes, store the
        // slot, and echo it back so the client view matches the server.
        ServerboundPlayPacket::SetCreativeSlot(p) => {
            let Some(stack) = creative_item else {
                anyhow::bail!("strict creative-slot decode omitted its validated item");
            };
            return handle_set_creative_slot(
                ctx,
                player,
                name,
                writer,
                inventory,
                p,
                stack,
                debug,
                compression,
            )
            .await;
        }
        // Set Held Item (serverbound): update the selected hotbar index and, on a
        // real change, broadcast the new held item to viewers.
        ServerboundPlayPacket::ServerboundSetHeldItem(p) => {
            return handle_set_held_item(ctx, player, inventory, p).await;
        }
        // Click Container: an open chest window applies the safe left-click subset
        // (everything else resyncs); a click on window 0 resyncs the inventory.
        ServerboundPlayPacket::WindowClick(p) => {
            return handle_window_click(
                ctx,
                player,
                writer,
                inventory,
                window_state,
                p,
                debug,
                compression,
            )
            .await;
        }
        // Close Container: return any carried item to the inventory and resync.
        ServerboundPlayPacket::CloseContainer(p) => {
            return handle_close_container(
                ctx,
                player,
                writer,
                inventory,
                window_state,
                p,
                debug,
                compression,
            )
            .await;
        }
        // The teleport confirmation (reply to the join position sync) and the
        // Keep Alive echo are accepted and need no action: the slice does not
        // validate teleport ids and the keep-alive timer is fire-and-forget.
        ServerboundPlayPacket::ConfirmTeleportation(_)
        | ServerboundPlayPacket::ServerboundKeepAlive(_) => return Ok(()),
        _ => {}
    }

    let event = NetEvent::play(player, packet);
    // A block break crosses the plugin intent boundary: the loaded plugins'
    // `before_block_break` decision hooks decide whether (and how) it proceeds.
    // Every other event reaching this fallthrough carries no block decision and
    // routes straight to the simulation; movement returned through its validated
    // admission path above.
    if let Some(GameInput::BlockBreak {
        position, sequence, ..
    }) = net_event_to_input(&event)
    {
        return handle_block_break(
            ctx,
            player,
            writer,
            position,
            sequence,
            event,
            debug,
            compression,
        )
        .await;
    }
    ctx.commands
        .send(SimCommand::Event {
            event,
            acceptance: None,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))
}

/// Decodes one complete serverbound Play body to the generated schema's precision.
///
/// Generated packet structs decode fields but deliberately leave frame
/// exhaustion to their caller. Fields explicitly modelled as opaque remainders,
/// such as signed-chat tails, remain the packet's payload. `SetCreativeSlot` is
/// the nested format this boundary owns: its generated item field consumes the
/// opaque remainder, so this function also decodes, validates, and finishes that
/// untrusted-slot reader before the packet can reach authorization or inventory
/// effects.
fn decode_play_body(body: &[u8]) -> Result<PlayBodyDecode, DecodeError> {
    let state = ConnectionState::Play;
    let mut reader = BoundedReader::new(body);
    let id = reader
        .read_var_int()
        .map_err(|_| DecodeError::MalformedBody { state })?;

    let packet = match ServerboundPlayPacket::decode(id, &mut reader) {
        Ok(packet) => packet,
        Err(ProtoError::UnknownPacketId { .. })
            if (0..=MAX_PROTOCOL_772_SERVERBOUND_PLAY_ID).contains(&id) =>
        {
            return Ok(PlayBodyDecode::Unmodelled { id });
        }
        Err(ProtoError::UnknownPacketId { .. }) => {
            return Err(DecodeError::UnknownPacket { state, id });
        }
        Err(_) => return Err(DecodeError::MalformedBody { state }),
    };

    match reader.finish() {
        Ok(()) => {}
        Err(ferrumc_codec::CodecError::TrailingBytes { remaining }) => {
            return Err(DecodeError::TrailingBytes {
                state,
                trailing: remaining,
            });
        }
        Err(_) => return Err(DecodeError::MalformedBody { state }),
    }

    let creative_item = if let ServerboundPlayPacket::SetCreativeSlot(packet) = &packet {
        let mut item_reader = BoundedReader::new(packet.item());
        let untrusted = UntrustedItemStack::decode(&mut item_reader)
            .map_err(|_| DecodeError::MalformedBody { state })?;
        match item_reader.finish() {
            Ok(()) => {}
            Err(ferrumc_codec::CodecError::TrailingBytes { remaining }) => {
                return Err(DecodeError::TrailingBytes {
                    state,
                    trailing: remaining,
                });
            }
            Err(_) => return Err(DecodeError::MalformedBody { state }),
        }
        Some(
            untrusted
                .into_validated()
                .map_err(|_| DecodeError::MalformedBody { state })?,
        )
    } else {
        None
    };

    Ok(PlayBodyDecode::Modelled(DecodedPlayBody {
        packet,
        creative_item,
    }))
}

/// One validated, atomically admitted movement observation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MovementObservation {
    /// Absolute position, absent on a rotation-only packet.
    position: Option<Vec3>,
    /// Body yaw, absent on a position-only packet.
    yaw: Option<f32>,
    /// Pitch, absent on a position-only packet.
    pitch: Option<f32>,
}

/// Classification of one decoded packet at the client-movement trust boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MovementValidation {
    /// This packet carries no player movement.
    NotMovement,
    /// Every carried component is finite and any position is in range.
    Valid(MovementObservation),
    /// At least one carried component is unsafe; the whole packet is rejected.
    Invalid,
}

/// Validates one decoded movement packet atomically.
///
/// Coordinates use the same inclusive finite/range predicate as restoration and
/// the simulation. Look angles may be arbitrary finite degrees—the protocol and
/// simulation allow wrapping—but NaN and infinities are rejected so they cannot
/// poison routing, placement, or persistence. A combined packet is all-or-nothing.
fn validated_movement(packet: &ServerboundPlayPacket) -> MovementValidation {
    let movement = match packet {
        ServerboundPlayPacket::SetPlayerPosition(p) => MovementObservation {
            position: Some(Vec3::new(p.x(), p.y(), p.z())),
            yaw: None,
            pitch: None,
        },
        ServerboundPlayPacket::SetPlayerPositionAndRotation(p) => MovementObservation {
            position: Some(Vec3::new(p.x(), p.y(), p.z())),
            yaw: Some(p.yaw()),
            pitch: Some(p.pitch()),
        },
        ServerboundPlayPacket::SetPlayerRotation(p) => MovementObservation {
            position: None,
            yaw: Some(p.yaw()),
            pitch: Some(p.pitch()),
        },
        _ => return MovementValidation::NotMovement,
    };

    let valid_position = match movement.position {
        Some(position) => is_valid_player_position(position),
        None => true,
    };
    let valid_yaw = match movement.yaw {
        Some(yaw) => yaw.is_finite(),
        None => true,
    };
    let valid_pitch = match movement.pitch {
        Some(pitch) => pitch.is_finite(),
        None => true,
    };
    if valid_position && valid_yaw && valid_pitch {
        MovementValidation::Valid(movement)
    } else {
        MovementValidation::Invalid
    }
}

/// Maps the wire hand index of a use-item packet to the typed [`InteractHand`].
///
/// The protocol encodes `0` as the main hand and `1` as the off-hand; any other
/// value is treated as the off-hand (the only non-main option), so a malformed
/// index can never be mistaken for a main-hand action.
fn interact_hand(hand: i32) -> InteractHand {
    if hand == 0 {
        InteractHand::Main
    } else {
        InteractHand::Off
    }
}

/// Handles a block break at the plugin intent boundary.
///
/// Consults the loaded plugins' `before_block_break` hooks (off the tick, under
/// the host mutex with no lock held across an `.await`) and resolves the combined
/// decision:
///
/// - [`Deny`](ResolvedDecision::Deny): the edit is dropped (the world is never
///   modified) and the actor's optimistic client-side prediction is healed through
///   the single reject funnel ([`reject_block_edit`]) — a [`SimCommand::RejectBlockEdit`]
///   to the block's owning shard, which reads the authoritative state and emits the
///   actor's mandatory resync `BlockUpdate` + `AcknowledgeBlockChange` (the same
///   path a sim-side rejection uses, so a Deny no longer leaves a ghost block). The
///   refused break heals to the block the client predicted removing
///   ([`BlockStateId::AIR`] is the predicted state passed for metric classification).
///   The rejection is counted once, on the sim's resulting `BlockChangeRejected`. If
///   the decision carries a message it is delivered as a system chat.
/// - [`Replace`](ResolvedDecision::Replace): the broken block is set to the
///   replacement state instead of air, by routing a [`SimCommand::SetBlockExact`]
///   at the break position — an exact write applied verbatim (never re-run through
///   `compute_placement`, so a rotated replacement survives).
/// - [`Allow`](ResolvedDecision::Allow): the original break routes to the
///   simulation as before.
///
/// Any emitted [`WorldIntent`]s are routed, and on a non-denied edit the
/// `after_block_break` notification fires.
#[allow(clippy::too_many_arguments)] // the connection threads its writer + trace context through
async fn handle_block_break(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    position: BlockPos,
    sequence: i32,
    event: NetEvent,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let perms = PermissionFacade::new(ctx.policy.permissions());
    let (decision, emitted) = ctx
        .block_events
        .before_block_break(&BlockBreakAttempt::new(player, position), &perms);

    match decision {
        ResolvedDecision::Deny { message } => {
            // Heal the actor through the single reject funnel: the sim reads the
            // authoritative state and sends the mandatory resync + ack (no ghost).
            // The predicted state for a break is air. The metric is counted on the
            // resulting BlockChangeRejected, so it is NOT recorded here too.
            reject_block_edit(ctx, player, position, sequence, BlockStateId::AIR).await?;
            deliver_deny_message(ctx, writer, debug, compression, message);
            route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression)
                .await?;
            return Ok(());
        }
        ResolvedDecision::Replace { block_state_id } => {
            // A plugin Replace supplies an EXACT state: write it verbatim through
            // the exact-write path so the simulation never re-runs it through
            // compute_placement (a rotated replacement must survive byte-for-byte).
            // Reject an unrepresentable state id (> i32::MAX) before it can be
            // stored — the mandatory resync must never meet a state the wire cannot
            // carry — and heal the actor, which predicted the break to air.
            if i32::try_from(block_state_id).is_err() {
                tracing::warn!(
                    block_state_id,
                    "plugin replace supplied an out-of-range block state id; healing actor and skipping the write"
                );
                reject_block_edit(ctx, player, position, sequence, BlockStateId::AIR).await?;
                route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression)
                    .await?;
                return Ok(());
            }
            send_sim_command_accepted(
                ctx,
                SimCommand::SetBlockExact {
                    player,
                    position,
                    sequence,
                    state: BlockStateId::new(block_state_id),
                    acceptance: None,
                },
            )
            .await?;
        }
        _ => {
            send_sim_command_accepted(
                ctx,
                SimCommand::Event {
                    event,
                    acceptance: None,
                },
            )
            .await?;
        }
    }

    route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression).await?;
    // The edit was accepted at the intent boundary and routed: notify after_*.
    let after = ctx.block_events.after_block_break(player, position, &perms);
    route_emitted_intents(ctx, player, writer, sequence, after, debug, compression).await
}

/// Delivers a plugin Deny message (if any) to the acting player as a system chat.
fn deliver_deny_message(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    message: Option<TextComponent>,
) {
    if let Some(message) = message {
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            &ctx.clock,
            ferrumc_session::system_chat(&message, false),
        );
    }
}

/// The single funnel that heals a block edit refused at the connection (a plugin
/// `Deny` / spawn-protection veto).
///
/// The net layer has no world access, so it cannot read the authoritative block
/// state needed to undo the client's optimistic prediction. Instead of authoring an
/// ack-only heal (which leaves a ghost block, since a `BlockUpdate` is swallowed
/// while a prediction is pending and an ack alone heals to the *predicted* state),
/// it routes a [`SimCommand::RejectBlockEdit`] to the block's owning shard. The
/// shard reads the authoritative state and emits a `BlockChangeRejected`, which the
/// router turns into the actor's mandatory resync `BlockUpdate` + `AcknowledgeBlockChange`
/// — the exact same path a sim-side rejection (out of reach, unloaded chunk) uses.
///
/// `requested_state` is the state the client predicted (air for a refused break,
/// the held block for a refused place); it is used only to classify the block-edit
/// metric on the resulting rejection, never applied to the world.
///
/// The call waits for bounded shard admission. Rejection healing uses the
/// control-reserved lane; if even that lane is physically full, the driver fails
/// closed and retains a `PlayerLeave` retry rather than acknowledging or silently
/// dropping the heal.
async fn reject_block_edit(
    ctx: &ConnContext,
    player: PlayerId,
    position: BlockPos,
    sequence: i32,
    requested_state: BlockStateId,
) -> anyhow::Result<()> {
    send_sim_command_accepted(
        ctx,
        SimCommand::RejectBlockEdit {
            player,
            position,
            sequence,
            requested_state,
            acceptance: None,
        },
    )
    .await
}

/// Routes the [`WorldIntent`]s a plugin emitted from a block decision (or an
/// after-* notification).
///
/// Mapping (bounded; world-changing intents wait for shard admission):
/// - [`WorldIntent::SetBlock`] -> [`SimCommand::SetBlockExact`] by the acting
///   player (an exact write applied verbatim, never refined by `compute_placement`).
/// - [`WorldIntent::Message`] -> a system chat to the acting player's own writer
///   when it targets them, otherwise a targeted [`SimCommand::SendSystemChat`] to
///   the named recipient (the connection task cannot reach another player's
///   outbound channel directly, so the driver-owned router delivers it).
/// - [`WorldIntent::Teleport`] -> a [`SimCommand::TeleportPlayer`], which the
///   driver-owned router fulfils (snap the target's client + route an
///   authoritative move).
async fn route_emitted_intents(
    ctx: &ConnContext,
    actor: PlayerId,
    writer: &mut PlayWriter,
    sequence: i32,
    intents: Vec<WorldIntent>,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    for intent in intents {
        match intent {
            WorldIntent::SetBlock {
                pos,
                block_state_id,
            } => {
                // A plugin SetBlock supplies an EXACT state: write it verbatim
                // (bypassing compute_placement) so a rotated state survives. Skip an
                // unrepresentable state id (> i32::MAX); an arbitrary plugin
                // set-block carries no client prediction to heal, so log and drop it
                // rather than store a state the wire cannot carry.
                if i32::try_from(block_state_id).is_err() {
                    tracing::warn!(
                        block_state_id,
                        "plugin set-block supplied an out-of-range block state id; skipping the write"
                    );
                    continue;
                }
                send_sim_command_accepted(
                    ctx,
                    SimCommand::SetBlockExact {
                        player: actor,
                        position: pos,
                        sequence,
                        state: BlockStateId::new(block_state_id),
                        acceptance: None,
                    },
                )
                .await?;
            }
            WorldIntent::Message { player, message } => {
                if player == actor {
                    // The actor is this connection: write straight to its socket.
                    enqueue_traced_classified(
                        writer,
                        debug,
                        compression,
                        &ctx.clock,
                        ferrumc_session::system_chat(&message, false),
                    );
                } else {
                    // A different recipient: route a TARGETED chat through the
                    // driver-owned router (only it holds other players' channels).
                    ctx.commands
                        .send(SimCommand::SendSystemChat {
                            player,
                            content: message,
                            overlay: false,
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
                }
            }
            WorldIntent::Teleport { player, position } => {
                if !is_valid_player_position(position) {
                    tracing::warn!(
                        %player,
                        x = position.x,
                        y = position.y,
                        z = position.z,
                        "plugin supplied an unsafe teleport destination; skipping the teleport"
                    );
                    continue;
                }
                // The connection cannot reach another player's channel; the
                // driver-owned router snaps the target and routes the authoritative
                // move.
                send_sim_command_accepted(
                    ctx,
                    SimCommand::TeleportPlayer {
                        player,
                        position,
                        acceptance: None,
                    },
                )
                .await?;
            }
            _ => {
                tracing::debug!("plugin emitted an intent with no connection-side route; skipping");
            }
        }
    }
    Ok(())
}

/// Dispatches a `/command` for `player`, reports the outcome to them, and applies
/// its side effect.
///
/// Dispatch goes through the shared command tree with the player's per-player
/// permission level and a node-string checker backed by the permission registry.
/// Every outcome is now reported to the issuer as a `SystemChat` on their writer:
/// a dispatch failure (unknown command, bad argument, permission denied) becomes a
/// red error line and the command stops there, while a handler that ran reports
/// its [`CommandResult`](ferrumc_command::CommandResult) feedback (for success or
/// logical failure).
///
/// On a successful `/spawn`, the authoritative move is admitted before
/// `SynchronizePlayerPosition` is queued to the socket. On a successful
/// `/gamemode <id>`, the authoritative mode change is admitted before the
/// `GameEvent` with reason `3` (`change_game_mode`) switches the client. Command
/// success feedback and plugin after-effects follow the same admission barrier.
#[allow(clippy::too_many_arguments)] // one command step: dispatch + feedback + side effects + I/O
#[allow(clippy::too_many_lines)] // one ordered transaction keeps admission before every visible success
async fn handle_command(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    inventory: &mut PlayerInventory,
    player_yaw: &mut f32,
    player_pitch: &mut f32,
    command: &str,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let policy = &ctx.policy;
    let source = CommandSource::for_player(player, name, policy.permission_level(player));
    let allowed = |node: &str| policy.permissions().is_allowed(player, node);
    let result = match policy
        .command_tree()
        .dispatch_with(command, &source, &allowed)
    {
        Ok(result) => result,
        Err(err) => {
            // The handler never ran (unknown command / bad argument / permission
            // denied): report why to the issuer as a red system-chat line.
            let message = TextComponent::text(err.to_string()).with_color(TextColor::Red);
            enqueue_traced_classified(
                writer,
                debug,
                compression,
                &ctx.clock,
                ferrumc_session::system_chat(&message, false),
            );
            return Ok(());
        }
    };

    if !result.is_success() {
        if !result.feedback().to_plain_string().is_empty() {
            enqueue_traced_classified(
                writer,
                debug,
                compression,
                &ctx.clock,
                ferrumc_session::system_chat(result.feedback(), false),
            );
        }
        return Ok(());
    }

    // Route every driver-owned command effect before publishing success feedback.
    // Shard-mutating commands wait for their explicit bounded admission; pure
    // driver-owned presentation/time commands need only cross the already-bounded
    // app command channel.
    for sim_command in crate::command::region_commands(command, player) {
        if sim_command.supports_delivery_acceptance() {
            send_sim_command_accepted(ctx, sim_command).await?;
        } else {
            ctx.commands
                .send(sim_command)
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
        }
    }

    let first_token = command.split_whitespace().next();
    if first_token == Some(SPAWN_COMMAND) {
        let spawn = policy.spawn();
        // Snap the player to spawn at their CURRENT look: the previous `0.0/0.0`
        // reset them to facing south and level on every `/spawn`.
        let sync = spawn_sync(JOIN_TELEPORT_ID, spawn, *player_yaw, *player_pitch);
        let move_event = NetEvent::play(
            player,
            ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(
                spawn.x, spawn.y, spawn.z, 0,
            )),
        );
        send_sim_command_accepted(
            ctx,
            SimCommand::Event {
                event: move_event,
                acceptance: None,
            },
        )
        .await?;
        // Only after admission may the client/persistence mirrors preview the
        // teleport. A rejected route terminates the overloaded session above and
        // never reaches these effects.
        mirror_server_teleport(chunk_stream, player_yaw, player_pitch, &sync);
        send_mandatory(
            writer,
            debug,
            compression,
            &ctx.clock,
            ClientboundPlayPacket::SynchronizePlayerPosition(sync),
        )?;
    } else if first_token == Some(GAMEMODE_COMMAND) {
        if let Some(mode) = parse_gamemode(command) {
            send_sim_command_accepted(
                ctx,
                SimCommand::SetGameMode {
                    player,
                    mode,
                    acceptance: None,
                },
            )
            .await?;
            // Switch both client-side mirrors only after authoritative shard
            // admission, so saturation cannot split visual and simulation modes.
            enqueue_traced_classified(
                writer,
                debug,
                compression,
                &ctx.clock,
                ClientboundPlayPacket::GameEvent(GameEvent::new(
                    GAME_EVENT_CHANGE_GAMEMODE,
                    f32::from(mode.as_id()),
                )),
            );
            inventory.set_game_mode(mode);
        }
    }

    // The handler ran and every authoritative side effect above was accepted:
    // only now may the issuer receive success feedback.
    if !result.feedback().to_plain_string().is_empty() {
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            &ctx.clock,
            ferrumc_session::system_chat(result.feedback(), false),
        );
    }

    // Presentation and scoreboard commands have no shard mutation. Enqueue their
    // clientbound effects after the same successful dispatch point.
    for packet in crate::command::presentation_packets(command, policy.spawn()) {
        enqueue_traced_classified(writer, debug, compression, &ctx.clock, packet);
    }
    for packet in crate::command::scoreboard_packets(command, name)? {
        enqueue_traced_classified(writer, debug, compression, &ctx.clock, packet);
    }
    Ok(())
}

/// Handles a serverbound `UseItemOn`: place the held block at the targeted cell,
/// after consulting the loaded plugins at the intent boundary.
///
/// Resolves the held hotbar stack to a block-state. An empty hand, a non-placeable
/// item, or a malformed face places nothing but still acknowledges the
/// block-action sequence so the client's optimistic prediction ends. A placeable
/// block is offered to the plugins' `before_block_place` hooks (off the tick); the
/// combined decision is then resolved:
///
/// - [`Deny`](ResolvedDecision::Deny): nothing is placed; the actor is healed
///   through the single reject funnel ([`reject_block_edit`]) — the sim reads the
///   authoritative state at the target (usually air) and sends the mandatory resync
///   and ack, so no ghost block remains — and any Deny message is delivered as a
///   system chat. The rejection is counted on the sim's resulting
///   `BlockChangeRejected`.
/// - [`Replace`](ResolvedDecision::Replace): the replacement block-state is written
///   verbatim (an exact write via [`SimCommand::SetBlockExact`], never refined by
///   `compute_placement`) instead of the held one.
/// - [`Allow`](ResolvedDecision::Allow): the held block is placed (creative never
///   decrements the stack) and refined by `compute_placement` — the only path that
///   runs it.
///
/// On a non-denied placement any emitted intents are routed and the
/// `after_block_place` notification fires with the FINAL state: the exact
/// replacement for a Replace, or the driver-previewed computed state for an Allow.
#[allow(clippy::too_many_arguments)] // one place step: inventory + plugin policy + placement context + I/O
#[allow(clippy::too_many_lines)] // one place dispatch: deny + exact replace + refined allow arms
async fn handle_use_item_on(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    packet: &UseItemOn,
    player_yaw: f32,
    player_position: Option<Vec3>,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let sequence = packet.sequence();
    // A malformed face index yields no target: just ack so the prediction ends.
    let Some(position) = use_item_on_target(packet) else {
        ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
        return Ok(());
    };
    // The clicked face shares `use_item_on_target`'s face decoding, so a `Some`
    // target guarantees a `Some` face; the fallback is unreachable but fails safe.
    let clicked_face = use_item_on_face(packet).unwrap_or(Direction::Up);
    // The cursor hit point inside the clicked block (`0.0..=1.0`), widened to the
    // f64 the placement context uses.
    let cursor_position = Vec3::new(
        f64::from(packet.cursor_x()),
        f64::from(packet.cursor_y()),
        f64::from(packet.cursor_z()),
    );

    let perms = PermissionFacade::new(ctx.policy.permissions());

    // Consult the plugins at the interaction intent boundary FIRST: a right-click on
    // a block is an interaction regardless of what (if anything) it would place. A
    // Deny stops the interaction here — heal the client's prediction, show the
    // reason, and do not place — before the block-place decision even runs. (The
    // current protocol slice only carries use-item-on-block, so the target is always
    // a `Block`; air/entity interactions are a future wiring.)
    let (interact_decision, interact_intents) = ctx.block_events.before_interact(
        &InteractAttempt::new(
            player,
            interact_hand(packet.hand()),
            InteractTarget::Block {
                pos: position,
                face: clicked_face,
            },
        ),
        player_position,
        &perms,
    );
    if let PluginEventDecision::Deny { message } = interact_decision {
        // The interaction is refused. If the held item would have placed a block,
        // the client already predicted that placement at `position`; an ack alone
        // heals to the predicted state and leaves a ghost block (see
        // `reject_block_edit`), so route the same heal a refused place uses. A
        // non-placeable interaction predicts no block, so a plain ack ends it.
        if let Some(held_state) = inventory.held().placeable_block() {
            reject_block_edit(
                ctx,
                player,
                position,
                sequence,
                BlockStateId::new(held_state),
            )
            .await?;
        } else {
            ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
        }
        deliver_deny_message(ctx, writer, debug, compression, message);
        return route_emitted_intents(
            ctx,
            player,
            writer,
            sequence,
            interact_intents,
            debug,
            compression,
        )
        .await;
    }
    // Allowed: route any intents the interaction emitted, then continue to placement.
    route_emitted_intents(
        ctx,
        player,
        writer,
        sequence,
        interact_intents,
        debug,
        compression,
    )
    .await?;

    // Right-clicking a chest opens its container instead of placing (vanilla opens
    // regardless of a held placeable block when not sneaking). Net never reads the
    // world, so ask the clicked block's owning shard whether it is an openable chest
    // and, if so, snapshot its contents. A non-chest falls through to placement.
    let clicked = use_item_on_block(packet);
    let (open_tx, open_rx) = oneshot::channel();
    ctx.commands
        .send(SimCommand::OpenContainer {
            player,
            position: clicked,
            reply: open_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
    let chest_open = open_rx
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver dropped the open-container reply"))?;
    if let Some(slots) = chest_open {
        // End the client's interaction prediction, then show the container screen.
        ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
        return open_chest_window(
            ctx,
            player,
            writer,
            inventory,
            window_state,
            clicked,
            slots,
            debug,
            compression,
        )
        .await;
    }

    // Empty hand or non-placeable item: nothing to place, just ack. The block-place
    // plugins are not consulted for a no-op placement (the interaction already was).
    let Some(held_state) = inventory.held().placeable_block() else {
        ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
        return Ok(());
    };

    let (decision, emitted) = ctx.block_events.before_block_place(
        &BlockPlaceAttempt::new(player, position, held_state),
        &perms,
    );

    match decision {
        ResolvedDecision::Deny { message } => {
            // Heal the actor through the single reject funnel: the sim reads the
            // authoritative state at the target (usually air) and sends the
            // mandatory resync + ack (no ghost). The predicted state for a place is
            // the held block; it heals the target back to its current block. The
            // metric is counted on the resulting BlockChangeRejected, not here.
            reject_block_edit(
                ctx,
                player,
                position,
                sequence,
                BlockStateId::new(held_state),
            )
            .await?;
            deliver_deny_message(ctx, writer, debug, compression, message);
            route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression).await
        }
        ResolvedDecision::Replace { block_state_id } => {
            // A plugin Replace supplies an EXACT state: write it verbatim through
            // the exact-write path so compute_placement never re-derives it (a
            // rotated replacement must survive). The clicked face / cursor / yaw are
            // deliberately NOT passed — the plugin already chose the final state.
            // Reject an unrepresentable id (> i32::MAX) and heal the actor (it
            // predicted the held block).
            if i32::try_from(block_state_id).is_err() {
                tracing::warn!(
                    block_state_id,
                    "plugin replace supplied an out-of-range block state id; healing actor and skipping the place"
                );
                reject_block_edit(
                    ctx,
                    player,
                    position,
                    sequence,
                    BlockStateId::new(held_state),
                )
                .await?;
                return route_emitted_intents(
                    ctx,
                    player,
                    writer,
                    sequence,
                    emitted,
                    debug,
                    compression,
                )
                .await;
            }
            send_sim_command_accepted(
                ctx,
                SimCommand::SetBlockExact {
                    player,
                    position,
                    sequence,
                    state: BlockStateId::new(block_state_id),
                    acceptance: None,
                },
            )
            .await?;
            route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression)
                .await?;
            // The exact replacement state IS the final state, so the after-hook
            // observes it directly (no refinement to wait on).
            let after =
                ctx.block_events
                    .after_block_place(player, position, block_state_id, &perms);
            route_emitted_intents(ctx, player, writer, sequence, after, debug, compression).await
        }
        _ => {
            // Allow: a player placement refined by compute_placement (the ONLY path
            // that runs it). Creative places the held block without touching the
            // stack; the clicked face, cursor hit point, and player yaw ride along
            // so the sim derives the correct rotated/faced/halved state. After
            // bounded shard admission, the driver previews that final state and
            // replies before the tick, so the after-hook fires with the state the
            // world will hold (e.g. a side-faced log's axis=x), not the held
            // default.
            let (reply_tx, reply_rx) = oneshot::channel();
            ctx.commands
                .send(SimCommand::PlaceBlock {
                    player,
                    position,
                    sequence,
                    state: BlockStateId::new(held_state),
                    clicked_face,
                    cursor_position,
                    player_yaw,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
            let computed = reply_rx
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?
                .map_err(|err| anyhow::anyhow!("block placement rejected: {err}"))?;
            route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression)
                .await?;
            let after =
                ctx.block_events
                    .after_block_place(player, position, computed.as_u32(), &perms);
            route_emitted_intents(ctx, player, writer, sequence, after, debug, compression).await
        }
    }
}

/// Handles a serverbound Update Sign: route the player's new sign text to the
/// block's owning shard as a [`SimCommand::UpdateSign`].
///
/// All validation — the editor is within reach and a non-waxed sign block-entity
/// exists at the target — happens in the simulation at the tick boundary, since
/// net never reads or writes the world directly. This only converts the wire
/// fields (the packed location and the four bounded line strings) and forwards
/// them; an edit the sim refuses is a silent no-op.
async fn handle_update_sign(
    ctx: &ConnContext,
    player: PlayerId,
    packet: &UpdateSign,
) -> anyhow::Result<()> {
    let loc = packet.location();
    let position = BlockPos::new(loc.x(), loc.y(), loc.z());
    let lines = [
        packet.line_1().as_str().to_owned(),
        packet.line_2().as_str().to_owned(),
        packet.line_3().as_str().to_owned(),
        packet.line_4().as_str().to_owned(),
    ];
    send_sim_command_accepted(
        ctx,
        SimCommand::UpdateSign {
            player,
            position,
            is_front: packet.is_front_text(),
            lines,
            acceptance: None,
        },
    )
    .await
}

/// Handles a serverbound Set Creative Slot: validate the untrusted item bytes,
/// store the slot, and echo it.
///
/// Requires the player to be authoritatively creative (read from the connection's
/// drift-free game-mode mirror); a non-creative sender is ignored. The `slot` must
/// be in `0..=45` (a `-1` "drop outside" or any other out-of-range value is
/// ignored). The item bytes have already gone through strict
/// [`UntrustedItemStack::decode`], nested reader exhaustion, and `into_validated`
/// before this policy handler runs. Validation clamps the count and strips
/// dangerous/unknown components. A malformed item is therefore fatal before
/// either the game-mode/slot gate or any inventory effect. On success the slot
/// is stored, the state id bumped, and a mandatory `SetContainerSlot` echoes the
/// authoritative slot back so the client view matches the server.
///
/// If the changed slot is mirrored into the player's visible equipment (held main
/// hand, off hand, or worn armor — see [`PlayerInventory::is_equipment_slot`]), the
/// full equipment set is rebroadcast to viewers via [`SimCommand::SetEquipment`] so
/// they see the new piece — or its removal, since empty slots encode as air. An
/// encode failure for the broadcast is logged and skipped (never fatal).
#[allow(clippy::too_many_arguments)] // one creative-slot step: identity + window state + I/O + trace
async fn handle_set_creative_slot(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    packet: &SetCreativeSlot,
    stack: ItemStack,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    // Authoritative-creative gate: only a creative player may author slots.
    if inventory.game_mode() != GameMode::Creative {
        tracing::debug!(
            player = name,
            "ignoring set-creative-slot from non-creative player"
        );
        return Ok(());
    }
    // Bounds: -1 (drop) or anything outside 0..=45 is ignored.
    let Ok(index) = usize::try_from(packet.slot()) else {
        return Ok(());
    };
    if index >= SLOT_COUNT {
        return Ok(());
    }
    inventory.set_creative_slot(index, stack);

    // Echo the authoritative slot (mandatory) so the client view matches.
    let mut item_bytes = Vec::new();
    let Some(stored) = inventory.slot(index) else {
        return Ok(());
    };
    if let Err(err) = stored.encode_slot(&mut item_bytes) {
        tracing::warn!(player = name, %err, "failed to encode creative-slot echo");
        return Ok(());
    }
    send_mandatory(
        writer,
        debug,
        compression,
        &ctx.clock,
        ClientboundPlayPacket::SetContainerSlot(SetContainerSlot::new(
            WINDOW_ID,
            inventory.state_id(),
            packet.slot(),
            item_bytes,
        )),
    )?;

    // If the changed slot is mirrored into the player's visible equipment (held
    // main hand, off hand, or worn armor), rebroadcast the full equipment set so
    // viewers see the new piece — or its removal, since empty slots encode as air.
    // Non-equipment slots (storage grid, crafting) need no broadcast.
    if inventory.is_equipment_slot(index) {
        match inventory.equipment_body() {
            Ok(equipment) => {
                ctx.commands
                    .send(SimCommand::SetEquipment { player, equipment })
                    .await
                    .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
            }
            Err(err) => {
                tracing::warn!(
                    player = name,
                    %err,
                    "failed to encode equipment after creative slot set; skipping broadcast"
                );
            }
        }
    }
    Ok(())
}

/// Handles a serverbound Set Held Item: update the selected hotbar index and, on a
/// real change, broadcast the new main-hand item to viewers.
///
/// The wire slot is an `i16`; values outside `0..=8` are ignored (the client
/// already moved its own selector, so no clientbound reply is needed and nothing
/// is broadcast). On a valid change the new main-hand equipment body is encoded
/// from the connection-local inventory and routed as a [`SimCommand::SetEquipment`]
/// so the driver-owned router relays it (droppable) to the viewers that have this
/// player spawned — the inventory stays connection-local; only the opaque body
/// crosses to the router. A failed encode is logged and skipped (never fatal).
async fn handle_set_held_item(
    ctx: &ConnContext,
    player: PlayerId,
    inventory: &mut PlayerInventory,
    packet: &ServerboundSetHeldItem,
) -> anyhow::Result<()> {
    let Ok(slot) = u8::try_from(packet.slot()) else {
        return Ok(());
    };
    // Only broadcast when the selection actually changed (an out-of-range slot
    // leaves `selected` untouched and returns false).
    if !inventory.set_selected(slot) {
        return Ok(());
    }
    let equipment = match inventory.equipment_body() {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(%err, "failed to encode held-item equipment; skipping broadcast");
            return Ok(());
        }
    };
    ctx.commands
        .send(SimCommand::SetEquipment { player, equipment })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))
}

/// Handles a serverbound Click Container.
///
/// On the open chest window, the SAFE click subset is applied directly:
/// **normal-mode left-click** (`mode = 0`, `button = 0`) on a chest or
/// player-inventory slot runs the item-count-conserving exchange (pickup / place /
/// merge / swap — chest mutations round-trip atomically to the simulation). EVERY
/// other case — right-click, shift-click, drag, number-key swap, double-click,
/// drop-outside, an out-of-window slot, or a malformed body — is treated as a
/// no-op and the whole window is resynced (authoritative `SetContainerContent`)
/// rather than guessed, so the protocol can never duplicate or lose an item. A
/// click on window 0 (the always-open inventory) resyncs it. Never disconnects,
/// never trusts the client's claimed result, never panics.
#[allow(clippy::too_many_arguments)] // one click step: identity + window state + I/O + trace
async fn handle_window_click(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    packet: &WindowClick,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let window_id = packet.window_id();
    // A click on the open chest window: apply the safe subset, then resync (which
    // both confirms a handled click and heals an unhandled/ambiguous one).
    if window_state
        .open()
        .is_some_and(|open| open.window_id() == window_id)
    {
        apply_chest_window_click(ctx, player, inventory, window_state, packet).await?;
        return resync_chest_window(ctx, writer, inventory, window_state, debug, compression);
    }
    // Otherwise the always-open player inventory (window 0): conservative resync.
    if window_id != WINDOW_ID {
        return Ok(());
    }
    inventory.bump_state_id();
    let payload = match inventory.container_content_payload() {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(%err, "failed to encode container-content resync");
            return Ok(());
        }
    };
    send_mandatory(
        writer,
        debug,
        compression,
        &ctx.clock,
        ClientboundPlayPacket::SetContainerContent(SetContainerContent::new(
            WINDOW_ID,
            inventory.state_id(),
            payload,
        )),
    )
}

/// Applies the SAFE subset of a click on the open chest window to the
/// authoritative state, mutating the chest mirror / cursor / player inventory.
///
/// Only normal-mode left-click is applied; any other mode/button, an out-of-window
/// slot, or a malformed body is a no-op (the caller's resync heals the client).
/// The server recomputes the outcome from its own authoritative state and never
/// trusts the click body's trailing changed-slots / cursor (`HashedSlot`) fields,
/// which are not even parsed.
async fn apply_chest_window_click(
    ctx: &ConnContext,
    player: PlayerId,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    packet: &WindowClick,
) -> anyhow::Result<()> {
    // Parse only the fixed prefix: slot (i16), button (i8), mode (varint). A
    // truncated/malformed body resyncs (no-op here).
    let Some((slot, button, mode)) = decode_click_prefix(packet.rest()) else {
        return Ok(());
    };
    // Normal-mode left-click only; everything else resyncs.
    if mode != 0 || button != 0 {
        return Ok(());
    }
    let Some(target) = OpenContainer::classify_slot(slot) else {
        return Ok(());
    };
    match target {
        WindowSlot::Chest(chest_slot) => {
            // Round-trip to the sim for an atomic, conserving chest mutation. Send
            // the current cursor; adopt the authoritative result.
            let Some(open) = window_state.open() else {
                return Ok(());
            };
            let position = open.position();
            let cursor = open.cursor().clone();
            let (reply_tx, reply_rx) = oneshot::channel();
            ctx.commands
                .send(SimCommand::ContainerLeftClick {
                    player,
                    position,
                    slot: chest_slot,
                    cursor,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
            let outcome = reply_rx
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver dropped the container reply"))?;
            // `None` (chest gone / out of reach) keeps the cursor; the resync heals.
            if let (Some((new_cursor, snapshot)), Some(open)) = (outcome, window_state.open_mut()) {
                open.set_cursor(new_cursor);
                open.set_chest_slots(snapshot);
            }
        }
        WindowSlot::Player(index) => {
            // A player-inventory slot shown in the window: a purely local conserving
            // exchange against the connection-owned inventory and cursor.
            if let (Some(open), Some(slot_ref)) =
                (window_state.open_mut(), inventory.slot_mut(index))
            {
                left_click_exchange(slot_ref, open.cursor_mut());
            }
        }
    }
    Ok(())
}

/// Decodes the fixed prefix of a Click Container body: slot (`i16`), button
/// (`i8`), and mode (`VarInt`).
///
/// Returns `None` for a truncated or malformed body (the caller then resyncs). It
/// never panics on hostile input: [`BoundedReader`] rejects a short read or an
/// over-long `VarInt` with an error rather than reading out of bounds. The trailing
/// changed-slots / cursor (`HashedSlot`) fields are deliberately NOT decoded — the
/// server recomputes the click outcome from its own authoritative state and never
/// trusts the client's claimed result.
fn decode_click_prefix(rest: &[u8]) -> Option<(i16, i8, i32)> {
    let mut reader = BoundedReader::new(rest);
    let slot = reader.read_i16().ok()?;
    let button = reader.read_i8().ok()?;
    let mode = reader.read_var_int().ok()?;
    Some((slot, button, mode))
}

/// Resyncs the open chest window: bumps its state id and re-sends the full
/// authoritative `SetContainerContent` (chest slots, player section, cursor).
///
/// This is the single confirm/heal path — sent after every handled click and
/// after every unhandled/ambiguous one — so the client's view always converges on
/// the server's authoritative state regardless of what it predicted.
fn resync_chest_window(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    inventory: &PlayerInventory,
    window_state: &mut WindowState,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let Some(open) = window_state.open_mut() else {
        return Ok(());
    };
    open.bump_state_id();
    let payload = match open.content_payload(inventory) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(%err, "failed to encode chest-window resync");
            return Ok(());
        }
    };
    send_mandatory(
        writer,
        debug,
        compression,
        &ctx.clock,
        ClientboundPlayPacket::SetContainerContent(SetContainerContent::new(
            open.window_id(),
            open.state_id(),
            payload,
        )),
    )
}

/// Opens a chest window: assigns a window id, stores the open state seeded with the
/// sim's slot snapshot, and sends `OpenScreen` + the initial `SetContainerContent`.
///
/// If a container is already open, it is closed first (returning its cursor) so a
/// carried item is never discarded when one window replaces another.
#[allow(clippy::too_many_arguments)] // one open step: identity + I/O + the chest snapshot
async fn open_chest_window(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    position: BlockPos,
    slots: Vec<ItemStack>,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    if window_state.open().is_some() {
        close_open_container(
            ctx,
            player,
            writer,
            inventory,
            window_state,
            debug,
            compression,
        )
        .await?;
    }
    let window_id = window_state.open_chest(position, slots);
    // OpenScreen first (generic_9x3, titled "Chest"), then the initial contents.
    send_mandatory(
        writer,
        debug,
        compression,
        &ctx.clock,
        open_screen(window_id, GENERIC_9X3_TYPE, &TextComponent::text("Chest")),
    )?;
    resync_chest_window(ctx, writer, inventory, window_state, debug, compression)
}

/// Handles a serverbound Close Container: close any open container, returning its
/// carried item to the inventory.
#[allow(clippy::too_many_arguments)] // one close step: identity + window state + I/O + trace
async fn handle_close_container(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    _packet: &CloseContainer,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    close_open_container(
        ctx,
        player,
        writer,
        inventory,
        window_state,
        debug,
        compression,
    )
    .await
}

/// Closes the open container (if any): deposits its carried item into the player
/// inventory (never lost), resyncs window 0 (player-slot clicks during the session
/// changed it), and refreshes the broadcast held item.
async fn close_open_container(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let Some(open) = window_state.take() else {
        return Ok(());
    };
    // Return the carried item to the inventory; a non-empty leftover means the
    // inventory was full (logged, never silently dropped — extremely unlikely in
    // the creative slice, which always has empty slots).
    let leftover = inventory.deposit(open.cursor().clone());
    if leftover.item().is_some() {
        tracing::warn!(
            ?player,
            count = leftover.count(),
            "inventory full on container close; carried items could not be returned"
        );
    }
    // Resync window 0 so the client sees both the returned cursor and any inventory
    // changes the session's player-slot clicks made.
    inventory.bump_state_id();
    let payload = match inventory.container_content_payload() {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(%err, "failed to encode inventory resync on container close");
            return Ok(());
        }
    };
    send_mandatory(
        writer,
        debug,
        compression,
        &ctx.clock,
        ClientboundPlayPacket::SetContainerContent(SetContainerContent::new(
            WINDOW_ID,
            inventory.state_id(),
            payload,
        )),
    )?;
    // The held hotbar slot may have changed during the session; refresh the
    // broadcast equipment so viewers see the right main-hand item (the full set is
    // sent, so off-hand/armor stay in sync too).
    match inventory.equipment_body() {
        Ok(equipment) => ctx
            .commands
            .send(SimCommand::SetEquipment { player, equipment })
            .await
            .map_err(|_| anyhow::anyhow!("simulation driver is gone")),
        Err(err) => {
            tracing::warn!(%err, "failed to encode equipment after container close");
            Ok(())
        }
    }
}

/// Answers a serverbound tab-complete request, enqueuing a `TabCompleteResponse`
/// built from the command tree's suggestion engine.
///
/// The request `text` is the full chat-box content including the leading `/`; the
/// slash is stripped before suggesting, and `start`/`length` are computed so the
/// client replaces exactly the in-progress token. Suggestions are filtered to the
/// literals the player's permission level *and* granted permission nodes allow
/// (matching the permission-filtered command graph the join kit declared), and
/// argument *hints* such as `<mode: 0..3>` are dropped — only concrete literal
/// completions are sent, never placeholder text the client would insert verbatim.
/// The offsets are character positions (the units the protocol's Command
/// Suggestions field expects), so a non-ASCII prefix is indexed correctly.
fn handle_tab_complete(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    transaction_id: i32,
    text: &str,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) {
    let level = ctx.policy.permission_level(player);
    let allowed = |node: &str| ctx.policy.permissions().is_allowed(player, node);
    let (start, length, suggestions) =
        tab_complete_reply(ctx.policy.command_tree(), level, &allowed, text);

    let matches: Vec<CommandSuggestionMatch> = suggestions
        .into_iter()
        .filter_map(|suggestion| {
            BoundedString::<32_767>::new(suggestion)
                .ok()
                // MVP: no hover tooltip (a single absent-flag byte on the wire).
                .map(|suggestion| CommandSuggestionMatch::new(suggestion, None))
        })
        .collect();

    let response = TabCompleteResponse::new(
        transaction_id,
        i32::try_from(start).unwrap_or(i32::MAX),
        i32::try_from(length).unwrap_or(i32::MAX),
        matches,
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        &ctx.clock,
        ClientboundPlayPacket::TabCompleteResponse(response),
    );
}

/// Computes the tab-complete reply for `text` at permission `level` against
/// `tree`, gating permission-node-declared commands through `is_allowed`: the
/// `(start, length)` *character* span of `text` the matches replace, and the
/// filtered list of literal completions.
///
/// Pure (no I/O), so it is unit-tested directly. The leading `/` is stripped
/// before suggesting; `start`/`length` delimit the in-progress token (from after
/// the last whitespace to the end of `text`). They are reported in character units
/// — what the protocol's Command Suggestions field expects — so a non-ASCII prefix
/// is indexed correctly rather than by UTF-8 byte offset. Matches are filtered to
/// the literals the player's `level` and granted permission nodes allow (the
/// declared graph is filtered the same way), and argument *hints* (which begin with
/// `<`) are dropped so the client is never sent placeholder text to insert verbatim.
fn tab_complete_reply(
    tree: &CommandTree,
    level: u8,
    is_allowed: &dyn Fn(&str) -> bool,
    text: &str,
) -> (usize, usize, Vec<String>) {
    let input = text.strip_prefix('/').unwrap_or(text);
    let offset = text.len() - input.len();
    // Byte index in `input` just past the last whitespace char (the token start).
    // Stepping by the whitespace char's UTF-8 width (not a bare `+ 1`) keeps the
    // index on a char boundary even for a multi-byte whitespace char, so the slices
    // below never panic on hostile input.
    let token_start = input.rfind(char::is_whitespace).map_or(0, |idx| {
        idx + input[idx..].chars().next().map_or(1, char::len_utf8)
    });
    let start_bytes = offset + token_start;
    // The protocol carries start/length as character positions, so convert the byte
    // offsets to char counts; for ASCII (the common case) the two coincide.
    let start = text[..start_bytes].chars().count();
    let length = text[start_bytes..].chars().count();

    let graph = tree.to_brigadier(level, is_allowed);
    let allowed: Vec<&str> = graph
        .nodes()
        .iter()
        .filter_map(|node| node.name())
        .collect();
    let matches = tree
        .suggest(input)
        .into_iter()
        .filter(|suggestion| !suggestion.starts_with('<'))
        .filter(|suggestion| allowed.contains(&suggestion.as_str()))
        .collect();
    (start, length, matches)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use bytes::BytesMut;
    use ferrumc_codec::{write_var_int, BoundedReader};
    use ferrumc_config::AccessConfig;
    use ferrumc_core::{GameMode, PlayerId, PluginId};
    use ferrumc_math::{ShardPos, Vec3, WorldIntent};
    use ferrumc_net::{CompressionState, ConnectionLimits, DisconnectReason, PlayWriter};
    use ferrumc_observability::{CounterRegistry, NetTelemetryHub, ServerClock, SessionDebug};
    use ferrumc_plugin_api::{
        Capability, CapabilityManifest, EventContext, EventKind, Plugin, PluginError, PluginEvent,
        PluginMetadata, SetupContext, Version,
    };
    use ferrumc_plugin_host::PluginHost;
    use ferrumc_proto::generated::play::{
        ClientboundPlayPacket, ServerboundKeepAlive, ServerboundPlayPacket, SetCenterChunk,
        SetCreativeSlot, SetPlayerPosition, SetPlayerPositionAndRotation, SetPlayerRotation,
        UpdateSign,
    };
    use tokio::sync::mpsc;

    use super::{
        decode_click_prefix, decode_play_body, handle_play_body, route_emitted_intents,
        tab_complete_reply, validated_movement, MovementObservation, MovementValidation,
        PlayBodyDecode,
    };
    use crate::command::{build_command_tree, GAMEMODE_COMMAND, SPAWN_COMMAND};
    use crate::config::AppConfig;
    use crate::connection::chunk_stream::{apply_chunk_stream, ChunkStream};
    use crate::connection::context::{build_status_response, ConnContext};
    use crate::connection::rate_limiter::ChatRateLimiter;
    use crate::driver::SimCommand;
    use crate::inventory::PlayerInventory;
    use crate::plugins::{build_play_policy, BlockEventDispatcher};
    use crate::registries::ConfigRegistries;
    use crate::window::WindowState;
    use crate::world::build_world;

    fn strict_body_disconnect_reason(body: &[u8]) -> DisconnectReason {
        let Err(error) = decode_play_body(body) else {
            panic!("body unexpectedly decoded");
        };
        DisconnectReason::from_disconnect_class(error.disconnect_class())
    }

    #[test]
    fn strict_body_decode_preserves_disconnect_classes_and_compatibility() {
        assert_eq!(
            strict_body_disconnect_reason(&[]),
            DisconnectReason::MalformedPacket,
        );

        let mut trailing = BytesMut::new();
        ServerboundKeepAlive::new(9)
            .encode(&mut trailing)
            .expect("Keep Alive encodes");
        trailing.extend_from_slice(&[0xDE, 0xAD]);
        assert_eq!(
            strict_body_disconnect_reason(&trailing),
            DisconnectReason::ProtocolViolation,
        );

        let mut invalid_id = Vec::new();
        write_var_int(&mut invalid_id, 0x77);
        assert_eq!(
            strict_body_disconnect_reason(&invalid_id),
            DisconnectReason::ProtocolViolation,
        );

        let mut nested_item = Vec::new();
        write_var_int(&mut nested_item, 1);
        write_var_int(&mut nested_item, 1);
        write_var_int(&mut nested_item, 0);
        write_var_int(&mut nested_item, 0);
        nested_item.extend_from_slice(&[0xDE, 0xAD]);
        let mut creative = BytesMut::new();
        SetCreativeSlot::new(9, nested_item)
            .encode(&mut creative)
            .expect("creative slot encodes");
        assert_eq!(
            strict_body_disconnect_reason(&creative),
            DisconnectReason::ProtocolViolation,
        );

        let mut tick_end = Vec::new();
        write_var_int(&mut tick_end, 0x0c);
        assert!(matches!(
            decode_play_body(&tick_end),
            Ok(PlayBodyDecode::Unmodelled { id: 0x0c })
        ));
    }
    /// Test plugin that makes a `PlayerMove` callback observable without changing
    /// production metrics or relying on captured logs.
    struct MoveProbe {
        calls: Arc<AtomicUsize>,
    }

    impl Plugin for MoveProbe {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata::new(
                PluginId::new("packet26-move-probe"),
                "packet26-move-probe",
                Version::new(0, 1, 0),
                CapabilityManifest::empty().with(Capability::ReceiveEvents),
            )
        }

        fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
            ctx.events()?.subscribe(EventKind::PlayerMove);
            Ok(())
        }

        fn on_event(&mut self, event: &PluginEvent, _ctx: &mut EventContext<'_>) {
            if matches!(event, PluginEvent::PlayerMove { .. }) {
                self.calls.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Builds the real handler context around a bounded command spy and a
    /// movement-notification probe.
    async fn movement_handler_context(
        commands: mpsc::Sender<SimCommand>,
        move_calls: Arc<AtomicUsize>,
    ) -> ConnContext {
        let config = AppConfig {
            spawn_chunk_radius: 0,
            view_distance: 0,
            ..AppConfig::default()
        };
        let setup = build_world(&config, ShardPos::new(0, 0))
            .await
            .expect("build one-column handler world");
        let (policy, _default_dispatcher) =
            build_play_policy(&config).expect("build handler policy");

        let mut host = PluginHost::in_memory();
        let probe = host
            .register(Box::new(MoveProbe { calls: move_calls }))
            .expect("register movement probe");
        host.enable(&probe).expect("enable movement probe");

        let access = AccessConfig::default()
            .resolve(Path::new("."))
            .expect("resolve default access policy");
        ConnContext {
            limits: ConnectionLimits::default(),
            io_timeout: config.io_timeout,
            compression_threshold: config.compression_threshold,
            join_kit: setup.join_kit,
            config: Arc::new(ConfigRegistries::build().expect("build registries")),
            keep_alive_interval: config.keep_alive_interval,
            chunk_stream_interval: config.chunk_stream_interval,
            commands,
            player_store: setup.player_store,
            policy: Arc::new(policy),
            block_events: Arc::new(BlockEventDispatcher::new(host)),
            status_response: Arc::new(build_status_response(1).expect("build status response")),
            view_distance: config.view_distance,
            metrics: Arc::new(CounterRegistry::new()),
            clock: ServerClock::new(),
            net_telemetry: Arc::new(NetTelemetryHub::new()),
            access: Arc::new(access),
            budget: config.budget,
        }
    }

    #[test]
    fn decode_click_prefix_parses_valid_and_rejects_malformed_without_panicking() {
        // Valid: slot 5 (i16 big-endian), button 0, mode 0; trailing bytes ignored.
        assert_eq!(
            decode_click_prefix(&[0x00, 0x05, 0x00, 0x00]),
            Some((5, 0, 0))
        );
        assert_eq!(
            decode_click_prefix(&[0x00, 0x05, 0x01, 0x01, 0xDE, 0xAD]),
            Some((5, 1, 1)),
            "trailing changed-slots/cursor bytes are ignored, not trusted"
        );
        // Truncated bodies are rejected (no panic), not read out of bounds.
        assert_eq!(decode_click_prefix(&[]), None);
        assert_eq!(decode_click_prefix(&[0x00]), None); // half a slot
        assert_eq!(decode_click_prefix(&[0x00, 0x05]), None); // no button
        assert_eq!(decode_click_prefix(&[0x00, 0x05, 0x00]), None); // no mode
                                                                    // An over-long (6-byte) VarInt for mode is rejected by the bounded reader.
        assert_eq!(
            decode_click_prefix(&[0x00, 0x05, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]),
            None
        );
    }

    const OP_LEVEL: u8 = 4;
    const MEMBER_LEVEL: u8 = 0;

    #[test]
    fn movement_validation_preserves_packet_component_shape() {
        // A turn-in-place (SetPlayerRotation) updates the yaw the placement path
        // reads, so rotating then placing orients stairs/furnaces correctly. This
        // locks in that the rotation-only packet feeds the place yaw.
        let turn = ServerboundPlayPacket::SetPlayerRotation(SetPlayerRotation::new(90.0, -10.0, 1));
        assert_eq!(
            validated_movement(&turn),
            MovementValidation::Valid(MovementObservation {
                position: None,
                yaw: Some(90.0),
                pitch: Some(-10.0),
            })
        );

        // A position-only move carries no yaw, so it must leave the mirrored yaw
        // untouched (None), not reset it to 0.
        let strafe =
            ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(1.0, 2.0, 3.0, 0));
        assert_eq!(
            validated_movement(&strafe),
            MovementValidation::Valid(MovementObservation {
                position: Some(Vec3::new(1.0, 2.0, 3.0)),
                yaw: None,
                pitch: None,
            })
        );
    }

    #[test]
    fn invalid_movement_components_are_rejected_before_observation() {
        for (label, coordinate) in [
            ("NaN", f64::NAN),
            ("positive infinity", f64::INFINITY),
            ("negative infinity", f64::NEG_INFINITY),
            ("above the positive boundary", 30_000_001.0),
            ("below the negative boundary", -30_000_001.0),
        ] {
            let packet = ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(
                coordinate, 64.0, 8.0, 0,
            ));
            assert_eq!(
                validated_movement(&packet),
                MovementValidation::Invalid,
                "{label} reached the connection's stateful movement path"
            );
        }

        for coordinate in [-30_000_000.0, 30_000_000.0] {
            let packet = ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(
                coordinate, 64.0, 8.0, 0,
            ));
            assert_eq!(
                validated_movement(&packet),
                MovementValidation::Valid(MovementObservation {
                    position: Some(Vec3::new(coordinate, 64.0, 8.0)),
                    yaw: None,
                    pitch: None,
                }),
                "the inclusive simulation boundary must stay accepted",
            );
        }

        let invalid_yaw =
            ServerboundPlayPacket::SetPlayerRotation(SetPlayerRotation::new(f32::NAN, 0.0, 0));
        assert_eq!(
            validated_movement(&invalid_yaw),
            MovementValidation::Invalid,
            "a non-finite yaw must not enter the persistence mirror"
        );

        let invalid_pitch = ServerboundPlayPacket::SetPlayerPositionAndRotation(
            SetPlayerPositionAndRotation::new(8.0, 64.0, 8.0, 0.0, f32::NEG_INFINITY, 0),
        );
        assert_eq!(
            validated_movement(&invalid_pitch),
            MovementValidation::Invalid,
            "a non-finite pitch must not enter the persistence mirror"
        );

        let valid_position_invalid_look = ServerboundPlayPacket::SetPlayerPositionAndRotation(
            SetPlayerPositionAndRotation::new(8.0, 64.0, 8.0, f32::INFINITY, 15.0, 0),
        );
        assert_eq!(
            validated_movement(&valid_position_invalid_look),
            MovementValidation::Invalid,
            "a combined packet is rejected atomically instead of applying its position"
        );

        let invalid_position_valid_look = ServerboundPlayPacket::SetPlayerPositionAndRotation(
            SetPlayerPositionAndRotation::new(30_000_001.0, 64.0, 8.0, 90.0, 15.0, 0),
        );
        assert_eq!(
            validated_movement(&invalid_position_valid_look),
            MovementValidation::Invalid,
            "a combined packet is rejected atomically instead of applying its look"
        );

        let wrapped_finite_look = ServerboundPlayPacket::SetPlayerRotation(SetPlayerRotation::new(
            100_000.0, -100_000.0, 0,
        ));
        assert_eq!(
            validated_movement(&wrapped_finite_look),
            MovementValidation::Valid(MovementObservation {
                position: None,
                yaw: Some(100_000.0),
                pitch: Some(-100_000.0),
            }),
            "finite angles remain protocol-valid and may wrap"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one ordered trust-boundary transaction and its spies
    async fn movement_handler_rejects_before_every_observable_side_effect() {
        // At most two sequential positive controls are emitted. Eight slots keep
        // diagnostic headroom while the active receiver makes queue saturation
        // impossible, and the capacity remains explicitly bounded.
        const COMMAND_CAPACITY: usize = 8;

        let command_calls = Arc::new(AtomicUsize::new(0));
        let move_calls = Arc::new(AtomicUsize::new(0));
        let (commands_tx, mut commands_rx) = mpsc::channel(COMMAND_CAPACITY);
        let command_calls_for_task = Arc::clone(&command_calls);
        let command_task = tokio::spawn(async move {
            while let Some(command) = commands_rx.recv().await {
                command_calls_for_task.fetch_add(1, Ordering::Relaxed);
                let (SimCommand::Event { acceptance, .. }
                | SimCommand::TeleportPlayer { acceptance, .. }) = command
                else {
                    panic!("movement test emitted an unexpected command");
                };
                if let Some(reply) = acceptance {
                    let _ = reply.send(Ok(()));
                }
            }
        });

        let ctx = movement_handler_context(commands_tx, Arc::clone(&move_calls)).await;
        let player = PlayerId::offline("MovementHandlerProbe");
        let mut writer = PlayWriter::with_defaults(ctx.limits);
        let mut chunk_stream = ChunkStream::new(&ctx);
        let base = Vec3::new(8.5, 64.0, 8.5);
        chunk_stream.observe(base);
        assert_eq!(chunk_stream.block_crossing(base), None);
        let mut chat_limiter = ChatRateLimiter::new(ctx.clock.now());
        let mut inventory = PlayerInventory::with_creative_kit(GameMode::Creative);
        let mut window_state = WindowState::new();
        let mut yaw = 15.0;
        let mut pitch = -10.0;
        let mut debug = SessionDebug::new("movement-handler-probe");
        let compression = CompressionState::disabled();

        let invalid =
            [
                ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(
                    f64::NAN,
                    64.0,
                    8.5,
                    0,
                )),
                ServerboundPlayPacket::SetPlayerRotation(SetPlayerRotation::new(
                    f32::INFINITY,
                    20.0,
                    0,
                )),
                ServerboundPlayPacket::SetPlayerPositionAndRotation(
                    SetPlayerPositionAndRotation::new(9.5, 64.0, 8.5, 30.0, f32::NEG_INFINITY, 0),
                ),
                ServerboundPlayPacket::SetPlayerPositionAndRotation(
                    SetPlayerPositionAndRotation::new(30_000_001.0, 64.0, 8.5, 120.0, 30.0, 0),
                ),
            ];
        for packet in invalid {
            let mut body = BytesMut::new();
            packet.encode(&mut body).expect("encode hostile movement");
            handle_play_body(
                &ctx,
                player,
                "MovementHandlerProbe",
                &mut writer,
                &mut chunk_stream,
                &mut chat_limiter,
                &mut inventory,
                &mut window_state,
                &mut yaw,
                &mut pitch,
                &body,
                &mut debug,
                &compression,
            )
            .await
            .expect("reject hostile movement cleanly");
        }
        assert_eq!(command_calls.load(Ordering::Relaxed), 0);
        assert_eq!(move_calls.load(Ordering::Relaxed), 0);
        assert_eq!(chunk_stream.last_position(), Some(base));
        assert_eq!(chunk_stream.pending_position_for_test(), Some(base));
        assert_eq!((yaw, pitch), (15.0, -10.0));
        assert_eq!(writer.total_queued(), 0);

        // The probe is not vacuous: a valid admitted crossing reaches exactly one
        // driver event and one plugin notification, and updates every local mirror.
        let accepted = Vec3::new(9.5, 64.0, 8.5);
        let packet =
            SetPlayerPositionAndRotation::new(accepted.x, accepted.y, accepted.z, 45.0, -25.0, 0);
        let mut body = BytesMut::new();
        packet.encode(&mut body).expect("encode accepted movement");
        handle_play_body(
            &ctx,
            player,
            "MovementHandlerProbe",
            &mut writer,
            &mut chunk_stream,
            &mut chat_limiter,
            &mut inventory,
            &mut window_state,
            &mut yaw,
            &mut pitch,
            &body,
            &mut debug,
            &compression,
        )
        .await
        .expect("admit valid movement");
        assert_eq!(command_calls.load(Ordering::Relaxed), 1);
        assert_eq!(move_calls.load(Ordering::Relaxed), 1);
        assert_eq!(chunk_stream.last_position(), Some(accepted));
        assert_eq!(chunk_stream.pending_position_for_test(), Some(accepted));
        assert_eq!((yaw, pitch), (45.0, -25.0));

        // A later hostile combined packet cannot partially replace the accepted
        // position, look, plugin baseline, or routed event.
        let packet = SetPlayerPositionAndRotation::new(10.5, 64.0, 8.5, f32::NAN, -30.0, 0);
        let mut body = BytesMut::new();
        packet
            .encode(&mut body)
            .expect("encode later hostile movement");
        handle_play_body(
            &ctx,
            player,
            "MovementHandlerProbe",
            &mut writer,
            &mut chunk_stream,
            &mut chat_limiter,
            &mut inventory,
            &mut window_state,
            &mut yaw,
            &mut pitch,
            &body,
            &mut debug,
            &compression,
        )
        .await
        .expect("reject later hostile movement");
        assert_eq!(command_calls.load(Ordering::Relaxed), 1);
        assert_eq!(move_calls.load(Ordering::Relaxed), 1);
        assert_eq!(chunk_stream.last_position(), Some(accepted));
        assert_eq!(chunk_stream.pending_position_for_test(), Some(accepted));
        assert_eq!((yaw, pitch), (45.0, -25.0));
        assert_eq!(writer.total_queued(), 0);

        for position in [
            Vec3::new(f64::NAN, 64.0, 8.0),
            Vec3::new(30_000_001.0, 64.0, 8.0),
        ] {
            route_emitted_intents(
                &ctx,
                player,
                &mut writer,
                0,
                vec![WorldIntent::Teleport { player, position }],
                &mut debug,
                &compression,
            )
            .await
            .expect("skip unsafe plugin teleport cleanly");
        }
        assert_eq!(
            command_calls.load(Ordering::Relaxed),
            1,
            "unsafe plugin teleports never reach the driver"
        );

        route_emitted_intents(
            &ctx,
            player,
            &mut writer,
            0,
            vec![WorldIntent::Teleport {
                player,
                position: Vec3::new(20.0, 70.0, -5.0),
            }],
            &mut debug,
            &compression,
        )
        .await
        .expect("route a safe plugin teleport");
        assert_eq!(
            command_calls.load(Ordering::Relaxed),
            2,
            "the plugin teleport gate is not vacuously dropping every intent"
        );

        let mut overflow_stream = ChunkStream::new(&ctx);
        overflow_stream.observe(Vec3::new(40.5, 64.0, 8.5));
        let mut fills = 0usize;
        loop {
            let outcome = writer.enqueue_classified(ClientboundPlayPacket::SetCenterChunk(
                SetCenterChunk::new(0, 0),
            ));
            if outcome.is_dropped() {
                break;
            }
            fills += 1;
            assert!(
                fills < 1_000,
                "the bounded State queue must eventually fill"
            );
        }
        let error = apply_chunk_stream(
            &ctx,
            &mut writer,
            &mut overflow_stream,
            &mut debug,
            &compression,
        )
        .await
        .expect_err("a dropped mandatory stream centre terminates the session");
        assert!(error.to_string().contains("mandatory clientbound packet"));
        assert_eq!(
            overflow_stream.center_for_test(),
            ctx.join_kit.spawn_chunk(),
            "a dropped centre cannot commit app-side stream ownership"
        );
        assert_eq!(
            command_calls.load(Ordering::Relaxed),
            2,
            "a dropped centre cannot proceed into chunk ticket work"
        );

        drop(ctx);
        command_task
            .await
            .expect("bounded movement command spy shuts down");
    }

    #[test]
    fn tab_complete_offers_literal_completion_for_a_prefix() {
        let tree = build_command_tree();
        // "/sp" -> the in-progress token "sp" (char 1..3) completes to "spawn".
        let (start, length, matches) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/sp");
        assert_eq!((start, length), (1, 2));
        assert_eq!(matches, vec![SPAWN_COMMAND.to_string()]);
    }

    #[test]
    fn tab_complete_lists_all_commands_after_the_slash() {
        let tree = build_command_tree();
        let (start, length, matches) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/");
        assert_eq!((start, length), (1, 0));
        assert!(matches.contains(&SPAWN_COMMAND.to_string()));
        assert!(matches.contains(&GAMEMODE_COMMAND.to_string()));
    }

    #[test]
    fn tab_complete_hides_gated_commands_from_low_level_players() {
        let tree = build_command_tree();
        // A level-0 player gets `/spawn` but never `/gamemode`.
        let (_, _, op_matches) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/ga");
        assert_eq!(op_matches, vec![GAMEMODE_COMMAND.to_string()]);
        let (_, _, member_matches) = tab_complete_reply(&tree, MEMBER_LEVEL, &|_| true, "/ga");
        assert!(member_matches.is_empty());
    }

    #[test]
    fn tab_complete_drops_argument_hints() {
        let tree = build_command_tree();
        // After "/gamemode " the only candidate is the `<mode: 0..3>` hint, which is
        // display-only and must not be sent as an insertable match.
        let (start, length, matches) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/gamemode ");
        assert_eq!((start, length), (10, 0));
        assert!(matches.is_empty());
    }

    #[test]
    fn tab_complete_range_is_in_char_units_for_non_ascii() {
        let tree = build_command_tree();
        // "/éa": the accented `é` is two UTF-8 bytes, so the in-progress token "éa"
        // begins at character 1 (right after the slash) and is two characters long.
        // The old byte computation would report length 3, mis-indexing the client.
        let (start, length, _) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/\u{e9}a");
        assert_eq!((start, length), (1, 2));
    }

    /// Decodes a serverbound play body the same way `handle_play_body` does:
    /// read the `VarInt` id, then dispatch to the typed packet.
    fn decode_play(body: &[u8]) -> Result<ServerboundPlayPacket, ()> {
        let mut reader = BoundedReader::new(body);
        let id = reader.read_var_int().map_err(|_| ())?;
        ServerboundPlayPacket::decode(id, &mut reader).map_err(|_| ())
    }

    #[test]
    fn update_sign_decodes_a_well_formed_body() {
        // id + 8-byte packed location (all zero -> (0,0,0)) + is_front(1) + four
        // zero-length line strings.
        let mut body = Vec::new();
        write_var_int(&mut body, UpdateSign::PACKET_ID);
        body.extend_from_slice(&[0u8; 8]); // packed BlockPosition (0,0,0)
        body.push(0x01); // is_front_text = true
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // four empty VarInt-prefixed strings

        let ServerboundPlayPacket::UpdateSign(packet) = decode_play(&body).expect("valid body")
        else {
            panic!("expected an UpdateSign");
        };
        let loc = packet.location();
        assert_eq!((loc.x(), loc.y(), loc.z()), (0, 0, 0));
        assert!(packet.is_front_text());
        assert!(packet.line_1().as_str().is_empty());
        assert!(packet.line_4().as_str().is_empty());
    }

    #[test]
    fn update_sign_rejects_a_truncated_body() {
        // Only the location is present: the decoder needs the is_front bool and four
        // line strings next, so a body that stops here is malformed and rejected
        // rather than silently accepted with garbage.
        let mut truncated = Vec::new();
        write_var_int(&mut truncated, UpdateSign::PACKET_ID);
        truncated.extend_from_slice(&[0u8; 8]); // location only, nothing after
        assert!(decode_play(&truncated).is_err());

        // Even shorter: the location itself is incomplete (4 of 8 bytes).
        let mut shorter = Vec::new();
        write_var_int(&mut shorter, UpdateSign::PACKET_ID);
        shorter.extend_from_slice(&[0u8; 4]);
        assert!(decode_play(&shorter).is_err());

        // A line string claims a length far past the remaining bytes.
        let mut bad_string = Vec::new();
        write_var_int(&mut bad_string, UpdateSign::PACKET_ID);
        bad_string.extend_from_slice(&[0u8; 8]); // location
        bad_string.push(0x00); // is_front = false
        write_var_int(&mut bad_string, 4096); // line_1 claims 4096 bytes...
        bad_string.extend_from_slice(b"hi"); // ...but only 2 follow
        assert!(decode_play(&bad_string).is_err());
    }
}
