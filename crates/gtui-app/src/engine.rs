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

/// A fixed, historical query window. Its range is copied as-is on every
/// refresh and never consults the refresh clock.
#[derive(Debug, Clone)]
pub struct AbsoluteWindow {
    range: TimeRange,
}

impl AbsoluteWindow {
    pub fn new(range: TimeRange) -> Self {
        Self { range }
    }

    pub fn range(&self) -> TimeRange {
        self.range.clone()
    }
}

/// The policy used to resolve a panel query window.
///
/// The distinct wrapper types are intentional: an absolute range cannot be
/// accidentally advanced, and a relative duration cannot accidentally retain a
/// construction-time end instant.
#[derive(Debug, Clone)]
pub enum QueryWindow {
    Relative(RelativeWindow),
    Absolute(AbsoluteWindow),
}

impl QueryWindow {
    pub fn relative(duration: std::time::Duration) -> Self {
        Self::Relative(RelativeWindow::new(duration))
    }

    pub fn absolute(range: TimeRange) -> Self {
        Self::Absolute(AbsoluteWindow::new(range))
    }

    /// Adapt the legacy app boundary, whose `TimeRange` represents a configured
    /// live duration. Historical callers should use [`QueryWindow::absolute`].
    /// A configuration reload must preserve this variant and replace only its
    /// duration unless the caller explicitly chooses a historical range.
    fn relative_from_range(range: &TimeRange) -> Self {
        let duration = range
            .to
            .signed_duration_since(range.from)
            .to_std()
            .unwrap_or_default();
        Self::relative(duration)
    }
}

/// A command from the UI thread to the engine. Sent via a non-blocking
/// unbounded channel from `handle_input`, so it never stalls the loop.
pub enum EngineCmd {
    /// Re-run every panel now (manual refresh).
    Requery,
    /// Change the legacy live-window duration and re-query.
    SetTimeRange(TimeRange),
    /// Replace the window policy and re-query. This is the explicit path for
    /// fixed historical ranges.
    SetWindow(QueryWindow),
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
    window: QueryWindow,
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
            QueryWindow::relative_from_range(&time_range),
            refresh,
            Arc::new(Utc::now),
            waker,
        )
    }

    /// Spawn an engine with an explicit relative or absolute window and clock.
    /// Production uses [`Utc::now`] through the default [`Self::spawn`] adapter;
    /// tests and deterministic callers can provide a fake clock here.
    pub fn spawn_with_window(
        rt: Handle,
        dashboard: Dashboard,
        sources: HashMap<String, Arc<dyn DataSource>>,
        window: QueryWindow,
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
                        self.window = QueryWindow::relative_from_range(&tr);
                        self.query_all().await;
                    }
                    Some(EngineCmd::SetWindow(window)) => {
                        self.window = window;
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
        match self.window.clone() {
            QueryWindow::Absolute(window) => window.range(),
            QueryWindow::Relative(window) => {
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
                let duration = ChronoDuration::from_std(window.duration()).ok();
                let from = duration
                    .and_then(|duration| to.checked_sub_signed(duration))
                    .unwrap_or(to);
                TimeRange { from, to }
            }
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

    #[cfg(test)]
    fn pause(&mut self) {
        self.paused = true;
    }

    #[cfg(test)]
    fn resume(&mut self) {
        self.paused = false;
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

    struct RecordingSource {
        queries: Arc<Mutex<Vec<Vec<Query>>>>,
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

    fn engine(
        window: QueryWindow,
        clock: Clock,
        queries: Arc<Mutex<Vec<Vec<Query>>>>,
    ) -> QueryEngine {
        let mut sources: HashMap<String, Arc<dyn DataSource>> = HashMap::new();
        sources.insert("test".to_string(), Arc::new(RecordingSource { queries }));
        let (frames_tx, _frames_rx) = mpsc::unbounded_channel();
        QueryEngine {
            dashboard: dashboard(2),
            sources,
            window,
            clock,
            last_relative_to: None,
            paused: false,
            refresh: Duration::from_secs(60),
            frames_tx,
            waker: Arc::new(|| {}),
        }
    }

    fn clock(times: Vec<DateTime<Utc>>) -> Clock {
        let times = Arc::new(Mutex::new(VecDeque::from(times)));
        Arc::new(move || {
            times
                .lock()
                .unwrap()
                .pop_front()
                .expect("test clock exhausted")
        })
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
    async fn relative_windows_advance_with_the_same_duration() {
        let first = DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let second = first + ChronoDuration::minutes(5);
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(
            QueryWindow::relative(std::time::Duration::from_secs(15 * 60)),
            clock(vec![first, second]),
            queries.clone(),
        );

        engine.query_all().await;
        engine.query_all().await;

        let ranges = recorded_ranges(&queries);
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges[0].to, first);
        assert_eq!(ranges[1].to, first);
        assert_eq!(ranges[2].to, second);
        assert_eq!(ranges[3].to, second);
        assert_eq!(ranges[0].to - ranges[0].from, ChronoDuration::minutes(15));
        assert_eq!(ranges[2].to - ranges[2].from, ChronoDuration::minutes(15));
    }

    #[tokio::test]
    async fn absolute_windows_are_immutable_and_do_not_read_the_clock() {
        let from = DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let to = from + ChronoDuration::hours(1);
        let clock_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = clock_calls.clone();
        let clock: Clock = Arc::new(move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Utc::now()
        });
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(
            QueryWindow::absolute(TimeRange { from, to }),
            clock,
            queries.clone(),
        );

        engine.query_all().await;
        engine.query_all().await;

        let ranges = recorded_ranges(&queries);
        assert_eq!(clock_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(
            ranges
                .iter()
                .all(|range| range.from == from && range.to == to)
        );
    }

    #[tokio::test]
    async fn every_panel_in_a_refresh_shares_one_range() {
        let now = DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(
            QueryWindow::relative(std::time::Duration::from_secs(300)),
            clock(vec![now]),
            queries.clone(),
        );

        engine.query_all().await;

        let ranges = recorded_ranges(&queries);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].from, ranges[1].from);
        assert_eq!(ranges[0].to, ranges[1].to);
    }

    #[tokio::test]
    async fn backwards_clock_keeps_relative_range_ordered_and_monotonic() {
        let first = DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let backwards = first - ChronoDuration::minutes(5);
        let forwards = first + ChronoDuration::minutes(5);
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(
            QueryWindow::relative(std::time::Duration::from_secs(60)),
            clock(vec![first, backwards, forwards]),
            queries.clone(),
        );

        engine.query_all().await;
        engine.query_all().await;
        engine.query_all().await;

        let ranges = recorded_ranges(&queries);
        assert_eq!(ranges[0].to, first);
        assert_eq!(ranges[2].to, first);
        assert_eq!(ranges[4].to, forwards);
        assert!(ranges.iter().all(|range| range.from <= range.to));
        assert!(
            ranges
                .iter()
                .all(|range| range.to - range.from == ChronoDuration::minutes(1))
        );
    }

    #[tokio::test]
    async fn pause_freezes_and_resume_advances_the_relative_window() {
        let first = DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let second = first + ChronoDuration::minutes(10);
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(
            QueryWindow::relative(std::time::Duration::from_secs(60)),
            clock(vec![first, second]),
            queries.clone(),
        );

        engine.query_all().await;
        engine.pause();
        engine.query_all().await;
        engine.resume();
        engine.query_all().await;

        let ranges = recorded_ranges(&queries);
        assert_eq!(ranges[0].to, first);
        assert_eq!(ranges[2].to, first);
        assert_eq!(ranges[4].to, second);
    }
}
