//! `ObserveApp` — the UI-thread view-model for the Observe tab.
//!
//! It owns the dashboard and the latest per-panel frames, and talks to the
//! off-loop [`crate::engine::QueryEngine`] over two channels: `cmd_tx` (requests
//! out, from `handle_input`) and `frames_rx` (results in, drained in [`tick`]).
//! Everything here is synchronous and non-blocking — no I/O, no `await` — so the
//! host loop is never stalled.
//!
//! [`tick`]: ObserveApp::tick

use std::collections::HashMap;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc;

use gtui_core::dashboard::Dashboard;
use gtui_core::datasource::{DataSource, TimeRange};
use gtui_core::frame::Frame;

use crate::engine::{EngineCmd, PanelUpdate, QueryEngine, Waker};

/// The latest state of a single panel.
#[derive(Debug, Clone)]
pub enum PanelState {
    /// No result has arrived yet.
    Loading,
    /// The most recent successful query result (one frame per target).
    Ready(Vec<Frame>),
    /// The most recent query failed; carries a user-facing message.
    Error(String),
}

pub struct ObserveApp {
    pub dashboard: Dashboard,
    pub time_range: TimeRange,
    /// Which panel is focused (index into `dashboard.panels`), for input nav.
    pub selected_panel: usize,
    panels: HashMap<u32, PanelState>,
    frames_rx: mpsc::UnboundedReceiver<PanelUpdate>,
    cmd_tx: mpsc::UnboundedSender<EngineCmd>,
}

impl ObserveApp {
    /// Build the view-model and spawn its query engine on `rt`. `sources` maps a
    /// datasource name (matching `Panel::datasource`) to its backend.
    pub fn new(
        rt: Handle,
        dashboard: Dashboard,
        sources: HashMap<String, Arc<dyn DataSource>>,
        time_range: TimeRange,
        refresh: std::time::Duration,
        waker: Waker,
    ) -> Self {
        let panels = dashboard
            .panels
            .iter()
            .map(|p| (p.id, PanelState::Loading))
            .collect();
        let (cmd_tx, frames_rx) = QueryEngine::spawn(
            rt,
            dashboard.clone(),
            sources,
            time_range.clone(),
            refresh,
            waker,
        );
        Self {
            dashboard,
            time_range,
            selected_panel: 0,
            panels,
            frames_rx,
            cmd_tx,
        }
    }

    /// Drain every pending engine update into the panel map. Returns whether
    /// anything changed (so the tile can report a redraw). Non-blocking.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(update) = self.frames_rx.try_recv() {
            let state = match update.result {
                Ok(frames) => PanelState::Ready(frames),
                Err(e) => PanelState::Error(e.to_string()),
            };
            self.panels.insert(update.panel_id, state);
            changed = true;
        }
        changed
    }

    /// Latest state for a panel (`Loading` until its first result lands).
    pub fn panel_state(&self, panel_id: u32) -> &PanelState {
        self.panels.get(&panel_id).unwrap_or(&PanelState::Loading)
    }

    /// Ask the engine to re-run every panel now. Non-blocking best-effort.
    pub fn request_requery(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Requery);
    }

    /// Change the query window and re-query. Non-blocking best-effort.
    pub fn set_time_range(&mut self, time_range: TimeRange) {
        self.time_range = time_range.clone();
        let _ = self.cmd_tx.send(EngineCmd::SetTimeRange(time_range));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gtui_core::dashboard::builtin_host_dashboard;

    fn now_range() -> TimeRange {
        let to = Utc::now();
        TimeRange {
            from: to - chrono::Duration::minutes(15),
            to,
        }
    }

    #[tokio::test]
    async fn panels_start_loading_and_absent_ids_report_loading() {
        let app = ObserveApp::new(
            Handle::current(),
            builtin_host_dashboard(),
            HashMap::new(),
            now_range(),
            std::time::Duration::from_secs(15),
            Arc::new(|| {}),
        );
        assert!(matches!(app.panel_state(1), PanelState::Loading));
        // An id with no panel still reports Loading, never panics.
        assert!(matches!(app.panel_state(9999), PanelState::Loading));
    }

    #[tokio::test]
    async fn tick_folds_engine_results_into_panel_state() {
        // No sources ⇒ the engine emits an Error PanelUpdate for each panel; a
        // short pump lets those land, exercising the drain path.
        let mut app = ObserveApp::new(
            Handle::current(),
            builtin_host_dashboard(),
            HashMap::new(),
            now_range(),
            std::time::Duration::from_secs(15),
            Arc::new(|| {}),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(app.tick(), "expected at least one panel update to drain");
        assert!(matches!(app.panel_state(1), PanelState::Error(_)));
    }
}
