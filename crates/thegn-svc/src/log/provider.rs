use notify::{
    Watcher,
    event::{EventKind, ModifyKind},
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TrySendError},
    },
};

const MIN_READ_GAP: Duration = Duration::from_secs(1);
// Missing log directories retry every 30 seconds; watching their nearest
// existing ancestor is a follow-up for zero-wake fresh-state startup.
const WATCH_RETRY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Wake {
    Nudge,
    Retry,
    Closed,
}

pub trait LogProvider: Send + Sync {
    /// Start fetching and streaming logs into the provided sender
    fn start_stream(&self, tx: super::ingress::LogIngress, waker: Arc<dyn Fn() + Send + Sync>);
}

pub struct FileLogProvider {
    pub path: PathBuf,
}

/// Clamp and parse source lines into bounded pushes. `push` returns whether
/// its queue transitioned from empty and therefore needs a host wake.
fn push_bounded(
    lines: Vec<String>,
    mut push: impl FnMut(Vec<thegn_core::log::parser::ParsedLog>) -> bool,
) -> bool {
    let mut woke = false;
    let mut batch = Vec::with_capacity(super::ingress::MAX_BATCH_RECORDS);
    let mut batch_bytes = 0usize;
    for mut line in lines {
        super::ingress::clamp_record(&mut line);
        let parsed = thegn_core::log::parser::parse_log(&line);
        let parsed_bytes = super::ingress::parsed_cost(&parsed);
        if !batch.is_empty()
            && (batch.len() >= super::ingress::MAX_BATCH_RECORDS
                || batch_bytes.saturating_add(parsed_bytes) > super::ingress::MAX_BATCH_BYTES)
        {
            woke |= push(std::mem::take(&mut batch));
            batch = Vec::with_capacity(super::ingress::MAX_BATCH_RECORDS);
            batch_bytes = 0;
        }
        batch_bytes = batch_bytes.saturating_add(parsed_bytes);
        batch.push(parsed);
    }
    if !batch.is_empty() {
        woke |= push(batch);
    }
    woke
}

impl LogProvider for FileLogProvider {
    fn start_stream(&self, tx: super::ingress::LogIngress, waker: Arc<dyn Fn() + Send + Sync>) {
        let path = self.path.clone();
        // QoS: Background, set by the host hook (follow-up).
        drop(thegn_core::thread_name::spawn_named(
            "thegn-log-tail",
            move || tail_file(&path, tx, waker),
        ));
    }
}

/// Tail `path`, parsing appended lines into batches pushed over `tx` and pulsing
/// `waker`. Starts at end-of-file (we don't replay historical logs — the view is
/// live tail, not a backfill) and exits on the next file event after the consumer
/// is gone.
fn tail_file(
    path: &std::path::Path,
    tx: super::ingress::LogIngress,
    waker: Arc<dyn Fn() + Send + Sync>,
) {
    let mut follower = super::tail::FileFollower::new(path.to_path_buf());
    let watcher: Mutex<Option<(notify::RecommendedWatcher, Receiver<()>, Arc<AtomicBool>)>> =
        Mutex::new(None);
    let mut last_read = None;
    tail_loop(
        || {
            let lines = follower.poll();
            (lines, follower.has_more())
        },
        |lines| {
            // The follower bounds source reads to 64 KiB plus its 1 MiB
            // partial-record cap; parsed copies stay in smaller batches.
            if push_bounded(lines, |batch| tx.push(batch)) {
                waker();
            }
        },
        TailControl {
            ensure_watcher: || {
                let mut watcher = watcher
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if watcher.is_some() {
                    return true;
                }
                match watch_log_directory(path) {
                    Ok((created, events, reset_requested)) => {
                        *watcher = Some((created, events, reset_requested));
                        true
                    }
                    Err(error) => {
                        tracing::debug!(target: "thegn::log", %error, "log file watcher unavailable; retrying slowly");
                        false
                    }
                }
            },
            wait: |watching, retry| {
                if tx.is_orphaned() {
                    return Wake::Closed;
                }
                let wake = {
                    let watcher = watcher
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    watcher.as_ref().map(|(_, events, reset_requested)| {
                        events.recv().map(|()| {
                            if reset_requested.swap(false, Ordering::AcqRel) {
                                Wake::Closed
                            } else {
                                Wake::Nudge
                            }
                        })
                    })
                };
                if let Some(wake) = wake {
                    match wake {
                        Ok(wake) => wake,
                        Err(_) => Wake::Closed,
                    }
                } else if watching {
                    Wake::Closed
                } else {
                    std::thread::sleep(retry);
                    Wake::Retry
                }
            },
            orphaned: || tx.is_orphaned(),
            watcher_closed: || {
                *watcher
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            },
        },
        TailClock {
            now: Instant::now,
            sleep: std::thread::sleep,
        },
        &mut last_read,
    );
}

