//! ABI major/minor compatibility policy.

use core::fmt;

/// The current ABI major version.
pub const ABI_MAJOR: u16 = 1;

/// The current ABI minor version.
pub const ABI_MINOR: u16 = 0;

/// The ABI version implemented by this crate.
pub const CURRENT_ABI: AbiVersion = AbiVersion::new(ABI_MAJOR, ABI_MINOR);

/// A plugin ABI major/minor pair.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AbiVersion {
    major: u16,
    minor: u16,
}

impl AbiVersion {
    /// Creates a version from its major and minor components.
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Returns the major component.
    pub const fn major(self) -> u16 {
        self.major
    }

    /// Returns the minor component.
    pub const fn minor(self) -> u16 {
        self.minor
    }
}

impl fmt::Display for AbiVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// Host-side compatibility policy for one ABI major/minor pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbiVersionPolicy {
    host: AbiVersion,
}

impl AbiVersionPolicy {
    /// Creates a policy for `host`.
    pub const fn new(host: AbiVersion) -> Self {
        Self { host }
    }

    /// Returns the host version enforced by this policy.
    pub const fn host(self) -> AbiVersion {
        self.host
    }

    /// Negotiates `plugin` against this host.
    ///
    /// Major compatibility is checked first so a mismatch can be rejected
    /// before plugin initialization. For the same major, this host accepts its
    /// current minor and every earlier minor, and rejects later minors.
    pub const fn negotiate(self, plugin: AbiVersion) -> Result<AbiVersion, AbiVersionError> {
        if plugin.major != self.host.major {
            return Err(AbiVersionError::MajorMismatch {
                host: self.host,
                plugin,
            });
        }
        if plugin.minor > self.host.minor {
            return Err(AbiVersionError::MinorTooNew {
                host: self.host,
                plugin,
            });
        }
        Ok(plugin)
    }
}

/// Negotiates a plugin version against [`CURRENT_ABI`].
pub const fn negotiate_current(plugin: AbiVersion) -> Result<AbiVersion, AbiVersionError> {
    AbiVersionPolicy::new(CURRENT_ABI).negotiate(plugin)
}

/// A plugin ABI version is incompatible with the host policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbiVersionError {
    /// The plugin and host have different ABI major versions.
    MajorMismatch {
        /// The host ABI version.
        host: AbiVersion,
        /// The plugin ABI version.
        plugin: AbiVersion,
    },
    /// The plugin uses a newer minor than this host implements.
    MinorTooNew {
        /// The host ABI version.
        host: AbiVersion,
        /// The plugin ABI version.
        plugin: AbiVersion,
    },
}

impl fmt::Display for AbiVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MajorMismatch { host, plugin } => write!(
                formatter,
                "plugin ABI major {} does not match host major {}",
                plugin.major, host.major
            ),
            Self::MinorTooNew { host, plugin } => write!(
                formatter,
                "plugin ABI minor {} is newer than host minor {} for major {}",
                plugin.minor, host.minor, host.major
            ),
        }
    }
}

impl std::error::Error for AbiVersionError {}

#[cfg(test)]
mod tests {
    use super::{negotiate_current, AbiVersion, AbiVersionError, AbiVersionPolicy, CURRENT_ABI};

    #[test]
    fn current_policy_accepts_current_and_rejects_incompatible_versions() {
        assert_eq!(negotiate_current(CURRENT_ABI), Ok(CURRENT_ABI));
        assert!(matches!(
            negotiate_current(AbiVersion::new(CURRENT_ABI.major().wrapping_add(1), 0)),
            Err(AbiVersionError::MajorMismatch { .. })
        ));
        assert!(matches!(
            negotiate_current(AbiVersion::new(
                CURRENT_ABI.major(),
                CURRENT_ABI.minor().wrapping_add(1)
            )),
            Err(AbiVersionError::MinorTooNew { .. })
        ));
    }

    #[test]
    fn version_accept_reject_matrix_covers_every_previous_minor() {
        let host = AbiVersion::new(1, 3);
        let policy = AbiVersionPolicy::new(host);
        let cases = [
            (AbiVersion::new(1, 0), true),
            (AbiVersion::new(1, 1), true),
            (AbiVersion::new(1, 2), true),
            (AbiVersion::new(1, 3), true),
            (AbiVersion::new(1, 4), false),
            (AbiVersion::new(0, 3), false),
            (AbiVersion::new(2, 0), false),
            (AbiVersion::new(u16::MAX, u16::MAX), false),
        ];

        for (plugin, accepted) in cases {
            assert_eq!(
                policy.negotiate(plugin).is_ok(),
                accepted,
                "host {host} compatibility for plugin {plugin}"
            );
        }
    }

    #[test]
    fn major_mismatch_is_classified_before_minor_mismatch() {
        let host = AbiVersion::new(1, 2);
        let plugin = AbiVersion::new(2, u16::MAX);
        assert_eq!(
            AbiVersionPolicy::new(host).negotiate(plugin),
            Err(AbiVersionError::MajorMismatch { host, plugin })
        );
    }

    #[test]
    fn zero_minor_host_accepts_only_zero_minor_of_same_major() {
        let policy = AbiVersionPolicy::new(AbiVersion::new(1, 0));
        assert!(policy.negotiate(AbiVersion::new(1, 0)).is_ok());
        assert!(matches!(
            policy.negotiate(AbiVersion::new(1, 1)),
            Err(AbiVersionError::MinorTooNew { .. })
        ));
    }
}
