//! Shared, read-only connection context and the prebuilt status response.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use ferrumc_codec::BoundedString;
use ferrumc_config::{LoginDecision, PacketBudgetConfig, ResolvedAccess};
use ferrumc_core::PlayerId;
use ferrumc_net::{ConnectionLimits, OutboundPacket, StatusInfo};
use ferrumc_observability::{CounterRegistry, NetTelemetryHub, ServerClock};
use ferrumc_proto::generated::login::{ClientboundLoginPacket, LoginDisconnect};
use ferrumc_proto::generated::status::{ClientboundStatusPacket, StatusResponse};
use ferrumc_registry::PROTOCOL_VERSION;
use ferrumc_storage::PlayerStore;

use crate::driver::SimCommand;
use crate::plugins::{BlockEventDispatcher, PlayPolicy};
use crate::registries::ConfigRegistries;
use crate::world::JoinKit;

/// Human-readable version label shown in the client's multiplayer list.
const STATUS_VERSION_NAME: &str = "FerrumC 1.21.8";

/// MOTD text rendered as the status `description`.
const STATUS_MOTD: &str = "FerrumC";

/// Upper bound on the status-response JSON, matching the wire `StatusResponse`
/// string cap (chat-component max, 32767 chars).
const STATUS_JSON_MAX_CHARS: usize = 32_767;

/// Immutable context shared by every connection task.
///
/// Cloned cheaply (it is small and the [`JoinKit`] is behind an [`Arc`]) and
/// handed to each [`handle_connection`](super::handle_connection) call.
#[derive(Clone)]
pub(crate) struct ConnContext {
    /// Per-state hostile-input frame caps.
    pub(crate) limits: ConnectionLimits,
    /// Deadline applied to each socket read and write.
    pub(crate) io_timeout: Duration,
    /// Negotiated compression threshold, or `None` to leave compression off.
    pub(crate) compression_threshold: Option<i32>,
    /// The clientbound payload replayed when a client reaches play.
    pub(crate) join_kit: Arc<JoinKit>,
    /// The Known Packs advertisement and the registry packets sent during the
    /// configuration phase.
    pub(crate) config: Arc<ConfigRegistries>,
    /// Interval between clientbound play-phase Keep Alive pings.
    pub(crate) keep_alive_interval: Duration,
    /// How often a standing player's chunk view is pumped toward the full
    /// advertised view distance, independent of movement packets (see
    /// [`AppConfig::chunk_stream_interval`](crate::config::AppConfig::chunk_stream_interval)).
    pub(crate) chunk_stream_interval: Duration,
    /// Bounded channel to the simulation/session driver.
    pub(crate) commands: mpsc::Sender<SimCommand>,
    /// The per-player record store. A connection loads the joiner's saved state
    /// (position, look, inventory, held slot) on join and saves it on leave or
    /// shutdown. Shared (behind an [`Arc`]) with the same backend the world store
    /// wraps; player records are byte-oriented to storage (see
    /// [`crate::player_data`]).
    pub(crate) player_store: Arc<dyn PlayerStore>,
    /// The shared play policy: bypass permissions, the spawn position, and the
    /// command tree consulted for serverbound play packets.
    pub(crate) policy: Arc<PlayPolicy>,
    /// The shared block-event dispatcher: the long-lived plugin host the
    /// connection consults at the intent boundary for every block break/place.
    ///
    /// The plugins' `before_block_*` decision hooks run here (synchronously,
    /// panic-isolated, under a mutex with no lock held across an `.await`) — never
    /// inside the deterministic, plugin-free simulation tick.
    pub(crate) block_events: Arc<BlockEventDispatcher>,
    /// The prebuilt server-list status response, rendered once at startup and
    /// replayed for every status (`next_state == 1`) handshake. Behind an [`Arc`]
    /// so cloning the context per connection is a pointer bump, not a re-render.
    pub(crate) status_response: Arc<OutboundPacket>,
    /// The configured play view distance, in chunks: the square radius of chunks
    /// streamed around a player as they move (clamped to a sane maximum per
    /// connection, see [`STREAM_VIEW_DISTANCE_MAX`](super::chunk_stream::STREAM_VIEW_DISTANCE_MAX)).
    pub(crate) view_distance: i32,
    /// The shared metric registry every connection task feeds (chunk sends,
    /// decode errors, vetoed mutations, outbound queue depth).
    pub(crate) metrics: Arc<CounterRegistry>,
    /// The shared server clock (driver-written, connection-read) used to stamp
    /// packet traces with the current simulation tick.
    pub(crate) clock: ServerClock,
    /// The shared per-connection network-telemetry hub. Each play connection
    /// publishes its latest counters and packet-trace tallies here at its
    /// outbound queue-depth sample (off the per-packet hot path); the driver
    /// folds every session's snapshot into the per-tick `ServerSnapshot`.
    pub(crate) net_telemetry: Arc<NetTelemetryHub>,
    /// Resolved access control (per-IP limit, bans, whitelist), shared across the
    /// acceptor and every connection task. Consulted at login by
    /// [`login_denial`](Self::login_denial); the IP-level checks run in the accept
    /// loop before a task is spawned.
    pub(crate) access: Arc<ResolvedAccess>,
    /// The serverbound packet budget (token-bucket sustained rate and burst) each
    /// play connection builds its own [`PacketBudget`](ferrumc_net::PacketBudget)
    /// from. Plain `Copy` config, not shared mutable state: every connection owns a
    /// private bucket so one peer's flood never affects another's headroom.
    pub(crate) budget: PacketBudgetConfig,
}

