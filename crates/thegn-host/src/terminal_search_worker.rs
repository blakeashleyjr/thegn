//! Off-loop terminal history search.
//!
//! The search overlay used to score every keystroke against the whole
//! scrollback on the event loop. A fuzzy match over a busy workspace (hundreds
//! of thousands of lines) blocked input and rendering for the duration — the
//! one thing the loop exists to never do.
//!
//! `TerminalSearchWorker` owns one parked thread that does the scoring. The loop
//! hands it a *snapshot* of the in-scope histories plus the query, mode and
//! options, and receives results back in bounded batches; each delivery pulses
//! the loop through the wake callback (the terminal's `TerminalWaker`, the same
//! wake path every other off-loop producer uses, openspec `event-loop`).
//!
//! The rules that keep the loop honest:
//!
//! - **Latest request wins.** Each submit bumps a generation and stamps it into
//!   the request under the same lock that queues it. The worker drops a request
//!   it has not started and abandons a scan in flight at its next cancellation
//!   poll. It tags results with the generation *of the request it scored*, never
//!   with a freshly loaded one, so a batch produced just before a newer
//!   keystroke can never be mistaken for the newer query's answer.
//! - **The loop never waits on the worker.** `drain` only ever does a
//!   `try_recv`; the bounded channel is backpressure for the *worker*.
//! - **The loop touches no history.** Scoring reads the snapshot, never the
//!   live buffers.
//! - **Snapshots are cheap.** A history buffer stores `Arc<str>` lines, so
//!   snapshotting a changed pane bumps one refcount per line (no string copy),
//!   and a pane whose history did not change is reused as the same `Arc`.
//! - **Nothing runs while the overlay is closed.** The worker is parked in a
//!   condvar wait when idle, and dropping it invalidates the generation and
//!   wakes the thread so it exits.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use thegn_core::history::HistoryBuffer;
use thegn_core::search::{
    MatchMode, MatchOptions, SearchEngine, SearchMatch, SearchScope, SearchSource,
};

/// Matches handed to the loop per batch: small enough that the first screenful
/// paints on the first wake, large enough that the channel is not the cost.
const RESULT_BATCH: usize = 64;

/// Batches the worker may run ahead of the loop before it blocks. The loop
/// drains everything queued on every wake, so this is pure backpressure.
const RESULT_QUEUE: usize = 4;

/// One pane's history, owned by the worker side so scoring never borrows the
/// live `Panes` (which only the event loop may touch).
pub(crate) struct SourceSnapshot {
    pub(crate) id: u32,
    pub(crate) label: String,
    pub(crate) history: HistoryBuffer,
}

impl SourceSnapshot {
    /// True when this snapshot still describes `history` exactly: same pane,
    /// same label, same line count and same write count (a rewrite of a line
    /// bumps the write count, so an in-place edit is not missed).
    fn matches(&self, id: u32, label: &str, history: &HistoryBuffer) -> bool {
        self.id == id
            && self.label == label
            && self.history.len() == history.len()
            && self.history.total_pushed() == history.total_pushed()
    }
}

/// The overlay's snapshot of every pane in scope. One `Arc` per pane, so an
/// unchanged pane is shared across keystrokes instead of re-copied.
pub(crate) type SnapshotSources = Vec<Arc<SourceSnapshot>>;

/// Snapshot `sources` for the worker, reusing any `previous` entry whose pane
/// is unchanged. A changed pane is re-cloned, which is one refcount bump per
/// line (history lines are `Arc<str>`), never a string copy. Previous entries
/// are indexed by pane id so this is linear in the pane count.
pub(crate) fn take_snapshot(
    sources: &[SearchSource<'_>],
    previous: &[Arc<SourceSnapshot>],
) -> SnapshotSources {
    let by_id: HashMap<u32, &Arc<SourceSnapshot>> =
        previous.iter().map(|old| (old.id, old)).collect();
    sources
        .iter()
        .map(|&(id, label, history)| match by_id.get(&id) {
            Some(old) if old.matches(id, label, history) => Arc::clone(old),
            _ => Arc::new(SourceSnapshot {
                id,
                label: label.to_owned(),
                history: history.clone(),
            }),
        })
        .collect()
}

/// Borrow a snapshot as the engine's source list.
pub(crate) fn source_refs(snapshot: &[Arc<SourceSnapshot>]) -> Vec<SearchSource<'_>> {
    snapshot
        .iter()
        .map(|source| (source.id, source.label.as_str(), &source.history))
        .collect()
}

