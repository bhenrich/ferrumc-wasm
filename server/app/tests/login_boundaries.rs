//! End-to-end protocol-version and username admission-boundary regressions.
//!
//! Every case uses a real socket. Incompatible handshakes are half-closed after
//! the handshake alone, proving the server rejects them before `LoginStart`.
//! Invalid names are deliberately admitted by the configured whitelist and
//! followed, on the old behavior, by a chat carrier. The fixed boundary must
//! reject them before identity, access, Play, persistence, metrics, chat, or
//! plugin state can observe them.

mod common;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::timeout;
use uuid::Uuid;

use ferrumc_app::AppConfig;
use ferrumc_codec::{write_var_int, BoundedReader, BoundedString};
use ferrumc_config::AccessConfig;
use ferrumc_core::PlayerId;
use ferrumc_nbt::NbtTag;
use ferrumc_observability::{PluginDecisionSnapshot, SnapshotPublisher};
use ferrumc_proto::generated::configuration::{
    AckFinishConfiguration, ClientboundConfigurationPacket, KnownPack, ServerboundKnownPacks,
};
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::login::{ClientboundLoginPacket, LoginAcknowledged};
use ferrumc_proto::generated::play::{ChatMessage, ClientboundPlayPacket};
use ferrumc_proto::generated::status::{ClientboundStatusPacket, StatusRequest};
use ferrumc_storage::{PlayerStore, RedbStore};

use common::{encode, login_to_play, TestClient};

/// Protocol version for Minecraft Java 1.21.8.
const PROTOCOL_VERSION: i32 = 772;
/// Handshake intent selecting Login.
const NEXT_STATE_LOGIN: i32 = 2;
/// Handshake intent selecting the server-list Status flow.
const NEXT_STATE_STATUS: i32 = 1;
/// Diagnostic guard for each complete socket flow.
const GUARD: Duration = Duration::from_secs(20);
/// Controlled rejection required for every structurally decodable bad name.
const INVALID_USERNAME_REASON: &str =
    "Invalid username: use 1-16 ASCII letters, digits, or underscores.";
/// Message sent only if an invalid name incorrectly reaches Play.
const LEAK_CARRIER: &str = "invalid-boundary-carrier";
/// Valid observer message used as a FIFO-visible chat fence.
const OBSERVER_FENCE: &str = "invalid-boundary-fence";
/// Valid identity held in Play while invalid admissions are attempted.
const OBSERVER: &str = "BoundaryObserver";

/// Invalid name and the wire-level rejection expected for it.
struct InvalidCase {
    /// Diagnostic label that itself contains no hostile terminal characters.
    label: &'static str,
    /// Exact hostile Login Start name.
    name: &'static str,
    /// Whether the generated structural string bound can decode the name.
    semantic_rejection: bool,
}

/// Outcome of attempting one Login Start.
enum LoginOutcome {
    /// A Login Disconnect was received, carrying its decoded plain-text reason.
    Rejected(String),
    /// The structurally overlong Login Start closed without a modeled response.
    Closed,
    /// The hostile identity incorrectly reached Play; retained to keep its
    /// roster and telemetry state observable until the assertions complete.
    Accepted(TestClient),
}

/// Creates an isolated world directory under the repository-owned scratch root.
fn temp_world() -> tempfile::TempDir {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".codex-tmp");
    std::fs::create_dir_all(&scratch).expect("create repository scratch directory");
    tempfile::Builder::new()
        .prefix("packet28-login-boundaries-")
        .tempdir_in(scratch)
        .expect("create test-local world directory")
}

/// Runs an async operation under the suite-wide diagnostic guard.
async fn guarded<T>(
    label: &str,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    timeout(GUARD, future)
        .await
        .map_err(|_| anyhow::anyhow!("{label} exceeded the timeout guard"))?
}

