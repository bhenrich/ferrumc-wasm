//! Absolute connection-progress and keep-alive deadlines.
//!
//! [`ConnectionLiveness`] is socket-free policy state for a connection driver.
//! The caller supplies Tokio [`Instant`] values, reports partial or complete
//! inbound frames, and selects against [`next_deadline`](ConnectionLiveness::next_deadline).
//! Deadlines are stored as absolute instants, so rebuilding a `select!` after an
//! unrelated chunk, outbound, or metrics wakeup cannot extend them.

use std::time::Duration;

use tokio::time::Instant;

use crate::play::DisconnectReason;
use crate::state::ConnectionState;

/// Default absolute deadline for valid progress within one protocol state.
///
/// Thirty seconds gives normal clients ample time in each state while bounding
/// a peer that never completes the next valid packet.
pub const DEFAULT_STATE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Default time allowed to finish one frame after its first partial bytes arrive.
///
/// This is independent of the state-progress deadline and never moves when more
/// partial bytes arrive.
pub const DEFAULT_FRAME_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);

/// Default response deadline for one clientbound play Keep Alive.
///
/// Protocol 772 specifies that the vanilla server disconnects a client that has
/// not echoed the Keep Alive ID within 15 seconds.
pub const DEFAULT_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Durations used by [`ConnectionLiveness`].
///
/// These are policy durations, not wall-clock reads: callers provide every
/// [`Instant`], which keeps the tracker deterministic and compatible with
/// Tokio's paused test clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessConfig {
    state_progress: Duration,
    frame_completion: Duration,
    keep_alive: Duration,
}

impl LivenessConfig {
    /// Creates an explicit liveness policy.
    ///
    /// A zero duration expires at the same instant the corresponding deadline
    /// starts. If adding a duration would overflow [`Instant`], the deadline
    /// fails closed at `now`.
    pub const fn new(
        state_progress_timeout: Duration,
        frame_completion_timeout: Duration,
        keep_alive_timeout: Duration,
    ) -> Self {
        Self {
            state_progress: state_progress_timeout,
            frame_completion: frame_completion_timeout,
            keep_alive: keep_alive_timeout,
        }
    }

    /// The absolute valid-progress window for every protocol state.
    pub const fn state_progress_timeout(self) -> Duration {
        self.state_progress
    }

    /// The absolute completion window for one partially received frame.
    pub const fn frame_completion_timeout(self) -> Duration {
        self.frame_completion
    }

    /// The response window for one outstanding play Keep Alive.
    pub const fn keep_alive_timeout(self) -> Duration {
        self.keep_alive
    }
}

impl Default for LivenessConfig {
    fn default() -> Self {
        Self::new(
            DEFAULT_STATE_PROGRESS_TIMEOUT,
            DEFAULT_FRAME_COMPLETION_TIMEOUT,
            DEFAULT_KEEP_ALIVE_TIMEOUT,
        )
    }
}

/// The exact deadline that expired.
///
/// State and frame timeouts classify as
/// [`ProgressTimeout`](DisconnectReason::ProgressTimeout); a missing Keep Alive
/// response retains its distinct
/// [`KeepAliveTimeout`](DisconnectReason::KeepAliveTimeout) classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LivenessTimeout {
    /// No complete valid packet advanced the current protocol state in time.
    #[error("no valid progress in the {state:?} state before its deadline")]
    StateProgress {
        /// The protocol state whose progress window expired.
        state: ConnectionState,
    },
    /// A frame remained incomplete after its first bytes arrived.
    #[error("partial frame in the {state:?} state missed its completion deadline")]
    FrameCompletion {
        /// The protocol state in which the partial frame began.
        state: ConnectionState,
    },
    /// The client did not echo the one outstanding Keep Alive ID in time.
    #[error("keep-alive id {id} missed its response deadline")]
    KeepAlive {
        /// The outstanding Keep Alive ID.
        id: i64,
    },
}

impl LivenessTimeout {
    /// The connection-level reason corresponding to this timeout.
    pub const fn disconnect_reason(self) -> DisconnectReason {
        match self {
            Self::StateProgress { .. } | Self::FrameCompletion { .. } => {
                DisconnectReason::ProgressTimeout
            }
            Self::KeepAlive { .. } => DisconnectReason::KeepAliveTimeout,
        }
    }
}

