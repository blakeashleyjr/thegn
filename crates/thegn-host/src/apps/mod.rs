//! App tabs — a generic framework for hosting full sibling TUIs as top-level
//! tabs alongside the `work` IDE. Apps register in [`registry::APP_BUILDERS`];
//! one is registered today: **`observe`**, the gtui dashboards tile
//! ([`start_slot_tile`] → [`build_observe_tile`] →
//! `gtui_embed::embed::ObserveTile`), which [`AppHost::from_config`] gives a
//! slot when `[observe]` is enabled. It is live code, not scaffolding — the
//! run loop drives it (input routing, frame takeover, the app-event channel).
//! What *is* speculative is the plural: the slot vector, chip strip, and
//! multi-tab switching are built for more apps than the one that exists.
//!
//! Each app implements [`tg_kit::AppTile`] and is driven by the host loop the
//! same way standalone runs drive it: [`pump`] folds async results delivered
//! via a [`ChangeHook`] (wired to the host's `TerminalWaker`), [`render`]
//! paints a ratatui buffer, and [`bridge::blit`] copies that buffer into the
//! termwiz surface. Apps lazy-start on first focus and only the focused tile
//! renders; unfocused running tiles still pump so their chip badges stay live.
//!
//! This module is the host-side machinery (the bridge, the input translator,
//! the slot bookkeeping, and the live-`Palette` → [`tg_kit::Theme`] converter).
//! Run-loop wiring (input routing, frame takeover, the app-event channel) hangs
//! off [`AppHost`].
//!
//! [`pump`]: tg_kit::AppTile::pump
//! [`render`]: tg_kit::AppTile::render
//! [`ChangeHook`]: tg_kit::ChangeHook

pub mod bridge;
pub mod input;
pub mod registry;

use tg_kit::ratatui::buffer::Buffer;
use tg_kit::{AppTile, Theme};
use thegn_core::theme::Palette;

/// Which top-level tab is active. `Work` is the existing worktree IDE chrome;
/// `Tile(i)` is the app in slot `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveApp {
    Work,
    Tile(usize),
}

/// The lifecycle of an app slot. Apps cost nothing until first focused.
pub enum SlotState {
    /// Not yet constructed.
    Unloaded,
    /// Construction kicked off (e.g. a daemon connect on a blocking task);
    /// the chip shows a spinner until the tile arrives.
    // `observe` builds synchronously in `start_slot_tile`, so nothing takes the
    // async-start path yet; kept for the first tile that connects off-thread.
    #[allow(dead_code)]
    Starting,
    /// Live and drivable.
    Running(Box<dyn AppTile>),
    /// Construction or the connection failed; carries a user-facing reason.
    Failed(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "cause is surfaced via msg::warn + tracing at build time; the stored copy is only read by tests"
            )
        )]
        String,
    ),
}

impl SlotState {
    pub fn tile_mut(&mut self) -> Option<&mut (dyn AppTile + 'static)> {
        match self {
            SlotState::Running(t) => Some(t.as_mut()),
            _ => None,
        }
    }
}

/// One app tab.
pub struct AppSlot {
    /// Stable id / config key for an embedded app tab.
    pub id: &'static str,
    /// Chip label fallback before the tile is running (the running tile's
    /// `title()` takes over, badges included).
    pub label: String,
    pub state: SlotState,
    /// The last rendered buffer, re-blitted on frames where the tile reported
    /// no change.
    // Not read yet: the run loop re-renders the focused tile every frame rather
    // than reusing this. Wiring it is the caching optimization, not a fix.
    #[allow(dead_code)]
    pub last_buf: Option<Buffer>,
}

impl AppSlot {
    pub fn new(id: &'static str, label: impl Into<String>) -> AppSlot {
        AppSlot {
            id,
            label: label.into(),
            state: SlotState::Unloaded,
            last_buf: None,
        }
    }

