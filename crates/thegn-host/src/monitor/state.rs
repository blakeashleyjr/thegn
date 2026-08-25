//! Monitor preferences: the per-tab toggles, and their round trip through the
//! `ui_state` key/value table.
//!
//! Keys are namespaced `<tab-slug>.<field>` under the `monitor` scope, and every
//! slug comes from a `key()` method rather than a display label — so renaming a
//! tab, a window, or a scale in the UI can never orphan what a user saved.
//! Unknown or malformed values fall back to the default instead of erroring: a
//! preference file is a convenience, not a contract.

use super::{GraphStyle, MonitorTab, ProcSort, TabPrefs};
use crate::telemetry::{ScaleMode, Window, WindowLadder};

/// The `ui_state` scope these live under.
pub(crate) const SCOPE: &str = "monitor";

/// Every tab's toggles plus the cross-tab ones.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorPrefs {
    per_tab: [TabPrefs; MonitorTab::ALL.len()],
    /// The `[`/`]` rungs, from `[monitor] window_ladder`.
    ///
    /// Config-derived, so deliberately **absent from [`Self::entries`]**:
    /// `load` layers the DB over config, so persisting this would let a stale
    /// row outvote a later `config.toml` edit — the edit would appear to do
    /// nothing.
    ladder: WindowLadder,
    pub proc_sort: ProcSort,
    pub proc_desc: bool,
    /// Group the Processes list by the sampled parent chain.
    pub proc_tree: bool,
    /// The tab the monitor reopens on.
    pub last_tab: MonitorTab,
}

impl Default for MonitorPrefs {
    fn default() -> Self {
        MonitorPrefs {
            per_tab: [TabPrefs::default(); MonitorTab::ALL.len()],
            ladder: WindowLadder::default_ladder(),
            proc_sort: ProcSort::default(),
            // Descending: the point of the Processes tab is the heaviest
            // process, not the lightest.
            proc_desc: true,
            // Flat by default: the tree view is opt-in, since the flat top-N is
            // the fastest answer to "what is eating the box".
            proc_tree: false,
            last_tab: MonitorTab::default(),
        }
    }
}

impl MonitorPrefs {
    /// Defaults seeded from config. `[monitor]` supplies the ladder plus the
    /// starting window, style and scale for every tab; per-tab overrides layer
    /// on top from the DB.
    pub fn from_config(cfg: &thegn_core::config::MonitorConfig) -> MonitorPrefs {
        let ladder = WindowLadder::parse(&cfg.window_ladder);
        // Snap the configured default onto the ladder, so `[`/`]` always start
        // from a rung they can walk rather than from a value between two.
        let window = Window::from_key(&cfg.default_window)
            .map(|w| ladder.nearest(w))
            .unwrap_or_else(|| ladder.nearest(Window::DEFAULT));
        let base = TabPrefs {
            style: GraphStyle::from_key(&cfg.default_style).unwrap_or_default(),
            scale: ScaleMode::from_key(&cfg.default_scale).unwrap_or_default(),
            window,
        };
        MonitorPrefs {
            per_tab: [base; MonitorTab::ALL.len()],
            ladder,
            ..Default::default()
        }
    }

    pub fn tab(&self, t: MonitorTab) -> TabPrefs {
        self.per_tab[t.index()]
    }

    pub fn tab_mut(&mut self, t: MonitorTab) -> &mut TabPrefs {
        &mut self.per_tab[t.index()]
    }

    #[allow(dead_code)] // read by tests; the key handlers go through widen/narrow
    pub fn ladder(&self) -> &WindowLadder {
        &self.ladder
    }

    /// Widen `t`'s window one rung.
    ///
    /// A method rather than an exposed ladder because `tab_mut` borrows `self`
    /// mutably — a caller reading `self.ladder` alongside it would not
    /// borrow-check.
    pub fn widen(&mut self, t: MonitorTab) {
        let next = self.ladder.wider(self.tab(t).window);
        self.tab_mut(t).window = next;
    }

