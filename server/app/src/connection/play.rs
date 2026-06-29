//! The play phase: join the simulation, replay the join kit, then pump
//! serverbound packets and clientbound outputs until disconnect.

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, watch};
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};

use ferrumc_codec::BoundedString;
use ferrumc_core::{GameMode, PlayerId};
use ferrumc_math::Vec3;
use ferrumc_net::{
    CompressionState, ConnectionState, Criticality, DisconnectReason, InboundDecoder,
    InboundPacket, PlayWriter,
};
use ferrumc_observability::{PacketState, SessionDebug};
use ferrumc_proto::generated::play::{ClientboundKeepAlive, ClientboundPlayPacket};
use ferrumc_session::{NetEvent, PlayerSessionHandle};

use crate::driver::SimCommand;
use crate::inventory::PlayerInventory;
use crate::observe;
use crate::player_data::PlayerData;
use crate::window::WindowState;

use super::chunk_stream::{
    apply_chunk_stream, mirror_server_teleport, pump_chunk_stream, ChunkStream,
};
use super::context::ConnContext;
use super::handlers::handle_play_body;
use super::join::send_join_kit;
use super::outbound::{
    enqueue_traced, enqueue_traced_classified, flush_writer, is_mandatory_overflow,
    observe_queue_len,
};
use super::rate_limiter::ChatRateLimiter;
use super::serverbound_budget::ServerboundBudget;
use super::{Connection, READ_CHUNK};