/// One search: everything the worker needs, owned, so the loop can get back to
/// pumping input the moment it is submitted.
pub(crate) struct SearchRequest {
    /// Stamped by [`TerminalSearchWorker::submit`]; callers pass `0`. The worker
    /// tags every message with this value and polls it for cancellation.
    pub(crate) generation: u64,
    pub(crate) scope: SearchScope,
    pub(crate) max_results: usize,
    pub(crate) query: String,
    pub(crate) mode: MatchMode,
    pub(crate) options: MatchOptions,
    pub(crate) sources: SnapshotSources,
}

/// What one [`TerminalSearchWorker::drain`] pulled out of the channel.
#[derive(Default)]
pub(crate) struct Drained {
    /// New results in rank order; concatenate in order.
    pub(crate) batches: Vec<Vec<SearchMatch>>,
    /// `Some((total, error))` once the current generation finished.
    pub(crate) done: Option<(usize, Option<String>)>,
    /// The worker thread is gone (it panicked) and nothing more will arrive.
    pub(crate) disconnected: bool,
}

impl Drained {
    pub(crate) fn is_empty(&self) -> bool {
        self.batches.is_empty() && self.done.is_none() && !self.disconnected
    }
}

/// A channel message. Every message carries the generation that produced it so
/// the loop can drop the results of a request it has already superseded.
enum Message {
    Batch {
        generation: u64,
        matches: Vec<SearchMatch>,
    },
    Done {
        generation: u64,
        total: usize,
        error: Option<String>,
    },
}

#[derive(Default)]
struct Inner {
    /// The newest submitted request, if the worker has not picked it up yet.
    pending: Option<SearchRequest>,
    /// Set by `Drop`: the thread exits instead of waiting for more work.
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    inner: Mutex<Inner>,
    /// Signalled on submit (work arrived) and on drop (exit).
    wait: Condvar,
}

impl Shared {
    /// A panicking scan must not wedge the overlay: the state is only ever
    /// "which request is pending", so a poisoned lock is still usable.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Callback that nudges the event loop awake after a delivery.
pub(crate) type WakeFn = Box<dyn Fn() + Send>;

pub(crate) struct TerminalSearchWorker {
    shared: Arc<Shared>,
    results: Receiver<Message>,
    /// Generation of the most recent submit; the thread polls it to decide
    /// whether its current scan is still wanted.
    current: Arc<AtomicU64>,
    /// Loop-side counter mirrored into `current`.
    generation: u64,
    #[cfg(test)]
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TerminalSearchWorker {
    /// Spawn the parked worker. `None` when the OS refuses the thread; the
    /// caller then scores on the loop instead of showing a stuck spinner.
    pub(crate) fn new(wake: WakeFn) -> Option<Self> {
        let shared = Arc::new(Shared::default());
        let current = Arc::new(AtomicU64::new(0));
        let (tx, results) = sync_channel(RESULT_QUEUE);
        let thread_shared = Arc::clone(&shared);
        let thread_current = Arc::clone(&current);
        let spawned = thegn_core::thread_name::try_spawn_named("thegn-term-srch", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            run_worker(&thread_shared, &thread_current, &tx, wake.as_ref());
        });
        let _handle = spawned.ok()?;
        Some(TerminalSearchWorker {
            shared,
            results,
            current,
            generation: 0,
            #[cfg(test)]
            handle: Some(_handle),
        })
    }

