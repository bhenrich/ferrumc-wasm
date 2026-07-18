//! Classified testhost configuration and replay failures.

use core::fmt;

use ferrumc_plugin_sdk::{Capability, EventKind, Tick};

use super::PluginRun;

/// Lifecycle location of a failed callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PluginCallbackPhase {
    /// Plugin initialization.
    Load,
    /// One scheduled event.
    Event {
        /// Zero-based index in the replay log.
        index: usize,
        /// Deterministic callback tick.
        tick: Tick,
        /// Event discriminant.
        kind: EventKind,
    },
    /// Plugin shutdown.
    Unload,
}

/// Packaging-neutral classification of a replay failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PluginFailureKind {
    /// Plugin code returned a cooperative error.
    Cooperative,
    /// Plugin code reported or triggered an unwinding panic.
    Panicked,
    /// A prior panic made the instance unusable.
    Poisoned,
    /// A required effective capability was unavailable.
    CapabilityDenied(Capability),
    /// An adapter did not understand a future event.
    UnsupportedEvent,
    /// The callback's bounded semantic-effect stage was full.
    BufferFull,
    /// A passive notification was not subscribed during load.
    EventNotSubscribed(EventKind),
    /// A decision callback completed without exactly one decision.
    MissingDecision,
    /// A callback attempted to record more than one decision.
    DuplicateDecision,
    /// A callback recorded a decision incompatible with its event route.
    WrongDecision,
    /// Adding a timer delay overflowed the deterministic tick domain.
    TimerOverflow,
    /// The native callback returned a non-success ABI status.
    AbiStatus(i32),
    /// ABI-system invocation failed before a callback status was available.
    AbiInvocation(String),
    /// A native request or command violated the semantic ABI v1 grammar.
    AbiProtocol(String),
    /// A future SDK value has no canonical semantic-v1 assignment.
    UnsupportedSemanticValue(&'static str),
}

impl fmt::Display for PluginFailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cooperative => formatter.write_str("plugin failed cooperatively"),
            Self::Panicked => formatter.write_str("plugin callback panicked"),
            Self::Poisoned => formatter.write_str("plugin instance is poisoned"),
            Self::CapabilityDenied(capability) => {
                write!(formatter, "required capability {capability} was denied")
            }
            Self::UnsupportedEvent => formatter.write_str("adapter does not support the event"),
            Self::BufferFull => formatter.write_str("bounded callback effect stage is full"),
            Self::EventNotSubscribed(kind) => {
                write!(formatter, "plugin did not subscribe to event {kind:?}")
            }
            Self::MissingDecision => {
                formatter.write_str("decision callback emitted no route-valid decision")
            }
            Self::DuplicateDecision => {
                formatter.write_str("decision callback emitted more than one decision")
            }
            Self::WrongDecision => {
                formatter.write_str("callback emitted a decision for the wrong event route")
            }
            Self::TimerOverflow => formatter.write_str("timer due tick overflowed"),
            Self::AbiStatus(status) => {
                write!(formatter, "native callback returned status {status}")
            }
            Self::AbiInvocation(reason) => {
                write!(formatter, "native callback invocation failed: {reason}")
            }
            Self::AbiProtocol(reason) => write!(formatter, "native ABI payload rejected: {reason}"),
            Self::UnsupportedSemanticValue(resource) => {
                write!(
                    formatter,
                    "{resource} has no canonical semantic-v1 encoding"
                )
            }
        }
    }
}

/// A failed callback plus the committed partial report.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginReplayFailure {
    phase: PluginCallbackPhase,
    kind: PluginFailureKind,
    report: PluginRun,
}

impl PluginReplayFailure {
    pub(crate) fn new(
        phase: PluginCallbackPhase,
        kind: PluginFailureKind,
        report: PluginRun,
    ) -> Self {
        Self {
            phase,
            kind,
            report,
        }
    }

    /// Returns where replay failed.
    pub const fn phase(&self) -> PluginCallbackPhase {
        self.phase
    }

    /// Returns the packaging-neutral failure class.
    pub const fn kind(&self) -> &PluginFailureKind {
        &self.kind
    }

    /// Returns committed state/effects and retained diagnostics up to failure.
    ///
    /// The failing callback's mutations and decision are absent because its
    /// fresh transactional stage was discarded.
    pub const fn report(&self) -> &PluginRun {
        &self.report
    }
}

impl fmt::Display for PluginReplayFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "plugin replay failed during {:?}: {}",
            self.phase, self.kind
        )
    }
}

impl std::error::Error for PluginReplayFailure {}

/// Deterministic plugin testhost configuration, load, or callback failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PluginTestHostError {
    /// Callback stage capacity was zero or above the hard ceiling.
    #[error("callback effect capacity {requested} is outside 1..={maximum}")]
    InvalidCapacity {
        /// Rejected capacity.
        requested: usize,
        /// Hard maximum.
        maximum: usize,
    },
    /// A seeded player position contained a non-finite component.
    #[error("seeded player position must contain only finite coordinates")]
    NonFinitePlayerPosition,
    /// A seeded storage key or value violated shared SDK bounds.
    #[error("seeded storage value is invalid: {0}")]
    InvalidStorage(String),
    /// A configured dynamic resource handle was zero or aliased another type.
    #[error("dynamic resource handles must be nonzero and distinct")]
    InvalidResourceHandles,
    /// A replay log exceeded the deterministic event-count ceiling.
    #[error("scheduled event count {len} exceeds the maximum of {maximum}")]
    TooManyScheduledEvents {
        /// Rejected event count.
        len: usize,
        /// Hard maximum.
        maximum: usize,
    },
    /// A future SDK event has no canonical semantic-v1 assignment.
    #[error("{resource} has no canonical semantic-v1 encoding")]
    UnsupportedSemanticValue {
        /// Unsupported semantic value class.
        resource: &'static str,
    },
    /// Scheduled ticks decreased.
    #[error(
        "scheduled event {index} tick {current} precedes prior tick {previous}; logs must be nondecreasing"
    )]
    DecreasingTick {
        /// Zero-based rejected event index.
        index: usize,
        /// Previous tick.
        previous: Tick,
        /// Rejected current tick.
        current: Tick,
    },
    /// The trusted native plugin library could not be opened or raw-validated.
    #[error(transparent)]
    DynamicLoad(#[from] ferrumc_plugin_abi_sys::LoadError),
    /// A lifecycle or event callback failed after replay state existed.
    #[error(transparent)]
    Replay(Box<PluginReplayFailure>),
}

impl PluginTestHostError {
    /// Returns a partial committed report for callback failures.
    pub const fn partial_report(&self) -> Option<&PluginRun> {
        match self {
            Self::Replay(failure) => Some(failure.report()),
            _ => None,
        }
    }
}

impl From<PluginReplayFailure> for PluginTestHostError {
    fn from(failure: PluginReplayFailure) -> Self {
        Self::Replay(Box::new(failure))
    }
}
