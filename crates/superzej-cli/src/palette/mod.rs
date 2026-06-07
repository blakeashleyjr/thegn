//! The cmd+k command palette: a native, instant, full-screen iocraft TUI that
//! replaces the old external-picker `menu`. Runs as a floating pane (the
//! `Super+K` keybind already spawns `superzej menu`), so it has the DB, config,
//! git, and every command function in `crate::commands` directly at hand and
//! dispatches actions by calling them — no IPC, no external fuzzy finder.
//!
//! Layout: a prefix-routed input (`mode`), a nucleo-backed streaming result list
//! (`engine` + `sources`, orchestrated by `core`), and a `dispatch` sink that
//! runs the chosen `Action` after the TUI exits.

mod app;
mod core;
mod dispatch;
mod engine;
mod frecency;
mod item;
mod mode;
mod preview;
mod sources;
mod ui;

#[cfg(test)]
mod e2e;
#[cfg(test)]
pub mod testutil;

use crate::config::Config;
use anyhow::Result;
use core::Core;
use item::Row;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

/// State shared between `run` and the iocraft components, passed via context.
/// The plain `Arc`/atomics (not iocraft `State`) are read by the key handler and
/// the dispatch step; mutating them never triggers a redraw on its own.
#[derive(Clone)]
pub struct Shared {
    /// The engine brain; locked briefly each frame to feed input and read rows.
    core: Arc<Mutex<Core>>,
    /// Set when the user activates a row; enacted after the loop exits.
    chosen: Arc<Mutex<Option<Row>>>,
    /// The currently-ranked rows, mirrored each frame so the key handler can
    /// resolve the selected index without re-locking the core.
    current: Arc<Mutex<Vec<Row>>>,
    /// Matched-row count, mirrored each frame so the handler can clamp the cursor.
    total: Arc<AtomicUsize>,
    /// The row the per-item action menu (Tab) was opened on, if any.
    menu_row: Arc<Mutex<Option<Row>>>,
}

/// A fresh shared-state bundle for one palette session.
fn new_shared(cfg: &Config) -> Shared {
    Shared {
        core: Arc::new(Mutex::new(Core::new(cfg.clone()))),
        chosen: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(Vec::new())),
        total: Arc::new(AtomicUsize::new(0)),
        menu_row: Arc::new(Mutex::new(None)),
    }
}

/// After the loop exits (terminal restored), enact the chosen row: record its
/// frecency and run its action. No-op if nothing was chosen.
fn finish(shared: &Shared, cfg: &Config) -> Result<()> {
    let chosen = shared.chosen.lock().unwrap().take();
    if let Some(row) = chosen {
        frecency::bump(&row);
        dispatch::dispatch(cfg, row.action)?;
    }
    Ok(())
}

/// Open the palette and block until the user runs an action or dismisses it.
pub fn run(cfg: &Config) -> Result<()> {
    use iocraft::prelude::*;

    let shared = new_shared(cfg);
    let mut elem = element! {
        ContextProvider(value: Context::owned(shared.clone())) {
            app::Palette
        }
    };
    // iocraft's render loop is async; smol drives it (and the `smol::Timer`
    // poll ticks the sources use). block_on keeps `run` a normal sync command.
    smol::block_on(elem.render_loop().fullscreen())?;

    // The terminal is restored now — safe to spawn panes / run zellij actions.
    finish(&shared, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::item::{Action, Row, RowKind};

    #[test]
    fn new_shared_starts_empty() {
        testutil::sandbox();
        let s = new_shared(&Config::default());
        assert!(s.chosen.lock().unwrap().is_none());
        assert!(s.current.lock().unwrap().is_empty());
        assert_eq!(s.total.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn finish_runs_chosen_action_and_clears_it() {
        testutil::sandbox();
        let cfg = Config::default();
        let shared = new_shared(&cfg);
        // GotoTab is a safe action (zellij no-op without a session).
        *shared.chosen.lock().unwrap() = Some(Row {
            glyph: "x".into(),
            hue: crate::theme::TEAL,
            label: "t".into(),
            detail: String::new(),
            haystack: String::new(),
            kind: RowKind::Tab,
            action: Action::GotoTab("nonexistent/tab".into()),
            frecency_key: Some("nav:probe".into()),
            preview_path: None,
        });
        finish(&shared, &cfg).unwrap();
        assert!(shared.chosen.lock().unwrap().is_none(), "chosen consumed");
    }

    #[test]
    fn finish_is_noop_without_a_choice() {
        testutil::sandbox();
        let cfg = Config::default();
        let shared = new_shared(&cfg);
        finish(&shared, &cfg).unwrap();
    }
}
