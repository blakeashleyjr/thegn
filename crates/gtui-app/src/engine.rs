//! `QueryEngine` — the off-loop query executor.
//!
//! Runs as a single detached tokio task. It owns the datasources, the dashboard,
//! and the outbound `PanelUpdate` channel; on an interval (and on demand) it runs
//! each panel's targets against its datasource and streams the results back to
//! [`crate::app::ObserveApp`], pulsing the host waker after each panel completes.
//!
//! All I/O lives here — never on the UI thread — so the host's "never block the
//! loop / 0% idle" invariant holds: the tile only ever drains a channel.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use gtui_core::dashboard::Dashboard;
use gtui_core::datasource::{DataSource, Query, QueryError, TimeRange};
use gtui_core::frame::Frame;

/// Wake callback fired off-thread after new data lands (posts the slot index +
/// pulses the terminal waker in the host).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A command from the UI thread to the engine. Sent via a non-blocking
/// unbounded channel from `handle_input`, so it never stalls the loop.
pub enum EngineCmd {
    /// Re-run every panel now (manual refresh).
    Requery,
    /// Change the window and re-query.
    SetTimeRange(TimeRange),
}

/// One panel's query result, delivered to the view-model.
pub struct PanelUpdate {
    pub panel_id: u32,
    pub result: Result<Vec<Frame>, QueryError>,
}

pub struct QueryEngine {
    dashboard: Dashboard,
    sources: HashMap<String, Arc<dyn DataSource>>,
    time_range: TimeRange,
    refresh: Duration,
    frames_tx: mpsc::UnboundedSender<PanelUpdate>,
    waker: Waker,
}

impl QueryEngine {
    /// Spawn the engine on `rt` and return the command sender + result receiver.
    /// The task lives until the command sender is dropped (tab closed).
    pub fn spawn(
        rt: Handle,
        dashboard: Dashboard,
        sources: HashMap<String, Arc<dyn DataSource>>,
        time_range: TimeRange,
        refresh: Duration,
        waker: Waker,
    ) -> (
        mpsc::UnboundedSender<EngineCmd>,
        mpsc::UnboundedReceiver<PanelUpdate>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (frames_tx, frames_rx) = mpsc::unbounded_channel();
        let engine = QueryEngine {
            dashboard,
            sources,
            time_range,
            refresh,
            frames_tx,
            waker,
        };
        rt.spawn(engine.run(cmd_rx));
        (cmd_tx, frames_rx)
    }

    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<EngineCmd>) {
        let mut ticker = interval(self.refresh);
        // `interval` fires immediately on the first tick (initial load), then on
        // the cadence; skip catch-up bursts if the machine was asleep.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => self.query_all().await,
                cmd = cmd_rx.recv() => match cmd {
                    Some(EngineCmd::Requery) => self.query_all().await,
                    Some(EngineCmd::SetTimeRange(tr)) => {
                        self.time_range = tr;
                        self.query_all().await;
                    }
                    // Every sender dropped ⇒ the view-model (and tab) is gone.
                    None => break,
                }
            }
        }
    }

    /// Query every panel sequentially, emitting each result (and waking the loop)
    /// as it completes so the UI fills in incrementally. Awaiting here parks the
    /// task, never the UI thread.
    async fn query_all(&self) {
        for panel in &self.dashboard.panels {
            let result = match self.resolve_source(&panel.datasource) {
                Some(source) => {
                    let queries: Vec<Query> = panel
                        .targets
                        .iter()
                        .map(|t| Query {
                            ref_id: t.ref_id.clone(),
                            expr: t.expr.clone(),
                            time_range: self.time_range.clone(),
                        })
                        .collect();
                    source.query(queries).await
                }
                None => Err(QueryError::Other(format!(
                    "no datasource '{}'",
                    panel.datasource
                ))),
            };
            // Best-effort: if the receiver is gone the tab is closing.
            let _ = self.frames_tx.send(PanelUpdate {
                panel_id: panel.id,
                result,
            });
            (self.waker)();
        }
    }

    /// Resolve a panel's datasource by name, falling back to the sole registered
    /// source when the panel leaves it unset (the built-in host dashboard case).
    fn resolve_source(&self, name: &str) -> Option<Arc<dyn DataSource>> {
        if !name.is_empty()
            && let Some(s) = self.sources.get(name)
        {
            return Some(s.clone());
        }
        // Deterministic only with a single source (Phase 1); explicit `datasource`
        // is required once multiple are registered.
        self.sources.values().next().cloned()
    }
}
