//! Dynamic packaging wrapper around the shared testhost fixture logic.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod plugin;

pub use plugin::{
    TesthostFixturePlugin, CAPACITY_TRIGGER_STATE, DECISION_ALLOW_STATE, DECISION_DENY_STATE,
    DIAGNOSTIC_ONLY_POS, FIXTURE_HANDLER_RAW, FIXTURE_TIMER_RAW,
};

#[cfg(feature = "dynamic")]
ferrumc_plugin_sdk_dynamic::export_plugin!(crate::TesthostFixturePlugin);
