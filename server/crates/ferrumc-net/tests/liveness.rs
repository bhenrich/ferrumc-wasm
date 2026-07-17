use std::time::Duration;

use ferrumc_net::{
    ConnectionLiveness, ConnectionState, DisconnectReason, KeepAliveError, LivenessActivityError,
    LivenessConfig, LivenessTimeout, DEFAULT_FRAME_COMPLETION_TIMEOUT, DEFAULT_KEEP_ALIVE_TIMEOUT,
    DEFAULT_STATE_PROGRESS_TIMEOUT,
};
use tokio::time::{advance, Instant};

fn config(
    state_progress: Duration,
    frame_completion: Duration,
    keep_alive: Duration,
) -> LivenessConfig {
    LivenessConfig::new(state_progress, frame_completion, keep_alive)
}

fn play_tracker(start: Instant, keep_alive: Duration) -> ConnectionLiveness {
    let mut tracker = ConnectionLiveness::new(
        start,
        config(Duration::from_mins(1), Duration::from_secs(10), keep_alive),
    );
    tracker
        .enter_state(ConnectionState::Play, start)
        .expect("an open tracker can enter play");
    tracker
}

#[tokio::test(start_paused = true)]
async fn silent_client_closes_at_exact_deadline_despite_unrelated_wakeups() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_secs(15));
    tracker
        .keep_alive_sent(41, start)
        .expect("the first keep-alive becomes outstanding");
    let deadline = tracker.next_deadline().expect("response deadline");
    assert_eq!(deadline.at(), start + Duration::from_secs(15));
    assert_eq!(deadline.timeout(), LivenessTimeout::KeepAlive { id: 41 });

    // Model unrelated chunk/outbound wakeups that repeatedly rebuild a select
    // loop. Polling must never slide the absolute response deadline.
    for second in 1..15 {
        advance(Duration::from_secs(1)).await;
        assert_eq!(
            tracker.poll_timeout(Instant::now()),
            None,
            "unrelated wakeup {second} must not reset liveness",
        );
    }
    advance(Duration::from_secs(1)).await;
    assert_eq!(
        tracker.poll_timeout(Instant::now()),
        Some(LivenessTimeout::KeepAlive { id: 41 }),
    );
    assert!(tracker.is_closed());
}

#[test]
fn default_policy_matches_the_documented_deadlines() {
    let config = LivenessConfig::default();
    assert_eq!(
        config.state_progress_timeout(),
        DEFAULT_STATE_PROGRESS_TIMEOUT,
    );
    assert_eq!(
        config.frame_completion_timeout(),
        DEFAULT_FRAME_COMPLETION_TIMEOUT,
    );
    assert_eq!(config.keep_alive_timeout(), DEFAULT_KEEP_ALIVE_TIMEOUT);
}

#[test]
fn keep_alive_exchange_is_rejected_outside_play_without_closing() {
    let start = Instant::now();
    let mut tracker = ConnectionLiveness::new(start, LivenessConfig::default());
    assert_eq!(
        tracker
            .keep_alive_sent(1, start)
            .expect_err("keep-alive is play-only"),
        KeepAliveError::NotInPlay {
            state: ConnectionState::Handshaking,
        },
    );
    assert!(!tracker.is_closed());
    assert_eq!(tracker.outstanding_keep_alive(), None);
}

#[tokio::test(start_paused = true)]
async fn play_has_an_absolute_progress_deadline_before_keep_alive_starts() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_secs(15));

    let deadline = tracker
        .next_deadline()
        .expect("entering play starts its progress deadline");
    assert_eq!(deadline.at(), start + Duration::from_mins(1));
    assert_eq!(
        deadline.timeout(),
        LivenessTimeout::StateProgress {
            state: ConnectionState::Play,
        },
    );

    advance(Duration::from_mins(1)).await;
    assert_eq!(
        tracker.poll_timeout(Instant::now()),
        Some(LivenessTimeout::StateProgress {
            state: ConnectionState::Play,
        }),
    );
}

#[tokio::test(start_paused = true)]
async fn keep_alive_tick_cannot_revive_expired_play_progress() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_secs(15));
    advance(Duration::from_mins(1)).await;

    let error = tracker
        .keep_alive_sent(5, Instant::now())
        .expect_err("an unrelated keep-alive tick cannot outrun the deadline");
    assert_eq!(
        error,
        KeepAliveError::TimedOut {
            timeout: LivenessTimeout::StateProgress {
                state: ConnectionState::Play,
            },
        },
    );
    assert_eq!(
        error.disconnect_reason(),
        Some(DisconnectReason::ProgressTimeout),
    );
    assert!(tracker.is_closed());
}