fn watch_log_directory(
    path: &Path,
) -> notify::Result<(notify::RecommendedWatcher, Receiver<()>, Arc<AtomicBool>)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().map(std::ffi::OsStr::to_os_string);
    let parent_path = parent.to_path_buf();
    let reset_requested = Arc::new(AtomicBool::new(false));
    let callback_reset = Arc::clone(&reset_requested);
    let (sender, receiver) = mpsc::sync_channel(1);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            let kind = &event.kind;
            let is_change = matches!(
                kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            );
            let is_metadata = matches!(kind, EventKind::Modify(ModifyKind::Metadata(_)));
            let is_parent_reset = matches!(kind, EventKind::Create(_) | EventKind::Remove(_))
                && event
                    .paths
                    .iter()
                    .any(|event_path| event_path == &parent_path);
            let is_file_change = is_change
                && !is_metadata
                && file_name.as_deref().is_some_and(|name| {
                    event
                        .paths
                        .iter()
                        .any(|event_path| event_path.file_name() == Some(name))
                });
            let signal = if is_parent_reset {
                callback_reset.store(true, Ordering::Release);
                true
            } else if is_file_change {
                true
            } else {
                false
            };
            if signal {
                // best-effort: a full queue records a pending read; a disconnected
                // receiver means the tailer is already shutting down.
                match sender.try_send(()) {
                    Ok(()) | Err(TrySendError::Full(())) => {}
                    Err(TrySendError::Disconnected(())) => {}
                }
            }
        }
    })?;
    watcher.watch(parent, notify::RecursiveMode::NonRecursive)?;
    if let Some(grandparent) = parent.parent()
        && grandparent.is_dir()
    {
        watcher.watch(grandparent, notify::RecursiveMode::NonRecursive)?;
    }
    Ok((watcher, receiver, reset_requested))
}

struct TailControl<E, W, O, C> {
    ensure_watcher: E,
    wait: W,
    orphaned: O,
    watcher_closed: C,
}

struct TailClock<N, S> {
    now: N,
    sleep: S,
}