impl ConnContext {
    /// The active compression threshold (`>= 0`), or `None` when disabled.
    pub(super) fn enabled_threshold(&self) -> Option<i32> {
        self.compression_threshold
            .filter(|threshold| *threshold >= 0)
    }

    /// Decides whether the player named `name` may complete login, returning the
    /// clientbound Login Disconnect packet to send when they may not.
    ///
    /// `Ok(None)` means the login is allowed; `Ok(Some(packet))` means it is
    /// rejected (banned or not whitelisted) and the caller should send `packet`
    /// and close. `player` is the canonical identity established by the login
    /// state machine, so UUID rules inspect the same UUID exposed to the client
    /// and carried into Play.
    ///
    /// # Errors
    ///
    /// Returns an error only if the (controlled, short) kick reason cannot be
    /// encoded into the wire string bound — effectively impossible in practice.
    pub(super) fn login_denial(
        &self,
        name: &str,
        player: PlayerId,
    ) -> anyhow::Result<Option<OutboundPacket>> {
        match self.access.login_decision(name, player.as_uuid()) {
            LoginDecision::Allow => Ok(None),
            LoginDecision::Deny(reason) => Ok(Some(login_disconnect(reason.message())?)),
        }
    }
}

/// Builds a clientbound Login Disconnect carrying `message` as a JSON text
/// component (the login-state disconnect wire format).
///
/// The JSON serializer escapes the reason for its final wire context even though
/// current callers supply short, controlled diagnostics.
pub(super) fn login_disconnect(message: &str) -> anyhow::Result<OutboundPacket> {
    // Serialize the component rather than interpolating JSON by hand. Today's
    // callers use controlled reasons, but this keeps every future diagnostic
    // safe if it ever contains quotes, controls, or other JSON metacharacters.
    let json = serde_json::json!({ "text": message }).to_string();
    let reason = BoundedString::<262_144>::new(json)
        .map_err(|err| anyhow::anyhow!("login disconnect reason exceeds the wire bound: {err}"))?;
    Ok(OutboundPacket::Login(
        ClientboundLoginPacket::LoginDisconnect(LoginDisconnect::new(reason)),
    ))
}

/// Builds the server-list status response advertised to clients that handshake
/// with `next_state == 1`.
///
/// The `{version, players, description}` JSON is rendered (and escaped) by
/// `ferrumc-net`'s [`StatusInfo`] so the shape matches the crate's own status
/// server: version `{"name": "FerrumC 1.21.8", "protocol": 772}` (772 marks the
/// server COMPATIBLE for a 1.21.8 client), `players` advertising `max_players`
/// with none online and an empty sample, and the MOTD as the description text.
///
/// Built once at startup and shared behind an [`Arc`]; each status request only
/// re-sends it.
///
/// # Errors
///
/// Returns an error if the rendered JSON exceeds the wire string bound — only
/// possible with an absurdly long MOTD or version label, neither of which the
/// server sets.
pub(crate) fn build_status_response(max_players: u32) -> anyhow::Result<OutboundPacket> {
    let info = StatusInfo::new(
        STATUS_VERSION_NAME,
        PROTOCOL_VERSION,
        max_players,
        0,
        STATUS_MOTD,
    );
    let json = BoundedString::<STATUS_JSON_MAX_CHARS>::new(info.to_json())
        .map_err(|err| anyhow::anyhow!("status response JSON exceeds the wire bound: {err}"))?;
    Ok(OutboundPacket::Status(
        ClientboundStatusPacket::StatusResponse(StatusResponse::new(json)),
    ))
}
