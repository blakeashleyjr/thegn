//! Pinned programs / daemons — the native host's pin model and supervisor.
//!
//! A *pin* is a configured program (`[[pins]]`) the host manages independently
//! of the tab/worktree it was summoned from: it can render in the top **strip**,
//! as a **float**ing scratch overlay, in the active tab's **layout**, or as its
//! own **tab**. The [`PinSupervisor`] owns the live registrations
//! ([`PinInstance`]s) and the strip's visibility/ratio; it outlives tab and
//! workspace switches, so a daemon pin keeps running while you move around.
//!
//! The supervisor is deliberately *pure*: it never spawns a PTY itself. The event
//! loop (`run.rs`) owns the `Panes` table and asks the supervisor *what* to do
//! (which argv, which env, whether to restart, where the strip panes go); the
//! supervisor records the resulting pane ids. That seam keeps every lifecycle
//! decision unit-testable without a terminal.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use superzej_core::config::{Config, Pin, PinLocation, PinRestart};

use crate::compositor::Rect;

/// A persisted running pin: its name + placement. The supervisor re-launches
/// these on resurrect (looking the pin up in config by name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedPin {
    pub name: String,
    pub placement: String,
}

/// Where a live pin is placed on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinPlacement {
    /// The top strip band (rendered live, side by side).
    Strip,
    /// A floating scratch overlay (one at a time, like the drawer).
    Float,
    /// A tiled pane spliced into the focused tab's center tree.
    Layout,
    /// Its own session tab.
    Tab,
}

impl PinPlacement {
    pub fn from_location(loc: PinLocation) -> Self {
        match loc {
            PinLocation::Strip => PinPlacement::Strip,
            PinLocation::Float => PinPlacement::Float,
            PinLocation::Layout => PinPlacement::Layout,
            PinLocation::Tab => PinPlacement::Tab,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PinPlacement::Strip => "strip",
            PinPlacement::Float => "float",
            PinPlacement::Layout => "layout",
            PinPlacement::Tab => "tab",
        }
    }
}

/// A pin's liveness, surfaced as the `●/◌/✖` status glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinHealth {
    /// Process is up.
    Running,
    /// Not started, or cleanly stopped / unpinned.
    Stopped,
    /// Exited unexpectedly (and not restarted).
    Failed,
}

impl PinHealth {
    /// The status glyph shown in the strip header and tabbar chips. Sourced from
    /// the active terminal glyph set (`● ○ ✖` on capable terminals, `* o x` when
    /// degraded to ASCII).
    pub fn glyph(self) -> char {
        let g = crate::caps::active_glyphs();
        let s = match self {
            PinHealth::Running => g.dot_filled,
            PinHealth::Stopped => g.dot_hollow,
            PinHealth::Failed => g.cross_heavy,
        };
        s.chars().next().unwrap_or('?')
    }
}

/// What [`PinSupervisor::on_exit`] decided to do with a dead pin pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Respawn the pin (caller spawns a fresh pane and calls `attach`).
    Respawn,
    /// Leave it down; mark `health` per policy (Stopped for clean, Failed else).
    Leave,
}

/// A live pin registration. `pane` is `Some` once materialized.
#[derive(Debug, Clone)]
pub struct PinInstance {
    /// The configured pin name (the dedupe key for singletons).
    pub name: String,
    /// Display label (strip header / chip text).
    pub label: String,
    pub placement: PinPlacement,
    /// The live PTY pane id, when materialized.
    pub pane: Option<u32>,
    pub health: PinHealth,
    pub restart: PinRestart,
    /// Strip apportionment weight (only meaningful for `Strip`).
    pub weight: f32,
}

impl PinInstance {
    fn from_pin(pin: &Pin) -> Self {
        PinInstance {
            name: pin.name.clone(),
            label: pin.display_label().to_string(),
            placement: PinPlacement::from_location(pin.location),
            pane: None,
            health: PinHealth::Stopped,
            restart: pin.restart.clone(),
            weight: pin.strip_weight(),
        }
    }
}

/// A chip rendered in the tabbar: label + status glyph, in `Alt-N` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinChip {
    pub index: usize,
    pub label: String,
    pub glyph: char,
}

