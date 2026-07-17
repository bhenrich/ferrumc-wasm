//! Typed shard-input delivery policy and recoverable rejection errors.

use ferrumc_sim::GameInput;

use crate::SessionError;

/// Backpressure policy for a simulation input.
///
/// The policy tells the app what an explicit queue rejection permits. It never
/// turns a rejected input into an implicit success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryPolicy {
    /// State-changing input that must be accepted, explicitly rejected, retried,
    /// or terminate the overloaded session.
    Authoritative,
    /// Latest-state input that may replace an older pending input for the same
    /// player, but may not disappear without an explicit coalescing decision.
    Coalescible,
    /// Presentation-only input that a caller may deliberately shed.
    ///
    /// No current [`GameInput`] is best-effort; the variant makes the policy
    /// exhaustive for future non-authoritative simulation notifications.
    BestEffort,
}

/// Bounded shard-input lane selected for an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryLane {
    /// Ordinary state traffic, stopped before the queue consumes its reserved
    /// lifecycle capacity.
    Data,
    /// Lifecycle and client-healing traffic allowed to consume the reserved tail
    /// of the bounded shard queue.
    Control,
}

/// Complete delivery classification for one [`GameInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputDeliveryPolicy {
    policy: DeliveryPolicy,
    lane: DeliveryLane,
}

impl InputDeliveryPolicy {
    /// Classifies a simulation input by loss policy and bounded queue lane.
    ///
    /// All currently modelled state-changing inputs are authoritative except
    /// ordinary player movement, which is latest-wins/coalescible. Join, leave,
    /// and rejected-edit healing use the control lane. Future variants default
    /// to authoritative data traffic so adding a new input cannot silently make
    /// it droppable.
    #[must_use]
    pub fn for_input(input: &GameInput) -> Self {
        match input {
            GameInput::PlayerJoin { .. }
            | GameInput::PlayerLeave { .. }
            | GameInput::RejectBlockEdit { .. } => {
                Self::new(DeliveryPolicy::Authoritative, DeliveryLane::Control)
            }
            GameInput::PlayerMove { .. } => {
                Self::new(DeliveryPolicy::Coalescible, DeliveryLane::Data)
            }
            GameInput::BlockBreak { .. }
            | GameInput::BlockPlace { .. }
            | GameInput::SetBlockExact { .. }
            | GameInput::SetGameMode { .. }
            | GameInput::RegionEdit { .. }
            | GameInput::RegionUndo { .. }
            | GameInput::UpdateSign { .. } => {
                Self::new(DeliveryPolicy::Authoritative, DeliveryLane::Data)
            }
            _ => Self::new(DeliveryPolicy::Authoritative, DeliveryLane::Data),
        }
    }

    /// Returns the loss/backpressure policy.
    #[must_use]
    pub const fn policy(self) -> DeliveryPolicy {
        self.policy
    }

    /// Returns the bounded shard-input lane.
    #[must_use]
    pub const fn lane(self) -> DeliveryLane {
        self.lane
    }

    /// Builds an authoritative policy for a caller whose operation is stricter
    /// than the raw input variant, such as a server-commanded teleport carried by
    /// [`GameInput::PlayerMove`].
    pub(crate) const fn authoritative(lane: DeliveryLane) -> Self {
        Self::new(DeliveryPolicy::Authoritative, lane)
    }

    const fn new(policy: DeliveryPolicy, lane: DeliveryLane) -> Self {
        Self { policy, lane }
    }
}

/// A rejected shard input together with its exact ownership and delivery policy.
///
/// Callers can inspect the classified [`SessionError`], coalesce or retry the
/// returned input when its policy permits, or terminate the owning session. The
/// router never destroys the last copy merely to report backpressure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{error}")]
pub struct InputDeliveryError {
    input: Box<GameInput>,
    policy: InputDeliveryPolicy,
    error: SessionError,
}

impl InputDeliveryError {
    pub(crate) fn new(input: GameInput, policy: InputDeliveryPolicy, error: SessionError) -> Self {
        Self {
            input: Box::new(input),
            policy,
            error,
        }
    }

    /// Returns the rejected input without consuming the error.
    #[must_use]
    pub fn input(&self) -> &GameInput {
        &self.input
    }

    /// Returns the rejected input's delivery policy.
    #[must_use]
    pub const fn policy(&self) -> InputDeliveryPolicy {
        self.policy
    }

    /// Returns the classified session error that prevented delivery.
    #[must_use]
    pub const fn error(&self) -> &SessionError {
        &self.error
    }

    /// Consumes the rejection and returns the classified session error.
    #[must_use]
    pub fn into_error(self) -> SessionError {
        self.error
    }

    /// Consumes the rejection and returns the input, its policy, and its error.
    #[must_use]
    pub fn into_parts(self) -> (GameInput, InputDeliveryPolicy, SessionError) {
        (*self.input, self.policy, self.error)
    }
}