    /// Hand `request` to the worker, superseding anything queued or running.
    pub(crate) fn submit(&mut self, mut request: SearchRequest) {
        self.generation += 1;
        request.generation = self.generation;
        {
            // Publish the new generation and queue the request under one lock,
            // so the worker can never take a request and then observe a newer
            // generation than the one stamped on it.
            let mut inner = self.shared.lock();
            self.current.store(self.generation, Ordering::Release);
            inner.pending = Some(request);
        }
        self.shared.wait.notify_one();
    }

    /// Take everything the worker produced for the current generation.
    pub(crate) fn drain(&self) -> Drained {
        let mut drained = Drained::default();
        loop {
            match self.results.try_recv() {
                Ok(Message::Batch {
                    generation,
                    matches,
                }) => {
                    if generation == self.generation {
                        drained.batches.push(matches);
                    }
                }
                Ok(Message::Done {
                    generation,
                    total,
                    error,
                }) => {
                    if generation == self.generation {
                        drained.done = Some((total, error));
                    }
                }
                Err(TryRecvError::Empty) => return drained,
                Err(TryRecvError::Disconnected) => {
                    drained.disconnected = true;
                    return drained;
                }
            }
        }
    }
}

impl Drop for TerminalSearchWorker {
    fn drop(&mut self) {
        // Invalidate the active generation first: a scan in flight stops at its
        // next poll instead of running to completion for a closed overlay.
        self.current.fetch_add(1, Ordering::AcqRel);
        {
            let mut inner = self.shared.lock();
            inner.pending = None;
            inner.stopped = true;
        }
        self.shared.wait.notify_one();
        // The thread owns its results sender, so it wakes up, sees `stopped`
        // and returns. Its `JoinHandle` is detached: nothing here waits.
    }
}

/// The worker thread: park until a request arrives, score it, stream it.
fn run_worker(shared: &Shared, current: &AtomicU64, tx: &SyncSender<Message>, wake: &dyn Fn()) {
    // One engine across requests keeps the literal refine cache alive: a
    // growing query re-filters the previous hits instead of rescanning every
    // line of the corpus.
    let mut engine = SearchEngine::new(SearchScope::Worktree, 1);
    loop {
        let Some(request) = take_request(shared) else {
            return;
        };
        // The generation stamped on this request, never a fresh load of
        // `current`: a newer submit must make this scan stale, not relabel it.
        let generation = request.generation;
        let sources = source_refs(&request.sources);
        apply_request(&mut engine, &request);
        let cancelled = || current.load(Ordering::Acquire) != generation;
        if !engine.recompute_cancellable(&sources, &cancelled) {
            // Superseded mid-scan: drop these results and take the newer
            // request (it is already in `pending`).
            continue;
        }
        let total = engine.total();
        let error = engine.error().map(str::to_owned);
        if !stream(tx, wake, current, generation, engine.matches()) {
            continue;
        }
        let done = Message::Done {
            generation,
            total,
            error,
        };
        if tx.send(done).is_err() {
            // The receiver is gone: the overlay closed.
            return;
        }
        // A superseded generation just means a newer request is already queued.
        pulse(wake, current, generation);
    }
}

/// Block until there is a request; `None` means the worker was dropped.
fn take_request(shared: &Shared) -> Option<SearchRequest> {
    let mut inner = shared.lock();
    loop {
        if let Some(request) = inner.pending.take() {
            return Some(request);
        }
        if inner.stopped {
            return None;
        }
        inner = shared
            .wait
            .wait(inner)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

/// Apply only what changed: the `queue_*` setters reset the refine cache, and
/// resetting it on every keystroke would throw away the incremental scan.
fn apply_request(engine: &mut SearchEngine, request: &SearchRequest) {
    if engine.scope() != request.scope {
        engine.queue_scope(request.scope);
    }
    if engine.mode() != request.mode {
        engine.queue_mode(request.mode);
    }
    if engine.options() != request.options {
        engine.queue_options(request.options);
    }
    engine.queue_max_results(request.max_results);
    if engine.query() != request.query {
        engine.queue_query(&request.query);
    }
}

/// Send `matches` in bounded batches. `false` when the scan was superseded or
/// the loop is gone; the caller then drops the rest.
fn stream(
    tx: &SyncSender<Message>,
    wake: &dyn Fn(),
    current: &AtomicU64,
    generation: u64,
    matches: &[SearchMatch],
) -> bool {
    for batch in matches.chunks(RESULT_BATCH) {
        if current.load(Ordering::Acquire) != generation {
            return false;
        }
        let message = Message::Batch {
            generation,
            matches: batch.to_vec(),
        };
        if tx.send(message).is_err() {
            return false;
        }
        if !pulse(wake, current, generation) {
            return false;
        }
    }
    current.load(Ordering::Acquire) == generation
}

/// Pulse the event loop out of its input wait. `false` when the scan was
/// superseded while the batch was in flight, so the caller stops streaming it.
///
/// Best-effort: a pulse that cannot be written is logged by the callback and not
/// retried here. The loop blocks with no polling timeout (openspec
/// `event-loop`), so a lost pulse leaves results undrawn until the next input
/// event; stopping the scan over it would strand the overlay on `searching…`.
fn pulse(wake: &dyn Fn(), current: &AtomicU64, generation: u64) -> bool {
    if current.load(Ordering::Acquire) != generation {
        return false;
    }
    wake();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn corpus(lines: usize) -> SnapshotSources {
        let mut history = HistoryBuffer::new(lines.max(1));
        for i in 0..lines {
            if i % 3 == 0 {
                history.push_line(format!("ssh zzz line {i}"));
            } else {
                history.push_line(format!("ssh line {i}"));
            }
        }
        vec![Arc::new(SourceSnapshot {
            id: 1,
            label: "pane".into(),
            history,
        })]
    }

    fn request(
        sources: &SnapshotSources,
        query: &str,
        mode: MatchMode,
        max_results: usize,
    ) -> SearchRequest {
        SearchRequest {
            generation: 0,
            scope: SearchScope::Pane(1),
            max_results,
            query: query.into(),
            mode,
            options: MatchOptions::default(),
            sources: sources.clone(),
        }
    }

    fn noop_wake() -> WakeFn {
        Box::new(|| {})
    }

    /// A wake callback whose calls block until `release` is dropped, plus a
    /// receiver that fires each time the worker enters it.
    fn gated_wake() -> (WakeFn, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let wake: WakeFn = Box::new(move || {
            entered_tx.send(()).ok();
            release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .ok();
        });
        (wake, entered_rx, release_tx)
    }

    /// Drain until the current generation's `Done`, returning what streamed.
    fn collect(worker: &TerminalSearchWorker) -> (Vec<SearchMatch>, usize, Option<String>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut matches = Vec::new();
        loop {
            let drained = worker.drain();
            assert!(!drained.disconnected, "the worker thread died");
            for batch in drained.batches {
                matches.extend(batch);
            }
            if let Some((total, error)) = drained.done {
                return (matches, total, error);
            }
            assert!(Instant::now() < deadline, "no Done within the deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn sync_ranking(
        sources: &SnapshotSources,
        query: &str,
        mode: MatchMode,
        max_results: usize,
    ) -> (Vec<SearchMatch>, usize) {
        let refs = source_refs(sources);
        let mut engine = SearchEngine::new(SearchScope::Pane(1), max_results);
        engine.set_mode(mode, &refs);
        engine.set_query(query, &refs);
        (engine.matches().to_vec(), engine.total())
    }

    #[test]
    fn a_superseded_generation_is_dropped() {
        let sources = corpus(300);
        let (wake, entered, release) = gated_wake();
        let mut worker = TerminalSearchWorker::new(wake).expect("spawn");

        // "ssh" matches every line: several batches, so the worker is parked in
        // its first wake with the rest of the stream still to send.
        worker.submit(request(&sources, "ssh", MatchMode::Literal, 1000));
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("worker streamed its first batch");
        worker.submit(request(&sources, "zzz", MatchMode::Literal, 1000));
        drop(release);

        let (matches, total, error) = collect(&worker);
        let (expected, expected_total) = sync_ranking(&sources, "zzz", MatchMode::Literal, 1000);
        assert_eq!(error, None);
        assert_eq!(total, expected_total);
        assert_eq!(matches, expected, "only the newer query's results arrive");
        assert!(matches.iter().all(|m| m.line.contains("zzz")));
    }

    #[test]
    fn closing_the_overlay_mid_scan_stops_the_thread() {
        let sources = corpus(300);
        let (wake, entered, release) = gated_wake();
        let mut worker = TerminalSearchWorker::new(wake).expect("spawn");
        let handle = worker.handle.take().expect("join handle");

        worker.submit(request(&sources, "ssh", MatchMode::Literal, 1000));
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("worker streamed its first batch");
        drop(worker);
        drop(release);

        handle.join().expect("worker thread exits cleanly");
    }

    #[test]
    fn a_dropped_idle_worker_exits() {
        let mut worker = TerminalSearchWorker::new(noop_wake()).expect("spawn");
        let handle = worker.handle.take().expect("join handle");
        drop(worker);
        handle.join().expect("parked worker wakes and exits");
    }

    #[test]
    fn streamed_results_equal_the_synchronous_ranking() {
        let sources = corpus(700);
        let cases = [
            ("ssh", MatchMode::Literal, 1000),
            ("zzz", MatchMode::Literal, 1000),
            ("", MatchMode::Literal, 1000),
            ("sshline", MatchMode::Fuzzy, 1000),
            ("ssh", MatchMode::Literal, 50),
        ];
        let mut worker = TerminalSearchWorker::new(noop_wake()).expect("spawn");
        for (query, mode, max) in cases {
            worker.submit(request(&sources, query, mode, max));
            let (matches, total, error) = collect(&worker);
            let (expected, expected_total) = sync_ranking(&sources, query, mode, max);
            assert_eq!(error, None, "query {query:?}");
            assert_eq!(total, expected_total, "query {query:?}");
            assert_eq!(matches, expected, "query {query:?} ({mode:?}, max {max})");
        }
    }

    #[test]
    fn a_panicking_worker_reports_disconnect() {
        let sources = corpus(10);
        let mut worker = TerminalSearchWorker::new(Box::new(|| {
            panic!("boom");
        }))
        .expect("spawn");
        worker.submit(request(&sources, "ssh", MatchMode::Literal, 1000));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if worker.drain().disconnected {
                return;
            }
            assert!(Instant::now() < deadline, "disconnect never reported");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn snapshots_reuse_unchanged_panes_and_reclone_changed_ones() {
        let mut quiet = HistoryBuffer::new(10);
        let mut busy = HistoryBuffer::new(10);
        quiet.push_line("one".into());
        busy.push_line("two".into());
        let first = take_snapshot(&[(1, "quiet", &quiet), (2, "busy", &busy)], &[]);

        busy.push_line("three".into());
        let second = take_snapshot(&[(1, "quiet", &quiet), (2, "busy", &busy)], &first);
        assert!(Arc::ptr_eq(&first[0], &second[0]), "unchanged pane shared");
        assert!(!Arc::ptr_eq(&first[1], &second[1]), "changed pane re-taken");
        assert_eq!(second[1].history.len(), 2);

        // A rewrite keeps the line count but bumps the write count.
        assert!(busy.replace_from_end(1, "THREE".into()));
        let third = take_snapshot(&[(1, "quiet", &quiet), (2, "busy", &busy)], &second);
        assert!(!Arc::ptr_eq(&second[1], &third[1]));
        assert_eq!(third[1].history.get(1), Some("THREE"));
    }
}
