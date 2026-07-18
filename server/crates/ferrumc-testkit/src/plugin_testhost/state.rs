//! Normalized semantic effects, final state, and canonical run digests.

use core::fmt;

use ferrumc_plugin_sdk::{
    BlockDecision, BlockPos, CommandDefinition, CommandNodeKind, DiagnosticLevel, EventDecision,
    EventKind, MessageOperation, PermissionNode, PlayerId, Resolution, TeleportOperation, Tick,
    TimerId, Vec3,
};
use sha2::{Digest, Sha256};

/// One normalized, committed plugin effect.
///
/// Values contain no package path, target triple, raw ABI envelope, or resource
/// handle, so built-in and trusted native plugin runs can be compared directly.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PluginEffect {
    /// Subscribe to a notification event.
    SubscribeEvent(EventKind),
    /// Register one pure-data command tree.
    RegisterCommand(CommandDefinition),
    /// Set one block state.
    SetBlock {
        /// Typed block position.
        pos: BlockPos,
        /// Opaque registry block-state identifier.
        block_state_id: u32,
    },
    /// Teleport one player.
    Teleport(TeleportOperation),
    /// Send one plain-text player message.
    Message(MessageOperation),
    /// Store one namespaced value.
    StoragePut {
        /// Host-selected-namespace key.
        key: String,
        /// Stored bytes.
        value: Vec<u8>,
    },
    /// Delete one namespaced value.
    StorageDelete {
        /// Host-selected-namespace key.
        key: String,
    },
    /// Schedule or replace one deterministic timer.
    ScheduleTimer {
        /// Stable timer identifier.
        id: TimerId,
        /// Absolute due tick derived from the callback tick and delay.
        due_tick: Tick,
    },
    /// Cancel one deterministic timer.
    CancelTimer {
        /// Stable timer identifier.
        id: TimerId,
    },
    /// Commit the outcome of a block-placement decision callback.
    BlockDecision(BlockDecision),
    /// Commit the outcome of a break, chat, or interaction decision callback.
    EventDecision {
        /// Attempt kind whose decision was recorded.
        kind: EventKind,
        /// Allow-or-deny result.
        decision: EventDecision,
    },
}

/// Callback phase associated with one retained diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PluginDiagnosticPhase {
    /// Plugin initialization.
    Load,
    /// One event callback at its deterministic tick.
    Event(Tick),
    /// Plugin shutdown.
    Unload,
}

/// One bounded plugin-authored diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginDiagnostic {
    phase: PluginDiagnosticPhase,
    level: DiagnosticLevel,
    message: String,
}

impl PluginDiagnostic {
    pub(crate) fn new(
        phase: PluginDiagnosticPhase,
        level: DiagnosticLevel,
        message: String,
    ) -> Self {
        Self {
            phase,
            level,
            message,
        }
    }

    /// Returns the callback phase that emitted the diagnostic.
    pub const fn phase(&self) -> PluginDiagnosticPhase {
        self.phase
    }

    /// Returns its SDK severity.
    pub const fn level(&self) -> DiagnosticLevel {
        self.level
    }

    /// Returns its plain-text message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One permission result seeded into a testhost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionSetting {
    player: PlayerId,
    node: PermissionNode,
    resolution: Resolution,
}

impl PermissionSetting {
    pub(crate) fn new(player: PlayerId, node: PermissionNode, resolution: Resolution) -> Self {
        Self {
            player,
            node,
            resolution,
        }
    }

    /// Returns the player being resolved.
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns the validated permission node.
    pub const fn node(&self) -> &PermissionNode {
        &self.node
    }

    /// Returns the configured resolution.
    pub const fn resolution(&self) -> Resolution {
        self.resolution
    }
}

/// One key/value entry in the final namespaced storage snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageEntry {
    key: String,
    value: Vec<u8>,
}

impl StorageEntry {
    pub(crate) fn new(key: String, value: Vec<u8>) -> Self {
        Self { key, value }
    }

    /// Returns the key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the stored bytes.
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// One timer remaining in the final state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledTimer {
    id: TimerId,
    due_tick: Tick,
}

impl ScheduledTimer {
    pub(crate) const fn new(id: TimerId, due_tick: Tick) -> Self {
        Self { id, due_tick }
    }

    /// Returns the timer identifier.
    pub const fn id(self) -> TimerId {
        self.id
    }

