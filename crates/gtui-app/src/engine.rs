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
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, MissedTickBehavior, interval};

use gtui_core::dashboard::Dashboard;
use gtui_core::datasource::{DataSource, Query, QueryError, TimeRange};
use gtui_core::frame::Frame;

/// Wake callback fired off-thread after new data lands (posts the slot index +
/// pulses the terminal waker in the host).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Shared wake state between the UI-side view-model and the engine task.
///
/// * `visible` is flipped synchronously by the UI thread, so a result that
///   completes after the tab was hidden never wakes the host loop (the engine
///   only learns of the hide via its command queue, between cycles).
/// * `pending` coalesces wakes: at most one host wake is outstanding until the
///   UI side drains ([`WakeGate::drained`]). Producers that find it already set
///   skip the callback, because the pending drain will see their result too.
#[derive(Debug)]
pub struct WakeGate {
    visible: AtomicBool,
    pending: AtomicBool,
}

impl WakeGate {
    pub fn new(visible: bool) -> Arc<Self> {
        Arc::new(Self {
            visible: AtomicBool::new(visible),
            pending: AtomicBool::new(false),
        })
    }

    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Release);
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    /// Fire `waker` unless hidden or a wake is already pending.
    fn wake(&self, waker: &Waker) {
        if self.is_visible() && !self.pending.swap(true, Ordering::AcqRel) {
            waker();
        }
    }

    /// The UI side is about to drain: clear the pending flag *before* reading
    /// the queue so a result landing mid-drain schedules a fresh wake.
    pub fn drained(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

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
    /// The tab was shown (`true`, refreshes immediately) or hidden (`false`,
    /// parks the refresh ticker and every datasource's background sampling).
    Visible(bool),
}

/// One panel's query result, delivered to the view-model.
pub struct PanelUpdate {
    pub panel_id: u32,
    pub targets: Vec<TargetUpdate>,
    /// The resolved timestamp window used by this refresh, shared by all panels.
    pub window: TimeRange,
}

/// One target's independently queryable result, retaining its dashboard identity.
#[derive(Debug, Clone)]
pub struct TargetUpdate {
    pub ref_id: String,
    pub result: Result<Vec<Frame>, String>,
}

pub struct QueryEngine {
    dashboard: Dashboard,
    sources: HashMap<String, Arc<dyn DataSource>>,
    window: RelativeWindow,
    clock: Clock,
    last_relative_to: Option<DateTime<Utc>>,
    paused: bool,
    hidden: bool,
    refresh: Duration,
    frames_tx: mpsc::Sender<PanelUpdate>,
    waker: Waker,
    gate: Arc<WakeGate>,
}

/// Bound dashboard fan-out independently of dashboard size.
const MAX_CONCURRENT_PANELS: usize = 4;
/// Bound completed panel results waiting for the UI to drain them.
#[cfg(not(test))]
const RESULT_CHANNEL_CAPACITY: usize = 16;
#[cfg(test)]
const RESULT_CHANNEL_CAPACITY: usize = 2;
/// Bound target fan-out and the amount of per-panel state retained by a refresh.
const MAX_TARGETS_PER_PANEL: usize = 32;
/// A panel that does not finish cannot hold the next refresh forever.
#[cfg(not(test))]
const PANEL_DEADLINE: Duration = Duration::from_secs(15);
#[cfg(test)]
const PANEL_DEADLINE: Duration = Duration::from_millis(300);
type PanelQueryJob = (
    u32,
    String,
    Option<Arc<dyn DataSource>>,
    Vec<(String, Query)>,
);