    /// The chip text: the running tile's live title (badge included) or the
    /// configured fallback label.
    pub fn chip_label(&self) -> String {
        match &self.state {
            SlotState::Running(t) => t.title(),
            SlotState::Starting => format!("{}…", self.label),
            // The cause is logged when the build fails; a tab chip has no
            // room for it.
            SlotState::Failed(_) => format!("{} (failed)", self.label),
            _ => self.label.clone(),
        }
    }
}

/// The set of app tabs and which one is active. Lives on the host App state.
pub struct AppHost {
    pub slots: Vec<AppSlot>,
    pub active: ActiveApp,
    tab_order: Vec<ActiveApp>,
    default_tab: String,
}

impl AppHost {
    /// Build from an explicit slot list. `from_config` is what the host calls;
    /// this stays as the seam tests and future callers construct through.
    #[allow(dead_code)]
    pub fn new(slots: Vec<AppSlot>) -> AppHost {
        let tab_order = std::iter::once(ActiveApp::Work)
            .chain((0..slots.len()).map(ActiveApp::Tile))
            .collect();
        AppHost {
            slots,
            active: ActiveApp::Work,
            tab_order,
            default_tab: "work".into(),
        }
    }

    pub fn from_config(cfg: &thegn_core::config::Config) -> AppHost {
        let mut host = AppHost::new(Vec::new());
        host.reconcile(cfg);
        host
    }

    /// Reconcile config against stable app ids. Existing enabled slots (and
    /// their live tiles) survive reorder; removed slots are dropped, and new
    /// slots remain lazy. An unchanged default preserves the user's selection.
    /// When the configured default changes, it takes effect immediately. If
    /// the active tile is removed or disabled, the configured default wins.
    pub fn reconcile(&mut self, cfg: &thegn_core::config::Config) {
        let active_id = self.active_id().map(str::to_owned);
        let previous_default = self.default_tab.clone();
        let default_id = cfg.apps.normalized_default_tab(cfg.observe.enabled);
        let enabled_ids: std::collections::HashSet<&str> =
            registry::enabled(cfg).map(|builder| builder.id).collect();
        let tab_ids = cfg.apps.effective_tab_order(cfg.observe.enabled);
        let wanted_slots: Vec<&str> = tab_ids
            .iter()
            .filter(|id| enabled_ids.contains(id.as_str()))
            .filter_map(|id| registry::builder(id).map(|builder| builder.id))
            .collect();
        let wanted_order: Vec<&str> = tab_ids
            .iter()
            .filter_map(|id| {
                if id.as_str() == "work" {
                    Some("work")
                } else if enabled_ids.contains(id.as_str()) && registry::builder(id).is_some() {
                    Some(id.as_str())
                } else {
                    None
                }
            })
            .collect();
        let current_slots: Vec<&str> = self.slots.iter().map(|slot| slot.id).collect();
        let current_order: Vec<&str> = self
            .tab_order
            .iter()
            .filter_map(|target| match target {
                ActiveApp::Work => Some("work"),
                ActiveApp::Tile(index) => self.slots.get(*index).map(|slot| slot.id),
            })
            .collect();
        if default_id == previous_default
            && current_slots == wanted_slots
            && current_order == wanted_order
        {
            return;
        }

        let mut old_slots: std::collections::HashMap<&'static str, AppSlot> =
            self.slots.drain(..).map(|slot| (slot.id, slot)).collect();
        let mut slots = Vec::new();
        for id in &tab_ids {
            if !enabled_ids.contains(id.as_str()) {
                continue;
            }
            let Some(builder) = registry::builder(id) else {
                continue;
            };
            slots.push(
                old_slots
                    .remove(builder.id)
                    .unwrap_or_else(|| AppSlot::new(builder.id, builder.label)),
            );
        }
        self.slots = slots;
        self.tab_order = tab_ids
            .iter()
            .filter_map(|id| {
                if id.as_str() == "work" {
                    Some(ActiveApp::Work)
                } else {
                    self.slots
                        .iter()
                        .position(|slot| slot.id == id.as_str())
                        .map(ActiveApp::Tile)
                }
            })
            .collect();
        if self.tab_order.is_empty() {
            self.tab_order.push(ActiveApp::Work);
        }
        let target_for = |id: &str, slots: &[AppSlot]| {
            if id == "work" {
                Some(ActiveApp::Work)
            } else {
                slots
                    .iter()
                    .position(|slot| slot.id == id)
                    .map(ActiveApp::Tile)
            }
        };
        let selected = if default_id != previous_default {
            target_for(&default_id, &self.slots)
        } else {
            active_id
                .as_deref()
                .and_then(|id| target_for(id, &self.slots))
        };
        self.active = selected
            .or_else(|| target_for(&default_id, &self.slots))
            .unwrap_or(ActiveApp::Work);
        self.default_tab = default_id;
    }