/// Owns the live pin registrations and the strip's runtime state.
#[derive(Debug, Clone)]
pub struct PinSupervisor {
    instances: Vec<PinInstance>,
    strip_visible: bool,
    strip_ratio: f32,
}

impl PinSupervisor {
    pub fn new(strip_visible: bool, strip_ratio: f32) -> Self {
        PinSupervisor {
            instances: Vec::new(),
            strip_visible,
            strip_ratio: strip_ratio.clamp(0.05, 0.9),
        }
    }

    pub fn from_config(cfg: &Config) -> Self {
        Self::new(cfg.strip.visible, cfg.strip.clamped_ratio())
    }

    // --- pure queries -------------------------------------------------------

    pub fn strip_visible(&self) -> bool {
        self.strip_visible
    }

    pub fn strip_ratio(&self) -> f32 {
        self.strip_ratio
    }

    pub fn instances(&self) -> &[PinInstance] {
        &self.instances
    }

    /// True when there is at least one live strip pane (drives strip reservation).
    pub fn has_strip_panes(&self) -> bool {
        self.instances
            .iter()
            .any(|i| i.placement == PinPlacement::Strip && i.pane.is_some())
    }

    /// The instance owning `pane`, if any.
    pub fn instance_of_pane(&self, pane: u32) -> Option<&PinInstance> {
        self.instances.iter().find(|i| i.pane == Some(pane))
    }

