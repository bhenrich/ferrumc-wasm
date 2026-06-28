//! Per-connection serverbound packet-budget configuration: the token-bucket
//! sustained rate and burst that gate how fast a client may send play frames.
//!
//! [`PacketBudgetConfig`] is the *declarative* shape an operator writes in the
//! `[budget]` TOML table; it is pure data and performs no I/O. The networking
//! lane turns it into a live token bucket per connection. Call
//! [`validate`](PacketBudgetConfig::validate) at startup to reject a
//! misconfiguration (a non-positive rate would silently brick every login).

use serde::Deserialize;
use thiserror::Error;

/// Default sustained serverbound play-frame rate, in frames per second.
///
/// Mirrors `ferrumc_net`'s `DEFAULT_PLAY_FRAME_RATE` (the networking model's
/// 300 frames/sec budget). Duplicated as a literal here because the config crate
/// is a leaf and must not depend on the networking crate.
pub const DEFAULT_PLAY_FRAME_RATE: f64 = 300.0;

/// Default token-bucket burst capacity, in frames.
///
/// Mirrors `ferrumc_net`'s `DEFAULT_PLAY_FRAME_BURST`: twice the sustained rate,
/// so a brief spike is absorbed while a sustained flood drains the bucket.
pub const DEFAULT_PLAY_FRAME_BURST: f64 = 600.0;

/// Declarative serverbound packet-budget configuration, deserialized from the
/// `[budget]` TOML table.
///
/// Both fields default (300 sustained / 600 burst), so an omitted `[budget]`
/// table — or any omitted field within it — yields the safe defaults. The values
/// are plain data; [`validate`](Self::validate) rejects a degenerate
/// configuration before it reaches the networking lane.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PacketBudgetConfig {
    /// Sustained serverbound frame rate, in frames per second: the token-bucket
    /// refill rate. Must be finite and strictly positive.
    pub sustained_rate: f64,
    /// Token-bucket burst capacity, in frames: the most a client may send in a
    /// spike before the sustained rate governs. Must be finite, strictly
    /// positive, and at least [`sustained_rate`](Self::sustained_rate).
    pub burst: f64,
}

impl Default for PacketBudgetConfig {
    fn default() -> Self {
        Self {
            sustained_rate: DEFAULT_PLAY_FRAME_RATE,
            burst: DEFAULT_PLAY_FRAME_BURST,
        }
    }
}

impl PacketBudgetConfig {
    /// Validates that both knobs are finite and strictly positive and that the
    /// burst is at least the sustained rate.
    ///
    /// A zero/negative or non-finite rate would degrade the bucket to
    /// "always over budget", silently disconnecting every client, so it is
    /// rejected at startup rather than discovered in production.
    ///
    /// # Errors
    ///
    /// Returns a [`PacketBudgetConfigError`] describing the first invalid field.
    pub fn validate(&self) -> Result<(), PacketBudgetConfigError> {
        if !self.sustained_rate.is_finite() || self.sustained_rate <= 0.0 {
            return Err(PacketBudgetConfigError::NonPositiveRate {
                value: self.sustained_rate,
            });
        }
        if !self.burst.is_finite() || self.burst <= 0.0 {
            return Err(PacketBudgetConfigError::NonPositiveBurst { value: self.burst });
        }
        if self.burst < self.sustained_rate {
            return Err(PacketBudgetConfigError::BurstBelowRate {
                burst: self.burst,
                rate: self.sustained_rate,
            });
        }
        Ok(())
    }
}

/// Why a [`PacketBudgetConfig`] failed [`validate`](PacketBudgetConfig::validate).
#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum PacketBudgetConfigError {
    /// `sustained_rate` was non-finite or not strictly positive.
    #[error("packet budget sustained_rate must be finite and greater than zero, got {value}")]
    NonPositiveRate {
        /// The offending configured rate.
        value: f64,
    },
    /// `burst` was non-finite or not strictly positive.
    #[error("packet budget burst must be finite and greater than zero, got {value}")]
    NonPositiveBurst {
        /// The offending configured burst.
        value: f64,
    },
    /// `burst` was below `sustained_rate`, leaving the bucket unable to hold even
    /// one second of the sustained rate.
    #[error(
        "packet budget burst ({burst}) must be greater than or equal to sustained_rate ({rate})"
    )]
    BurstBelowRate {
        /// The configured burst.
        burst: f64,
        /// The configured sustained rate it fell below.
        rate: f64,
    },
}

#[cfg(test)]
mod tests {
    // The configured values under test are exact, representable literals, so exact
    // float comparison is intentional here.
    #![allow(clippy::float_cmp)]

    use super::*;

    #[test]
    fn default_is_three_hundred_over_six_hundred() {
        let config = PacketBudgetConfig::default();
        assert_eq!(config.sustained_rate, 300.0);
        assert_eq!(config.burst, 600.0);
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn an_omitted_table_keeps_the_defaults() {
        let parsed: PacketBudgetConfig = toml::from_str("").expect("empty table is valid");
        assert_eq!(parsed, PacketBudgetConfig::default());
    }

    #[test]
    fn fields_parse_from_toml() {
        let parsed: PacketBudgetConfig =
            toml::from_str("sustained_rate = 120.0\nburst = 240.0").expect("valid budget table");
        assert_eq!(parsed.sustained_rate, 120.0);
        assert_eq!(parsed.burst, 240.0);
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(toml::from_str::<PacketBudgetConfig>("bogus = 1").is_err());
    }

    #[test]
    fn a_non_positive_rate_is_rejected() {
        let config = PacketBudgetConfig {
            sustained_rate: 0.0,
            burst: 600.0,
        };
        assert_eq!(
            config.validate(),
            Err(PacketBudgetConfigError::NonPositiveRate { value: 0.0 })
        );
    }

    #[test]
    fn a_non_finite_burst_is_rejected() {
        let config = PacketBudgetConfig {
            sustained_rate: 300.0,
            burst: f64::INFINITY,
        };
        assert_eq!(
            config.validate(),
            Err(PacketBudgetConfigError::NonPositiveBurst {
                value: f64::INFINITY
            })
        );
    }

    #[test]
    fn a_burst_below_the_rate_is_rejected() {
        let config = PacketBudgetConfig {
            sustained_rate: 300.0,
            burst: 100.0,
        };
        assert_eq!(
            config.validate(),
            Err(PacketBudgetConfigError::BurstBelowRate {
                burst: 100.0,
                rate: 300.0,
            })
        );
    }
}