/// Encodes a handshake with a caller-selected protocol version and intent.
fn handshake(addr: std::net::SocketAddr, protocol: i32, intent: i32) -> anyhow::Result<Vec<u8>> {
    let address = BoundedString::<255>::new("127.0.0.1".to_string())?;
    Ok(encode(|buf| {
        Handshake::new(protocol, address.clone(), addr.port(), intent).encode(buf)
    }))
}

/// Encodes Login Start without constructing `BoundedString<16>`, allowing the
/// 17-character structural-boundary case to reach the real decoder.
fn raw_login_start(name: &str) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    write_var_int(&mut body, 0);
    write_var_int(&mut body, i32::try_from(name.len())?);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(Uuid::nil().as_bytes());
    Ok(body)
}

/// Decodes a Login Disconnect JSON component into its controlled text.
fn disconnect_text(reason: &BoundedString<262_144>) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_str(reason.as_str())?;
    value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("disconnect reason had no string `text` field"))
}

/// Sends a plain unsigned chat message.
async fn send_chat(client: &mut TestClient, message: &str) -> anyhow::Result<()> {
    let message = BoundedString::<256>::new(message.to_string())?;
    client
        .send_frame(&encode(|buf| {
            ChatMessage::new(message.clone(), Vec::new()).encode(buf)
        }))
        .await
}

/// Extracts the plain text of a network-NBT text component.
fn component_text(content: &NbtTag) -> anyhow::Result<&str> {
    let NbtTag::Compound(root) = content else {
        anyhow::bail!("system-chat content was not a compound");
    };
    match root.get("text") {
        Some(NbtTag::String(text)) => Ok(text),
        _ => anyhow::bail!("system-chat component had no string `text` field"),
    }
}

/// Reads the clientbound half of an already-pipelined configuration exchange and
/// waits until the carrier reaches Play.
async fn await_pipelined_play(client: &mut TestClient) -> anyhow::Result<()> {
    loop {
        let frame = client.next_frame().await?;
        let mut reader = BoundedReader::new(&frame);
        let id = reader.read_var_int()?;
        match ClientboundConfigurationPacket::decode(id, &mut reader)? {
            ClientboundConfigurationPacket::FinishConfiguration(_) => break,
            ClientboundConfigurationPacket::ClientboundKnownPacks(_)
            | ClientboundConfigurationPacket::RegistryData(_) => {}
        }
    }
    loop {
        if matches!(
            client.next_play().await?,
            ClientboundPlayPacket::JoinGame(_)
        ) {
            return Ok(());
        }
    }
}

/// Attempts one raw username and, if the old implementation admits it, drives a
/// chat carrier through both the plugin and broadcast paths before returning.
async fn attempt_username(addr: std::net::SocketAddr, name: &str) -> anyhow::Result<LoginOutcome> {
    let mut client = TestClient::connect(addr).await?;
    let hello = handshake(addr, PROTOCOL_VERSION, NEXT_STATE_LOGIN)?;
    let start = raw_login_start(name)?;
    let login_ack = encode(|buf| LoginAcknowledged.encode(buf));
    let core_pack = KnownPack::new(
        BoundedString::<32_767>::new("minecraft".to_string())?,
        BoundedString::<32_767>::new("core".to_string())?,
        BoundedString::<32_767>::new("1.21.8".to_string())?,
    );
    let known_packs = encode(|buf| ServerboundKnownPacks::new(vec![core_pack.clone()]).encode(buf));
    let finish_ack = encode(|buf| AckFinishConfiguration.encode(buf));
    let carrier_message = BoundedString::<256>::new(LEAK_CARRIER.to_string())?;
    let carrier = encode(|buf| ChatMessage::new(carrier_message.clone(), Vec::new()).encode(buf));
    client
        .send_frames(&[
            &hello,
            &start,
            &login_ack,
            &known_packs,
            &finish_ack,
            &carrier,
        ])
        .await?;

    let Ok(frame) = client.next_frame().await else {
        return Ok(LoginOutcome::Closed);
    };
    let mut reader = BoundedReader::new(&frame);
    let id = reader.read_var_int()?;
    match ClientboundLoginPacket::decode(id, &mut reader)? {
        ClientboundLoginPacket::LoginDisconnect(disconnect) => Ok(LoginOutcome::Rejected(
            disconnect_text(disconnect.reason())?,
        )),
        ClientboundLoginPacket::LoginSuccess(_) => {
            await_pipelined_play(&mut client).await?;
            loop {
                if let ClientboundPlayPacket::SystemChat(chat) = client.next_play().await? {
                    if component_text(chat.content())?.contains(LEAK_CARRIER) {
                        return Ok(LoginOutcome::Accepted(client));
                    }
                }
            }
        }
        ClientboundLoginPacket::SetCompression(_) => {
            anyhow::bail!("test config unexpectedly enabled compression")
        }
    }
}

