//! The app-tab registry: one entry per embeddable [`AppTile`], so adding a
//! tile is adding a row here — `AppHost::from_config` and `start_slot_tile`
//! iterate it instead of carrying per-app arms.
//!
//! A static slice rather than `inventory`/`linkme`: every tile is in-tree,
//! link-section collection is the one mechanism that behaves differently on
//! the not-yet-green macOS/Windows legs, and a slice is greppable. Out-of-tree
//! tiles (a plugin-supplied grid view) will be one dynamic entry appended at
//! load time, not a second registry.

use tg_kit::AppTile;
use thegn_core::config::Config;

/// One registrable app tab.
pub struct AppBuilder {
    /// Stable tab id (`[apps] tab_order` / `default_tab` name it).
    pub id: &'static str,
    /// Chip label.
    pub label: &'static str,
    /// Whether this config opts the tab in. Off by default keeps the AI-free
    /// shell a single `work` tab.
    pub enabled: fn(&Config) -> bool,
    /// Construct the tile. Runs on the loop thread on first focus; anything
    /// slow must go off-thread and report back through the [`ChangeHook`].
    ///
    /// [`ChangeHook`]: tg_kit::ChangeHook
    pub build: fn(tg_kit::ChangeHook, &Config, tokio::runtime::Handle) -> Box<dyn AppTile>,
}

pub static APP_BUILDERS: &[AppBuilder] = &[AppBuilder {
    id: "observe",
    label: "Observe",
    enabled: |cfg| cfg.observe.enabled,
    build: |hook, cfg, rt| super::build_observe_tile(hook, &cfg.observe, rt),
}];

/// The registered builder for a tab id.
pub fn builder(id: &str) -> Option<&'static AppBuilder> {
    APP_BUILDERS.iter().find(|b| b.id == id)
}

/// Builders this config enables, in registry order.
pub fn enabled(cfg: &Config) -> impl Iterator<Item = &'static AppBuilder> {
    APP_BUILDERS.iter().filter(move |b| (b.enabled)(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique_and_not_the_work_tab() {
        let mut seen = std::collections::HashSet::new();
        for b in APP_BUILDERS {
            assert!(seen.insert(b.id), "duplicate app id {}", b.id);
            assert_ne!(b.id, "work", "`work` is the built-in IDE tab, not an app");
            assert!(!b.label.is_empty());
            assert_eq!(builder(b.id).map(|x| x.id), Some(b.id));
        }
        assert!(builder("nope").is_none());
    }

    #[test]
    fn enabled_builders_appear_in_the_host_tab_order() {
        let mut cfg = Config::default();
        assert_eq!(enabled(&cfg).count(), 0, "apps are opt-in");
        cfg.observe.enabled = true;
        let ids: Vec<&str> = enabled(&cfg).map(|b| b.id).collect();
        assert_eq!(ids, ["observe"]);
        let host = super::super::AppHost::from_config(&cfg);
        let labels = host.tab_labels();
        assert!(
            labels.iter().any(|l| l.contains("Observe")),
            "enabled app missing from tab order: {labels:?}"
        );
        let off = super::super::AppHost::from_config(&Config::default());
        assert!(!off.tab_labels().iter().any(|l| l.contains("Observe")));
    }
}