fn tail_loop<P, U, E, W, O, C, N, S>(
    mut poll: P,
    mut push: U,
    mut control: TailControl<E, W, O, C>,
    mut clock: TailClock<N, S>,
    last_read: &mut Option<Instant>,
) where
    P: FnMut() -> (Vec<String>, bool),
    U: FnMut(Vec<String>),
    E: FnMut() -> bool,
    W: FnMut(bool, Duration) -> Wake,
    O: FnMut() -> bool,
    C: FnMut(),
    N: FnMut() -> Instant,
    S: FnMut(Duration),
{
    loop {
        if (control.orphaned)() {
            break;
        }
        let watching = (control.ensure_watcher)();
        if let Some(previous) = *last_read {
            let elapsed = (clock.now)().saturating_duration_since(previous);
            if elapsed < MIN_READ_GAP {
                (clock.sleep)(MIN_READ_GAP - elapsed);
            }
        }
        loop {
            if (control.orphaned)() {
                return;
            }
            let (lines, more) = poll();
            *last_read = Some((clock.now)());
            push(lines);
            if !more {
                break;
            }
        }
        match (control.wait)(watching, WATCH_RETRY) {
            Wake::Nudge => {}
            Wake::Retry => {}
            Wake::Closed => (control.watcher_closed)(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{Read, Write};

    fn noop_waker() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    fn fake_clock<'a>(
        now: &'a Cell<Instant>,
        sleeps: &'a Cell<usize>,
        slept: &'a Cell<Duration>,
    ) -> TailClock<impl FnMut() -> Instant + 'a, impl FnMut(Duration) + 'a> {
        TailClock {
            now: move || now.get(),
            sleep: move |duration| {
                sleeps.set(sleeps.get() + 1);
                slept.set(slept.get() + duration);
                now.set(now.get() + duration);
            },
        }
    }

    #[test]
    fn idle_tailer_does_not_poll_without_events() {
        let now = Cell::new(Instant::now());
        let sleeps = Cell::new(0);
        let slept = Cell::new(Duration::ZERO);
        let polls = Cell::new(0);
        let nudges = Cell::new(0);
        let stopped = Cell::new(false);
        let mut last_read: Option<Instant> = None;

        tail_loop(
            || {
                polls.set(polls.get() + 1);
                (Vec::new(), false)
            },
            |_| {},
            TailControl {
                ensure_watcher: || true,
                wait: |_, _| {
                    if nudges.get() < 3 {
                        nudges.set(nudges.get() + 1);
                        Wake::Nudge
                    } else {
                        stopped.set(true);
                        Wake::Closed
                    }
                },
                orphaned: || stopped.get(),
                watcher_closed: || {},
            },
            fake_clock(&now, &sleeps, &slept),
            &mut last_read,
        );

        assert_eq!(polls.get(), 4, "one initial poll plus one per event nudge");
        assert_eq!(sleeps.get(), 3);
        assert_eq!(slept.get(), Duration::from_secs(3));
    }

    #[test]
    fn burst_of_nudges_is_read_at_most_once_per_second() {
        let now = Cell::new(Instant::now());
        let sleeps = Cell::new(0);
        let slept = Cell::new(Duration::ZERO);
        let polls = Cell::new(0);
        let wait_calls = Cell::new(0);
        let stopped = Cell::new(false);
        let mut last_read: Option<Instant> = None;

        tail_loop(
            || {
                polls.set(polls.get() + 1);
                (Vec::new(), false)
            },
            |_| {},
            TailControl {
                ensure_watcher: || true,
                wait: |_, _| {
                    if wait_calls.get() == 0 {
                        wait_calls.set(1);
                        let (sender, receiver) = mpsc::sync_channel(1);
                        let mut accepted = 0;
                        for _ in 0..10 {
                            match sender.try_send(()) {
                                Ok(()) => accepted += 1,
                                Err(TrySendError::Full(())) => {}
                                Err(TrySendError::Disconnected(())) => {
                                    panic!("receiver disconnected")
                                }
                            }
                        }
                        assert_eq!(accepted, 1, "capacity-one channel coalesces the burst");
                        receiver.recv().unwrap();
                        now.set(now.get() + Duration::from_millis(100));
                        Wake::Nudge
                    } else {
                        stopped.set(true);
                        Wake::Closed
                    }
                },
                orphaned: || stopped.get(),
                watcher_closed: || {},
            },
            fake_clock(&now, &sleeps, &slept),
            &mut last_read,
        );

        assert_eq!(polls.get(), 2, "initial read plus one coalesced burst read");
        assert_eq!(sleeps.get(), 1, "the burst needs one remainder wait");
        assert_eq!(slept.get(), Duration::from_millis(900));
    }

    #[test]
    fn backlog_is_drained_without_inter_chunk_delay() {
        let now = Cell::new(Instant::now());
        let sleeps = Cell::new(0);
        let slept = Cell::new(Duration::ZERO);
        let polls = Cell::new(0);
        let stopped = Cell::new(false);
        let mut last_read: Option<Instant> = None;

        tail_loop(
            || {
                polls.set(polls.get() + 1);
                (Vec::new(), polls.get() < 3)
            },
            |_| {},
            TailControl {
                ensure_watcher: || true,
                wait: |_, _| {
                    stopped.set(true);
                    Wake::Closed
                },
                orphaned: || stopped.get(),
                watcher_closed: || {},
            },
            fake_clock(&now, &sleeps, &slept),
            &mut last_read,
        );

        assert_eq!(polls.get(), 3, "all pending chunks drain before waiting");
        assert_eq!(sleeps.get(), 0, "backlog chunks have no read-gap sleep");
        assert_eq!(slept.get(), Duration::ZERO);
    }

    #[test]
    fn missing_directory_retries_slowly() {
        let now = Cell::new(Instant::now());
        let sleeps = Cell::new(0);
        let slept = Cell::new(Duration::ZERO);
        let polls = Cell::new(0);
        let attempts = Cell::new(0);
        let waits = Cell::new(0);
        let stopped = Cell::new(false);
        let mut last_read: Option<Instant> = None;

        tail_loop(
            || {
                polls.set(polls.get() + 1);
                (Vec::new(), false)
            },
            |_| {},
            TailControl {
                ensure_watcher: || {
                    attempts.set(attempts.get() + 1);
                    attempts.get() > 2
                },
                wait: |watching: bool, retry: Duration| {
                    if !watching {
                        waits.set(waits.get() + 1);
                        assert_eq!(retry, WATCH_RETRY);
                        now.set(now.get() + retry);
                        Wake::Retry
                    } else {
                        stopped.set(true);
                        Wake::Closed
                    }
                },
                orphaned: || stopped.get(),
                watcher_closed: || {},
            },
            fake_clock(&now, &sleeps, &slept),
            &mut last_read,
        );

        assert_eq!(attempts.get(), 3);
        assert_eq!(
            waits.get(),
            2,
            "failed watcher attempts wait 30 seconds each"
        );
        assert_eq!(polls.get(), 3, "read once after every watcher attempt");
        assert_eq!(
            sleeps.get(),
            0,
            "retry does not use the 1-second read delay"
        );
    }

    #[test]
    fn bounded_push_splits_many_short_lines() {
        let mut sizes = Vec::new();
        let woke = push_bounded((0..200).map(|i| format!("line-{i}")).collect(), |batch| {
            sizes.push(batch.len());
            false
        });

        assert!(!woke);
        assert_eq!(sizes.iter().sum::<usize>(), 200);
        assert!(sizes.len() > 1);
        assert!(
            sizes
                .iter()
                .all(|size| *size <= super::super::ingress::MAX_BATCH_RECORDS)
        );
    }

    #[test]
    fn bounded_push_truncates_an_oversized_record() {
        let mut records = Vec::new();
        assert!(!push_bounded(vec!["x".repeat(100 * 1024)], |batch| {
            records.extend(batch);
            false
        }));

        assert_eq!(records.len(), 1);
        assert!(records[0].original.ends_with(" [record truncated]"));
        assert!(
            records[0].original.len()
                <= super::super::ingress::MAX_RECORD_BYTES + " [record truncated]".len()
        );
    }

    #[test]
    fn real_watcher_ignores_open_reads_and_nudges_on_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thegn.log");
        std::fs::write(&path, b"initial\n").unwrap();
        let (_watcher, events, _reset_requested) = watch_log_directory(&path).unwrap();

        let mut file = std::fs::File::open(&path).unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"initial\n");
        assert!(matches!(
            events.recv_timeout(Duration::from_millis(300)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"appended\n").unwrap();
        events.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn removed_log_directory_resets_the_watch_for_rearming() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir(&log_dir).unwrap();
        let path = log_dir.join("thegn.log");
        std::fs::write(&path, b"initial\n").unwrap();
        let (watcher, events, reset_requested) = watch_log_directory(&path).unwrap();

        std::fs::remove_dir_all(&log_dir).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reset_requested.load(Ordering::Acquire) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "directory removal did not reset the watch"
            );
            events.recv_timeout(remaining).unwrap();
        }
        drop(watcher);

        std::fs::create_dir(&log_dir).unwrap();
        let (_rearmed, _events, _reset_requested) = watch_log_directory(&path).unwrap();
    }

    #[test]
    fn tails_only_appends_not_history() {
        let dir = std::env::temp_dir().join(format!("tg-logprov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.log");
        // Pre-existing history that must NOT be replayed.
        std::fs::write(&path, "old line 1\nold line 2\n").unwrap();

        let tx = super::super::ingress::LogIngress::new();
        let rx = tx.clone();
        let p = path.clone();
        let handle = std::thread::spawn(move || tail_file(&p, tx, noop_waker()));

        // Give the tailer a moment to open + seek to end, then append.
        std::thread::sleep(Duration::from_millis(50));
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "new line").unwrap();
        }

        // The first (and only) batch we receive must be the appended line, never
        // the pre-existing history.
        let batch = loop {
            let batch = rx.take(100);
            if !batch.is_empty() {
                break batch;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(batch.len(), 1, "expected only the appended line: {batch:?}");
        assert!(
            batch[0].message.contains("new line"),
            "batch not the appended line: {batch:?}"
        );

        // Dropping the receiver must make the tailer exit (not spin forever).
        drop(rx);
        // Prod it out of any in-flight sleep with another append + wait.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "trailing").unwrap();
        }
        let joined = {
            // Poll the thread for a bounded time — it must terminate.
            let start = std::time::Instant::now();
            loop {
                if handle.is_finished() {
                    break true;
                }
                if start.elapsed() > Duration::from_secs(5) {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        assert!(joined, "tailer did not exit after the consumer was dropped");
        let _ = handle.join(); // best-effort: join failure must not mask the assert above
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test tmp cleanup
    }
}
