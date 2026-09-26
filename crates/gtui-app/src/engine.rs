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

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use gtui_core::dashboard::Dashboard;
use gtui_core::datasource::{DataSource, Query, QueryError, TimeRange};
use gtui_core::frame::Frame;

/// Wake callback fired off-thread after new data lands (posts the slot index +
/// pulses the terminal waker in the host).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A clock seam for resolving relative query windows.
///
/// The engine samples this exactly once at the beginning of each refresh cycle.
/// Keeping the clock outside the query builder makes refresh ranges deterministic
/// in tests and prevents panels in one cycle from observing different instants.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// A duration-based query window. Relative windows are resolved against the
/// refresh clock and therefore advance on every refresh while active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeWindow {
    duration: std::time::Duration,
}

impl RelativeWindow {
    pub fn new(duration: std::time::Duration) -> Self {
        Self { duration }
    }

    pub fn duration(self) -> std::time::Duration {
        self.duration
    }
}

impl RelativeWindow {
    /// Adapt the app boundary, whose `TimeRange` represents a configured live
    /// duration. Observe currently has no historical-range caller, so retaining
    /// the construction-time `to` would create an absolute-looking API that no
    /// product path can use correctly.
    ///
    /// A config reload does not retarget an existing Observe tile; the new
    /// config applies to the next tile. Live reconfiguration belongs to THE-407,
    /// which must add its own generation/cancellation contract.
    fn from_range(range: &TimeRange) -> Self {
        let duration = range
            .to
            .signed_duration_since(range.from)
            .to_std()
            .unwrap_or_default();
        Self::new(duration)
    }
}

/// A command from the UI thread to the engine. Sent via a non-blocking
/// unbounded channel from `handle_input`, so it never stalls the loop.
pub enum EngineCmd {
    /// Re-run every panel now (manual refresh).
    Requery,
    /// Change the legacy live-window duration and re-query.
    SetTimeRange(TimeRange),
    /// Freeze the last resolved relative range and suppress scheduled refreshes.
    Pause,
    /// Resume scheduled refreshes; the next query resolves against the clock.
    Resume,
}

/// One panel's query result, delivered to the view-model.
pub struct PanelUpdate {
    pub panel_id: u32,
    pub result: Result<Vec<Frame>, QueryError>,
}

