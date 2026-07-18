#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! # Overview
//!
//! This crate is test-only: it provides the protocol test harness that later
//! integration tests build on. Nothing here panics on its own — comparison and
//! round-trip helpers return a `Result` carrying a descriptive error, so the
//! calling test decides how to fail (typically `.unwrap()` / `?`).
//!
//! The pieces:
//!
//! - [`HexFixture`] / [`parse_hex`] / [`to_hex`] / [`hex_diff`]: load and render
//!   hex fixtures and compare byte runs with a readable [`HexDiff`].
//! - [`assert_packet_roundtrip`]: encode a `ferrumc-proto` packet, decode it
//!   back, and confirm the value (and wire bytes) survive the trip.
//! - [`assert_wire_frame`] / [`assert_frame_length`] / [`frame`]: the strict
//!   frame oracle — assert an encoded packet forms a well-shaped
//!   `[VarInt len][VarInt id][body]` frame (correct length prefix, expected id,
//!   no trailing bytes) and optionally matches a committed golden fixture.
//! - [`PacketScript`] / [`ScriptEntry`] / [`Replay`]: an ordered, directional
//!   record of wire bytes with a record/replay API and a serializable text
//!   transcript ([`PacketScript::to_transcript`] /
//!   [`PacketScript::from_transcript`]).
//! - [`ScriptedClient`]: an in-memory duplex byte pipe modelling a fake client
//!   that records its traffic for assertion against a [`PacketScript`]. The
//!   actual server wiring lands with M09/M11/M22.
//! - [`FaultInjectingStore`]: an in-memory
//!   [`ferrumc_storage::WorldStore`]/[`ferrumc_storage::PlayerStore`] with
//!   deterministic commit/response cutpoints, idempotent journal-receipt replay,
//!   committed-state inspection, and an ordered operation trace for
//!   crash-consistency tests.
//! - [`DurabilityFaultBattery`]: a fixed, seeded matrix that applies those
//!   cutpoints to world, player, and journal writes and returns exact attempted,
//!   committed, outcome, and trace reports.
//! - [`DurabilitySoakHarness`]: a 43-step deterministic lifecycle soak that
//!   composes the complete battery with capacity-one edit/persist/restart cycles
//!   and a canonical end-state digest.
//! - [`PluginTestHost`]: deterministic, bounded shared-SDK lifecycle and event
//!   replay through compiled-in and trusted native plugin packaging, with
//!   comparable semantic effects, final state, and canonical digests.

mod client;
mod durability_battery;
mod durability_soak;
mod fault_store;
mod hex;
mod oracle;
pub mod plugin_testhost;
mod roundtrip;
mod transcript;

pub use client::ScriptedClient;
pub use durability_battery::{
    DurabilityBatteryError, DurabilityBatteryReport, DurabilityCaseReport, DurabilityFaultBattery,
    DurabilityOutcome, DurabilityScenario, DurabilitySurface,
};
pub use durability_soak::{
    DurabilitySoakAction, DurabilitySoakDigest, DurabilitySoakEndState, DurabilitySoakError,
    DurabilitySoakFault, DurabilitySoakHarness, DurabilitySoakReport, DurabilitySoakScenario,
    DurabilitySoakStep, DurabilitySoakStepOutcome, DURABILITY_SOAK_CYCLES,
    DURABILITY_SOAK_PENDING_CAPACITY, DURABILITY_SOAK_STEPS, MAX_DURABILITY_SOAK_STORE_TRACE,
};
pub use fault_store::{
    FaultGate, FaultInjectingStore, FaultOperation, FaultStage, FaultStoreAttempt,
    FaultStoreAttemptedState, FaultStoreControlError, FaultStoreSnapshot, FaultTraceEntry,
    MAX_FAULT_SCHEDULE,
};
pub use hex::{hex_diff, parse_hex, to_hex, HexDiff, HexError, HexFixture};
pub use oracle::{assert_frame_length, assert_wire_frame, frame, FrameOracleError};
pub use plugin_testhost::{
    PermissionSetting, PluginCallbackPhase, PluginDiagnostic, PluginDiagnosticPhase, PluginEffect,
    PluginFailureKind, PluginReplayFailure, PluginRun, PluginStateSnapshot, PluginTestHost,
    PluginTestHostError, ScheduledPluginEvent, ScheduledTimer, SemanticDigest, StorageEntry,
    MAX_CALLBACK_EFFECTS, MAX_SCHEDULED_EVENTS,
};
pub use roundtrip::{assert_packet_roundtrip, RoundtripError};
pub use transcript::{PacketScript, Replay, ScriptEntry, ScriptMismatch, TranscriptError};
