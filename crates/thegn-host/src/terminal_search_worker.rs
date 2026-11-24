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
//! the loop through the terminal's [`TerminalWaker`], the same wake path every
//! other off-loop producer uses (openspec `event-loop`).
//!
//! The rules that keep the loop honest:
//!
//! - **Latest request wins.** Each submit bumps a generation. The worker drops a
//!   request it has not started and abandons a scan in flight at its next
//!   cancellation poll. Results carry their generation, so a batch produced just
//!   before a newer keystroke is discarded on the loop side instead of landing
//!   in the newer query's list.
//! - **The loop never waits on the worker.** `drain` only ever does a
//!   `try_recv`; the bounded channel is backpressure for the *worker*.
//! - **The loop touches no history.** Scoring reads the snapshot, never the
//!   live buffers.
//! - **Nothing is copied per keystroke.** A pane's history is snapshotted once
//!   and reused, as the same `Arc`, until that pane's history changes.
//! - **Nothing runs while the overlay is closed.** The worker is parked in a
//!   condvar wait when idle, and dropping it invalidates the generation and
//!   wakes the thread so it exits.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use termwiz::terminal::TerminalWaker;

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

/// The overlay's snapshot of every pane in scope. One `Arc` per pane, so an
/// unchanged pane is shared across keystrokes instead of re-copied.
pub(crate) type SnapshotSources = Vec<Arc<SourceSnapshot>>;

/// Identity of one pane's history: a change means the snapshot must be rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotKey {
    id: u32,
    label: String,
    lines: usize,
    pushed: u64,
}

impl SnapshotKey {
    fn of(id: u32, label: &str, history: &HistoryBuffer) -> Self {
        SnapshotKey {
            id,
            label: label.to_owned(),
            lines: history.len(),
            pushed: history.total_pushed(),
        }
    }
}

/// The keys for `sources`, in order. Cheap: two integers and a label per pane.
pub(crate) fn snapshot_keys(sources: &[SearchSource<'_>]) -> Vec<SnapshotKey> {
    sources
        .iter()
        .map(|&(id, label, history)| SnapshotKey::of(id, label, history))
        .collect()
}

/// Copy `sources` for the worker, reusing any `previous` entry whose history is
/// unchanged (matched by [`SnapshotKey`]).
pub(crate) fn take_snapshot(
    sources: &[SearchSource<'_>],
    previous: &[Arc<SourceSnapshot>],
) -> SnapshotSources {
    sources
        .iter()
        .map(|&(id, label, history)| {
            let key = SnapshotKey::of(id, label, history);
            previous
                .iter()
                .find(|old| SnapshotKey::of(old.id, &old.label, &old.history) == key)
                .cloned()
                .unwrap_or_else(|| {
                    Arc::new(SourceSnapshot {
                        id,
                        label: label.to_owned(),
                        history: history.clone(),
                    })
                })
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
}

impl Drained {
    pub(crate) fn is_empty(&self) -> bool {
        self.batches.is_empty() && self.done.is_none()
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

pub(crate) struct TerminalSearchWorker {
    shared: Arc<Shared>,
    results: Receiver<Message>,
    /// Generation of the most recent submit; the thread reads it to decide
    /// whether its current scan is still wanted.
    current: Arc<AtomicU64>,
    /// Loop-side counter mirrored into `current`.
    generation: u64,
}

impl TerminalSearchWorker {
    /// Spawn the parked worker. `None` when the OS refuses the thread — the
    /// caller then scores on the loop instead of showing a stuck spinner.
    pub(crate) fn new(waker: TerminalWaker) -> Option<Self> {
        let shared = Arc::new(Shared::default());
        let current = Arc::new(AtomicU64::new(0));
        let (tx, results) = sync_channel(RESULT_QUEUE);
        let thread_shared = Arc::clone(&shared);
        let thread_current = Arc::clone(&current);
        let spawned = thegn_core::thread_name::try_spawn_named("thegn-term-srch", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            run_worker(&thread_shared, &thread_current, &tx, &waker);
        });
        if spawned.is_err() {
            return None;
        }
        Some(TerminalSearchWorker {
            shared,
            results,
            current,
            generation: 0,
        })
    }

    /// Hand `request` to the worker, superseding anything queued or running.
    pub(crate) fn submit(&mut self, request: SearchRequest) {
        self.generation += 1;
        // Publish before queueing: the worker may observe the new generation
        // while still finishing the old scan, and must not stream that scan's
        // results as if they answered this request.
        self.current.store(self.generation, Ordering::Release);
        {
            let mut inner = self.shared.lock();
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
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return drained,
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
fn run_worker(
    shared: &Shared,
    current: &AtomicU64,
    tx: &SyncSender<Message>,
    waker: &TerminalWaker,
) {
    // One engine across requests keeps the literal refine cache alive: a
    // growing query re-filters the previous hits instead of rescanning every
    // line of the corpus.
    let mut engine = SearchEngine::new(SearchScope::Worktree, 1);
    loop {
        let Some(request) = take_request(shared) else {
            return;
        };
        let generation = current.load(Ordering::Acquire);
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
        if !stream(tx, waker, current, generation, engine.matches()) {
            continue;
        }
        let done = Message::Done {
            generation,
            total,
            error,
        };
        if tx.send(done).is_err() || !pulse(waker, current, generation) {
            return;
        }
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
    waker: &TerminalWaker,
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
        if !pulse(waker, current, generation) {
            return false;
        }
    }
    current.load(Ordering::Acquire) == generation
}

/// Pulse the event loop out of its input poll. `false` when the scan was
/// superseded while the batch was in flight, so the caller stops streaming it.
///
/// A pulse that cannot be written is *not* fatal: the loop's own poll timeout
/// still picks the batch up (openspec `event-loop`), and stopping over a lost
/// nudge would strand the overlay on `searching…` with no way to restart it.
fn pulse(waker: &TerminalWaker, current: &AtomicU64, generation: u64) -> bool {
    if current.load(Ordering::Acquire) != generation {
        return false;
    }
    if let Err(error) = waker.wake() {
        tracing::debug!(%error, "terminal search waker pulse failed");
    }
    true
}