    /// Pins visible from `workspace`, scope-filtered (reuses the core helper).
    pub fn resolve<'a>(cfg: &'a Config, workspace: Option<&str>) -> Vec<&'a Pin> {
        cfg.pins_for_workspace(workspace)
    }

    /// The eager pins to launch at startup for `workspace` (config order).
    pub fn eager_pins<'a>(cfg: &'a Config, workspace: Option<&str>) -> Vec<&'a Pin> {
        use superzej_core::config::PinStart;
        Self::resolve(cfg, workspace)
            .into_iter()
            .filter(|p| p.start == PinStart::Eager)
            .collect()
    }

    /// Tabbar chips for the resolved pin set: 1-based `Alt-N` index, label, glyph
    /// (live health if registered, else Stopped). Config order == `Alt-N` order.
    pub fn chips(&self, cfg: &Config, workspace: Option<&str>) -> Vec<PinChip> {
        Self::resolve(cfg, workspace)
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let health = self
                    .instances
                    .iter()
                    .find(|inst| inst.name == p.name && inst.pane.is_some())
                    .map(|inst| inst.health)
                    .unwrap_or(PinHealth::Stopped);
                PinChip {
                    index: i + 1,
                    label: p.display_label().to_string(),
                    glyph: health.glyph(),
                }
            })
            .collect()
    }

    // --- adapter: launch spec ----------------------------------------------

    /// The argv to launch a pin: explicit `args` run directly, else `command`
    /// through the login shell (`exec`'d so signals reach the program).
    pub fn argv(pin: &Pin) -> Vec<String> {
        if !pin.args.is_empty() {
            let mut v = Vec::with_capacity(pin.args.len() + 1);
            v.push(pin.command.clone());
            v.extend(pin.args.iter().cloned());
            v
        } else {
            vec![
                superzej_core::util::shell(),
                "-lc".into(),
                format!("exec {}", pin.command.trim()),
            ]
        }
    }

    /// The per-program env to inject (a clone of the pin's `env` map).
    pub fn spawn_env(pin: &Pin) -> BTreeMap<String, String> {
        pin.env.clone()
    }

    // --- lifecycle ----------------------------------------------------------

    /// Find a live (materialized) instance by name — the singleton dedupe seam.
    pub fn live_instance(&self, name: &str) -> Option<&PinInstance> {
        self.instances
            .iter()
            .find(|i| i.name == name && i.pane.is_some())
    }

    /// Register a freshly-spawned pin pane. Returns the slot index.
    pub fn attach(&mut self, pin: &Pin, pane: u32) -> usize {
        let mut inst = PinInstance::from_pin(pin);
        inst.pane = Some(pane);
        inst.health = PinHealth::Running;
        self.instances.push(inst);
        self.instances.len() - 1
    }

    /// Decide what to do when a pin pane exits. `clean` = exited with status 0.
    /// Marks `health` and returns whether the caller should respawn.
    pub fn on_exit(&mut self, pane: u32, clean: bool) -> RestartDecision {
        let Some(inst) = self.instances.iter_mut().find(|i| i.pane == Some(pane)) else {
            return RestartDecision::Leave;
        };
        inst.pane = None;
        let restart = matches!(
            (&inst.restart, clean),
            (PinRestart::Always, _) | (PinRestart::OnFailure, false)
        );
        if restart {
            inst.health = PinHealth::Running; // about to respawn
            RestartDecision::Respawn
        } else {
            inst.health = if clean {
                PinHealth::Stopped
            } else {
                PinHealth::Failed
            };
            RestartDecision::Leave
        }
    }

    /// Rebind an instance to a respawned pane (after `on_exit` → `Respawn`).
    /// Matches the most-recent down instance of `name`.
    pub fn reattach(&mut self, name: &str, pane: u32) {
        if let Some(inst) = self
            .instances
            .iter_mut()
            .rev()
            .find(|i| i.name == name && i.pane.is_none())
        {
            inst.pane = Some(pane);
            inst.health = PinHealth::Running;
        }
    }

    /// Promote an existing pane (e.g. a focused center pane) into a pin slot.
    pub fn promote(&mut self, name: &str, label: &str, placement: PinPlacement, pane: u32) {
        self.instances.push(PinInstance {
            name: name.to_string(),
            label: label.to_string(),
            placement,
            pane: Some(pane),
            health: PinHealth::Running,
            restart: PinRestart::Never,
            weight: 1.0,
        });
    }

    /// Unpin (remove) the live instance with `name`. Returns the pane id the
    /// caller must reap, if it was materialized.
    pub fn unpin(&mut self, name: &str) -> Option<u32> {
        let idx = self
            .instances
            .iter()
            .position(|i| i.name == name && i.pane.is_some())?;
        let inst = self.instances.remove(idx);
        inst.pane
    }

    // --- persistence --------------------------------------------------------

    /// Snapshot the live (materialized) pins for resurrect, in registration order.
    pub fn persisted(&self) -> Vec<PersistedPin> {
        self.instances
            .iter()
            .filter(|i| i.pane.is_some())
            .map(|i| PersistedPin {
                name: i.name.clone(),
                placement: i.placement.as_str().to_string(),
            })
            .collect()
    }

    /// Serialize the live pin set to JSON for `session_state.pin_state`.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.persisted()).unwrap_or_else(|_| "[]".into())
    }

    /// Parse a persisted pin set; bad JSON degrades to empty (never blocks
    /// resurrect). Only pins still present in `cfg` (by name) are returned, so a
    /// removed `[[pins]]` entry doesn't resurrect.
    pub fn parse_persisted(json: &str, cfg: &Config) -> Vec<PersistedPin> {
        serde_json::from_str::<Vec<PersistedPin>>(json)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| cfg.pin(&p.name).is_some())
            .collect()
    }

    // --- strip geometry -----------------------------------------------------

    pub fn toggle_strip(&mut self) {
        self.strip_visible = !self.strip_visible;
    }

    /// Nudge the strip ratio by `delta`, clamped to the sane band.
    pub fn adjust_ratio(&mut self, delta: f32) {
        self.strip_ratio = (self.strip_ratio + delta).clamp(0.05, 0.9);
    }

    /// Lay out the live strip panes left-to-right within `rect`, apportioned by
    /// weight. A 1-row header per pin is the caller's concern; this hands back the
    /// full per-pin rect. Mirrors `center.rs`'s remainder-absorbing apportionment.
    pub fn strip_layout(&self, rect: Rect) -> Vec<(u32, Rect)> {
        let live: Vec<&PinInstance> = self
            .instances
            .iter()
            .filter(|i| i.placement == PinPlacement::Strip && i.pane.is_some())
            .collect();
        if live.is_empty() || rect.cols == 0 || rect.rows == 0 {
            return Vec::new();
        }
        let total: f32 = live.iter().map(|i| i.weight.max(0.0)).sum();
        let total = if total <= 0.0 {
            live.len() as f32
        } else {
            total
        };
        let mut out = Vec::with_capacity(live.len());
        let mut offset = 0usize;
        for (i, inst) in live.iter().enumerate() {
            let w = inst.weight.max(0.0);
            let cols = if i + 1 == live.len() {
                rect.cols.saturating_sub(offset)
            } else {
                ((w / total) * rect.cols as f32).round() as usize
            };
            out.push((
                inst.pane.unwrap(),
                Rect {
                    x: rect.x + offset,
                    y: rect.y,
                    cols,
                    rows: rect.rows,
                },
            ));
            offset += cols;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::config::{PinScope, PinStart};

    fn pin(name: &str, loc: PinLocation) -> Pin {
        Pin {
            name: name.into(),
            command: name.into(),
            args: Vec::new(),
            cwd: None,
            location: loc,
            scope: PinScope::Global,
            workspace: None,
            start: PinStart::Lazy,
            restart: PinRestart::Never,
            singleton: true,
            env: Default::default(),
            label: None,
            ratio: None,
        }
    }

    #[test]
    fn glyphs_reflect_health() {
        assert_eq!(PinHealth::Running.glyph(), '\u{25cf}');
        assert_eq!(PinHealth::Stopped.glyph(), '\u{25cb}');
        assert_eq!(PinHealth::Failed.glyph(), '\u{2716}');
    }

    #[test]
    fn argv_uses_explicit_args_directly_else_shell() {
        let mut p = pin("logs", PinLocation::Strip);
        assert_eq!(PinSupervisor::argv(&p)[0], superzej_core::util::shell());
        assert!(
            PinSupervisor::argv(&p)
                .last()
                .unwrap()
                .contains("exec logs")
        );

        p.command = "btop".into();
        p.args = vec!["--utf-force".into()];
        assert_eq!(PinSupervisor::argv(&p), vec!["btop", "--utf-force"]);
    }

    #[test]
    fn scope_filters_resolve_to_workspace() {
        let mut global = pin("mail", PinLocation::Float);
        global.scope = PinScope::Global;
        let mut local = pin("devlog", PinLocation::Strip);
        local.scope = PinScope::Workspace;
        local.workspace = Some("repoA".into());
        let cfg = Config {
            pins: vec![global, local],
            ..Config::default()
        };

        let in_a: Vec<_> = PinSupervisor::resolve(&cfg, Some("repoA"))
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(in_a, vec!["mail", "devlog"]);

        let in_b: Vec<_> = PinSupervisor::resolve(&cfg, Some("repoB"))
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(in_b, vec!["mail"]); // global only
    }

    #[test]
    fn eager_pins_are_the_eager_ones() {
        let mut a = pin("a", PinLocation::Strip);
        a.start = PinStart::Eager;
        let b = pin("b", PinLocation::Strip); // lazy
        let cfg = Config {
            pins: vec![a, b],
            ..Config::default()
        };
        let names: Vec<_> = PinSupervisor::eager_pins(&cfg, None)
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(names, vec!["a"]);
    }

    #[test]
    fn singleton_dedupe_via_live_instance() {
        let mut sup = PinSupervisor::new(true, 0.2);
        let p = pin("mail", PinLocation::Float);
        assert!(sup.live_instance("mail").is_none());
        sup.attach(&p, 7);
        assert_eq!(sup.live_instance("mail").map(|i| i.pane), Some(Some(7)));
    }

    #[test]
    fn on_exit_respects_restart_policy() {
        let mut sup = PinSupervisor::new(true, 0.2);

        let mut always = pin("a", PinLocation::Strip);
        always.restart = PinRestart::Always;
        sup.attach(&always, 1);
        assert_eq!(sup.on_exit(1, true), RestartDecision::Respawn);

        let mut onfail = pin("f", PinLocation::Strip);
        onfail.restart = PinRestart::OnFailure;
        sup.attach(&onfail, 2);
        assert_eq!(sup.on_exit(2, true), RestartDecision::Leave); // clean → no
        sup.reattach("f", 3);
        assert_eq!(sup.on_exit(3, false), RestartDecision::Respawn); // crash → yes

        let never = pin("n", PinLocation::Strip);
        sup.attach(&never, 4);
        assert_eq!(sup.on_exit(4, false), RestartDecision::Leave);
        assert_eq!(
            sup.instances()
                .iter()
                .find(|i| i.name == "n")
                .unwrap()
                .health,
            PinHealth::Failed
        );
    }

    #[test]
    fn unpin_removes_and_returns_pane() {
        let mut sup = PinSupervisor::new(true, 0.2);
        sup.attach(&pin("mail", PinLocation::Float), 9);
        assert_eq!(sup.unpin("mail"), Some(9));
        assert!(sup.live_instance("mail").is_none());
        assert_eq!(sup.unpin("mail"), None);
    }

    #[test]
    fn strip_layout_apportions_by_weight_without_gaps() {
        let mut sup = PinSupervisor::new(true, 0.2);
        let mut a = pin("a", PinLocation::Strip);
        a.ratio = Some(2.0);
        let b = pin("b", PinLocation::Strip); // weight 1.0
        let ia = sup.attach(&a, 1);
        let ib = sup.attach(&b, 2);
        assert_eq!((ia, ib), (0, 1));

        let rect = Rect {
            x: 0,
            y: 1,
            cols: 90,
            rows: 8,
        };
        let l = sup.strip_layout(rect);
        assert_eq!(l.len(), 2);
        // 2:1 of 90 → 60 + 30, no gaps.
        assert_eq!(l[0].1.cols + l[1].1.cols, 90);
        assert!(l[0].1.cols > l[1].1.cols);
        assert_eq!(l[1].1.x, l[0].1.cols);
        assert_eq!(l[0].1.y, 1);
    }

    #[test]
    fn strip_layout_skips_unmaterialized_and_non_strip() {
        let mut sup = PinSupervisor::new(true, 0.2);
        sup.attach(&pin("float", PinLocation::Float), 1); // wrong placement
        let strip = pin("s", PinLocation::Strip);
        let idx = sup.attach(&strip, 2);
        sup.instances[idx].pane = None; // not materialized
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 6,
        };
        assert!(sup.strip_layout(rect).is_empty());
        assert!(!sup.has_strip_panes());
    }

    #[test]
    fn toggle_and_ratio_adjust_clamp() {
        let mut sup = PinSupervisor::new(true, 0.2);
        sup.toggle_strip();
        assert!(!sup.strip_visible());
        sup.adjust_ratio(5.0);
        assert_eq!(sup.strip_ratio(), 0.9);
        sup.adjust_ratio(-5.0);
        assert_eq!(sup.strip_ratio(), 0.05);
    }

    #[test]
    fn persisted_round_trips_through_json_filtered_by_config() {
        let mut cfg = Config {
            pins: vec![
                pin("mail", PinLocation::Float),
                pin("logs", PinLocation::Strip),
            ],
            ..Config::default()
        };
        let mut sup = PinSupervisor::new(true, 0.2);
        sup.attach(&cfg.pins[0], 1);
        sup.attach(&cfg.pins[1], 2);
        let json = sup.to_json();
        let back = PinSupervisor::parse_persisted(&json, &cfg);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "mail");
        assert_eq!(back[0].placement, "float");
        assert_eq!(back[1].placement, "strip");

        // A pin no longer in config is dropped on resurrect.
        cfg.pins.remove(0);
        let back2 = PinSupervisor::parse_persisted(&json, &cfg);
        assert_eq!(back2.len(), 1);
        assert_eq!(back2[0].name, "logs");

        // Bad JSON degrades to empty.
        assert!(PinSupervisor::parse_persisted("not json", &cfg).is_empty());
    }

    #[test]
    fn persisted_only_includes_materialized_pins() {
        let mut sup = PinSupervisor::new(true, 0.2);
        let idx = sup.attach(&pin("down", PinLocation::Strip), 5);
        sup.instances[idx].pane = None; // not live
        sup.attach(&pin("up", PinLocation::Strip), 6);
        let names: Vec<_> = sup.persisted().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["up"]);
    }

    #[test]
    fn chips_track_health_in_alt_n_order() {
        let cfg = Config {
            pins: vec![pin("a", PinLocation::Strip), pin("b", PinLocation::Float)],
            ..Config::default()
        };
        let mut sup = PinSupervisor::new(true, 0.2);
        sup.attach(&cfg.pins[0], 1); // a is running
        let chips = sup.chips(&cfg, None);
        assert_eq!(chips.len(), 2);
        assert_eq!(chips[0].index, 1);
        assert_eq!(chips[0].label, "a");
        assert_eq!(chips[0].glyph, PinHealth::Running.glyph());
        assert_eq!(chips[1].glyph, PinHealth::Stopped.glyph());
    }
}
