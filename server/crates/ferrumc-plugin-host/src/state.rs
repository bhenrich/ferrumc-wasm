//! Per-plugin lifecycle state and accumulated statistics.

/// Why a plugin was disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableReason {
    /// The plugin panicked during a host call.
    Panicked,
    /// The plugin exceeded its per-call time budget (with overrun-disabling on).
    BudgetExceeded,
    /// The plugin returned an error from [`on_enable`](ferrumc_plugin_api::Plugin::on_enable).
    EnableFailed,
    /// The plugin was disabled by an explicit host request.
    Manual,
}

/// The lifecycle state of a registered plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// Registered but not yet enabled.
    Registered,
    /// Enabled and receiving events.
    Enabled,
    /// Disabled for the given reason; it will not be called again unless
    /// re-enabled.
    Disabled(DisableReason),
}

impl PluginState {
    /// Returns whether the plugin is currently enabled.
    pub const fn is_enabled(self) -> bool {
        matches!(self, PluginState::Enabled)
    }

    /// Returns the disable reason, if the plugin is disabled.
    pub const fn disable_reason(self) -> Option<DisableReason> {
        match self {
            PluginState::Disabled(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Counters accumulated about a plugin over its lifetime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginStats {
    pub(crate) panics: u32,
    pub(crate) budget_overruns: u32,
    pub(crate) allow: u64,
    pub(crate) deny: u64,
    pub(crate) replace: u64,
}

impl PluginStats {
    /// Returns how many times the plugin has panicked.
    pub const fn panics(self) -> u32 {
        self.panics
    }

    /// Returns how many times the plugin has exceeded its call budget.
    pub const fn budget_overruns(self) -> u32 {
        self.budget_overruns
    }

    /// Returns how many block-edit decisions the plugin let through (an
    /// `Allow`, an `EmitIntents`, or any future no-veto decision — none of which
    /// block the edit).
    pub const fn allow(self) -> u64 {
        self.allow
    }

    /// Returns how many block edits the plugin vetoed (`Deny`).
    pub const fn deny(self) -> u64 {
        self.deny
    }

    /// Returns how many block edits the plugin rewrote (`Replace`).
    pub const fn replace(self) -> u64 {
        self.replace
    }
}
