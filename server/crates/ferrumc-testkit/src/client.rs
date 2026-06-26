//! [`ScriptedClient`]: an in-memory, connection-state-agnostic byte pipe that
//! models one end of a client/server link for tests.
//!
//! There is no running server yet (that wiring lands with M09/M11/M22), so this
//! type stands in for the transport: it holds two byte lanes — serverbound
//! (client to server) and clientbound (server to client) — and records the
//! client's own traffic into a [`PacketScript`] that can be asserted against an
//! expected script. It deals only in raw bytes; framing, compression,
//! encryption and connection state belong to `ferrumc-net` and are out of scope
//! here.

use std::collections::VecDeque;

use crate::transcript::{PacketScript, ScriptMismatch};

/// A fake client backed by an in-memory duplex byte buffer.
///
/// Drive it from the client's perspective with [`send`](Self::send) (push
/// serverbound bytes) and [`recv`](Self::recv) / [`recv_all`](Self::recv_all)
/// (pull clientbound bytes); both are recorded into the client's
/// [`transcript`](Self::transcript). Stage the other side of the link with
/// [`feed`](Self::feed) (enqueue clientbound bytes as if the server sent them)
/// and inspect what the client emitted with
/// [`take_serverbound`](Self::take_serverbound); neither of those is recorded,
/// as they model the server/test harness rather than the client.
#[derive(Debug, Clone, Default)]
pub struct ScriptedClient {
    serverbound: VecDeque<u8>,
    clientbound: VecDeque<u8>,
    transcript: PacketScript,
}

impl ScriptedClient {
    /// Creates a client with empty lanes and an empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    /// Client side: pushes `bytes` onto the serverbound lane and records them as
    /// a serverbound transcript entry. Empty input is a no-op.
    pub fn send(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.serverbound.extend(bytes.iter().copied());
        self.transcript.record_serverbound(bytes.to_vec());
    }

    /// Client side: pulls up to `max` bytes off the clientbound lane, recording
    /// what it read as a clientbound transcript entry. Returns the bytes read
    /// (possibly fewer than `max`, or empty); an empty read is not recorded.
    pub fn recv(&mut self, max: usize) -> Vec<u8> {
        let take = max.min(self.clientbound.len());
        let bytes: Vec<u8> = self.clientbound.drain(..take).collect();
        if !bytes.is_empty() {
            self.transcript.record_clientbound(bytes.clone());
        }
        bytes
    }

    /// Client side: pulls every queued clientbound byte, recording the read.
    pub fn recv_all(&mut self) -> Vec<u8> {
        self.recv(self.clientbound.len())
    }

    /// Server/test side: enqueues `bytes` on the clientbound lane so the client
    /// can later [`recv`](Self::recv) them. Not recorded into the transcript.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.clientbound.extend(bytes.iter().copied());
    }

    /// Server/test side: drains up to `max` bytes the client has sent on the
    /// serverbound lane. Not recorded into the transcript.
    pub fn take_serverbound(&mut self, max: usize) -> Vec<u8> {
        let take = max.min(self.serverbound.len());
        self.serverbound.drain(..take).collect()
    }

    /// Server/test side: drains every byte the client has sent.
    pub fn take_serverbound_all(&mut self) -> Vec<u8> {
        self.take_serverbound(self.serverbound.len())
    }

    /// Bytes queued on the serverbound lane (client to server) not yet drained.
    pub fn serverbound_len(&self) -> usize {
        self.serverbound.len()
    }

    /// Bytes queued on the clientbound lane (server to client) not yet read.
    pub fn clientbound_len(&self) -> usize {
        self.clientbound.len()
    }

    /// The transcript of this client's recorded traffic.
    pub fn transcript(&self) -> &PacketScript {
        &self.transcript
    }

    /// Asserts the recorded traffic matches `expected`, returning `Ok(())` on a
    /// match or a [`ScriptMismatch`] describing the first divergence.
    pub fn verify_against(&self, expected: &PacketScript) -> Result<(), ScriptMismatch> {
        self.transcript.verify_eq(expected)
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_proto::Direction;

    use super::ScriptedClient;
    use crate::transcript::PacketScript;

    #[test]
    fn duplex_lanes_are_independent() {
        let mut client = ScriptedClient::new();
        client.send(&[0x10, 0x11]);
        client.feed(&[0xaa, 0xbb, 0xcc]);

        // The server side sees exactly what the client sent.
        assert_eq!(client.serverbound_len(), 2);
        assert_eq!(client.take_serverbound_all(), vec![0x10, 0x11]);
        assert_eq!(client.serverbound_len(), 0);

        // The client reads the fed bytes in two chunks.
        assert_eq!(client.recv(2), vec![0xaa, 0xbb]);
        assert_eq!(client.clientbound_len(), 1);
        assert_eq!(client.recv_all(), vec![0xcc]);
        assert_eq!(client.recv_all(), Vec::<u8>::new());
    }

    #[test]
    fn empty_send_and_recv_are_not_recorded() {
        let mut client = ScriptedClient::new();
        client.send(&[]); // no-op
        let _ = client.recv(8); // nothing queued
        assert!(client.transcript().is_empty());
    }

    #[test]
    fn transcript_records_client_perspective_only() {
        let mut client = ScriptedClient::new();
        client.send(&[0x01]);
        client.feed(&[0x02, 0x03]);
        let _ = client.recv_all();

        let entries = client.transcript().entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].direction(), Direction::Serverbound);
        assert_eq!(entries[0].bytes(), &[0x01]);
        assert_eq!(entries[1].direction(), Direction::Clientbound);
        assert_eq!(entries[1].bytes(), &[0x02, 0x03]);
    }

    #[test]
    fn verify_against_expected_script() {
        let mut client = ScriptedClient::new();
        client.send(&[0x01]);
        client.feed(&[0x02, 0x03]);
        let _ = client.recv_all();

        let mut expected = PacketScript::new();
        expected.record_serverbound(vec![0x01]);
        expected.record_clientbound(vec![0x02, 0x03]);
        assert!(client.verify_against(&expected).is_ok());

        let mut wrong = PacketScript::new();
        wrong.record_serverbound(vec![0x09]);
        wrong.record_clientbound(vec![0x02, 0x03]);
        assert!(client.verify_against(&wrong).is_err());
    }
}