    /// Returns its absolute due tick.
    pub const fn due_tick(self) -> Tick {
        self.due_tick
    }
}

/// Packaging-neutral final state after a replay.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginStateSnapshot {
    pub(crate) loaded_chunks: Vec<ferrumc_plugin_sdk::ChunkPos>,
    pub(crate) blocks: Vec<(BlockPos, u32)>,
    pub(crate) player_positions: Vec<(PlayerId, Vec3)>,
    pub(crate) permissions: Vec<PermissionSetting>,
    pub(crate) storage: Vec<StorageEntry>,
    pub(crate) timers: Vec<ScheduledTimer>,
    pub(crate) subscriptions: Vec<EventKind>,
    pub(crate) commands: Vec<CommandDefinition>,
    pub(crate) messages: Vec<MessageOperation>,
}

impl PluginStateSnapshot {
    /// Returns whether a chunk is loaded.
    pub fn is_chunk_loaded(&self, chunk: ferrumc_plugin_sdk::ChunkPos) -> bool {
        self.loaded_chunks.binary_search(&chunk).is_ok()
    }

    /// Returns a final block state, or `None` when absent.
    pub fn block_state_id(&self, pos: BlockPos) -> Option<u32> {
        self.blocks
            .binary_search_by_key(&pos, |(candidate, _)| *candidate)
            .ok()
            .map(|index| self.blocks[index].1)
    }

    /// Returns a final player position, or `None` when absent.
    pub fn player_position(&self, player: PlayerId) -> Option<Vec3> {
        self.player_positions
            .binary_search_by_key(&player, |(candidate, _)| *candidate)
            .ok()
            .map(|index| self.player_positions[index].1)
    }

    /// Returns the configured permission resolution, defaulting to unset.
    pub fn permission(&self, player: PlayerId, node: &PermissionNode) -> Resolution {
        self.permissions
            .iter()
            .find(|setting| setting.player == player && setting.node == *node)
            .map_or(Resolution::Unset, PermissionSetting::resolution)
    }

    /// Returns one final storage value.
    pub fn storage_value(&self, key: &str) -> Option<&[u8]> {
        self.storage
            .binary_search_by(|entry| entry.key.as_str().cmp(key))
            .ok()
            .map(|index| self.storage[index].value.as_slice())
    }

    /// Returns remaining timers in identifier order.
    pub fn timers(&self) -> &[ScheduledTimer] {
        &self.timers
    }

    /// Returns subscribed event kinds in stable discriminant order.
    pub fn subscriptions(&self) -> &[EventKind] {
        &self.subscriptions
    }

    /// Returns registered command trees in commit order.
    pub fn commands(&self) -> &[CommandDefinition] {
        &self.commands
    }

    /// Returns emitted player messages in commit order.
    pub fn messages(&self) -> &[MessageOperation] {
        &self.messages
    }

    /// Returns final namespaced storage entries in bytewise key order.
    pub fn storage(&self) -> &[StorageEntry] {
        &self.storage
    }
}

/// A canonical SHA-256 digest of semantic effects and final state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SemanticDigest([u8; 32]);

impl SemanticDigest {
    /// Returns the digest bytes.
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }

    /// Returns lowercase hexadecimal.
    pub fn as_hex(self) -> String {
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ignored = write!(output, "{byte:02x}");
        }
        output
    }
}

impl fmt::Display for SemanticDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_hex())
    }
}

/// Completed or partial deterministic plugin replay report.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginRun {
    effects: Vec<PluginEffect>,
    diagnostics: Vec<PluginDiagnostic>,
    state: PluginStateSnapshot,
    digest: SemanticDigest,
}

impl PluginRun {
    pub(crate) fn new(
        effects: Vec<PluginEffect>,
        diagnostics: Vec<PluginDiagnostic>,
        state: PluginStateSnapshot,
    ) -> Result<Self, &'static str> {
        let digest = semantic_digest(&effects, &state)?;
        Ok(Self {
            effects,
            diagnostics,
            state,
            digest,
        })
    }

    /// Returns committed normalized effects in replay order.
    pub fn effects(&self) -> &[PluginEffect] {
        &self.effects
    }

    /// Returns retained diagnostics, including those from rolled-back callbacks.
    pub fn diagnostics(&self) -> &[PluginDiagnostic] {
        &self.diagnostics
    }

    /// Returns final committed semantic state.
    pub const fn state(&self) -> &PluginStateSnapshot {
        &self.state
    }

    /// Returns the canonical semantic digest.
    pub const fn digest(&self) -> SemanticDigest {
        self.digest
    }
}

