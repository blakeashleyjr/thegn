//! App tabs — a generic framework for hosting full sibling TUIs as top-level
//! tabs alongside the `work` IDE. No builders are registered today (`work` is
//! the only tab); the machinery below is kept for future embedded apps.
//!
//! Each app implements [`sz_kit::AppTile`] and is driven by the host loop the
//! same way standalone runs drive it: [`pump`] folds async results delivered
//! via a [`ChangeHook`] (wired to the host's `TerminalWaker`), [`render`]
//! paints a ratatui buffer, and [`bridge::blit`] copies that buffer into the
//! termwiz surface. Apps lazy-start on first focus and only the focused tile
//! renders; unfocused running tiles still pump so their chip badges stay live.
//!
//! This module is the host-side machinery (the bridge, the input translator,
//! the slot bookkeeping, and the live-`Palette` → [`sz_kit::Theme`] converter).
//! Run-loop wiring (input routing, frame takeover, the app-event channel) hangs
//! off [`AppHost`].
//!
//! [`pump`]: sz_kit::AppTile::pump
//! [`render`]: sz_kit::AppTile::render
//! [`ChangeHook`]: sz_kit::ChangeHook
#![allow(dead_code)] // wired into run.rs incrementally (Phase 2)

pub mod bridge;
pub mod input;

use superzej_core::theme::Palette;
use sz_kit::ratatui::buffer::Buffer;
use sz_kit::{AppTile, Theme};

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
    Starting,
    /// Live and drivable.
    Running(Box<dyn AppTile>),
    /// Construction or the connection failed; carries a user-facing reason.
    Failed(String),
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
            _ => self.label.clone(),
        }
    }
}

/// The set of app tabs and which one is active. Lives on the host App state.
pub struct AppHost {
    pub slots: Vec<AppSlot>,
    pub active: ActiveApp,
    tab_order: Vec<ActiveApp>,
}

impl AppHost {
    pub fn new(slots: Vec<AppSlot>) -> AppHost {
        let tab_order = std::iter::once(ActiveApp::Work)
            .chain((0..slots.len()).map(ActiveApp::Tile))
            .collect();
        AppHost {
            slots,
            active: ActiveApp::Work,
            tab_order,
        }
    }

    pub fn from_config(cfg: &superzej_core::config::Config) -> AppHost {
        let tab_ids = cfg.apps.effective_tab_order();
        // No embedded app tabs are registered today, so `work` is the only tab.
        // When a builder is added, push an `AppSlot` for its id here.
        let slots: Vec<AppSlot> = Vec::new();

        let mut tab_order = Vec::new();
        for id in tab_ids {
            if id == "work" {
                tab_order.push(ActiveApp::Work);
            } else if let Some(idx) = slots.iter().position(|slot| slot.id == id) {
                tab_order.push(ActiveApp::Tile(idx));
            }
        }
        if tab_order.is_empty() {
            tab_order.push(ActiveApp::Work);
        }

        let default_id = cfg.apps.normalized_default_tab();
        let active = if default_id == "work" {
            ActiveApp::Work
        } else {
            slots
                .iter()
                .position(|slot| slot.id == default_id)
                .map(ActiveApp::Tile)
                .unwrap_or(tab_order[0])
        };

        AppHost {
            slots,
            active,
            tab_order,
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

/// Parse a `Palette` `"R;G;B"` fragment to an sRGB triple (missing channels → 0).
fn rgb(frag: &str) -> sz_kit::Rgb {
    let mut it = frag.split(';').map(|n| n.trim().parse::<u8>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Convert the host's live chrome [`Palette`] into a [`sz_kit::Theme`] so
/// embedded tiles render in the user's exact superzej colors (theme-cycle and
/// `[theme.colors]` overrides included). The field mapping mirrors
/// [`sz_kit::Theme::prism`]; a parity test pins the two together.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_parses_and_tolerates_short_fragments() {
        assert_eq!(rgb("110;231;216"), (110, 231, 216));
        assert_eq!(rgb("10;20"), (10, 20, 0));
        assert_eq!(rgb("bad;data;here"), (0, 0, 0));
    }

    /// The contract: sz-kit's baked prism defaults must equal the host's
    /// default chrome palette, field for field. If superzej changes a default
    /// color, this fails until sz-kit's `Theme::prism()` is updated to match.
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
        let mut cfg = superzej_core::config::Config::default();
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
}
pub mod agent;