    pub fn active_id(&self) -> Option<&str> {
        match self.active {
            ActiveApp::Work => Some("work"),
            ActiveApp::Tile(index) => self.slots.get(index).map(|slot| slot.id),
        }
    }

    pub fn tab_labels(&self) -> Vec<String> {
        self.tab_order
            .iter()
            .map(|target| match *target {
                ActiveApp::Work => "work".to_string(),
                ActiveApp::Tile(i) => self
                    .slots
                    .get(i)
                    .map(AppSlot::chip_label)
                    .unwrap_or_else(|| "?".into()),
            })
            .collect()
    }

    pub fn active_tab_index(&self) -> usize {
        self.tab_order
            .iter()
            .position(|target| *target == self.active)
            .unwrap_or(0)
    }

    pub fn tab_target(&self, index: usize) -> Option<ActiveApp> {
        self.tab_order.get(index).copied()
    }

    /// How many app tabs exist (Work + enabled tiles). With just one, the
    /// Alt+digit switch chords are pointless and the loop lets them fall
    /// through to the worktree-slot jumps.
    pub fn tab_count(&self) -> usize {
        self.tab_order.len()
    }

    pub fn cycle(&self, active: ActiveApp, delta: isize) -> ActiveApp {
        if self.tab_order.is_empty() {
            return ActiveApp::Work;
        }
        let cur = self
            .tab_order
            .iter()
            .position(|target| *target == active)
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(self.tab_order.len() as isize) as usize;
        self.tab_order[next]
    }

    /// The active tile, if an app tab (not `work`) is focused and running.
    pub fn active_tile_mut(&mut self) -> Option<&mut (dyn AppTile + 'static)> {
        match self.active {
            ActiveApp::Tile(i) => self.slots.get_mut(i).and_then(|s| s.state.tile_mut()),
            ActiveApp::Work => None,
        }
    }

    /// Drive every running tile's `pump` (cheap channel drain). Returns whether
    /// the active tile changed (the only one that triggers a redraw).
    pub fn pump_all(&mut self) -> bool {
        let active_idx = match self.active {
            ActiveApp::Tile(i) => Some(i),
            ActiveApp::Work => None,
        };
        let mut active_dirty = false;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if let SlotState::Running(t) = &mut slot.state {
                let changed = t.pump();
                if Some(i) == active_idx {
                    active_dirty |= changed;
                }
            }
        }
        active_dirty
    }
}

/// Construct an unloaded slot's tile by id (`observe`), storing it as
/// `Running`. Returns whether a tile was built. All the per-app wiring lives here
/// so the run-loop call site (`ensure_app_loaded`) stays a thin dispatch — both
/// `run.rs` and this dispatch are on a tokio runtime thread, so
/// `Handle::current()` is valid. Unknown ids no-op (unreachable: `from_config`
/// only creates slots the registry can build).
pub fn start_slot_tile(
    slot: &mut AppSlot,
    idx: usize,
    app_tx: &tokio::sync::mpsc::UnboundedSender<usize>,
    waker: &termwiz::terminal::TerminalWaker,
    cfg: &thegn_core::config::Config,
) -> bool {
    match registry::builder(slot.id) {
        Some(b) => {
            let hook = app_change_hook(app_tx, idx, waker);
            record_build_result(
                slot,
                (b.build)(hook, cfg, tokio::runtime::Handle::current()),
            )
        }
        None => false,
    }
}