/// Waits for a snapshot strictly later than `tick`, without a wall-clock sleep.
async fn snapshot_after(
    snapshots: &SnapshotPublisher,
    tick: u64,
) -> anyhow::Result<std::sync::Arc<ferrumc_observability::ServerSnapshot>> {
    loop {
        let snapshot = snapshots.latest();
        if snapshot.tick > tick {
            return Ok(snapshot);
        }
        tokio::task::yield_now().await;
    }
}

/// Reduces plugin decision rows to comparable inert tuples.
fn plugin_counts(rows: &[PluginDecisionSnapshot]) -> Vec<(String, u64, u64, u64, u64)> {
    rows.iter()
        .map(|row| {
            (
                row.plugin_name.clone(),
                row.decisions.allow,
                row.decisions.deny,
                row.decisions.replace,
                row.decisions.panic,
            )
        })
        .collect()
}

/// Returns the app-selected redb path for a persistent world directory.
fn database_path(world_dir: &Path) -> PathBuf {
    world_dir.join("world.redb")
}

#[tokio::test(flavor = "multi_thread")]
async fn login_protocol_table_rejects_mismatch_before_login_start() {
    let mut config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    config.access.per_ip_connection_limit = 32;
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    // Version discovery remains available to incompatible clients. Every status
    // response advertises the authoritative 772 even when the query does not.
    for protocol in [771, 772, 773] {
        let mut client = TestClient::connect(addr)
            .await
            .expect("status client connects");
        client
            .send_frame(&handshake(addr, protocol, NEXT_STATE_STATUS).expect("handshake encodes"))
            .await
            .expect("status handshake writes");
        client
            .send_frame(&encode(|buf| StatusRequest.encode(buf)))
            .await
            .expect("status request writes");
        let frame = guarded("cross-version status response", client.next_frame())
            .await
            .expect("status response arrives");
        let mut reader = BoundedReader::new(&frame);
        let id = reader.read_var_int().expect("status packet id");
        let ClientboundStatusPacket::StatusResponse(response) =
            ClientboundStatusPacket::decode(id, &mut reader).expect("status packet decodes")
        else {
            panic!("protocol {protocol} status query received a non-status response");
        };
        assert!(
            response.json().as_str().contains("\"protocol\":772"),
            "protocol {protocol} query must discover the authoritative protocol",
        );
    }

    for protocol in [771, 773] {
        let mut client = TestClient::connect(addr)
            .await
            .expect("incompatible client connects");
        client
            .send_frame(&handshake(addr, protocol, NEXT_STATE_LOGIN).expect("handshake encodes"))
            .await
            .expect("handshake writes");
        client
            .finish_writes()
            .await
            .expect("client half-closes after handshake");

        let frame = guarded("protocol rejection", client.next_frame())
            .await
            .expect("incompatible handshake receives a frame");
        let mut reader = BoundedReader::new(&frame);
        let id = reader.read_var_int().expect("login packet id");
        let ClientboundLoginPacket::LoginDisconnect(disconnect) =
            ClientboundLoginPacket::decode(id, &mut reader).expect("login packet decodes")
        else {
            panic!("protocol {protocol} received a non-disconnect login packet");
        };
        let reason = disconnect_text(disconnect.reason()).expect("disconnect reason decodes");
        assert!(
            reason.contains("772") && reason.contains("1.21.8"),
            "protocol {protocol} needs a clear compatibility reason, got {reason:?}",
        );
    }

    let compatible = guarded("protocol 772 login", login_to_play(addr, "Protocol772"))
        .await
        .expect("protocol 772 reaches Play");
    drop(compatible);

    guarded("protocol-table shutdown", server.shutdown())
        .await
        .expect("server shuts down cleanly");
}

