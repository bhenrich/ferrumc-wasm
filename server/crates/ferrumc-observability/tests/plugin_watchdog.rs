use std::time::Duration;

use ferrumc_observability::{PluginWatchdogConfig, WatchdogHardAction};

#[test]
fn plugin_watchdog_defaults_are_report_only() {
    let config = PluginWatchdogConfig::default();

    assert_eq!(config.soft_callback(), Duration::from_millis(50));
    assert_eq!(config.unhealthy_callback(), Duration::from_secs(1));
    assert_eq!(config.hard_callback(), Duration::from_secs(10));
    assert_eq!(config.hard_action(), WatchdogHardAction::ReportOnly);
}
