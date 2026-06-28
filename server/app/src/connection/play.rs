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

use super::chunk_stream::{apply_chunk_stream, pump_chunk_stream, ChunkStream};
use super::context::ConnContext;
use super::handlers::handle_play_body;
use super::join::send_join_kit;
use super::outbound::{
    enqueue_traced, enqueue_traced_classified, flush_writer, is_mandatory_overflow,
    observe_queue_len,
};
use super::rate_limiter::ChatRateLimiter;
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
    let position = ctx.join_kit.spawn_position();
    let mut handle = join_simulation(ctx, player, name.as_str(), position).await?;

    // The client already holds the spawn batch after the join kit; stream tracks
    // it from there so it never re-sends a spawn chunk and knows what to unload.
    let mut chunk_stream = ChunkStream::new(ctx);

    // Per-connection chat rate limiter, seeded at the current server tick. Lives
    // here (the connection task may use a non-deterministic time source) and is
    // never touched by the sim/session deterministic tick path.
    let mut chat_limiter = ChatRateLimiter::new(ctx.clock.now());

    // The authoritative server-side inventory for this connection. Seeded with the
    // creative starter kit and a creative game-mode mirror; the connection is the
    // sole writer of both, so the mirror cannot drift from the sim's mode. The
    // matching `SetGameMode` below makes the sim's authoritative mode agree.
    let mut inventory = PlayerInventory::with_creative_kit(GameMode::Creative);

    // The shard seeds every joiner's mode to the default (survival), but JoinGame
    // told the client creative — make the sim's authoritative mode creative too so
    // the creative-slot gate accepts this player and later enforcement is correct.
    ctx.commands
        .send(SimCommand::SetGameMode {
            player,
            mode: GameMode::Creative,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;

    // Replay the keystone payload, then drain any already-buffered play frames.
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
        &inventory,
    )
    .await?;
    pump_serverbound(
        &mut decoder,
        &compression,
        ctx,
        player,
        name.as_str(),
        &mut writer,
        &mut chunk_stream,
        &mut chat_limiter,
        &mut inventory,
        &mut debug,
    )
    .await?;
    flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await?;
    observe_queue_len(&mut debug, ctx, &writer);

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
                observe_queue_len(&mut debug, ctx, &writer);
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
                observe_queue_len(&mut debug, ctx, &writer);
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
                    observe_queue_len(&mut debug, ctx, &writer);
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
                        &mut inventory,
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
    observe_queue_len(&mut debug, ctx, &writer);
    debug.dump("disconnect");

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
async fn join_simulation(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    position: Vec3,
) -> anyhow::Result<PlayerSessionHandle> {
    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.commands
        .send(SimCommand::Join {
            player,
            name: name.to_owned(),
            position,
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
    inventory: &mut PlayerInventory,
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
        inventory,
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
    inventory: &mut PlayerInventory,
    debug: &mut SessionDebug,
) -> anyhow::Result<()> {
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
        handle_play_body(
            ctx,
            player,
            name,
            writer,
            chunk_stream,
            chat_limiter,
            inventory,
            &body,
            debug,
            compression,
        )
        .await?;
    }
    apply_chunk_stream(ctx, writer, chunk_stream, debug, compression).await
}