enum QueryOutcome {
    Complete(bool),
    Refresh,
    Hidden,
    Disconnected,
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
        gate: Arc<WakeGate>,
    ) -> (
        mpsc::UnboundedSender<EngineCmd>,
        mpsc::Receiver<PanelUpdate>,
    ) {
        Self::spawn_with_window(
            rt,
            dashboard,
            sources,
            RelativeWindow::from_range(&time_range),
            refresh,
            Arc::new(Utc::now),
            waker,
            gate,
        )
    }

    /// Spawn an engine with an explicit relative window and clock.
    /// Production uses [`Utc::now`] through the default [`Self::spawn`] adapter;
    /// tests and deterministic callers can provide a fake clock here.
    #[allow(clippy::too_many_arguments)] // the full injection seam; `spawn` is the short form
    pub fn spawn_with_window(
        rt: Handle,
        dashboard: Dashboard,
        sources: HashMap<String, Arc<dyn DataSource>>,
        window: RelativeWindow,
        refresh: Duration,
        clock: Clock,
        waker: Waker,
        gate: Arc<WakeGate>,
    ) -> (
        mpsc::UnboundedSender<EngineCmd>,
        mpsc::Receiver<PanelUpdate>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (frames_tx, frames_rx) = mpsc::channel(RESULT_CHANNEL_CAPACITY);
        let engine = QueryEngine {
            dashboard,
            sources,
            window,
            clock,
            last_relative_to: None,
            paused: false,
            hidden: !gate.is_visible(),
            refresh,
            frames_tx,
            waker,
            gate,
        };
        rt.spawn(engine.run(cmd_rx));
        (cmd_tx, frames_rx)
    }

    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<EngineCmd>) {
        let mut ticker = interval(self.refresh);
        // `interval` fires immediately on the first tick (initial load), then on
        // the cadence; skip catch-up bursts if the machine was asleep.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        if self.hidden {
            self.set_sources_active(false);
        }
        loop {
            tokio::select! {
                // Disabled while hidden: the branch future is not polled, so a
                // hidden tab holds no armed timer and takes no wakes.
                _ = ticker.tick(), if !self.hidden => {
                    if !self.paused && !self.refresh_and_coalesce(&mut cmd_rx, &mut ticker).await {
                        break;
                    }
                },
                cmd = cmd_rx.recv() => match cmd {
                    Some(cmd) => {
                        if self.apply_command(cmd, &mut ticker)
                            && !self.refresh_and_coalesce(&mut cmd_rx, &mut ticker).await
                        {
                            break;
                        }
                    }
                    // Every sender dropped => the view-model (and tab) is gone.
                    None => break,
                }
            }
        }
    }

    /// Apply queued state changes in order and collapse all refresh requests
    /// received during a refresh into one follow-up refresh.
    async fn refresh_and_coalesce(
        &mut self,
        cmd_rx: &mut mpsc::UnboundedReceiver<EngineCmd>,
        ticker: &mut tokio::time::Interval,
    ) -> bool {
        let mut outcome = self.query_all(cmd_rx, ticker).await;
        loop {
            let mut refresh = match outcome {
                QueryOutcome::Complete(refresh) => refresh,
                QueryOutcome::Refresh => true,
                QueryOutcome::Hidden => false,
                QueryOutcome::Disconnected => return false,
            };
            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => refresh |= self.apply_command(cmd, ticker),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return false,
                }
            }
            if !refresh || self.hidden {
                return true;
            }
            outcome = self.query_all(cmd_rx, ticker).await;
        }
    }

    /// Apply one command; return whether it requests an immediate refresh.
    fn apply_command(&mut self, cmd: EngineCmd, ticker: &mut tokio::time::Interval) -> bool {
        match cmd {
            EngineCmd::Requery => !self.hidden,
            EngineCmd::SetTimeRange(tr) => {
                self.window = RelativeWindow::from_range(&tr);
                !self.hidden
            }
            EngineCmd::Pause => {
                self.paused = true;
                false
            }
            EngineCmd::Resume => {
                self.paused = false;
                !self.hidden
            }
            EngineCmd::Visible(false) => {
                if !self.hidden {
                    self.hidden = true;
                    self.set_sources_active(false);
                }
                false
            }
            EngineCmd::Visible(true) => {
                if self.hidden {
                    self.hidden = false;
                    self.set_sources_active(true);
                    ticker.reset();
                    true
                } else {
                    false
                }
            }
        }
    }

    fn set_sources_active(&self, active: bool) {
        for source in self.sources.values() {
            source.set_active(active);
        }
    }

    /// Query panels with a sliding concurrency window, emitting each result as it completes.
    /// The UI thread is never involved in the waits and fan-out is capped.
    async fn query_all(
        &mut self,
        cmd_rx: &mut mpsc::UnboundedReceiver<EngineCmd>,
        ticker: &mut tokio::time::Interval,
    ) -> QueryOutcome {
        // Resolve once so every panel in a refresh sees an identical window.
        let time_range = self.resolve_window();
        let jobs: Vec<_> = self
            .dashboard
            .panels
            .iter()
            .map(|panel| {
                let source = self.resolve_source(&panel.datasource);
                let targets = panel
                    .targets
                    .iter()
                    .map(|t| {
                        (
                            t.ref_id.clone(),
                            Query {
                                ref_id: t.ref_id.clone(),
                                expr: t.expr.clone(),
                                time_range: time_range.clone(),
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                (panel.id, panel.datasource.clone(), source, targets)
            })
            .collect();
        let mut pending = JoinSet::new();
        let mut job_ids = HashMap::new();
        let mut jobs = jobs.into_iter();
        let spawn_job = |pending: &mut JoinSet<(u32, Vec<TargetUpdate>)>,
                         job_ids: &mut HashMap<tokio::task::Id, (u32, Vec<String>)>,
                         job: PanelQueryJob| {
            let (panel_id, datasource, source, targets) = job;
            let has_more_targets = targets.len() > MAX_TARGETS_PER_PANEL;
            let target_ids = targets
                .iter()
                .take(MAX_TARGETS_PER_PANEL)
                .map(|(ref_id, _)| ref_id.clone())
                .collect::<Vec<_>>();
            let targets = targets
                .into_iter()
                .take(MAX_TARGETS_PER_PANEL)
                .collect::<Vec<_>>();
            let abort = pending.spawn(async move {
                let mut results = Vec::with_capacity(targets.len());
                let deadline = tokio::time::Instant::now() + PANEL_DEADLINE;
                for (ref_id, query) in targets {
                    let result = match &source {
                        Some(source) => {
                            tokio::time::timeout_at(deadline, source.query(vec![query]))
                                .await
                                .unwrap_or(Err(QueryError::Timeout))
                                .map_err(|error| error.to_string())
                        }
                        None => Err(format!("no datasource '{datasource}'")),
                    };
                    results.push(TargetUpdate { ref_id, result });
                }
                if has_more_targets {
                    results.push(TargetUpdate {
                        ref_id: "…".into(),
                        result: Err(format!(
                            "additional targets omitted (limit {MAX_TARGETS_PER_PANEL})"
                        )),
                    });
                }
                (panel_id, results)
            });
            let mut target_ids = target_ids;
            if has_more_targets {
                target_ids.push("…".into());
            }
            drop(job_ids.insert(abort.id(), (panel_id, target_ids)));
        };
        for job in jobs.by_ref().take(MAX_CONCURRENT_PANELS) {
            spawn_job(&mut pending, &mut job_ids, job);
        }
        let mut refresh = false;
        while !pending.is_empty() {
            tokio::select! {
                joined = pending.join_next_with_id() => {
                    let Some(joined) = joined else { break };
                    let update = match joined {
                        Ok((id, (panel_id, targets))) => {
                            drop(job_ids.remove(&id));
                            Some(PanelUpdate {
                                panel_id,
                                targets,
                                window: time_range.clone(),
                            })
                        }
                        Err(join_error) => job_ids.remove(&join_error.id()).map(|(panel_id, target_ids)| {
                            let message = format!("panel query panicked: {join_error}");
                            let targets = target_ids
                                .into_iter()
                                .map(|ref_id| TargetUpdate {
                                    ref_id,
                                    result: Err(message.clone()),
                                })
                                .collect();
                            PanelUpdate {
                                panel_id,
                                targets,
                                window: time_range.clone(),
                            }
                        }),
                    };
                    if let Some(update) = update {
                        // Backpressure here also delays command handling while the UI is not
                        // draining (for example, while this tile is hidden). A queued hide,
                        // close, or time-range change is handled after capacity returns.
                        if self.frames_tx.send(update).await.is_err() {
                            pending.abort_all();
                            return QueryOutcome::Disconnected;
                        }
                        self.gate.wake(&self.waker);
                    }
                    if let Some(job) = jobs.next() {
                        spawn_job(&mut pending, &mut job_ids, job);
                    }
                },
                command = cmd_rx.recv() => match command {
                    None => {
                        pending.abort_all();
                        return QueryOutcome::Disconnected;
                    }
                    Some(EngineCmd::Visible(false)) => {
                        self.apply_command(EngineCmd::Visible(false), ticker);
                        pending.abort_all();
                        return QueryOutcome::Hidden;
                    }
                    Some(command @ EngineCmd::SetTimeRange(_)) => {
                        self.apply_command(command, ticker);
                        pending.abort_all();
                        return QueryOutcome::Refresh;
                    }
                    Some(command) => refresh |= self.apply_command(command, ticker),
                }
            }
        }
        QueryOutcome::Complete(refresh)
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

    /// Resolve an exact datasource name, or the sole source for an unnamed panel.
    fn resolve_source(&self, name: &str) -> Option<Arc<dyn DataSource>> {
        if name.is_empty() {
            if self.sources.len() == 1 {
                return self.sources.values().next().cloned();
            }
            return None;
        }
        self.sources.get(name).cloned()
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
        mpsc::Receiver<PanelUpdate>,
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
            WakeGate::new(true),
        );
        (cmd_tx, frames_rx, queries)
    }

    /// Spawn an engine over a counting source with an observable waker + gate.
    fn spawn_counting(
        panels: usize,
        refresh: Duration,
    ) -> (
        mpsc::UnboundedSender<EngineCmd>,
        mpsc::Receiver<PanelUpdate>,
        RecordedQueries,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<WakeGate>,
    ) {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut sources: HashMap<String, Arc<dyn DataSource>> = HashMap::new();
        sources.insert(
            "test".to_string(),
            Arc::new(RecordingSource {
                queries: queries.clone(),
            }),
        );
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wakes_cb = wakes.clone();
        let gate = WakeGate::new(true);
        let (cmd_tx, frames_rx) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard(panels),
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            refresh,
            Arc::new(Utc::now),
            Arc::new(move || {
                wakes_cb.fetch_add(1, Ordering::SeqCst);
            }),
            gate.clone(),
        );
        (cmd_tx, frames_rx, queries, wakes, gate)
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

    async fn receive_updates(frames_rx: &mut mpsc::Receiver<PanelUpdate>, count: usize) {
        for _ in 0..count {
            tokio::time::timeout(std::time::Duration::from_secs(1), frames_rx.recv())
                .await
                .expect("timed out waiting for the real refresh path")
                .expect("query engine exited before delivering a panel update");
        }
    }

    struct ConcurrentSource {
        active: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl DataSource for ConcurrentSource {
        fn query(
            &self,
            _queries: Vec<Query>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
            let active = self.active.clone();
            let peak = self.peak.clone();
            Box::pin(async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(Vec::new())
            })
        }
    }

    #[tokio::test]
    async fn panel_queries_are_concurrent_and_capped() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source: Arc<dyn DataSource> = Arc::new(ConcurrentSource {
            active,
            peak: peak.clone(),
        });
        let sources = HashMap::from([("test".to_string(), source)]);
        let (cmd, mut updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard(11),
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );
        receive_updates(&mut updates, 11).await;
        assert!(peak.load(Ordering::SeqCst) > 1, "panels should overlap");
        assert!(peak.load(Ordering::SeqCst) <= MAX_CONCURRENT_PANELS);
        drop(cmd);
    }

    #[tokio::test]
    async fn undrained_result_queue_bounds_buffering_and_resumes_progress() {
        let (cmd, mut updates, queries, _, _) = spawn_counting(12, Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(updates.len() <= RESULT_CHANNEL_CAPACITY);
        assert!(
            queries.lock().unwrap().len() <= RESULT_CHANNEL_CAPACITY + MAX_CONCURRENT_PANELS + 1,
            "completed result backpressure allowed too many panel queries to start"
        );

        receive_updates(&mut updates, 12).await;
        drop(cmd);
    }

    struct OneHungPanelSource;

    impl DataSource for OneHungPanelSource {
        fn query(
            &self,
            queries: Vec<Query>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
            if queries[0].expr == "hang" {
                Box::pin(std::future::pending())
            } else {
                Box::pin(async { Ok(Vec::new()) })
            }
        }
    }

    struct DropObservedSource {
        entered: Arc<tokio::sync::Notify>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    struct DropObserved(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropObserved {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl DataSource for DropObservedSource {
        fn query(
            &self,
            _queries: Vec<Query>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
            let entered = self.entered.clone();
            let dropped = self.dropped.clone();
            Box::pin(async move {
                let _guard = DropObserved(dropped);
                entered.notify_one();
                std::future::pending::<Result<Vec<Frame>, QueryError>>().await
            })
        }
    }

    fn spawn_drop_observed_engine() -> (
        mpsc::UnboundedSender<EngineCmd>,
        mpsc::Receiver<PanelUpdate>,
        Arc<tokio::sync::Notify>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sources = HashMap::from([(
            "test".to_string(),
            Arc::new(DropObservedSource {
                entered: entered.clone(),
                dropped: dropped.clone(),
            }) as Arc<dyn DataSource>,
        )]);
        let (cmd, updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard(1),
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );
        (cmd, updates, entered, dropped)
    }

    #[tokio::test]
    async fn dropping_command_sender_cancels_hung_panel_immediately() {
        let (cmd, mut updates, entered, dropped) = spawn_drop_observed_engine();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("hung query was never polled");
        drop(cmd);

        tokio::time::timeout(PANEL_DEADLINE / 2, async {
            assert!(updates.recv().await.is_none(), "engine remained alive");
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect did not cancel the query before its deadline");
    }

    #[tokio::test]
    async fn hiding_during_hung_panel_cancels_without_publishing() {
        let (cmd, mut updates, entered, dropped) = spawn_drop_observed_engine();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("hung query was never polled");
        cmd.send(EngineCmd::Visible(false)).unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("hidden panel query was not cancelled");
        assert!(
            tokio::time::timeout(PANEL_DEADLINE + Duration::from_millis(200), updates.recv())
                .await
                .is_err(),
            "hidden panel published an update"
        );
        drop(cmd);
    }

    #[tokio::test]
    async fn fast_panels_progress_while_a_sibling_is_hung() {
        let mut dashboard = dashboard(MAX_CONCURRENT_PANELS * 2 + 1);
        dashboard.panels[0].targets[0].expr = "hang".into();
        let sources = HashMap::from([(
            "test".to_string(),
            Arc::new(OneHungPanelSource) as Arc<dyn DataSource>,
        )]);
        let (cmd, mut updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard,
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );
        let mut arrived = Vec::new();
        for _ in 0..(MAX_CONCURRENT_PANELS * 2) {
            arrived.push(
                tokio::time::timeout(Duration::from_secs(1), updates.recv())
                    .await
                    .expect("fast panel stalled behind hung sibling")
                    .expect("query engine exited before fast panel update")
                    .panel_id,
            );
        }
        assert!(!arrived.contains(&0));
        arrived.sort_unstable();
        assert_eq!(
            arrived,
            (1..=MAX_CONCURRENT_PANELS as u32 * 2).collect::<Vec<_>>()
        );
        drop(cmd);
    }

    #[tokio::test]
    async fn panel_deadline_covers_all_targets_instead_of_restarting_per_target() {
        let mut panel_dashboard = dashboard(1);
        for ref_id in ["B", "C"] {
            panel_dashboard.panels[0].targets.push(Target {
                ref_id: ref_id.into(),
                expr: "hang".into(),
            });
        }
        panel_dashboard.panels[0].targets[0].expr = "hang".into();
        let sources = HashMap::from([(
            "test".to_string(),
            Arc::new(OneHungPanelSource) as Arc<dyn DataSource>,
        )]);
        let (cmd, mut updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            panel_dashboard,
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );

        let update = tokio::time::timeout(PANEL_DEADLINE + Duration::from_secs(1), updates.recv())
            .await
            .expect("panel deadline restarted for each target")
            .expect("query engine exited before deadline result");
        assert_eq!(update.targets.len(), 3);
        assert!(
            update
                .targets
                .iter()
                .all(|target| matches!(&target.result, Err(error) if error == "timeout"))
        );
        drop(cmd);
    }

    struct GatedRecordingSource {
        queries: RecordedQueries,
        entered: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
    }

    impl DataSource for GatedRecordingSource {
        fn query(
            &self,
            queries: Vec<Query>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
            self.queries.lock().unwrap().push(queries);
            let entered = self.entered.clone();
            let release = self.release.clone();
            Box::pin(async move {
                entered.add_permits(1);
                release.acquire().await.unwrap().forget();
                Ok(Vec::new())
            })
        }
    }

    #[tokio::test]
    async fn refresh_commands_during_work_coalesce_to_one_follow_up() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let sources = HashMap::from([(
            "test".to_string(),
            Arc::new(GatedRecordingSource {
                queries: queries.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }) as Arc<dyn DataSource>,
        )]);
        let (cmd, mut updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard(2),
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );
        entered.acquire_many(2).await.unwrap().forget();
        for _ in 0..5 {
            cmd.send(EngineCmd::Requery).unwrap();
        }
        release.add_permits(20);
        receive_updates(&mut updates, 4).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(updates.try_recv().is_err());

        assert_eq!(
            recorded_ranges(&queries).len(),
            4,
            "five queued refresh requests should produce one follow-up cycle"
        );
        drop(cmd);
    }

    struct PanicSource;

    impl DataSource for PanicSource {
        fn query(
            &self,
            _queries: Vec<Query>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
            Box::pin(async { panic!("fixture panic") })
        }
    }

    #[tokio::test]
    async fn panicking_panel_query_becomes_a_target_error() {
        let sources = HashMap::from([(
            "test".to_string(),
            Arc::new(PanicSource) as Arc<dyn DataSource>,
        )]);
        let (cmd, mut updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard(1),
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );
        let update = tokio::time::timeout(Duration::from_secs(1), updates.recv())
            .await
            .expect("panicking query failed to produce a result")
            .unwrap();
        assert!(
            matches!(&update.targets[0].result, Err(error) if error.contains("panel query panicked"))
        );
        drop(cmd);
    }

    struct MixedTargetSource;

    impl DataSource for MixedTargetSource {
        fn query(
            &self,
            queries: Vec<Query>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
            let query = queries.into_iter().next().unwrap();
            Box::pin(async move {
                if query.ref_id == "B" {
                    Err(QueryError::Other("target B failed".into()))
                } else {
                    Ok(vec![Frame::new(Vec::new())])
                }
            })
        }
    }

    #[tokio::test]
    async fn target_failure_does_not_erase_successful_sibling() {
        let mut dashboard = dashboard(1);
        dashboard.panels[0].targets.push(Target {
            ref_id: "B".into(),
            expr: "other".into(),
        });
        let sources = HashMap::from([(
            "test".to_string(),
            Arc::new(MixedTargetSource) as Arc<dyn DataSource>,
        )]);
        let (cmd, mut updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard,
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );
        let update = tokio::time::timeout(Duration::from_secs(1), updates.recv())
            .await
            .expect("target results did not arrive")
            .unwrap();
        assert_eq!(update.targets[0].ref_id, "A");
        assert_eq!(update.targets[0].result.as_ref().unwrap().len(), 1);
        assert_eq!(update.targets[1].ref_id, "B");
        assert!(
            matches!(&update.targets[1].result, Err(error) if error.contains("target B failed"))
        );
        drop(cmd);
    }

    struct CountingTargetSource(Arc<std::sync::atomic::AtomicUsize>);

    impl DataSource for CountingTargetSource {
        fn query(
            &self,
            _queries: Vec<Query>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[tokio::test]
    async fn excess_targets_are_bounded_and_reported() {
        let mut dashboard = dashboard(1);
        for index in 1..=MAX_TARGETS_PER_PANEL {
            dashboard.panels[0].targets.push(Target {
                ref_id: format!("T{index}"),
                expr: "value".into(),
            });
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sources = HashMap::from([(
            "test".to_string(),
            Arc::new(CountingTargetSource(calls.clone())) as Arc<dyn DataSource>,
        )]);
        let (cmd, mut updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            dashboard,
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );
        let update = tokio::time::timeout(Duration::from_secs(1), updates.recv())
            .await
            .expect("bounded target result did not arrive")
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), MAX_TARGETS_PER_PANEL);
        assert_eq!(update.targets.len(), MAX_TARGETS_PER_PANEL + 1);
        assert!(
            matches!(&update.targets[MAX_TARGETS_PER_PANEL].result, Err(error) if error.contains("additional targets omitted"))
        );
        drop(cmd);
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

        let update = tokio::time::timeout(std::time::Duration::from_secs(1), frames_rx.recv())
            .await
            .expect("timed out waiting for the real refresh path")
            .expect("query engine exited before delivering a panel update");
        assert_eq!(update.window.to, first);
        assert_eq!(update.window.from, first - ChronoDuration::seconds(60));
        receive_updates(&mut frames_rx, 1).await;
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

    #[tokio::test]
    async fn hidden_tile_produces_no_wakes_or_queries_and_resumes_with_an_immediate_refresh() {
        let (cmd_tx, mut frames_rx, queries, wakes, gate) =
            spawn_counting(2, Duration::from_millis(10));
        receive_updates(&mut frames_rx, 2).await;

        // Hide: the UI thread flips the gate synchronously, then tells the engine.
        gate.set_visible(false);
        cmd_tx.send(EngineCmd::Visible(false)).unwrap();
        // Let any in-flight cycle finish, drain, then measure a quiet window
        // spanning many refresh periods.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        while frames_rx.try_recv().is_ok() {}
        gate.drained();
        let wakes_before = wakes.load(Ordering::SeqCst);
        let queries_before = queries.lock().unwrap().len();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            wakes_before,
            "hidden tile woke the host"
        );
        assert_eq!(
            queries.lock().unwrap().len(),
            queries_before,
            "hidden tile kept querying"
        );
        assert!(
            frames_rx.try_recv().is_err(),
            "hidden tile delivered results"
        );

        // Show: an immediate refresh lands without waiting a refresh period.
        gate.set_visible(true);
        cmd_tx.send(EngineCmd::Visible(true)).unwrap();
        receive_updates(&mut frames_rx, 2).await;
        assert!(wakes.load(Ordering::SeqCst) > wakes_before);
        drop(cmd_tx);
    }

    #[tokio::test]
    async fn wakes_coalesce_to_one_pending_until_the_ui_drains() {
        let (cmd_tx, mut frames_rx, _queries, wakes, gate) =
            spawn_counting(5, Duration::from_secs(3600));
        receive_updates(&mut frames_rx, 5).await;
        // Five panels completed with no drain in between: one wake, not five.
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        // After the UI drains, the next result wakes again.
        gate.drained();
        cmd_tx.send(EngineCmd::Requery).unwrap();
        receive_updates(&mut frames_rx, 5).await;
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        drop(cmd_tx);
    }

    #[tokio::test]
    async fn unknown_named_datasource_errors_with_multiple_registered_sources() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut panel_dashboard = dashboard(1);
        panel_dashboard.panels[0].datasource = "typo".into();
        let sources = HashMap::from([
            (
                "alpha".into(),
                Arc::new(RecordingSource {
                    queries: queries.clone(),
                }) as Arc<dyn DataSource>,
            ),
            (
                "beta".into(),
                Arc::new(RecordingSource {
                    queries: queries.clone(),
                }) as Arc<dyn DataSource>,
            ),
        ]);
        let (cmd, mut updates) = QueryEngine::spawn_with_window(
            Handle::current(),
            panel_dashboard,
            sources,
            RelativeWindow::new(std::time::Duration::from_secs(60)),
            Duration::from_secs(3600),
            Arc::new(Utc::now),
            Arc::new(|| {}),
            WakeGate::new(true),
        );
        let update = tokio::time::timeout(Duration::from_secs(1), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&update.targets[0].result, Err(message) if message == "no datasource 'typo'")
        );
        assert!(
            queries.lock().unwrap().is_empty(),
            "unknown source must not route to a configured backend"
        );
        drop(cmd);
    }

    #[tokio::test]
    async fn unnamed_datasource_errors_with_two_sources_in_either_insertion_order() {
        for names in [["alpha", "beta"], ["beta", "alpha"]] {
            let queries = Arc::new(Mutex::new(Vec::new()));
            let mut panel_dashboard = dashboard(1);
            panel_dashboard.panels[0].datasource.clear();
            let mut sources: HashMap<String, Arc<dyn DataSource>> = HashMap::new();
            for name in names {
                sources.insert(
                    name.into(),
                    Arc::new(RecordingSource {
                        queries: queries.clone(),
                    }),
                );
            }
            let (cmd, mut updates) = QueryEngine::spawn_with_window(
                Handle::current(),
                panel_dashboard,
                sources,
                RelativeWindow::new(std::time::Duration::from_secs(60)),
                Duration::from_secs(3600),
                Arc::new(Utc::now),
                Arc::new(|| {}),
                WakeGate::new(true),
            );
            let update = tokio::time::timeout(Duration::from_secs(1), updates.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(&update.targets[0].result, Err(message) if message == "no datasource ''")
            );
            assert!(
                queries.lock().unwrap().is_empty(),
                "unnamed source must not route based on map order"
            );
            drop(cmd);
        }
    }
}
