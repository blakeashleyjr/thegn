//! Monitor preferences: the per-tab toggles, and their round trip through the
//! `ui_state` key/value table.
//!
//! Keys are namespaced `<tab-slug>.<field>` under the `monitor` scope, and every
//! slug comes from a `key()` method rather than a display label — so renaming a
//! tab, a window, or a scale in the UI can never orphan what a user saved.
//! Unknown or malformed values fall back to the default instead of erroring: a
//! preference file is a convenience, not a contract.

use super::{GraphStyle, MonitorTab, ProcSort, TabPrefs};
use crate::telemetry::{ScaleMode, Window};

/// The `ui_state` scope these live under.
pub(crate) const SCOPE: &str = "monitor";

/// Every tab's toggles plus the cross-tab ones.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorPrefs {
    per_tab: [TabPrefs; MonitorTab::ALL.len()],
    pub proc_sort: ProcSort,
    pub proc_desc: bool,
    /// The tab the monitor reopens on.
    pub last_tab: MonitorTab,
}

impl Default for MonitorPrefs {
    fn default() -> Self {
        MonitorPrefs {
            per_tab: [TabPrefs::default(); MonitorTab::ALL.len()],
            proc_sort: ProcSort::default(),
            // Descending: the point of the Processes tab is the heaviest
            // process, not the lightest.
            proc_desc: true,
            last_tab: MonitorTab::default(),
        }
    }
}

impl MonitorPrefs {
    /// Defaults seeded from config. `[monitor]` supplies the starting window,
    /// style and scale for every tab; per-tab overrides layer on top from the
    /// DB.
    pub fn from_config(cfg: &thegn_core::config::MonitorConfig) -> MonitorPrefs {
        let base = TabPrefs {
            style: GraphStyle::from_key(&cfg.default_style).unwrap_or_default(),
            scale: ScaleMode::from_key(&cfg.default_scale).unwrap_or_default(),
            window: Window::from_key(&cfg.default_window).unwrap_or_default(),
        };
        MonitorPrefs {
            per_tab: [base; MonitorTab::ALL.len()],
            ..Default::default()
        }
    }

    pub fn tab(&self, t: MonitorTab) -> TabPrefs {
        self.per_tab[t.index()]
    }

    pub fn tab_mut(&mut self, t: MonitorTab) -> &mut TabPrefs {
        &mut self.per_tab[t.index()]
    }