fn record_build_result(slot: &mut AppSlot, result: Result<Box<dyn AppTile>, String>) -> bool {
    match result {
        Ok(tile) => {
            slot.state = SlotState::Running(tile);
        }
        Err(error) => {
            tracing::warn!(target: "thegn::apps", app = slot.id, %error, "app construction failed");
            // Visible even when THEGN_LOG is unset; the tab chip stays terse.
            thegn_core::msg::warn(&format!("{} failed to start: {error}", slot.id));
            slot.state = SlotState::Failed(error);
        }
    }
    true
}

/// A tile's [`ChangeHook`](tg_kit::ChangeHook): fired off-thread when the tile
/// has new data, it posts the slot index on the app channel and pulses the
/// terminal waker so the loop drains `app_rx` → `pump_all()` → repaint. The
/// receiver treats the index only as a wake signal and pumps the current slot
/// set, so a late callback after reconciliation cannot address a new tile by
/// its old vector index.
fn app_change_hook(
    app_tx: &tokio::sync::mpsc::UnboundedSender<usize>,
    idx: usize,
    waker: &termwiz::terminal::TerminalWaker,
) -> tg_kit::ChangeHook {
    let tx = app_tx.clone();
    let wk = waker.clone();
    std::sync::Arc::new(move || {
        let _ = tx.send(idx); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = wk.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    })
}

/// Construct the "Observe" (gtui) app tile from resolved config.
pub fn build_observe_tile(
    hook: tg_kit::ChangeHook,
    cfg: &thegn_core::config_observe::ObserveConfig,
    rt: tokio::runtime::Handle,
    sampler_thread_start: impl FnOnce() + Send + 'static,
) -> Result<Box<dyn AppTile>, String> {
    let tile = gtui_embed::embed::ObserveTile::new(hook, cfg, rt, sampler_thread_start)
        .map_err(|error| error.to_string())?;
    Ok(Box::new(tile))
}