/// Joins the simulation and replays the join kit, then pumps the play link until
/// the client disconnects or the server shuts down.
#[allow(clippy::too_many_lines)] // one cohesive lifecycle: join, replay, pump, dump
pub(super) async fn enter_play(
    conn: Connection<'_>,
    name: BoundedString<16>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let Connection {
        mut stream,
        mut decoder,
        compression,
        ctx,
        mut debug,
        ..
    } = conn;
    // Upgrade the session label from the peer address to the player name now that
    // login has completed.
    debug.set_session(name.as_str());
    let player = PlayerId::offline(name.as_str());

    // Restore this player's persisted state if they have a saved record; a fresh
    // player — or one whose record is corrupt, written under an incompatible schema,
    // or whose load failed — starts from the spawn defaults. A storage hiccup must
    // never block login, so any load error is logged and treated as a fresh join.
    let restored = match ctx.player_store.load_player(player).await {
        Ok(Some(record)) => {
            let game_mode = record.game_mode();
            PlayerData::from_record(&record).map(|data| (data, game_mode))
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(%err, player = name.as_str(), "failed to load player state; joining fresh");
            None
        }
    };
    let is_returning = restored.is_some();

    // Resolve the join state from the restored record or the spawn defaults. The
    // authoritative server-side inventory is built here, BEFORE the join, so the
    // joiner's (restored) held item is cached in the router as part of the join —
    // viewers entering view then see it on the spawn rather than only after the next
    // hotbar change. The inventory mirrors the (restored or default-creative) game
    // mode; the connection is the sole writer of both, so the mirror cannot drift.
    let (position, mut player_yaw, mut player_pitch, game_mode, mut inventory) = match restored {
        Some((data, game_mode)) => (
            data.position(),
            data.yaw(),
            data.pitch(),
            game_mode,
            data.restore_inventory(game_mode),
        ),
        None => (
            ctx.join_kit.spawn_position(),
            0.0_f32,
            0.0_f32,
            GameMode::Creative,
            PlayerInventory::with_creative_kit(GameMode::Creative),
        ),
    };

    // The opaque equipment body threaded into the join: the player's full visible
    // set (main hand, off hand, and armor), reflecting the restored slots. The kit
    // never fails to encode; on the off chance it does, fall back to empty (no
    // equipment shown) rather than aborting the join.
    let equipment = inventory.equipment_body().unwrap_or_else(|err| {
        tracing::warn!(%err, "failed to encode initial equipment; joining without it");
        Vec::new()
    });
    let mut handle = join_simulation(ctx, player, name.as_str(), position, equipment).await?;

    // The client holds the spawn batch after the join kit; the stream tracks it from
    // there so it never re-sends a spawn chunk and knows what to unload. Seed it with
    // the player's actual (restored) position so the first streaming pass centres on
    // where they rejoin: if they left far from spawn, their chunk is then streamed in
    // (centre-first) and the loading screen releases without waiting for a move.
    let mut chunk_stream = ChunkStream::new(ctx);
    chunk_stream.observe(position);

    // Per-connection chat rate limiter, seeded at the current server tick. Lives
    // here (the connection task may use a non-deterministic time source) and is
    // never touched by the sim/session deterministic tick path.
    let mut chat_limiter = ChatRateLimiter::new(ctx.clock.now());

    // Per-connection serverbound packet budget: a token bucket (~300 frames/sec
    // sustained, 600 burst by default) charged one token per serverbound play
    // frame. A sustained flood drains the bucket and the connection is dropped with
    // `BudgetExceeded`, so a hostile client cannot exhaust the server by spamming
    // play packets. Like the chat limiter, it lives in the connection task (allowed
    // a non-deterministic clock) and never touches the deterministic tick path.
    let mut budget = ServerboundBudget::new(
        std::time::Instant::now(),
        ctx.budget.sustained_rate,
        ctx.budget.burst,
    );

    // Make the sim's authoritative mode match the restored (or default-creative)
    // mode so the creative-slot gate accepts this player and later enforcement
    // (creative no-decrement, break speed, flight) reads the right mode.
    ctx.commands
        .send(SimCommand::SetGameMode {
            player,
            mode: game_mode,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;

    // Replay the keystone payload, restoring the look and — for a returning player —
    // the game mode and held slot, then drain any already-buffered play frames.
    let mut writer = PlayWriter::with_defaults(ctx.limits);
    send_join_kit(
        &mut writer,
        &mut stream,
        &compression,
        ctx,
        &mut debug,
        player,
        name.as_str(),
        position,
        player_yaw,
        player_pitch,
        is_returning,
        &inventory,
    )
    .await?;
    // Drain the frames already buffered before the loop. Any error here — a
    // serverbound budget kick on a flood pipelined during the login handoff, or a
    // decode/socket failure — is deferred to a break inside the loop rather than
    // returned via `?`, so this connection still runs the shared leave-save and
    // chunk-ticket release teardown below instead of leaking the join's tickets.
    let mut deferred_break: Option<anyhow::Error> = None;
    // Per-connection open-container window state (a chest screen, when open). The
    // always-open player inventory lives in `inventory`; this tracks the at-most-one
    // container window layered on top of it.
    let mut window_state = WindowState::new();
    if let Err(err) = pump_serverbound(
        &mut decoder,
        &compression,
        ctx,
        player,
        name.as_str(),
        &mut writer,
        &mut chunk_stream,
        &mut chat_limiter,
        &mut budget,
        &mut inventory,
        &mut window_state,
        &mut player_yaw,
        &mut player_pitch,
        &mut debug,
    )
    .await
    {
        deferred_break = Some(err);
    }
    if deferred_break.is_none() {
        match flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
            Ok(()) => observe_queue_len(&mut debug, ctx, &writer, budget.over_budget()),
            Err(err) => deferred_break = Some(err),
        }
    }

    // Keep Alive: a real client disconnects if it hears nothing for 20 s. Ping on
    // an interval; the client echoes with a serverbound Keep Alive the play pump
    // decodes and ignores. The first tick fires one interval in, not immediately.
    let mut keep_alive = interval_at(
        Instant::now() + ctx.keep_alive_interval,
        ctx.keep_alive_interval,
    );
    keep_alive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut keep_alive_id: i64 = 0;

    // Chunk-stream pump: advance a standing player's view toward the full
    // advertised view distance without waiting for a movement packet. The initial
    // fill already ran in the first `pump_serverbound` above; this drains the
    // remaining backlog one bounded batch per interval. `Delay` skips missed ticks
    // under load rather than bursting to catch up.
    let mut chunk_pump = interval_at(
        Instant::now() + ctx.chunk_stream_interval,
        ctx.chunk_stream_interval,
    );
    chunk_pump.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut read_buf = [0u8; READ_CHUNK];
    let result = loop {
        // A deferred failure from the pre-loop drain (e.g. a budget kick on a
        // flood pipelined during the login handoff) breaks here so the teardown
        // below still runs — the same save + ticket-release path a steady-state
        // disconnect takes.
        if let Some(err) = deferred_break.take() {
            break Err(err);
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => break Ok(()),
            _ = keep_alive.tick() => {
                keep_alive_id = keep_alive_id.wrapping_add(1);
                let packet = ClientboundPlayPacket::ClientboundKeepAlive(
                    ClientboundKeepAlive::new(keep_alive_id),
                );
                let criticality = Criticality::for_packet(&packet);
                let outcome =
                    enqueue_traced_classified(&mut writer, &mut debug, &compression, &ctx.clock, packet);
                if is_mandatory_overflow(criticality, outcome) {
                    // A full Critical queue means the client cannot drain even
                    // keep-alives: the Layer-B mirror of the router's mandatory
                    // slow-client policy (DisconnectReason::OutboundOverflow).
                    break Err(anyhow::anyhow!(
                        "outbound overflow: a mandatory keep-alive was dropped at the connection writer"
                    ));
                }
                if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                    break Err(err);
                }
                observe_queue_len(&mut debug, ctx, &writer, budget.over_budget());
            }
            _ = chunk_pump.tick() => {
                // Advance the view one bounded batch toward full view distance from
                // the current center, even if the player never moved. Bounded per
                // pump, so this paces the backlog out without flooding the socket.
                if let Err(err) =
                    pump_chunk_stream(ctx, &mut writer, &mut chunk_stream, &mut debug, &compression).await
                {
                    break Err(err);
                }
                if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                    break Err(err);
                }
                observe_queue_len(&mut debug, ctx, &writer, budget.over_budget());
            }
            outbound = handle.recv() => match outbound {
                // Clientbound simulation output: queue and flush to the socket.
                Some(msg) => {
                    // The envelope carries the criticality AND priority the router
                    // (Layer A) assigned at the send site, so Layer B honors that
                    // intent instead of re-inferring it from packet type — which is
                    // wrong for context-dependent packets (an actor-resync BlockUpdate
                    // is mandatory, a viewer-broadcast BlockUpdate is droppable). The
                    // router already disconnects a slow client rather than silently
                    // drop a mandatory packet; this mirrors that here so a full
                    // priority queue can never silently drop a mandatory frame
                    // (despawn/spawn/ack/correction/resync) either.
                    let criticality = msg.criticality();
                    let priority = msg.priority();
                    let packet = msg.into_packet();
                    // A server-driven absolute teleport (a router `Teleport` intent or
                    // an anti-cheat `PlayerPositionCorrected`) reaches this client
                    // through its outbound channel. Mirror it into the persistence
                    // state so a leave-save with no follow-up client move still saves
                    // where the server actually put the player, not the stale
                    // pre-teleport position/look the mirror last held.
                    if let ClientboundPlayPacket::SynchronizePlayerPosition(sync) = &packet {
                        mirror_server_teleport(
                            &mut chunk_stream,
                            &mut player_yaw,
                            &mut player_pitch,
                            sync,
                        );
                    }
                    let outcome =
                        enqueue_traced(&mut writer, &mut debug, &compression, &ctx.clock, priority, packet);
                    if is_mandatory_overflow(criticality, outcome) {
                        break Err(anyhow::anyhow!(
                            "outbound overflow: a mandatory clientbound packet was dropped at the connection writer"
                        ));
                    }
                    // One channel message is enqueued and flushed per loop turn.
                    // The router's atomic resync+ack group relies on this: the FIFO
                    // channel yields the (Mandatory, State) resync before the ack, so
                    // batching several messages before a flush must NOT be introduced
                    // here without re-establishing that ordering, or a dropped resync
                    // could leave the ack behind.
                    if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                        break Err(err);
                    }
                    observe_queue_len(&mut debug, ctx, &writer, budget.over_budget());
                }
                // The router dropped the session.
                None => break Ok(()),
            },
            read = timeout(ctx.io_timeout, stream.read(&mut read_buf)) => {
                let outcome = match read {
                    Err(_) => break Err(anyhow::anyhow!("play socket read timed out")),
                    Ok(Err(err)) => break Err(err.into()),
                    Ok(Ok(0)) => break Ok(()),
                    Ok(Ok(n)) => read_and_pump(
                        &mut decoder,
                        &compression,
                        ctx,
                        player,
                        name.as_str(),
                        &mut writer,
                        &mut stream,
                        &mut chunk_stream,
                        &mut chat_limiter,
                        &mut budget,
                        &mut inventory,
                        &mut window_state,
                        &mut player_yaw,
                        &mut player_pitch,
                        &mut debug,
                        &read_buf[..n],
                    ).await,
                };
                if let Err(err) = outcome {
                    break Err(err);
                }
            },
        }
    };

    // The play link has ended (clean close, EOF, shutdown, or error). Sample the
    // final outbound queue depth and dump the retained traces (acceptance:
    // disconnect).
    observe_queue_len(&mut debug, ctx, &writer, budget.over_budget());
    debug.dump("disconnect");

    // Persist this player's state BEFORE releasing their chunk tickets, mirroring
    // the chunk-edit flush-before-release discipline (`release_chunks_acked` in the
    // driver): the save is durable on the shared store the instant it returns (the
    // in-memory backend inserts inline; a redb backend commits), so a fast rejoin —
    // and the post-shutdown drain that waits on this connection's concurrency permit
    // — reads the just-saved state rather than a stale one. The saved position is the
    // last one the client reported, or the join position if it never moved. Failures
    // are logged, never fatal: a connection ending must always run its teardown.
    // If the player disconnects with a container open, return any carried (cursor)
    // item to the inventory before the leave-save so it is persisted, not lost. The
    // client cleared its own cursor on disconnect, so no clientbound sync is needed.
    if let Some(open) = window_state.take() {
        let leftover = inventory.deposit(open.cursor().clone());
        if leftover.item().is_some() {
            tracing::warn!(
                player = name.as_str(),
                count = leftover.count(),
                "inventory full on disconnect; carried items could not be returned"
            );
        }
    }

    let save_position = chunk_stream.last_position().unwrap_or(position);
    let player_data = PlayerData::capture(save_position, player_yaw, player_pitch, &inventory);
    match player_data.to_record(inventory.game_mode()) {
        Ok(record) => {
            if let Err(err) = ctx.player_store.save_player(player, record).await {
                tracing::warn!(%err, player = name.as_str(), "failed to save player state on leave");
            }
        }
        Err(err) => {
            tracing::warn!(%err, player = name.as_str(), "failed to encode player state on leave");
        }
    }

    // Release every chunk this connection had the client holding so its player
    // tickets stop pinning chunks resident after it leaves. Best-effort: a gone
    // driver just means the whole simulation is winding down anyway.
    if !chunk_stream.loaded.is_empty() {
        let positions = chunk_stream.loaded.iter().copied().collect();
        let _ = ctx
            .commands
            .send(SimCommand::ReleaseChunks { positions })
            .await;
    }

    // Best-effort despawn notice regardless of how the link ended.
    let _ = ctx
        .commands
        .send(SimCommand::Event(NetEvent::disconnected(
            player,
            DisconnectReason::ServerShutdown,
        )))
        .await;
    result
}