/// A misuse, timeout, or invalid response involving one Keep Alive exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeepAliveError {
    /// The liveness tracker has already closed and accepts no more activity.
    #[error("connection liveness is already closed")]
    Closed,
    /// Keep Alive is a play-state exchange, but the caller used it in another
    /// state.
    #[error("keep-alive exchange attempted in the {state:?} state")]
    NotInPlay {
        /// The current protocol state.
        state: ConnectionState,
    },
    /// The caller tried to send another Keep Alive before the current one was
    /// answered.
    #[error("keep-alive id {id} is already outstanding")]
    AlreadyOutstanding {
        /// The ID that is still awaiting a response.
        id: i64,
    },
    /// A response arrived when no Keep Alive was outstanding, including a replay
    /// of an already accepted ID.
    #[error("unexpected keep-alive response id {received}")]
    Unexpected {
        /// The response ID received from the client.
        received: i64,
    },
    /// The response did not echo the outstanding ID.
    #[error("keep-alive response id {received} did not match outstanding id {expected}")]
    Mismatched {
        /// The ID the server sent.
        expected: i64,
        /// The ID the client returned.
        received: i64,
    },
    /// Keep Alive activity occurred at or after the earliest active liveness
    /// deadline.
    #[error("keep-alive activity occurred after liveness expired: {timeout}")]
    TimedOut {
        /// The exact state, frame, or Keep Alive deadline that expired first.
        timeout: LivenessTimeout,
    },
}

impl KeepAliveError {
    /// The peer-facing disconnect classification, if this error represents a
    /// peer failure.
    ///
    /// Closed/not-in-play/already-outstanding errors describe caller state and
    /// therefore return `None`. Unexpected or mismatched IDs are protocol
    /// violations; timeout errors retain the classification of the earliest
    /// expired state, frame, or Keep Alive deadline.
    pub const fn disconnect_reason(self) -> Option<DisconnectReason> {
        match self {
            Self::Unexpected { .. } | Self::Mismatched { .. } => {
                Some(DisconnectReason::ProtocolViolation)
            }
            Self::TimedOut { timeout } => Some(timeout.disconnect_reason()),
            Self::Closed | Self::NotInPlay { .. } | Self::AlreadyOutstanding { .. } => None,
        }
    }
}

/// Why general frame/state activity could not advance liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LivenessActivityError {
    /// The liveness tracker had already closed.
    #[error("connection liveness is already closed")]
    Closed,
    /// The activity arrived at or after an already-active absolute deadline.
    #[error(transparent)]
    TimedOut(#[from] LivenessTimeout),
}

impl LivenessActivityError {
    /// The peer-facing disconnect reason, if activity exposed a timeout.
    pub const fn disconnect_reason(self) -> Option<DisconnectReason> {
        match self {
            Self::Closed => None,
            Self::TimedOut(timeout) => Some(timeout.disconnect_reason()),
        }
    }
}

/// The next absolute liveness deadline and the timeout it will produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessDeadline {
    at: Instant,
    timeout: LivenessTimeout,
}

impl LivenessDeadline {
    /// The absolute Tokio instant at which this deadline expires.
    pub const fn at(self) -> Instant {
        self.at
    }

    /// The classified timeout produced at [`at`](Self::at).
    pub const fn timeout(self) -> LivenessTimeout {
        self.timeout
    }
}

/// One partially received frame's absolute completion state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PartialFrame {
    state: ConnectionState,
    deadline: Instant,
}

/// One clientbound Keep Alive awaiting its matching serverbound response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutstandingKeepAlive {
    id: i64,
    sent_at: Instant,
    deadline: Instant,
}

/// Tracks absolute state, frame, and Keep Alive liveness for one connection.
///
/// The tracker performs no I/O and owns no timer task. A socket driver reports
/// activity and selects against [`next_deadline`](Self::next_deadline), then
/// calls [`poll_timeout`](Self::poll_timeout) when the clock reaches that
/// instant. Because each deadline is absolute, unrelated select branches may
/// rebuild their sleep without extending the peer's window.
///
/// A complete valid packet—or, in Play, a valid Keep Alive echo—is the only
/// inbound activity that refreshes state progress and clears a partial-frame
/// deadline. Raw bytes call
/// [`partial_frame_observed`](Self::partial_frame_observed), which starts one
/// frame deadline and never moves it. A Keep Alive response must match the sole
/// outstanding ID before every active state, frame, and response deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionLiveness {
    config: LivenessConfig,
    state: ConnectionState,
    state_deadline: Option<Instant>,
    partial_frame: Option<PartialFrame>,
    keep_alive: Option<OutstandingKeepAlive>,
    last_valid_packet_at: Option<Instant>,
    closed: bool,
}