fn semantic_digest(
    effects: &[PluginEffect],
    state: &PluginStateSnapshot,
) -> Result<SemanticDigest, &'static str> {
    let mut writer = CanonicalWriter::new();
    writer.field(b"ferrumc.plugin-testhost.semantic.v1");
    writer.effects(effects)?;
    writer.state(state)?;
    Ok(SemanticDigest(writer.finish()))
}

struct CanonicalWriter {
    hasher: Sha256,
}

impl CanonicalWriter {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }

    fn byte(&mut self, value: u8) {
        self.hasher.update([value]);
    }

    fn u32(&mut self, value: u32) {
        self.hasher.update(value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.hasher.update(value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.hasher.update(value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.hasher.update(value.to_le_bytes());
    }

    fn field(&mut self, value: &[u8]) {
        self.u64(u64::try_from(value.len()).unwrap_or(u64::MAX));
        self.hasher.update(value);
    }

    fn player(&mut self, player: PlayerId) {
        self.hasher.update(player.as_uuid().into_bytes());
    }

    fn pos(&mut self, pos: BlockPos) {
        self.i32(pos.x());
        self.i32(pos.y());
        self.i32(pos.z());
    }

    fn vec3(&mut self, value: Vec3) {
        self.u64(normalized_f64_bits(value.x));
        self.u64(normalized_f64_bits(value.y));
        self.u64(normalized_f64_bits(value.z));
    }

    fn effects(&mut self, effects: &[PluginEffect]) -> Result<(), &'static str> {
        self.byte(1);
        self.u64(u64::try_from(effects.len()).unwrap_or(u64::MAX));
        for effect in effects {
            match effect {
                PluginEffect::SubscribeEvent(kind) => {
                    self.byte(1);
                    self.event_kind(*kind)?;
                }
                PluginEffect::RegisterCommand(command) => {
                    self.byte(2);
                    self.command(command)?;
                }
                PluginEffect::SetBlock {
                    pos,
                    block_state_id,
                } => {
                    self.byte(3);
                    self.pos(*pos);
                    self.u32(*block_state_id);
                }
                PluginEffect::Teleport(operation) => {
                    self.byte(4);
                    self.player(operation.player());
                    self.vec3(operation.position());
                }
                PluginEffect::Message(operation) => {
                    self.byte(5);
                    self.player(operation.player());
                    self.field(operation.message().as_bytes());
                }
                PluginEffect::StoragePut { key, value } => {
                    self.byte(6);
                    self.field(key.as_bytes());
                    self.field(value);
                }
                PluginEffect::StorageDelete { key } => {
                    self.byte(7);
                    self.field(key.as_bytes());
                }
                PluginEffect::ScheduleTimer { id, due_tick } => {
                    self.byte(8);
                    self.u64(id.get());
                    self.u64(due_tick.get());
                }
                PluginEffect::CancelTimer { id } => {
                    self.byte(9);
                    self.u64(id.get());
                }
                PluginEffect::BlockDecision(decision) => {
                    self.byte(10);
                    self.block_decision(decision)?;
                }
                PluginEffect::EventDecision { kind, decision } => {
                    self.byte(11);
                    self.event_kind(*kind)?;
                    self.event_decision(decision)?;
                }
            }
        }
        Ok(())
    }

    fn state(&mut self, state: &PluginStateSnapshot) -> Result<(), &'static str> {
        self.byte(2);
        self.u64(u64::try_from(state.loaded_chunks.len()).unwrap_or(u64::MAX));
        for chunk in &state.loaded_chunks {
            self.i32(chunk.x());
            self.i32(chunk.z());
        }
        self.u64(u64::try_from(state.blocks.len()).unwrap_or(u64::MAX));
        for (pos, block) in &state.blocks {
            self.pos(*pos);
            self.u32(*block);
        }
        self.u64(u64::try_from(state.player_positions.len()).unwrap_or(u64::MAX));
        for (player, position) in &state.player_positions {
            self.player(*player);
            self.vec3(*position);
        }
        self.u64(u64::try_from(state.permissions.len()).unwrap_or(u64::MAX));
        for setting in &state.permissions {
            self.player(setting.player);
            self.field(setting.node.as_str().as_bytes());
            self.byte(resolution_tag(setting.resolution));
        }
        self.u64(u64::try_from(state.storage.len()).unwrap_or(u64::MAX));
        for entry in &state.storage {
            self.field(entry.key.as_bytes());
            self.field(&entry.value);
        }
        self.u64(u64::try_from(state.timers.len()).unwrap_or(u64::MAX));
        for timer in &state.timers {
            self.u64(timer.id.get());
            self.u64(timer.due_tick.get());
        }
        self.u64(u64::try_from(state.subscriptions.len()).unwrap_or(u64::MAX));
        for kind in &state.subscriptions {
            self.event_kind(*kind)?;
        }
        self.u64(u64::try_from(state.commands.len()).unwrap_or(u64::MAX));
        for command in &state.commands {
            self.command(command)?;
        }
        self.u64(u64::try_from(state.messages.len()).unwrap_or(u64::MAX));
        for message in &state.messages {
            self.player(message.player());
            self.field(message.message().as_bytes());
        }
        Ok(())
    }

    fn command(&mut self, command: &CommandDefinition) -> Result<(), &'static str> {
        self.u64(u64::try_from(command.nodes().len()).unwrap_or(u64::MAX));
        for node in command.nodes() {
            self.u64(node.parent().map_or(u64::MAX, |value| {
                u64::try_from(value).unwrap_or(u64::MAX - 1)
            }));
            match node.kind() {
                CommandNodeKind::Literal => self.byte(0),
                CommandNodeKind::Word => self.byte(1),
                CommandNodeKind::GreedyText => self.byte(2),
                CommandNodeKind::Integer(bounds) => {
                    self.byte(3);
                    self.i64(bounds.min());
                    self.i64(bounds.max());
                }
                _ => return Err("command node kind"),
            }
            self.field(node.name().as_bytes());
            self.u64(node.handler().map_or(0, ferrumc_plugin_sdk::HandlerId::get));
            self.byte(node.required_level().unwrap_or(u8::MAX));
            self.field(
                node.required_permission()
                    .map_or(b"".as_slice(), |node| node.as_str().as_bytes()),
            );
        }
        Ok(())
    }

    fn event_kind(&mut self, kind: EventKind) -> Result<(), &'static str> {
        let tag = match kind {
            EventKind::PlayerJoin => 1,
            EventKind::PlayerLeave => 2,
            EventKind::BlockBreak => 3,
            EventKind::AfterBlockPlace => 4,
            EventKind::AfterBlockBreak => 5,
            EventKind::PlayerMove => 6,
            EventKind::BlockPlaceAttempt => 7,
            EventKind::BlockBreakAttempt => 8,
            EventKind::ChatAttempt => 9,
            EventKind::InteractAttempt => 10,
            EventKind::Command => 11,
            EventKind::Timer => 12,
            _ => return Err("event kind"),
        };
        self.byte(tag);
        Ok(())
    }

    fn block_decision(&mut self, decision: &BlockDecision) -> Result<(), &'static str> {
        match decision {
            BlockDecision::Allow => self.byte(0),
            BlockDecision::Deny(feedback) => {
                self.byte(1);
                self.optional_feedback(feedback.as_ref());
            }
            BlockDecision::Replace(block) => {
                self.byte(2);
                self.u32(*block);
            }
            _ => return Err("block decision"),
        }
        Ok(())
    }

    fn event_decision(&mut self, decision: &EventDecision) -> Result<(), &'static str> {
        match decision {
            EventDecision::Allow => self.byte(0),
            EventDecision::Deny(feedback) => {
                self.byte(1);
                self.optional_feedback(feedback.as_ref());
            }
            _ => return Err("event decision"),
        }
        Ok(())
    }

    fn optional_feedback(&mut self, feedback: Option<&ferrumc_plugin_sdk::Feedback>) {
        match feedback {
            Some(feedback) => {
                self.byte(1);
                self.field(feedback.message().as_bytes());
            }
            None => self.byte(0),
        }
    }
}

fn resolution_tag(resolution: Resolution) -> u8 {
    match resolution {
        Resolution::Unset => 0,
        Resolution::Allowed => 1,
        Resolution::Denied => 2,
    }
}

fn normalized_f64_bits(value: f64) -> u64 {
    if value == 0.0 {
        0
    } else {
        value.to_bits()
    }
}