#[tokio::test(start_paused = true)]
async fn one_byte_drip_does_not_reset_total_login_progress_deadline() {
    let start = Instant::now();
    let mut tracker = ConnectionLiveness::new(
        start,
        config(
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(15),
        ),
    );
    tracker
        .enter_state(ConnectionState::Login, start)
        .expect("an open tracker can enter login");

    tracker
        .partial_frame_observed(start)
        .expect("the first byte starts the frame timer");
    for _ in 1..5 {
        advance(Duration::from_secs(1)).await;
        tracker
            .partial_frame_observed(Instant::now())
            .expect("later bytes do not refresh either deadline");
        assert_eq!(tracker.poll_timeout(Instant::now()), None);
    }
    advance(Duration::from_secs(1)).await;
    let error = tracker
        .partial_frame_observed(Instant::now())
        .expect_err("bytes at the exact deadline cannot revive login");
    assert_eq!(
        error,
        LivenessActivityError::TimedOut(LivenessTimeout::StateProgress {
            state: ConnectionState::Login,
        }),
    );
    assert!(tracker.is_closed());
}

#[tokio::test(start_paused = true)]
async fn partial_frame_has_an_independent_absolute_completion_deadline() {
    let start = Instant::now();
    let mut tracker = ConnectionLiveness::new(
        start,
        config(
            Duration::from_mins(1),
            Duration::from_secs(5),
            Duration::from_secs(15),
        ),
    );
    tracker
        .partial_frame_observed(start)
        .expect("the first partial byte starts one deadline");

    for _ in 1..5 {
        advance(Duration::from_secs(1)).await;
        tracker
            .partial_frame_observed(Instant::now())
            .expect("a drip cannot move the original deadline");
        assert_eq!(tracker.poll_timeout(Instant::now()), None);
    }
    advance(Duration::from_secs(1)).await;
    assert_eq!(
        tracker.poll_timeout(Instant::now()),
        Some(LivenessTimeout::FrameCompletion {
            state: ConnectionState::Handshaking,
        }),
    );
}

#[tokio::test(start_paused = true)]
async fn complete_valid_packet_is_the_only_frame_activity_that_extends_liveness() {
    let start = Instant::now();
    let mut tracker = ConnectionLiveness::new(
        start,
        config(
            Duration::from_secs(10),
            Duration::from_secs(5),
            Duration::from_secs(15),
        ),
    );
    tracker
        .partial_frame_observed(start)
        .expect("partial frame starts its deadline");

    advance(Duration::from_secs(4)).await;
    tracker
        .valid_packet_observed(Instant::now())
        .expect("a complete valid packet clears partial-frame state");
    advance(Duration::from_secs(2)).await;
    assert_eq!(
        tracker.poll_timeout(Instant::now()),
        None,
        "the old frame and state deadlines were replaced only by valid activity",
    );
    advance(Duration::from_secs(8)).await;
    assert_eq!(
        tracker.poll_timeout(Instant::now()),
        Some(LivenessTimeout::StateProgress {
            state: ConnectionState::Handshaking,
        }),
    );
}

#[tokio::test(start_paused = true)]
async fn valid_packet_at_the_exact_deadline_cannot_revive_the_connection() {
    let start = Instant::now();
    let mut tracker = ConnectionLiveness::new(
        start,
        config(
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(15),
        ),
    );
    advance(Duration::from_secs(5)).await;

    let error = tracker
        .valid_packet_observed(Instant::now())
        .expect_err("activity at the inclusive deadline is expired");
    assert_eq!(
        error,
        LivenessActivityError::TimedOut(LivenessTimeout::StateProgress {
            state: ConnectionState::Handshaking,
        }),
    );
    assert_eq!(
        error.disconnect_reason(),
        Some(DisconnectReason::ProgressTimeout),
    );
    assert!(tracker.is_closed());
}

#[tokio::test(start_paused = true)]
async fn wrong_and_replayed_keep_alive_ids_are_protocol_violations() {
    let start = Instant::now();
    let mut wrong = play_tracker(start, Duration::from_secs(15));
    wrong
        .keep_alive_sent(7, start)
        .expect("one outstanding keep-alive");
    let mismatch = wrong
        .keep_alive_received(8, start)
        .expect_err("the response must echo the exact id");
    assert_eq!(
        mismatch,
        KeepAliveError::Mismatched {
            expected: 7,
            received: 8,
        },
    );
    assert_eq!(
        mismatch.disconnect_reason(),
        Some(DisconnectReason::ProtocolViolation),
    );
    assert!(wrong.is_closed());

    let mut replay = play_tracker(start, Duration::from_secs(15));
    replay
        .keep_alive_sent(9, start)
        .expect("one outstanding keep-alive");
    replay
        .keep_alive_received(9, start)
        .expect("the matching response is accepted once");
    let stale = replay
        .keep_alive_received(9, start)
        .expect_err("a replay has no outstanding request");
    assert_eq!(stale, KeepAliveError::Unexpected { received: 9 });
    assert_eq!(
        stale.disconnect_reason(),
        Some(DisconnectReason::ProtocolViolation),
    );
    assert!(replay.is_closed());
}

