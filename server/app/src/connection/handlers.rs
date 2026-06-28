//! Per-packet serverbound play handlers: chat/commands, inventory, the block
//! break/place plugin-intent boundary and its single rejection funnel, and tab
//! completion.

use ferrumc_codec::{BoundedReader, BoundedString};
use ferrumc_command::{CommandSource, CommandTree};
use ferrumc_core::{GameMode, PlayerId, TextColor, TextComponent};
use ferrumc_items::UntrustedItemStack;
use ferrumc_math::{BlockPos, Vec3, WorldIntent};
use ferrumc_net::{CompressionState, PlayWriter};
use ferrumc_observability::{PacketState, SessionDebug};
use ferrumc_plugin_api::{BlockBreakAttempt, BlockPlaceAttempt};
use ferrumc_plugin_host::ResolvedDecision;
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, CommandSuggestionMatch, GameEvent, ServerboundPlayPacket,
    ServerboundSetHeldItem, SetContainerContent, SetContainerSlot, SetCreativeSlot,
    SetPlayerPosition, TabCompleteResponse, UseItemOn, WindowClick,
};
use ferrumc_session::{net_event_to_input, use_item_on_target, NetEvent};
use ferrumc_sim::{BlockStateId, GameInput};

use crate::command::{parse_gamemode, GAMEMODE_COMMAND, SPAWN_COMMAND};
use crate::driver::SimCommand;
use crate::inventory::{PlayerInventory, SLOT_COUNT, WINDOW_ID};
use crate::observe;
use crate::plugins::PermissionFacade;

use super::chunk_stream::ChunkStream;
use super::context::ConnContext;
use super::outbound::{ack_sequence, enqueue_traced_classified, send_mandatory};
use super::rate_limiter::ChatRateLimiter;
use super::{spawn_sync, JOIN_TELEPORT_ID};

/// `Game Event` reason `3`: "change game mode". The event `value` is the game-mode
/// id as a float; sending it switches the issuing client's mode (the carrier
/// `/gamemode` uses, since there is no dedicated set-game-mode packet).
const GAME_EVENT_CHANGE_GAMEMODE: u8 = 3;

/// Handles one decoded serverbound play-frame body.
///
/// Unknown or malformed play packets are ignored (the slice models only a
/// subset), as are the teleport confirmation and the Keep Alive echo. A
/// `ChatCommand` is dispatched locally; every other modelled packet is forwarded
/// to the simulation unless spawn protection vetoes it. A position packet is also
/// recorded on the chunk stream so the post-drain pass can react to a boundary
/// crossing.
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
    body: &[u8],
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let mut reader = BoundedReader::new(body);
    let Ok(id) = reader.read_var_int() else {
        // A play frame whose body has no readable packet id is malformed.
        ctx.metrics
            .record_packet_decode_error(PacketState::Play, "bad_packet_id");
        debug.dump("play_packet_decode_error");
        return Ok(());
    };
    let packet = match ServerboundPlayPacket::decode(id, &mut reader) {
        Ok(packet) => packet,
        Err(err) => {
            // An unknown id is an expected unmodelled packet (counted, not dumped);
            // a malformed body is a genuine decode error (counted and dumped).
            let (label, dump) = observe::play_decode_error(&err);
            ctx.metrics
                .record_packet_decode_error(PacketState::Play, label);
            if dump {
                debug.dump("play_packet_decode_error");
            }
            return Ok(());
        }
    };

    // Record the inbound play trace with the exact frame-body size.
    debug.record_inbound(observe::trace_inbound_play(
        &packet,
        body.len(),
        compression,
        &ctx.clock,
    ));

    // Track the client's reported position for chunk streaming. The packet is
    // still forwarded to the simulation below — this only mirrors the position the
    // stream centres on; the simulation stays authoritative.
    if let Some(position) = reported_position(&packet) {
        chunk_stream.observe(position);
    }

    match &packet {
        ServerboundPlayPacket::ChatCommand(command) => {
            let command = command.command().as_str().to_owned();
            return handle_command(
                ctx,
                player,
                name,
                writer,
                inventory,
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
            return handle_use_item_on(ctx, player, writer, inventory, p, debug, compression).await;
        }
        // Set Creative Slot (untrusted): validate the hostile item bytes, store the
        // slot, and echo it back so the client view matches the server.
        ServerboundPlayPacket::SetCreativeSlot(p) => {
            return handle_set_creative_slot(ctx, name, writer, inventory, p, debug, compression);
        }
        // Set Held Item (serverbound): update the selected hotbar index.
        ServerboundPlayPacket::ServerboundSetHeldItem(p) => {
            handle_set_held_item(inventory, p);
            return Ok(());
        }
        // Click Container: the slice models no click logic, so any click on window
        // 0 triggers a safe resync of the authoritative inventory.
        ServerboundPlayPacket::WindowClick(p) => {
            return handle_window_click(ctx, writer, inventory, p, debug, compression);
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
    // Every other event (movement, disconnect) carries no block decision and routes
    // straight to the simulation.
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
        .send(SimCommand::Event(event))
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))
}