pub struct QueryEngine {
    dashboard: Dashboard,
    sources: HashMap<String, Arc<dyn DataSource>>,
    window: RelativeWindow,
    clock: Clock,
    last_relative_to: Option<DateTime<Utc>>,
    paused: bool,
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
        Self::spawn_with_window(
            rt,
            dashboard,
            sources,
            RelativeWindow::from_range(&time_range),
            refresh,
            Arc::new(Utc::now),
            waker,
        )
    }

    /// Spawn an engine with an explicit relative window and clock.
    /// Production uses [`Utc::now`] through the default [`Self::spawn`] adapter;
    /// tests and deterministic callers can provide a fake clock here.
    pub fn spawn_with_window(
        rt: Handle,
        dashboard: Dashboard,
        sources: HashMap<String, Arc<dyn DataSource>>,
        window: RelativeWindow,
        refresh: Duration,
        clock: Clock,
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
            window,
            clock,
            last_relative_to: None,
            paused: false,
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
                _ = ticker.tick() => {
                    if !self.paused {
                        self.query_all().await;
                    }
                },
                cmd = cmd_rx.recv() => match cmd {
                    Some(EngineCmd::Requery) => self.query_all().await,
                    Some(EngineCmd::SetTimeRange(tr)) => {
                        self.window = RelativeWindow::from_range(&tr);
                        self.query_all().await;
                    }
                    Some(EngineCmd::Pause) => self.paused = true,
                    Some(EngineCmd::Resume) => {
                        self.paused = false;
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
    async fn query_all(&mut self) {
        // Resolve once before iterating panels. In particular, do not call the
        // clock from the target mapper: sequential panels must share one range.
        let time_range = self.resolve_window();
        for panel in &self.dashboard.panels {
            let result = match self.resolve_source(&panel.datasource) {
                Some(source) => {
                    let queries: Vec<Query> = panel
                        .targets
                        .iter()
                        .map(|t| Query {
                            ref_id: t.ref_id.clone(),
                            expr: t.expr.clone(),
                            time_range: time_range.clone(),
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
                // best-effort: UI may be gone during shutdown
                panel_id: panel.id,
                result,
            });
            (self.waker)();
        }
    }

    fn resolve_window(&mut self) -> TimeRange {
        let to = match (self.paused, self.last_relative_to) {
            (true, Some(frozen)) => frozen,
            _ => {
                let sampled = (self.clock)();
                match self.last_relative_to {
                    Some(previous) if sampled < previous => previous,
                    _ => sampled,
                }
            }
        };
        self.last_relative_to = Some(to);
        let duration = ChronoDuration::from_std(self.window.duration()).ok();
        let from = duration
            .and_then(|duration| to.checked_sub_signed(duration))
            .unwrap_or(to);
        TimeRange { from, to }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use gtui_core::dashboard::{Dashboard, GridPos, Panel, Target};
    use gtui_core::frame::Frame;
    use tokio::runtime::Handle;

    /// Recorded queries, shared with the spawned engine's data source.
    type RecordedQueries = Arc<Mutex<Vec<Vec<Query>>>>;

    /// A spawned engine plus the queries its source observed.
    type EngineHarness = (
        mpsc::UnboundedSender<EngineCmd>,
        mpsc::UnboundedReceiver<PanelUpdate>,
        RecordedQueries,
    );

    struct RecordingSource {
        queries: RecordedQueries,
    }

    impl DataSource for RecordingSource {
        fn query(
            &self,
            queries: Vec<Query>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
            self.queries.lock().unwrap().push(queries);
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn dashboard(panel_count: usize) -> Dashboard {
        Dashboard {
            title: "test".to_string(),
            description: None,
            refresh: None,
            panels: (0..panel_count)
                .map(|id| Panel {
                    id: id as u32,
                    title: format!("panel-{id}"),
                    panel_type: "stat".to_string(),
                    datasource: "test".to_string(),
                    grid_pos: GridPos {
                        x: 0,
                        y: id as u32,
                        w: 1,
                        h: 1,
                    },
                    targets: vec![Target {
                        ref_id: "A".to_string(),
                        expr: "value".to_string(),
                    }],
                })
                .collect(),
        }
    }

    fn spawn_engine(window: RelativeWindow, clock: Clock, refresh: Duration) -> EngineHarness {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut sources: HashMap<String, Arc<dyn DataSource>> = HashMap::new();
        sources.insert(
            "test".to_string(),
            Arc::new(RecordingSource {
                queries: queries.clone(),
            }),
        );
        let (cmd_tx, frames_rx) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard(2),
            sources,
            window,
            refresh,
            clock,
            Arc::new(|| {}),
        );
        (cmd_tx, frames_rx, queries)
    }

    fn clock(times: Vec<DateTime<Utc>>) -> Clock {
        let times = Arc::new(Mutex::new((VecDeque::from(times), None)));
        Arc::new(move || {
            let mut state = times.lock().unwrap();
            let next = state.0.pop_front().or(state.1).expect("empty test clock");
            state.1 = Some(next);
            next
        })
    }

    async fn receive_updates(frames_rx: &mut mpsc::UnboundedReceiver<PanelUpdate>, count: usize) {
        for _ in 0..count {
            tokio::time::timeout(std::time::Duration::from_secs(1), frames_rx.recv())
                .await
                .expect("timed out waiting for the real refresh path")
                .expect("query engine exited before delivering a panel update");
        }
    }

    fn recorded_ranges(queries: &Arc<Mutex<Vec<Vec<Query>>>>) -> Vec<TimeRange> {
        queries
            .lock()
            .unwrap()
            .iter()
            .flat_map(|batch| batch.iter().map(|query| query.time_range.clone()))
            .collect()
    }

    #[tokio::test]
    async fn successive_ticker_refreshes_advance_with_the_same_duration() {
        let first = DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let second = first + ChronoDuration::minutes(5);
        let (cmd_tx, mut frames_rx, queries) = spawn_engine(
            RelativeWindow::new(std::time::Duration::from_secs(15 * 60)),
            clock(vec![first, second]),
            Duration::from_millis(10),
        );

        // The first interval tick is immediate; the second arrives through the
        // same ticker-driven run loop after its configured cadence.
        receive_updates(&mut frames_rx, 4).await;

        let ranges = recorded_ranges(&queries);
        assert!(ranges.len() >= 4);
        assert_eq!(ranges[0].to, first);
        assert_eq!(ranges[1].to, first);
        assert_eq!(ranges[2].to, second);
        assert_eq!(ranges[3].to, second);
        assert_eq!(ranges[0].to - ranges[0].from, ChronoDuration::minutes(15));
        assert_eq!(ranges[2].to - ranges[2].from, ChronoDuration::minutes(15));
        drop(cmd_tx);
    }

    #[tokio::test]
    async fn configured_range_becomes_a_duration_without_retaining_its_end() {
        let from = DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let to = from + ChronoDuration::hours(1);
        assert_eq!(
            RelativeWindow::from_range(&TimeRange { from, to }).duration(),
            std::time::Duration::from_secs(3600)
        );
    }

    #[tokio::test]
    async fn backwards_ticker_clock_keeps_relative_range_ordered_and_monotonic() {
        let first = DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let backwards = first - ChronoDuration::minutes(5);
        let forwards = first + ChronoDuration::minutes(5);
        let (cmd_tx, mut frames_rx, queries) = spawn_engine(
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            clock(vec![first, backwards, forwards]),
            Duration::from_millis(10),
        );

        receive_updates(&mut frames_rx, 6).await;

        let ranges = recorded_ranges(&queries);
        assert!(ranges.len() >= 6);
        assert_eq!(ranges[0].to, first);
        assert_eq!(ranges[2].to, first);
        assert_eq!(ranges[4].to, forwards);
        assert!(ranges.iter().all(|range| range.from <= range.to));
        assert!(
            ranges
                .iter()
                .all(|range| range.to - range.from == ChronoDuration::minutes(1))
        );
        drop(cmd_tx);
    }

    #[tokio::test]
    async fn every_refresh_shares_one_range_across_all_panels() {
        let first = DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (cmd_tx, mut frames_rx, queries) = spawn_engine(
            RelativeWindow::new(std::time::Duration::from_secs(300)),
            clock(vec![first]),
            Duration::from_secs(3600),
        );

        receive_updates(&mut frames_rx, 2).await;

        let ranges = recorded_ranges(&queries);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].from, ranges[1].from);
        assert_eq!(ranges[0].to, ranges[1].to);
        drop(cmd_tx);
    }

    #[tokio::test]
    async fn pause_freezes_and_resume_advances_the_relative_window() {
        let first = DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let second = first + ChronoDuration::minutes(10);
        let (cmd_tx, mut frames_rx, queries) = spawn_engine(
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            clock(vec![first, second]),
            Duration::from_secs(3600),
        );

        receive_updates(&mut frames_rx, 2).await;
        cmd_tx.send(EngineCmd::Pause).unwrap();
        cmd_tx.send(EngineCmd::Requery).unwrap();
        receive_updates(&mut frames_rx, 2).await;
        cmd_tx.send(EngineCmd::Resume).unwrap();
        receive_updates(&mut frames_rx, 2).await;

        let ranges = recorded_ranges(&queries);
        assert_eq!(ranges[0].to, first);
        assert_eq!(ranges[2].to, first);
        assert_eq!(ranges[4].to, second);
        drop(cmd_tx);
    }
}