/// Sends a join request to the driver and awaits the session handle.
///
/// `equipment` is the joiner's pre-encoded main-hand `SetEquipment` body, cached
/// by the router at join so viewers entering view see the held item immediately.
async fn join_simulation(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    position: Vec3,
    equipment: Vec<u8>,
) -> anyhow::Result<PlayerSessionHandle> {
    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.commands
        .send(SimCommand::Join {
            player,
            name: name.to_owned(),
            position,
            equipment,
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver dropped the join reply"))?
        .map_err(|err| anyhow::anyhow!("join rejected: {err}"))
}

/// Pushes freshly read bytes through the decoder, handles every complete play
/// frame, then flushes any clientbound responses queued while handling them.
#[allow(clippy::too_many_arguments)] // one play step: framing + policy + I/O + trace state
async fn read_and_pump(
    decoder: &mut InboundDecoder,
    compression: &CompressionState,
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    chunk_stream: &mut ChunkStream,
    chat_limiter: &mut ChatRateLimiter,
    budget: &mut ServerboundBudget,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    player_yaw: &mut f32,
    player_pitch: &mut f32,
    debug: &mut SessionDebug,
    bytes: &[u8],
) -> anyhow::Result<()> {
    decoder.push(bytes)?;
    pump_serverbound(
        decoder,
        compression,
        ctx,
        player,
        name,
        writer,
        chunk_stream,
        chat_limiter,
        budget,
        inventory,
        window_state,
        player_yaw,
        player_pitch,
        debug,
    )
    .await?;
    flush_writer(writer, stream, compression, ctx.io_timeout).await
}

/// Drains every buffered serverbound play frame and handles each: a
/// `ChatCommand` runs through the command tree (queuing any clientbound response
/// into `writer`), a spawn-protected break/place is vetoed, and anything else is
/// forwarded to the simulation as a [`NetEvent`].
///
/// After the whole buffered batch is drained, a single chunk-streaming pass runs
/// against the latest position the batch reported, so many coalesced move packets
/// trigger at most one streaming evaluation per read.
#[allow(clippy::too_many_arguments)] // one play drain: framing + policy + trace state
async fn pump_serverbound(
    decoder: &mut InboundDecoder,
    compression: &CompressionState,
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    chat_limiter: &mut ChatRateLimiter,
    budget: &mut ServerboundBudget,
    inventory: &mut PlayerInventory,
    window_state: &mut WindowState,
    player_yaw: &mut f32,
    player_pitch: &mut f32,
    debug: &mut SessionDebug,
) -> anyhow::Result<()> {
    // One real-time reading for the whole drained batch: every frame buffered from
    // this read is charged against the budget at the same instant, so a single
    // flood read deterministically drains the bucket regardless of wall-clock
    // jitter. `std::time::Instant` (not the `tokio::time::Instant` alias in scope)
    // is what the token bucket expects.
    let now = std::time::Instant::now();
    loop {
        let next = match decoder.next_packet_compressed(ConnectionState::Play, compression) {
            Ok(next) => next,
            Err(err) => {
                // A frame/compression-level decode error during play: count it and
                // dump the retained traces before propagating (acceptance:
                // decode-error).
                ctx.metrics.record_packet_decode_error(
                    PacketState::Play,
                    observe::decode_error_label(&err),
                );
                debug.dump("play_decode_error");
                return Err(err.into());
            }
        };
        let Some(packet) = next else {
            break;
        };
        let InboundPacket::Play(body) = packet else {
            anyhow::bail!("non-play frame received in the play phase");
        };
        // Charge this serverbound frame against the per-connection packet budget
        // BEFORE handling it. Once the burst is drained and the sustained rate is
        // exceeded, the connection is dropped (`BudgetExceeded`) rather than left to
        // process an unbounded flood, so a hostile client cannot exhaust the server.
        if let Err(reason) = budget.admit(now) {
            tracing::warn!(
                player = name,
                ?reason,
                "serverbound packet budget exceeded; dropping connection"
            );
            debug.dump("play_budget_exceeded");
            return Err(anyhow::anyhow!(
                "serverbound packet budget exceeded ({reason:?})"
            ));
        }
        handle_play_body(
            ctx,
            player,
            name,
            writer,
            chunk_stream,
            chat_limiter,
            inventory,
            window_state,
            player_yaw,
            player_pitch,
            &body,
            debug,
            compression,
        )
        .await?;
    }
    apply_chunk_stream(ctx, writer, chunk_stream, debug, compression).await
}
