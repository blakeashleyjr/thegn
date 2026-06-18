//! The `dashboard` app tab — hosted inside superzej.
//!
//! Uses superzej-dashboard's DashboardUi.

use superzej_dashboard::DashboardUi;
use sz_kit::{AppTile, ChangeHook, Theme};

pub async fn build(
    rt: tokio::runtime::Handle,
    on_change: ChangeHook,
    theme: Theme,
) -> Box<dyn AppTile> {
    let interval_secs = 4; // Or read from config
    Box::new(DashboardUi::new(rt, Some(on_change), theme, interval_secs))
}