    /// Fold one persisted `(key, value)` pair in. Unrecognized keys are ignored
    /// so an older or newer thegn's leftovers can't break startup.
    pub fn apply(&mut self, key: &str, value: &str) {
        match key {
            "proc_sort" => {
                if let Some(s) = ProcSort::from_key(value) {
                    self.proc_sort = s;
                }
            }
            "proc_desc" => self.proc_desc = value == "1",
            "last_tab" => {
                if let Some(t) = MonitorTab::from_key(value) {
                    self.last_tab = t;
                }
            }
            _ => {
                let Some((tab, field)) = key.split_once('.') else {
                    return;
                };
                let Some(tab) = MonitorTab::from_key(tab) else {
                    return;
                };
                let p = self.tab_mut(tab);
                match field {
                    "style" => {
                        if let Some(v) = GraphStyle::from_key(value) {
                            p.style = v;
                        }
                    }
                    "scale" => {
                        if let Some(v) = ScaleMode::from_key(value) {
                            p.scale = v;
                        }
                    }
                    "window" => {
                        if let Some(v) = Window::from_key(value) {
                            p.window = v;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Every `(key, value)` pair to persist.
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut out = vec![
            ("proc_sort".into(), self.proc_sort.key().to_string()),
            (
                "proc_desc".into(),
                if self.proc_desc { "1" } else { "0" }.into(),
            ),
            ("last_tab".into(), self.last_tab.key().to_string()),
        ];
        for t in MonitorTab::ALL {
            let p = self.tab(t);
            out.push((format!("{}.style", t.key()), p.style.key().into()));
            out.push((format!("{}.scale", t.key()), p.scale.key().into()));
            out.push((format!("{}.window", t.key()), p.window.key().into()));
        }
        out
    }
}

/// Read the saved preferences, layered over the config defaults.
pub(crate) fn load(
    db: &thegn_core::db::Db,
    cfg: &thegn_core::config::MonitorConfig,
) -> MonitorPrefs {
    use thegn_core::store::WorkspaceStore;
    let mut prefs = MonitorPrefs::from_config(cfg);
    for (k, v) in db.ui_state_in_scope(SCOPE).unwrap_or_default() {
        prefs.apply(&k, &v);
    }
    prefs
}

/// Write the preferences back, off the event loop.
///
/// Best-effort: `ui_state` is a convenience cache, and a failed write must never
/// interrupt the user's keystroke.
pub(crate) fn persist(prefs: &MonitorPrefs) {
    let entries = prefs.entries();
    crate::db_task::persist(move |db| {
        use thegn_core::store::WorkspaceStore;
        for (k, v) in &entries {
            let _ = db.set_ui_state(SCOPE, k, v);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip_through_their_entries() {
        let mut p = MonitorPrefs::default();
        p.tab_mut(MonitorTab::Cpu).style = GraphStyle::Line;
        p.tab_mut(MonitorTab::Cpu).window = Window::Hour;
        p.tab_mut(MonitorTab::Network).scale = ScaleMode::Log;
        p.proc_sort = ProcSort::Rss;
        p.proc_desc = false;
        p.last_tab = MonitorTab::Disk;

        let mut back = MonitorPrefs::default();
        for (k, v) in p.entries() {
            back.apply(&k, &v);
        }
        assert_eq!(back, p);
    }

    #[test]
    fn a_toggle_is_scoped_to_its_own_tab() {
        // Changing CPU's style must not silently restyle every other tab.
        let mut p = MonitorPrefs::default();
        p.tab_mut(MonitorTab::Cpu).style = GraphStyle::Spark;
        assert_eq!(p.tab(MonitorTab::Cpu).style, GraphStyle::Spark);
        assert_eq!(p.tab(MonitorTab::Memory).style, GraphStyle::Area);
    }

    #[test]
    fn unknown_and_malformed_keys_are_ignored() {
        // Leftovers from a different thegn version must not break startup or
        // corrupt a valid preference.
        let mut p = MonitorPrefs::default();
        let before = p.clone();
        for (k, v) in [
            ("nonsense", "1"),
            ("cpu.nonsense", "1"),
            ("nosuchtab.style", "line"),
            ("cpu.style", "not-a-style"),
            ("cpu.window", "17 fortnights"),
            ("proc_sort", "by-vibes"),
            ("last_tab", "atlantis"),
            ("", ""),
        ] {
            p.apply(k, v);
        }
        assert_eq!(p, before);
    }

    #[test]
    fn config_defaults_seed_every_tab() {
        let cfg = thegn_core::config::MonitorConfig {
            default_style: "line".into(),
            default_scale: "log".into(),
            default_window: "10m".into(),
            ..Default::default()
        };
        let p = MonitorPrefs::from_config(&cfg);
        for t in MonitorTab::ALL {
            assert_eq!(p.tab(t).style, GraphStyle::Line, "{t:?}");
            assert_eq!(p.tab(t).scale, ScaleMode::Log, "{t:?}");
            assert_eq!(p.tab(t).window, Window::Long, "{t:?}");
        }
        // Descending by default — the heaviest process is the point.
        assert!(p.proc_desc);
        // A nonsense config value falls back rather than refusing to start.
        let bad = thegn_core::config::MonitorConfig {
            default_style: "hologram".into(),
            ..Default::default()
        };
        assert_eq!(
            MonitorPrefs::from_config(&bad).tab(MonitorTab::Cpu).style,
            GraphStyle::Area
        );
    }
}