    /// Narrow `t`'s window one rung.
    pub fn narrow(&mut self, t: MonitorTab) {
        let next = self.ladder.narrower(self.tab(t).window);
        self.tab_mut(t).window = next;
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
            "proc_tree" => self.proc_tree = value == "1",
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
            (
                "proc_tree".into(),
                if self.proc_tree { "1" } else { "0" }.into(),
            ),
            ("last_tab".into(), self.last_tab.key().to_string()),
        ];
        for t in MonitorTab::ALL {
            let p = self.tab(t);
            out.push((format!("{}.style", t.key()), p.style.key().into()));
            out.push((format!("{}.scale", t.key()), p.scale.key().into()));
            // `key()` already yields an owned String now that a window is a
            // duration rather than a `&'static str` variant.
            out.push((format!("{}.window", t.key()), p.window.key()));
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
        p.tab_mut(MonitorTab::Cpu).window = Window::from_secs(3600);
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
            assert_eq!(p.tab(t).window, Window::from_secs(600), "{t:?}");
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

    #[test]
    fn the_window_ladder_comes_from_config() {
        let cfg = thegn_core::config::MonitorConfig {
            window_ladder: vec!["1m".into(), "1h".into(), "12h".into()],
            default_window: "1m".into(),
            ..Default::default()
        };
        let mut p = MonitorPrefs::from_config(&cfg);
        assert_eq!(p.tab(MonitorTab::Cpu).window, Window::from_secs(60));
        p.widen(MonitorTab::Cpu);
        assert_eq!(p.tab(MonitorTab::Cpu).window, Window::from_secs(3600));
        p.widen(MonitorTab::Cpu);
        assert_eq!(p.tab(MonitorTab::Cpu).window, Window::from_secs(43_200));
        // Saturating at the configured top, not wrapping to the bottom.
        p.widen(MonitorTab::Cpu);
        assert_eq!(p.tab(MonitorTab::Cpu).window, Window::from_secs(43_200));
        // And a rung the ladder does NOT carry is unreachable by key.
        assert!(!p.ladder().contains(Window::from_secs(600)));
    }

    #[test]
    fn the_shipped_default_reaches_twelve_hours() {
        // The requirement, pinned where a config edit would break it.
        let p = MonitorPrefs::from_config(&thegn_core::config::MonitorConfig::default());
        assert!(p.ladder().contains(Window::from_secs(43_200)));
        assert!(p.ladder().contains(Window::EVERYTHING));
        // The shipped default window must itself be a rung, or the first
        // `[`/`]` press would visibly snap somewhere the user didn't ask for.
        let w = p.tab(MonitorTab::Cpu).window;
        assert!(p.ladder().contains(w), "default {w} is off-ladder");
    }

    #[test]
    fn a_configured_default_off_the_ladder_snaps_onto_it() {
        // `2m` was the old shipped default and is no longer a rung; an upgraded
        // config must land somewhere the keys can walk from.
        let cfg = thegn_core::config::MonitorConfig {
            default_window: "2m".into(),
            ..Default::default()
        };
        let p = MonitorPrefs::from_config(&cfg);
        let w = p.tab(MonitorTab::Cpu).window;
        assert!(p.ladder().contains(w), "{w} is off-ladder");
        assert_eq!(w, Window::from_secs(60), "2m should snap to the nearer 1m");
    }

    #[test]
    fn a_junk_ladder_falls_back_rather_than_disabling_the_keys() {
        let cfg = thegn_core::config::MonitorConfig {
            window_ladder: vec!["17 fortnights".into(), "banana".into()],
            ..Default::default()
        };
        let p = MonitorPrefs::from_config(&cfg);
        assert!(p.ladder().windows().len() > 1, "keys must stay usable");
    }

    #[test]
    fn the_ladder_is_not_persisted_so_config_edits_still_win() {
        // `load` layers the DB over config. If the ladder round-tripped through
        // `entries`, a stale row would outvote a later `config.toml` edit and
        // the edit would appear to do nothing.
        let p = MonitorPrefs::from_config(&thegn_core::config::MonitorConfig {
            window_ladder: vec!["1m".into(), "1h".into()],
            ..Default::default()
        });
        assert!(
            !p.entries().iter().any(|(k, _)| k.contains("ladder")),
            "the ladder must not be persisted: {:?}",
            p.entries()
        );
    }

    #[test]
    fn an_off_ladder_saved_window_is_kept_not_discarded() {
        // A preference saved under a wider ladder is still a valid window; it
        // just snaps to a neighbour the next time the user presses a key.
        let mut p = MonitorPrefs::from_config(&thegn_core::config::MonitorConfig {
            window_ladder: vec!["1m".into(), "1h".into()],
            ..Default::default()
        });
        p.apply("cpu.window", "10m");
        assert_eq!(p.tab(MonitorTab::Cpu).window, Window::from_secs(600));
        p.widen(MonitorTab::Cpu);
        assert_eq!(p.tab(MonitorTab::Cpu).window, Window::from_secs(3600));
    }
}
