//! The per-call timing budget used to flag slow plugins.

use std::time::Duration;

/// The default per-call budget: a fraction of a 50 ms (20 TPS) tick.
const DEFAULT_BUDGET: Duration = Duration::from_millis(5);

/// A wall-clock budget for a single plugin call.
///
/// The host compares successful compiled enable/event/decision hooks,
/// successful trusted-native initialization, and returning trusted-native
/// event/decision calls against this budget. Metadata and shutdown calls are
/// not timed. The comparison ([`CallBudget::evaluate`]) is a pure function so
/// it can be tested deterministically without sleeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallBudget {
    limit: Duration,
}

impl CallBudget {
    /// Creates a budget allowing each call up to `limit`.
    pub const fn new(limit: Duration) -> Self {
        Self { limit }
    }

    /// Returns the configured limit.
    pub const fn limit(self) -> Duration {
        self.limit
    }

    /// Returns whether `elapsed` exceeded the budget.
    pub fn is_exceeded(self, elapsed: Duration) -> bool {
        elapsed > self.limit
    }

    /// Classifies `elapsed` against the budget.
    pub fn evaluate(self, elapsed: Duration) -> BudgetOutcome {
        if elapsed > self.limit {
            BudgetOutcome::Exceeded {
                elapsed,
                limit: self.limit,
            }
        } else {
            BudgetOutcome::WithinBudget { elapsed }
        }
    }
}

impl Default for CallBudget {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET)
    }
}

/// The result of comparing a measured duration against a [`CallBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetOutcome {
    /// The call finished within budget.
    WithinBudget {
        /// How long the call took.
        elapsed: Duration,
    },
    /// The call exceeded its budget.
    Exceeded {
        /// How long the call took.
        elapsed: Duration,
        /// The budget it exceeded.
        limit: Duration,
    },
}

impl BudgetOutcome {
    /// Returns whether the call exceeded its budget.
    pub const fn is_exceeded(self) -> bool {
        matches!(self, BudgetOutcome::Exceeded { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_budget_is_not_exceeded() {
        let budget = CallBudget::new(Duration::from_millis(10));
        assert!(!budget.is_exceeded(Duration::from_millis(9)));
        assert!(!budget.is_exceeded(Duration::from_millis(10)));
        assert_eq!(
            budget.evaluate(Duration::from_millis(3)),
            BudgetOutcome::WithinBudget {
                elapsed: Duration::from_millis(3)
            }
        );
    }

    #[test]
    fn over_budget_is_exceeded() {
        let budget = CallBudget::new(Duration::from_millis(1));
        assert!(budget.is_exceeded(Duration::from_millis(2)));
        let outcome = budget.evaluate(Duration::from_millis(5));
        assert!(outcome.is_exceeded());
        assert_eq!(
            outcome,
            BudgetOutcome::Exceeded {
                elapsed: Duration::from_millis(5),
                limit: Duration::from_millis(1),
            }
        );
    }

    #[test]
    fn zero_budget_flags_any_nonzero_call() {
        let budget = CallBudget::new(Duration::ZERO);
        assert!(budget.is_exceeded(Duration::from_nanos(1)));
        assert!(!budget.is_exceeded(Duration::ZERO));
    }

    #[test]
    fn default_is_five_milliseconds() {
        assert_eq!(CallBudget::default().limit(), Duration::from_millis(5));
    }
}
