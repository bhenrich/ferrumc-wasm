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
//! - [`PacketScript`] / [`ScriptEntry`] / [`Replay`]: an ordered, directional
//!   record of wire bytes with a record/replay API and a serializable text
//!   transcript ([`PacketScript::to_transcript`] /
//!   [`PacketScript::from_transcript`]).
//! - [`ScriptedClient`]: an in-memory duplex byte pipe modelling a fake client
//!   that records its traffic for assertion against a [`PacketScript`]. The
//!   actual server wiring lands with M09/M11/M22.

mod client;
mod hex;
mod roundtrip;
mod transcript;

pub use client::ScriptedClient;
pub use hex::{hex_diff, parse_hex, to_hex, HexDiff, HexError, HexFixture};
pub use roundtrip::{assert_packet_roundtrip, RoundtripError};
pub use transcript::{PacketScript, Replay, ScriptEntry, ScriptMismatch, TranscriptError};