#[tokio::test(start_paused = true)]
async fn valid_keep_alive_extends_liveness_from_its_response() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_secs(15));
    tracker
        .keep_alive_sent(1, start)
        .expect("first request is outstanding");

    advance(Duration::from_secs(14)).await;
    let response_at = Instant::now();
    tracker
        .keep_alive_received(1, response_at)
        .expect("response before the deadline is valid");
    assert_eq!(tracker.last_valid_packet_at(), Some(response_at));
    assert_eq!(tracker.outstanding_keep_alive(), None);
    let progress_deadline = tracker
        .next_deadline()
        .expect("a valid echo refreshes play progress");
    assert_eq!(progress_deadline.at(), response_at + Duration::from_mins(1));
    assert_eq!(
        progress_deadline.timeout(),
        LivenessTimeout::StateProgress {
            state: ConnectionState::Play,
        },
    );

    // The original t+15 deadline is gone. A later request receives its own full
    // response window starting when that request is sent.
    advance(Duration::from_secs(1)).await;
    assert_eq!(tracker.poll_timeout(Instant::now()), None);
    tracker
        .keep_alive_sent(2, Instant::now())
        .expect("a response permits the next request");
    advance(Duration::from_secs(14)).await;
    assert_eq!(tracker.poll_timeout(Instant::now()), None);
    advance(Duration::from_secs(1)).await;
    assert_eq!(
        tracker.poll_timeout(Instant::now()),
        Some(LivenessTimeout::KeepAlive { id: 2 }),
    );
}

#[tokio::test(start_paused = true)]
async fn complete_valid_play_packet_refreshes_progress_without_answering_keep_alive() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_mins(2));
    tracker
        .keep_alive_sent(29, start)
        .expect("request is outstanding");

    advance(Duration::from_secs(30)).await;
    let packet_at = Instant::now();
    tracker
        .valid_packet_observed(packet_at)
        .expect("a complete valid play packet advances general progress");

    assert_eq!(tracker.outstanding_keep_alive(), Some((29, start)));
    let deadline = tracker
        .next_deadline()
        .expect("both progress and keep-alive deadlines remain active");
    assert_eq!(deadline.at(), packet_at + Duration::from_mins(1));
    assert_eq!(
        deadline.timeout(),
        LivenessTimeout::StateProgress {
            state: ConnectionState::Play,
        },
    );
}

#[tokio::test(start_paused = true)]
async fn keep_alive_received_at_the_deadline_is_timed_out() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_secs(15));
    tracker
        .keep_alive_sent(17, start)
        .expect("request is outstanding");
    advance(Duration::from_secs(15)).await;

    let error = tracker
        .keep_alive_received(17, Instant::now())
        .expect_err("the exact deadline is expired");
    assert_eq!(
        error,
        KeepAliveError::TimedOut {
            timeout: LivenessTimeout::KeepAlive { id: 17 },
        },
    );
    assert_eq!(
        error.disconnect_reason(),
        Some(DisconnectReason::KeepAliveTimeout),
    );
    assert!(tracker.is_closed());
}

#[tokio::test(start_paused = true)]
async fn keep_alive_cannot_clear_an_expired_partial_frame_deadline() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_secs(15));
    tracker
        .keep_alive_sent(23, start)
        .expect("request is outstanding");
    tracker
        .partial_frame_observed(start)
        .expect("partial response starts the shorter frame deadline");
    advance(Duration::from_secs(10)).await;

    let error = tracker
        .keep_alive_received(23, Instant::now())
        .expect_err("a response cannot revive an expired partial frame");
    assert_eq!(
        error,
        KeepAliveError::TimedOut {
            timeout: LivenessTimeout::FrameCompletion {
                state: ConnectionState::Play,
            },
        },
    );
    assert_eq!(
        error.disconnect_reason(),
        Some(DisconnectReason::ProgressTimeout),
    );
    assert!(tracker.is_closed());
}

#[tokio::test(start_paused = true)]
async fn second_keep_alive_is_rejected_while_one_is_outstanding() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_secs(15));
    tracker
        .keep_alive_sent(3, start)
        .expect("first request is outstanding");
    assert_eq!(
        tracker
            .keep_alive_sent(4, start)
            .expect_err("only one id may be outstanding"),
        KeepAliveError::AlreadyOutstanding { id: 3 },
    );
    assert_eq!(tracker.outstanding_keep_alive(), Some((3, start)));
    assert!(
        !tracker.is_closed(),
        "caller misuse does not blame the peer"
    );
}

#[tokio::test(start_paused = true)]
async fn timeout_is_cancelled_when_the_connection_closes() {
    let start = Instant::now();
    let mut tracker = play_tracker(start, Duration::from_secs(15));
    tracker
        .keep_alive_sent(99, start)
        .expect("request is outstanding");
    assert!(tracker.next_deadline().is_some());

    tracker.close();
    assert!(tracker.is_closed());
    assert_eq!(tracker.next_deadline(), None);
    advance(Duration::from_mins(1)).await;
    assert_eq!(tracker.poll_timeout(Instant::now()), None);
    assert_eq!(
        tracker
            .keep_alive_received(99, Instant::now())
            .expect_err("closed trackers accept no late packets"),
        KeepAliveError::Closed,
    );
}
