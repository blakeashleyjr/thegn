//! The `dashboard` app tab — hosted inside superzej.
//!
//! Uses superzej-dashboard's DashboardUi.

use superzej_dashboard::{DashboardOptions, DashboardUi};
use sz_kit::{AppTile, ChangeHook, Theme};

pub async fn build(
    rt: tokio::runtime::Handle,
    on_change: ChangeHook,
    theme: Theme,
    cfg: &superzej_core::config::DashboardConfig,
) -> Box<dyn AppTile> {
    let options = DashboardOptions {
        interval_secs: cfg.interval_secs,
        hacker_news: cfg.hacker_news,
        hacker_news_limit: cfg.hacker_news_limit,
    };
    Box::new(DashboardUi::new(rt, Some(on_change), theme, options))
}