/// Parse a `Palette` `"R;G;B"` fragment to an sRGB triple (missing channels → 0).
#[allow(dead_code)] // exercised by the kit_theme tests below; see kit_theme
fn rgb(frag: &str) -> tg_kit::Rgb {
    let mut it = frag.split(';').map(|n| n.trim().parse::<u8>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Convert the host's live chrome [`Palette`] into a [`tg_kit::Theme`] so
/// embedded tiles render in the user's exact thegn colors (theme-cycle and
/// `[theme.colors]` overrides included). The field mapping mirrors
/// [`tg_kit::Theme::prism`]; a parity test pins the two together.
///
/// NOTE: no render-path caller yet — `build_observe_tile` does not pass a
/// theme, so the Observe tile currently draws in gtui's own default colors
/// rather than the user's palette. The converter and its parity tests are
/// ready for whoever threads it through.
#[allow(dead_code)]
pub fn kit_theme(p: &Palette) -> Theme {
    Theme {
        bg0: rgb(&p.bg0),
        bg1: rgb(&p.bg1),
        panel: rgb(&p.panel),
        panel2: rgb(&p.panel2),
        raise: rgb(&p.raise),
        border: rgb(&p.border),
        focus: rgb(&p.focus),
        text: rgb(&p.text),
        dim: rgb(&p.dim),
        faint: rgb(&p.faint),
        ghost: rgb(&p.ghost),
        ghost2: rgb(&p.ghost2),
        ghost3: rgb(&p.ghost3),
        accent: rgb(&p.accent),
        chip_fg: rgb(&p.chip_fg),
        teal: rgb(&p.hues.teal),
        magenta: rgb(&p.hues.magenta),
        purple: rgb(&p.hues.purple),
        green: rgb(&p.hues.green),
        amber: rgb(&p.hues.amber),
        red: rgb(&p.hues.red),
        blue: rgb(&p.hues.blue),
        orange: rgb(&p.hues.orange),
    }
}

/// Which app-tab switch chord a keystroke is, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabChord {
    /// `Alt+<digit>` — select the tab at this 0-based index (may not exist).
    Select(usize),
    /// `Alt+]` / `Alt+[` — step this far through [`AppHost::cycle`].
    Cycle(isize),
}

/// Whether the run loop's pre-keymap app-tab intercept claims this keystroke.
///
/// This is the *whole* guard, factored out of the `event_loop` match so it can
/// be tested: the intercept runs before `keymap.dispatch`, so a keymap unit
/// test cannot see a chord this function wrongly claims — which is exactly how
/// THE-70's bugs survived a suite that pinned every binding.
///
/// Two rules, both load-bearing:
///
/// - **Exact ALT, never `contains(ALT)`.** `Ctrl+Alt+<digit>` is `summon-pin-N`
///   and `Ctrl+Alt+]`/`[` are `GrowStrip`/`ShrinkStrip`; a `contains` test ate
///   all of them before the keymap ever ran.
/// - **`tab_count > 1` gates both arms.** With a lone Work tab there is nothing
///   to switch or cycle between, and [`AppHost::cycle`] always returns a target
///   (the tab we are already on), so an ungated cycle arm swallowed
///   `Alt+]`/`[` on every configuration.
pub fn tab_chord(
    mods: termwiz::input::Modifiers,
    key: &termwiz::input::KeyCode,
    tab_count: usize,
) -> Option<TabChord> {
    use termwiz::input::{KeyCode, Modifiers};
    if mods != Modifiers::ALT || tab_count <= 1 {
        return None;
    }
    match key {
        KeyCode::Char(c @ '1'..='9') => Some(TabChord::Select(*c as usize - '1' as usize)),
        KeyCode::Char(']') => Some(TabChord::Cycle(1)),
        KeyCode::Char('[') => Some(TabChord::Cycle(-1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::input::{KeyCode, Modifiers};

    /// THE-70 root cause B. The intercept must claim plain `Alt+<digit>` and
    /// `Alt+]`/`[` — and nothing else.
    #[test]
    fn tab_chord_claims_only_exact_alt_with_more_than_one_tab() {
        // Claimed: exact Alt, >1 tab.
        assert_eq!(
            tab_chord(Modifiers::ALT, &KeyCode::Char('1'), 3),
            Some(TabChord::Select(0)),
        );
        assert_eq!(
            tab_chord(Modifiers::ALT, &KeyCode::Char('9'), 3),
            Some(TabChord::Select(8)),
        );
        assert_eq!(
            tab_chord(Modifiers::ALT, &KeyCode::Char(']'), 2),
            Some(TabChord::Cycle(1)),
        );
        assert_eq!(
            tab_chord(Modifiers::ALT, &KeyCode::Char('['), 2),
            Some(TabChord::Cycle(-1)),
        );
    }

    /// `Ctrl+Alt+N` is `summon-pin-N` and `Ctrl+Alt+]`/`[` are the strip
    /// resizers. The old `contains(ALT)` guard ate every one of them — pins
    /// died the moment a second app tab existed, and the strip resizers were
    /// unreachable on every configuration.
    #[test]
    fn tab_chord_never_claims_a_ctrl_alt_chord() {
        let ctrl_alt = Modifiers::CTRL | Modifiers::ALT;
        for key in [
            KeyCode::Char('1'),
            KeyCode::Char('9'),
            KeyCode::Char(']'),
            KeyCode::Char('['),
        ] {
            assert_eq!(tab_chord(ctrl_alt, &key, 5), None, "Ctrl+Alt+{key:?}");
        }
        // Nor any other superset of ALT.
        assert_eq!(
            tab_chord(Modifiers::ALT | Modifiers::SHIFT, &KeyCode::Char('1'), 5),
            None,
        );
        assert_eq!(
            tab_chord(Modifiers::ALT | Modifiers::SUPER, &KeyCode::Char(']'), 5),
            None,
        );
        // …and a bare digit belongs to the pane, not the tab strip.
        assert_eq!(tab_chord(Modifiers::NONE, &KeyCode::Char('1'), 5), None);
    }

    /// With one tab (or none) there is nothing to switch between, so both arms
    /// must fall through: `Alt+<digit>` to `summon-worktree-N`, `Alt+]`/`[` to
    /// whatever the keymap says.
    #[test]
    fn tab_chord_falls_through_without_a_second_tab() {
        for count in [0usize, 1] {
            for key in [KeyCode::Char('1'), KeyCode::Char(']'), KeyCode::Char('[')] {
                assert_eq!(tab_chord(Modifiers::ALT, &key, count), None, "{count} tabs");
            }
        }
    }

    /// `cycle` wraps, so the delta the intercept hands it must round-trip
    /// through the real tab order rather than being asserted in isolation.
    #[test]
    fn tab_chord_cycle_deltas_match_apphost_cycle() {
        let host = AppHost {
            slots: Vec::new(),
            active: ActiveApp::Work,
            tab_order: vec![ActiveApp::Work, ActiveApp::Tile(0), ActiveApp::Tile(1)],
            default_tab: "work".into(),
        };
        let delta = |k: char| match tab_chord(Modifiers::ALT, &KeyCode::Char(k), host.tab_count()) {
            Some(TabChord::Cycle(d)) => d,
            other => panic!("expected a cycle chord, got {other:?}"),
        };
        assert_eq!(host.cycle(ActiveApp::Work, delta(']')), ActiveApp::Tile(0));
        assert_eq!(host.cycle(ActiveApp::Work, delta('[')), ActiveApp::Tile(1));
    }

    #[test]
    fn rgb_parses_and_tolerates_short_fragments() {
        assert_eq!(rgb("110;231;216"), (110, 231, 216));
        assert_eq!(rgb("10;20"), (10, 20, 0));
        assert_eq!(rgb("bad;data;here"), (0, 0, 0));
    }

    /// The contract: tg-kit's baked prism defaults must equal the host's
    /// default chrome palette, field for field. If thegn changes a default
    /// color, this fails until tg-kit's `Theme::prism()` is updated to match.
    #[test]
    fn kit_prism_matches_host_default_palette() {
        assert_eq!(kit_theme(&Palette::default()), Theme::prism());
    }

    #[test]
    fn user_palette_overrides_flow_through() {
        let p = Palette {
            accent: "1;2;3".into(),
            ..Default::default()
        };
        assert_eq!(kit_theme(&p).accent, (1, 2, 3));
    }

    #[test]
    fn app_host_with_no_registered_tabs_is_work_only() {
        // No embedded app builders are registered, so unknown ids are dropped
        // and `work` is the only tab regardless of what the config requests.
        let mut cfg = thegn_core::config::Config::default();
        cfg.apps.tab_order = vec!["work".into()];
        cfg.apps.default_tab = "work".into();

        let host = AppHost::from_config(&cfg);

        assert!(host.slots.is_empty());
        assert_eq!(host.tab_labels(), vec!["work"]);
        assert_eq!(host.active, ActiveApp::Work);
        assert_eq!(host.active_tab_index(), 0);
        assert_eq!(host.tab_target(0), Some(ActiveApp::Work));
        // Cycling stays on the only tab.
        assert_eq!(host.cycle(ActiveApp::Work, 1), ActiveApp::Work);
    }

    #[test]
    fn observe_tab_registered_only_when_enabled() {
        let mut cfg = thegn_core::config::Config::default();
        // Default: observe disabled ⇒ work-only.
        assert!(AppHost::from_config(&cfg).slots.is_empty());

        cfg.observe.enabled = true;
        let host = AppHost::from_config(&cfg);
        assert_eq!(host.slots.len(), 1);
        assert_eq!(host.slots[0].id, "observe");
        assert_eq!(host.tab_labels(), vec!["work", "Observe"]);
        // The observe tile is reachable as the second tab.
        assert_eq!(host.tab_target(1), Some(ActiveApp::Tile(0)));
        assert_eq!(host.cycle(ActiveApp::Work, 1), ActiveApp::Tile(0));
    }

    #[test]
    fn app_construction_error_is_retained_as_a_visible_failed_slot() {
        let mut slot = AppSlot::new("observe", "Observe");
        assert!(record_build_result(
            &mut slot,
            Err("injected spawn failure".into())
        ));
        assert!(
            matches!(slot.state, SlotState::Failed(ref error) if error == "injected spawn failure")
        );
        assert_eq!(slot.chip_label(), "Observe (failed)");
    }

    #[test]
    fn configured_observe_order_and_default_are_honored() {
        let mut cfg = thegn_core::config::Config::default();
        cfg.observe.enabled = true;
        cfg.apps.default_tab = "observe".into();
        // Membership comes from `observe.enabled`; `[apps]` only orders the
        // enabled tab set, so an omitted app still follows `work` here.
        cfg.apps.tab_order = vec!["work".into()];
        let host = AppHost::from_config(&cfg);

        assert_eq!(host.tab_labels(), vec!["work", "Observe"]);
        assert_eq!(host.active_id(), Some("observe"));
        assert_eq!(host.active_tab_index(), 1);
    }

    #[test]
    fn reload_reconciles_by_id_and_preserves_selection_until_default_changes() {
        let mut cfg = thegn_core::config::Config::default();
        cfg.observe.enabled = true;
        cfg.apps.default_tab = "observe".into();
        cfg.apps.tab_order = vec!["observe".into(), "work".into()];
        let mut host = AppHost::from_config(&cfg);
        host.slots[0].state = SlotState::Failed("retained".into());

        // Same default with a changed order retains both selection and slot
        // lifecycle state by id, instead of interpreting the old vector index.
        cfg.apps.tab_order = vec!["work".into(), "observe".into()];
        host.reconcile(&cfg);
        assert_eq!(host.active_id(), Some("observe"));
        // The retained Failed slot keeps its visible failure chip.
        assert_eq!(host.tab_labels(), vec!["work", "Observe (failed)"]);
        assert!(matches!(host.slots[0].state, SlotState::Failed(_)));
        let slot_address = &host.slots[0] as *const AppSlot;
        host.reconcile(&cfg);
        assert_eq!(
            &host.slots[0] as *const AppSlot, slot_address,
            "unchanged config should not rebuild the slot collection"
        );

        // A changed default is applied immediately; if the active app is later
        // disabled, the configured default remains the deterministic fallback.
        cfg.apps.default_tab = "work".into();
        host.reconcile(&cfg);
        assert_eq!(host.active_id(), Some("work"));
        host.active = ActiveApp::Tile(0);
        cfg.observe.enabled = false;
        host.reconcile(&cfg);
        assert_eq!(host.active_id(), Some("work"));
        assert!(host.slots.is_empty());
    }

    #[test]
    fn reload_can_enable_a_lazy_app_and_select_it_as_the_new_default() {
        let mut cfg = thegn_core::config::Config::default();
        let mut host = AppHost::from_config(&cfg);
        assert!(host.slots.is_empty());

        cfg.observe.enabled = true;
        host.reconcile(&cfg);
        assert_eq!(host.tab_labels(), vec!["work", "Observe"]);
        assert_eq!(host.active_id(), Some("work"));
        assert!(matches!(host.slots[0].state, SlotState::Unloaded));

        cfg.apps.default_tab = "observe".into();
        host.reconcile(&cfg);
        assert_eq!(host.active_id(), Some("observe"));
        assert_eq!(host.active_tab_index(), 1);
    }
}
