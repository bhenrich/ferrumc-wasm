//! [`ConnectionLimits`]: per-state hostile-input size caps.

use crate::state::ConnectionState;

/// Maximum number of bytes a frame-length `VarInt` prefix may occupy.
///
/// A length prefix is a 32-bit `VarInt`, so it never legitimately exceeds five
/// bytes. The inbound buffer ceiling adds this to the largest frame cap so a
/// single maximum-size frame (prefix included) always fits.
pub const MAX_LENGTH_PREFIX_BYTES: usize = 5;

/// Default cap for [`Handshaking`](ConnectionState::Handshaking) frames: 4 KiB.
///
/// The handshake is a single tiny packet (protocol version, a ≤255-char
/// address, a port, and the next-state id). A small cap shuts down a peer that
/// declares a huge first frame before it can force any large allocation.
pub const DEFAULT_HANDSHAKE_MAX_FRAME: usize = 4 * 1024;

/// Default cap for [`Status`](ConnectionState::Status) frames: 32 KiB.
///
/// Serverbound status frames are tiny; the headroom covers the clientbound
/// status response JSON (MOTD plus an optional base64 favicon).
pub const DEFAULT_STATUS_MAX_FRAME: usize = 32 * 1024;

/// Default cap for [`Login`](ConnectionState::Login) frames: 64 KiB.
///
/// Covers login-success profile properties (e.g. base64 skin texture blobs)
/// while still rejecting absurd frames before they are buffered.
pub const DEFAULT_LOGIN_MAX_FRAME: usize = 64 * 1024;

/// Default cap for [`Configuration`](ConnectionState::Configuration) frames:
/// 2 MiB.
///
/// The configuration phase carries clientbound registry-data syncs, which are
/// the largest uncompressed frames the protocol sends; this matches the 2 MiB
/// decompressed-output ceiling used elsewhere in the networking model.
pub const DEFAULT_CONFIGURATION_MAX_FRAME: usize = 2 * 1024 * 1024;

/// Default cap for [`Play`](ConnectionState::Play) frames: 512 KiB.
///
/// The general-purpose in-game ceiling: large enough for chunk and entity
/// payloads, small enough to bound a malicious client's per-frame allocation.
pub const DEFAULT_PLAY_MAX_FRAME: usize = 512 * 1024;

/// Per-state hostile-input size caps for one connection.
///
/// Every inbound frame is rejected before its body is buffered if its declared
/// length exceeds the cap for the connection's current state, so a malicious
/// peer cannot drive a large allocation by advertising an enormous frame. The
/// same caps bound the outbound encoder so the server never emits a frame a
/// conforming client would refuse.
///
/// Construct with [`ConnectionLimits::default`] for the documented defaults, or
/// [`ConnectionLimits::new`] to override every cap explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    handshake: usize,
    status: usize,
    login: usize,
    configuration: usize,
    play: usize,
}

impl ConnectionLimits {
    /// Builds limits from explicit per-state maximum frame sizes (in bytes).
    ///
    /// Prefer [`ConnectionLimits::default`] unless a deployment needs to tighten
    /// or loosen specific phases.
    pub const fn new(
        handshake: usize,
        status: usize,
        login: usize,
        configuration: usize,
        play: usize,
    ) -> Self {
        Self {
            handshake,
            status,
            login,
            configuration,
            play,
        }
    }

    /// The maximum frame body size, in bytes, permitted in `state`.
    pub fn max_frame_size(&self, state: ConnectionState) -> usize {
        match state {
            ConnectionState::Handshaking => self.handshake,
            ConnectionState::Status => self.status,
            ConnectionState::Login => self.login,
            ConnectionState::Configuration => self.configuration,
            ConnectionState::Play => self.play,
        }
    }

    /// The largest per-state frame cap across all states.
    ///
    /// Used to size the inbound accumulation ceiling so a single maximum-length
    /// frame in any state still fits.
    pub fn max_frame_size_overall(&self) -> usize {
        self.handshake
            .max(self.status)
            .max(self.login)
            .max(self.configuration)
            .max(self.play)
    }

    /// The maximum number of bytes the inbound decoder will buffer.
    ///
    /// This is the largest frame body plus its length prefix: enough to hold one
    /// maximum-size frame mid-arrival, but no more. A peer that keeps sending
    /// without producing a complete, drainable frame is cut off here rather than
    /// being allowed to grow the buffer without bound.
    pub fn max_inbound_buffer(&self) -> usize {
        self.max_frame_size_overall()
            .saturating_add(MAX_LENGTH_PREFIX_BYTES)
    }
}

impl Default for ConnectionLimits {
    /// The documented per-state defaults: 4 KiB handshake, 32 KiB status, 64 KiB
    /// login, 2 MiB configuration, 512 KiB play.
    fn default() -> Self {
        Self::new(
            DEFAULT_HANDSHAKE_MAX_FRAME,
            DEFAULT_STATUS_MAX_FRAME,
            DEFAULT_LOGIN_MAX_FRAME,
            DEFAULT_CONFIGURATION_MAX_FRAME,
            DEFAULT_PLAY_MAX_FRAME,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_constants() {
        let limits = ConnectionLimits::default();
        assert_eq!(
            limits.max_frame_size(ConnectionState::Handshaking),
            DEFAULT_HANDSHAKE_MAX_FRAME
        );
        assert_eq!(
            limits.max_frame_size(ConnectionState::Status),
            DEFAULT_STATUS_MAX_FRAME
        );
        assert_eq!(
            limits.max_frame_size(ConnectionState::Login),
            DEFAULT_LOGIN_MAX_FRAME
        );
        assert_eq!(
            limits.max_frame_size(ConnectionState::Configuration),
            DEFAULT_CONFIGURATION_MAX_FRAME
        );
        assert_eq!(
            limits.max_frame_size(ConnectionState::Play),
            DEFAULT_PLAY_MAX_FRAME
        );
    }

    #[test]
    fn handshake_cap_is_smaller_than_play_cap() {
        // The milestone's core invariant: the handshake is tightly bounded while
        // play frames may be much larger.
        let limits = ConnectionLimits::default();
        assert!(
            limits.max_frame_size(ConnectionState::Handshaking)
                < limits.max_frame_size(ConnectionState::Play)
        );
    }

    #[test]
    fn overall_max_is_the_largest_cap() {
        let limits = ConnectionLimits::new(1, 2, 3, 9, 4);
        assert_eq!(limits.max_frame_size_overall(), 9);
        assert_eq!(limits.max_inbound_buffer(), 9 + MAX_LENGTH_PREFIX_BYTES);
    }

    #[test]
    fn inbound_buffer_saturates_instead_of_overflowing() {
        let limits = ConnectionLimits::new(0, 0, 0, 0, usize::MAX);
        assert_eq!(limits.max_inbound_buffer(), usize::MAX);
    }
}