/// The absolute position a serverbound play packet reports, if any.
///
/// Both absolute-move packets carry a position; the rotation-only and other
/// packets do not move the player and so report nothing.
fn reported_position(packet: &ServerboundPlayPacket) -> Option<Vec3> {
    match packet {
        ServerboundPlayPacket::SetPlayerPosition(p) => Some(Vec3::new(p.x(), p.y(), p.z())),
        ServerboundPlayPacket::SetPlayerPositionAndRotation(p) => {
            Some(Vec3::new(p.x(), p.y(), p.z()))
        }
        _ => None,
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
///   replacement state instead of air, by routing a [`SimCommand::PlaceBlock`] at
///   the break position.
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
            // Replace the broken block with the replacement state instead of air.
            ctx.commands
                .send(SimCommand::PlaceBlock {
                    player,
                    position,
                    sequence,
                    state: BlockStateId::new(block_state_id),
                })
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
        }
        _ => {
            ctx.commands
                .send(SimCommand::Event(event))
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
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
/// The heal is best-effort under overload: the [`SimCommand`] send is awaited (so it
/// backpressures rather than drops), but the driver's onward route to the block's
/// owning shard can still fail if that shard's inbox is saturated, in which case the
/// rejection is dropped and the ghost persists until the client's next interaction —
/// the same behaviour the original `BlockBreak` / `BlockPlace` inputs exhibit under
/// the same sustained backpressure.
async fn reject_block_edit(
    ctx: &ConnContext,
    player: PlayerId,
    position: BlockPos,
    sequence: i32,
    requested_state: BlockStateId,
) -> anyhow::Result<()> {
    ctx.commands
        .send(SimCommand::RejectBlockEdit {
            player,
            position,
            sequence,
            requested_state,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))
}

/// Routes the [`WorldIntent`]s a plugin emitted from a block decision (or an
/// after-* notification).
///
/// Mapping (best-effort; the emitted-intent surface is dev-only and bounded):
/// - [`WorldIntent::SetBlock`] -> [`SimCommand::PlaceBlock`] by the acting player.
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
                ctx.commands
                    .send(SimCommand::PlaceBlock {
                        player: actor,
                        position: pos,
                        sequence,
                        state: BlockStateId::new(block_state_id),
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
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
                // The connection cannot reach another player's channel; the
                // driver-owned router snaps the target and routes the authoritative
                // move.
                ctx.commands
                    .send(SimCommand::TeleportPlayer { player, position })
                    .await
                    .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
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
/// On a successful `/spawn` the player is also teleported: a
/// `SynchronizePlayerPosition` is queued to their socket and a move is sent to the
/// simulation so the authoritative position updates and viewers see the teleport.
/// On a successful `/gamemode <id>` a `GameEvent` with reason `3`
/// (`change_game_mode`) carrying the mode id is queued so the client actually
/// switches mode.
#[allow(clippy::too_many_arguments)] // one command step: dispatch + feedback + side effects + I/O
async fn handle_command(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
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

    // The handler ran: show its feedback to the issuer (covers both a success and a
    // `CommandResult::failure`).
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        &ctx.clock,
        ferrumc_session::system_chat(result.feedback(), false),
    );
    if !result.is_success() {
        return Ok(());
    }

    let first_token = command.split_whitespace().next();
    if first_token == Some(SPAWN_COMMAND) {
        let spawn = policy.spawn();
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            &ctx.clock,
            ClientboundPlayPacket::SynchronizePlayerPosition(spawn_sync(JOIN_TELEPORT_ID, spawn)),
        );
        let move_event = NetEvent::play(
            player,
            ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(
                spawn.x, spawn.y, spawn.z, 0,
            )),
        );
        ctx.commands
            .send(SimCommand::Event(move_event))
            .await
            .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
    } else if first_token == Some(GAMEMODE_COMMAND) {
        // Make the mode change observable: a GameEvent (reason 3 = change_game_mode)
        // carrying the mode id switches the client's mode. The argument is parsed
        // the same way the handler validated it so the two always agree.
        if let Some(mode) = parse_gamemode(command) {
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
            // The GameEvent only switches the CLIENT. Also mutate the authoritative
            // server-side mode (in the sim's PlayerState) so future enforcement
            // (creative no-decrement, break speed, flight) reads the right mode; the
            // visual switch and the authoritative state must not diverge.
            ctx.commands
                .send(SimCommand::SetGameMode { player, mode })
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
            // Keep the connection-local mirror in lockstep: it is the synchronous
            // source the creative-slot gate reads, and the connection is its sole
            // writer, so it must update here too.
            inventory.set_game_mode(mode);
        }
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
/// - [`Replace`](ResolvedDecision::Replace): the replacement block-state is placed
///   instead of the held one.
/// - [`Allow`](ResolvedDecision::Allow): the held block is placed (creative never
///   decrements the stack).
///
/// On a non-denied placement any emitted intents are routed and the
/// `after_block_place` notification fires.
async fn handle_use_item_on(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    inventory: &PlayerInventory,
    packet: &UseItemOn,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let sequence = packet.sequence();
    // A malformed face index yields no target: just ack so the prediction ends.
    let Some(position) = use_item_on_target(packet) else {
        ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
        return Ok(());
    };

    // Empty hand or non-placeable item: nothing to place, just ack. The plugins are
    // not consulted for a no-op placement.
    let Some(held_state) = inventory.held().placeable_block() else {
        ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
        return Ok(());
    };

    let perms = PermissionFacade::new(ctx.policy.permissions());
    let (decision, emitted) = ctx.block_events.before_block_place(
        &BlockPlaceAttempt::new(player, position, held_state),
        &perms,
    );

    // The state actually placed (the held block, or a plugin's replacement).
    let placed_state = match decision {
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
            route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression)
                .await?;
            return Ok(());
        }
        ResolvedDecision::Replace { block_state_id } => block_state_id,
        _ => held_state,
    };

    // Creative: place the (possibly replaced) block; never touch the stack.
    ctx.commands
        .send(SimCommand::PlaceBlock {
            player,
            position,
            sequence,
            state: BlockStateId::new(placed_state),
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;

    route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression).await?;
    // Accepted at the intent boundary and routed: notify after_*.
    let after = ctx
        .block_events
        .after_block_place(player, position, placed_state, &perms);
    route_emitted_intents(ctx, player, writer, sequence, after, debug, compression).await
}

/// Handles a serverbound Set Creative Slot: validate the untrusted item bytes,
/// store the slot, and echo it.
///
/// Requires the player to be authoritatively creative (read from the connection's
/// drift-free game-mode mirror); a non-creative sender is ignored. The `slot` must
/// be in `0..=45` (a `-1` "drop outside" or any other out-of-range value is
/// ignored). The item bytes go through [`UntrustedItemStack::decode`] +
/// `into_validated` (clamping the count, stripping dangerous/unknown components); a
/// decode/validate error is logged and ignored, never fatal. On success the slot is
/// stored, the state id bumped, and a mandatory `SetContainerSlot` echoes the
/// authoritative slot back so the client view matches the server.
fn handle_set_creative_slot(
    ctx: &ConnContext,
    name: &str,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    packet: &SetCreativeSlot,
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
    // Untrusted bytes -> validated stack; never trust the client's item bytes.
    let mut reader = BoundedReader::new(packet.item());
    let stack = match UntrustedItemStack::decode(&mut reader)
        .and_then(UntrustedItemStack::into_validated)
    {
        Ok(stack) => stack,
        Err(err) => {
            tracing::debug!(player = name, %err, "ignoring malformed creative slot");
            return Ok(());
        }
    };
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
    )
}

/// Handles a serverbound Set Held Item: update the selected hotbar index.
///
/// The wire slot is an `i16`; values outside `0..=8` are ignored (no clientbound
/// reply is needed — the client already moved its own selector).
fn handle_set_held_item(inventory: &mut PlayerInventory, packet: &ServerboundSetHeldItem) {
    if let Ok(slot) = u8::try_from(packet.slot()) {
        inventory.set_selected(slot);
    }
}

/// Handles a serverbound Click Container on window 0 with a conservative resync.
///
/// The slice models no click logic, so any click on the player inventory — a
/// state-id mismatch or otherwise — is answered by bumping the state id and
/// re-sending the full authoritative container content (mandatory). Clicks on any
/// other window are ignored. Never disconnects, never trusts the click, never
/// panics.
fn handle_window_click(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    packet: &WindowClick,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    if packet.window_id() != WINDOW_ID {
        return Ok(());
    }
    // Bump first so the resync carries a fresh state id the client adopts.
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
    use super::tab_complete_reply;
    use crate::command::{build_command_tree, GAMEMODE_COMMAND, SPAWN_COMMAND};

    const OP_LEVEL: u8 = 4;
    const MEMBER_LEVEL: u8 = 0;

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
}