impl ConnectionLiveness {
    /// Creates an open tracker in
    /// [`Handshaking`](ConnectionState::Handshaking), with its first state
    /// deadline measured from `now`.
    pub fn new(now: Instant, config: LivenessConfig) -> Self {
        Self {
            config,
            state: ConnectionState::Handshaking,
            state_deadline: Some(deadline_from(now, config.state_progress)),
            partial_frame: None,
            keep_alive: None,
            last_valid_packet_at: None,
            closed: false,
        }
    }

    /// The current protocol state.
    pub const fn state(&self) -> ConnectionState {
        self.state
    }

    /// Whether the connection was explicitly closed or timed out.
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// The most recent complete valid packet activity, if any.
    pub const fn last_valid_packet_at(&self) -> Option<Instant> {
        self.last_valid_packet_at
    }

    /// The outstanding Keep Alive `(id, sent_at)`, if one exists.
    pub const fn outstanding_keep_alive(&self) -> Option<(i64, Instant)> {
        match self.keep_alive {
            Some(keep_alive) => Some((keep_alive.id, keep_alive.sent_at)),
            None => None,
        }
    }

    /// Enters `state` after a valid protocol transition.
    ///
    /// Every entered state, including Play, starts a fresh absolute
    /// state-progress deadline. A state transition also clears any partial
    /// frame and prior Keep Alive state.
    ///
    /// # Errors
    ///
    /// Returns [`LivenessActivityError::Closed`] if the tracker has already
    /// closed, or [`LivenessActivityError::TimedOut`] when the transition
    /// arrives at or after an active deadline.
    pub fn enter_state(
        &mut self,
        state: ConnectionState,
        now: Instant,
    ) -> Result<(), LivenessActivityError> {
        self.ensure_activity_before_deadline(now)?;
        self.state = state;
        self.state_deadline = Some(deadline_from(now, self.config.state_progress));
        self.partial_frame = None;
        self.keep_alive = None;
        self.last_valid_packet_at = Some(now);
        Ok(())
    }

    /// Reports that bytes are buffered for a frame that is not complete yet.
    ///
    /// The first observation starts the frame-completion deadline. Every later
    /// partial read leaves the original deadline untouched, preventing a
    /// one-byte drip from extending the connection indefinitely.
    ///
    /// # Errors
    ///
    /// Returns [`LivenessActivityError::Closed`] if the tracker has already
    /// closed, or [`LivenessActivityError::TimedOut`] when bytes arrive at or
    /// after an active deadline.
    pub fn partial_frame_observed(&mut self, now: Instant) -> Result<(), LivenessActivityError> {
        self.ensure_activity_before_deadline(now)?;
        if self.partial_frame.is_none() {
            self.partial_frame = Some(PartialFrame {
                state: self.state,
                deadline: deadline_from(now, self.config.frame_completion),
            });
        }
        Ok(())
    }

    /// Reports one fully decoded, valid inbound packet.
    ///
    /// This is the only general activity event: it clears a partial-frame
    /// deadline, records `now`, and refreshes the current state's absolute
    /// progress window. In Play it does not satisfy an outstanding Keep Alive;
    /// only [`keep_alive_received`](Self::keep_alive_received) can do so.
    ///
    /// # Errors
    ///
    /// Returns [`LivenessActivityError::Closed`] if the tracker has already
    /// closed, or [`LivenessActivityError::TimedOut`] when the packet completes
    /// at or after an active deadline.
    pub fn valid_packet_observed(&mut self, now: Instant) -> Result<(), LivenessActivityError> {
        self.ensure_activity_before_deadline(now)?;
        self.partial_frame = None;
        self.last_valid_packet_at = Some(now);
        self.state_deadline = Some(deadline_from(now, self.config.state_progress));
        Ok(())
    }

    /// Records a clientbound Keep Alive as the sole outstanding request.
    ///
    /// The response deadline is measured from `sent_at`. Sending another ID
    /// before the first is answered is rejected without changing either
    /// deadline.
    ///
    /// # Errors
    ///
    /// Returns [`KeepAliveError::Closed`] after closure,
    /// [`KeepAliveError::NotInPlay`] outside Play,
    /// [`KeepAliveError::TimedOut`] after an active deadline, or
    /// [`KeepAliveError::AlreadyOutstanding`] when a live request is pending.
    pub fn keep_alive_sent(&mut self, id: i64, sent_at: Instant) -> Result<(), KeepAliveError> {
        if self.closed {
            return Err(KeepAliveError::Closed);
        }
        if !self.state.is_play() {
            return Err(KeepAliveError::NotInPlay { state: self.state });
        }
        if let Some(timeout) = self.poll_timeout(sent_at) {
            return Err(KeepAliveError::TimedOut { timeout });
        }
        if let Some(outstanding) = self.keep_alive {
            return Err(KeepAliveError::AlreadyOutstanding { id: outstanding.id });
        }
        self.keep_alive = Some(OutstandingKeepAlive {
            id,
            sent_at,
            deadline: deadline_from(sent_at, self.config.keep_alive),
        });
        Ok(())
    }