#[tokio::test(flavor = "multi_thread")]
async fn valid_username_boundary_table_reaches_play() {
    let mut config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    config.access.per_ip_connection_limit = 32;
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    for name in ["A", "_", "Alpha_123", "abcdefghijklmnop"] {
        let client = guarded("valid username login", login_to_play(addr, name))
            .await
            .unwrap_or_else(|error| panic!("valid username {name:?} was rejected: {error:#}"));
        drop(client);
    }

    guarded("valid-name shutdown", server.shutdown())
        .await
        .expect("server shuts down cleanly");
}

#[test]
#[allow(clippy::too_many_lines)] // one adversarial lifecycle across every downstream sink
fn invalid_username_table_has_no_identity_or_downstream_side_effects() {
    let world = temp_world();
    let invalid = [
        InvalidCase {
            label: "empty",
            name: "",
            semantic_rejection: true,
        },
        InvalidCase {
            label: "unicode",
            name: "玩家",
            semantic_rejection: true,
        },
        InvalidCase {
            label: "whitespace",
            name: "Bad Name",
            semantic_rejection: true,
        },
        InvalidCase {
            label: "control",
            name: "Bad\nName",
            semantic_rejection: true,
        },
        InvalidCase {
            label: "escape",
            name: "Bad\u{1b}Name",
            semantic_rejection: true,
        },
        InvalidCase {
            label: "angle",
            name: "<Admin>",
            semantic_rejection: true,
        },
        InvalidCase {
            label: "section",
            name: "\u{a7}cAdmin",
            semantic_rejection: true,
        },
        InvalidCase {
            label: "punctuation",
            name: "Bad-Name",
            semantic_rejection: true,
        },
        InvalidCase {
            label: "overlong",
            name: "abcdefghijklmnopq",
            semantic_rejection: false,
        },
    ];

    let mut config = AppConfig {
        bind: "127.0.0.1:0".parse().expect("loopback address"),
        spawn_chunk_radius: 1,
        world_dir: Some(world.path().to_path_buf()),
        access: AccessConfig {
            per_ip_connection_limit: 32,
            whitelist_enabled: true,
            whitelist: std::iter::once(OBSERVER.to_string())
                .chain(invalid.iter().map(|case| case.name.to_string()))
                .collect(),
            ..AccessConfig::default()
        },
        ..AppConfig::default()
    };
    // A UUID-only ban for this invalid name proves grammar rejection precedes
    // both UUID derivation and ACL policy: the visible reason must be the static
    // grammar reason, never the ban reason.
    config
        .access
        .bans
        .push(PlayerId::offline("Bad Name").as_uuid().to_string());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    runtime.block_on(async {
        let server = ferrumc_app::run(&config).await.expect("server starts");
        let addr = server.local_addr();
        let snapshots = server.snapshot_handle();
        let mut observer = guarded("observer login", login_to_play(addr, OBSERVER))
            .await
            .expect("observer reaches Play");
        let baseline = guarded(
            "baseline snapshot",
            snapshot_after(&snapshots, snapshots.latest().tick),
        )
        .await
        .expect("baseline snapshot advances");
        let baseline_plugins = plugin_counts(&baseline.plugin_decisions);

        let mut accepted = Vec::new();
        let mut violations = Vec::new();
        for case in &invalid {
            let outcome = guarded(case.label, attempt_username(addr, case.name))
                .await
                .unwrap_or_else(|error| panic!("{} username flow failed: {error:#}", case.label));
            match (case.semantic_rejection, outcome) {
                (true, LoginOutcome::Rejected(reason)) if reason == INVALID_USERNAME_REASON => {}
                (true, LoginOutcome::Rejected(reason)) => violations.push(format!(
                    "{} got the wrong rejection reason: {reason:?}",
                    case.label
                )),
                (true, LoginOutcome::Closed) => violations.push(format!(
                    "{} closed without the controlled username rejection",
                    case.label
                )),
                (false, LoginOutcome::Closed | LoginOutcome::Rejected(_)) => {}
                (_, LoginOutcome::Accepted(client)) => {
                    violations.push(format!("{} reached Play", case.label));
                    accepted.push(client);
                }
            }
        }

        let after_attempt_tick = snapshots.latest().tick;
        let after = guarded(
            "post-rejection snapshot",
            snapshot_after(&snapshots, after_attempt_tick),
        )
        .await
        .expect("post-rejection snapshot advances");
        let invalid_names: Vec<&str> = invalid.iter().map(|case| case.name).collect();
        for player in &after.players {
            if invalid_names.contains(&player.name.as_str()) {
                violations.push(format!(
                    "invalid identity reached the player snapshot: {:?}",
                    player.name
                ));
            }
        }
        for row in &after.network_per_player {
            if invalid_names.contains(&row.player_name.as_str()) {
                violations.push(format!(
                    "invalid identity reached network metrics: {:?}",
                    row.player_name
                ));
            }
        }
        if plugin_counts(&after.plugin_decisions) != baseline_plugins {
            violations.push("invalid identity reached plugin callbacks".to_string());
        }

        // Every erroneously accepted carrier has already reached its own socket,
        // which means it was enqueued to the observer before this later fence.
        // Take the plugin comparison above: this valid observer chat intentionally
        // fires the chat callback and must not contaminate the rejection oracle.
        guarded("observer chat fence", async {
            send_chat(&mut observer, OBSERVER_FENCE).await?;
            loop {
                if let ClientboundPlayPacket::SystemChat(chat) = observer.next_play().await? {
                    let text = component_text(chat.content())?;
                    if text.contains(LEAK_CARRIER) {
                        violations.push(format!("invalid identity reached chat: {text:?}"));
                    }
                    if text.contains(&format!("<{OBSERVER}> {OBSERVER_FENCE}")) {
                        return Ok(());
                    }
                }
            }
        })
        .await
        .expect("observer reaches the chat fence");

        drop(accepted);
        drop(observer);
        guarded("invalid-name shutdown", server.shutdown())
            .await
            .expect("server shuts down cleanly");

        assert!(
            violations.is_empty(),
            "invalid username boundary leaked:\n{}",
            violations.join("\n")
        );
    });
    drop(runtime);

    let store = RedbStore::open(database_path(world.path())).expect("reopen durable world store");
    assert!(
        runtime_free_load(&store, PlayerId::offline(OBSERVER)).is_some(),
        "valid observer must persist so the negative persistence oracle is live",
    );
    for case in &invalid {
        let player = PlayerId::offline(case.name);
        assert!(
            runtime_free_load(&store, player).is_none(),
            "{} invalid identity reached persistence",
            case.label,
        );
    }
}

/// Loads one player record without retaining an async runtime around redb.
fn runtime_free_load(store: &RedbStore, player: PlayerId) -> Option<ferrumc_storage::PlayerRecord> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build store runtime");
    runtime
        .block_on(store.load_player(player))
        .expect("load player record")
}