    /// Validates one serverbound Keep Alive response.
    ///
    /// A response is accepted exactly once when its ID matches and `received_at`
    /// is strictly before every active state, frame, and Keep Alive deadline.
    /// Unexpected, mismatched, or late responses close the tracker immediately
    /// so their deadline cannot fire again.
    ///
    /// # Errors
    ///
    /// Returns a precise [`KeepAliveError`] for closed/caller state, a replay,
    /// an ID mismatch, or an expired response.
    pub fn keep_alive_received(
        &mut self,
        received: i64,
        received_at: Instant,
    ) -> Result<(), KeepAliveError> {
        if self.closed {
            return Err(KeepAliveError::Closed);
        }
        if !self.state.is_play() {
            return Err(KeepAliveError::NotInPlay { state: self.state });
        }
        if let Some(timeout) = self.poll_timeout(received_at) {
            return Err(KeepAliveError::TimedOut { timeout });
        }
        let Some(outstanding) = self.keep_alive else {
            self.close();
            return Err(KeepAliveError::Unexpected { received });
        };
        if received != outstanding.id {
            self.close();
            return Err(KeepAliveError::Mismatched {
                expected: outstanding.id,
                received,
            });
        }

        self.keep_alive = None;
        self.partial_frame = None;
        self.last_valid_packet_at = Some(received_at);
        self.state_deadline = Some(deadline_from(received_at, self.config.state_progress));
        Ok(())
    }

    /// The earliest active absolute deadline, or `None` after closure.
    ///
    /// Callers can rebuild `tokio::time::sleep_until(deadline.at())` after any
    /// unrelated wakeup without changing the stored instant.
    pub fn next_deadline(&self) -> Option<LivenessDeadline> {
        if self.closed {
            return None;
        }

        let mut earliest = self.state_deadline.map(|at| LivenessDeadline {
            at,
            timeout: LivenessTimeout::StateProgress { state: self.state },
        });
        if let Some(frame) = self.partial_frame {
            earliest = Some(earlier(
                earliest,
                LivenessDeadline {
                    at: frame.deadline,
                    timeout: LivenessTimeout::FrameCompletion { state: frame.state },
                },
            ));
        }
        if let Some(keep_alive) = self.keep_alive {
            earliest = Some(earlier(
                earliest,
                LivenessDeadline {
                    at: keep_alive.deadline,
                    timeout: LivenessTimeout::KeepAlive { id: keep_alive.id },
                },
            ));
        }
        earliest
    }

    /// Returns and consumes the timeout due at `now`, closing the tracker.
    ///
    /// Expiry is inclusive: a deadline is due when `now >= deadline.at()`.
    /// Calling this on unrelated wakeups before that instant has no effect.
    pub fn poll_timeout(&mut self, now: Instant) -> Option<LivenessTimeout> {
        let deadline = self.next_deadline()?;
        if now < deadline.at {
            return None;
        }
        self.close();
        Some(deadline.timeout)
    }

    /// Cancels every deadline and rejects all later activity.
    ///
    /// This is idempotent, allowing connection teardown to call it regardless of
    /// which socket/timer/select branch ended the link.
    pub fn close(&mut self) {
        self.closed = true;
        self.state_deadline = None;
        self.partial_frame = None;
        self.keep_alive = None;
    }

    /// Rejects activity after closure or at an already-expired deadline.
    fn ensure_activity_before_deadline(
        &mut self,
        now: Instant,
    ) -> Result<(), LivenessActivityError> {
        if self.closed {
            return Err(LivenessActivityError::Closed);
        }
        if let Some(timeout) = self.poll_timeout(now) {
            return Err(LivenessActivityError::TimedOut(timeout));
        }
        Ok(())
    }
}

/// Adds `duration` to `now`, failing closed at `now` if the instant range would
/// overflow.
fn deadline_from(now: Instant, duration: Duration) -> Instant {
    now.checked_add(duration).unwrap_or(now)
}

/// Keeps the earlier of one current deadline and one candidate.
///
/// Ties retain the existing candidate, making classification deterministic.
fn earlier(current: Option<LivenessDeadline>, candidate: LivenessDeadline) -> LivenessDeadline {
    match current {
        Some(existing) if existing.at <= candidate.at => existing,
        _ => candidate,
    }
}
