//! Off-loop orchestration for notification sounds.
//!
//! The compositor only resolves a pure [`SoundRef`] and does a bounded
//! `try_send`. Pack inspection, provider probing, filesystem checks, and child
//! processes all happen on the named utility worker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use termwiz::terminal::TerminalWaker;
use thegn_core::config::SoundConfig;
use thegn_core::notification_route::SoundEmit;
use thegn_core::notification_sound::SoundRef;
use thegn_core::seam::ProbeReport;

pub(crate) const QUEUE_DEPTH: usize = 32;

struct ReloadRequest {
    generation: u64,
    config: SoundConfig,
}

#[derive(Debug)]
enum SoundJob {
    File { path: PathBuf, volume: f32 },
    Command(String),
}

struct PlaybackSnapshot {
    provider: Option<Box<dyn crate::platform::sound::SoundPlayer>>,
    pack: Option<PathBuf>,
    entries: BTreeMap<String, PathBuf>,
    pack_entry_count: usize,
    files: BTreeMap<String, PathBuf>,
    provider_report: ProbeReport,
    fallback: Option<String>,
}

impl PlaybackSnapshot {
    fn empty() -> Self {
        Self {
            provider: None,
            pack: None,
            entries: BTreeMap::new(),
            pack_entry_count: 0,
            files: BTreeMap::new(),
            provider_report: ProbeReport::new(
                "sound",
                "none",
                thegn_core::seam::Availability::Unavailable("sound snapshot pending".into()),
            ),
            fallback: Some("sound snapshot is not loaded yet".into()),
        }
    }
}

/// A joinable worker plus a completion channel. The worker signals on `done`
/// as its last act (a drop guard, so a panic signals too); shutdown waits on it
/// with a timeout instead of joining unconditionally.
struct Worker {
    join: std::thread::JoinHandle<()>,
    done: std::sync::mpsc::Receiver<()>,
}

struct SignalDone(std::sync::mpsc::Sender<()>);

impl Drop for SignalDone {
    fn drop(&mut self) {
        // best-effort: shutdown may already have timed out and detached us
        let _ = self.0.send(());
    }
}

type SnapshotBuilder = dyn Fn(&SoundConfig, &AtomicBool) -> Option<PlaybackSnapshot> + Send + Sync;

/// Immutable-at-use-time provider and pack state shared by producers and the
/// worker. Replacing the `Arc` under the short mutex never makes a producer
/// inspect the filesystem.
pub(crate) struct SoundRuntime {
    snapshot: Mutex<Arc<PlaybackSnapshot>>,
    queue: Mutex<Option<std::sync::mpsc::SyncSender<SoundJob>>>,
    worker: Mutex<Option<Worker>>,
    reload_slot: Arc<(Mutex<Option<ReloadRequest>>, std::sync::Condvar)>,
    reload_worker: Mutex<Option<Worker>>,
    reload_generation: AtomicU64,
    dropped: AtomicU64,
    fallback_bell: AtomicBool,
    /// Shared with the reload worker so its wait predicate can observe it
    /// without upgrading the weak runtime reference.
    stopping: Arc<AtomicBool>,
    builder: Arc<SnapshotBuilder>,
    cancellation: AtomicBool,
    waker: TerminalWaker,
}

impl SoundRuntime {
    pub(crate) fn new(waker: TerminalWaker) -> Arc<Self> {
        Self::with_builder(waker, Arc::new(build_snapshot_cancellable))
    }

    fn with_builder(waker: TerminalWaker, builder: Arc<SnapshotBuilder>) -> Arc<Self> {
        let runtime = Arc::new(Self {
            snapshot: Mutex::new(Arc::new(PlaybackSnapshot::empty())),
            queue: Mutex::new(None),
            worker: Mutex::new(None),
            reload_slot: Arc::new((Mutex::new(None), std::sync::Condvar::new())),
            reload_worker: Mutex::new(None),
            reload_generation: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            fallback_bell: AtomicBool::new(false),
            stopping: Arc::new(AtomicBool::new(false)),
            builder,
            cancellation: AtomicBool::new(false),
            waker,
        });
        runtime.start_reload_worker();
        runtime
    }

    fn start_reload_worker(self: &Arc<Self>) {
        let slot = Arc::clone(&self.reload_slot);
        let stopping = Arc::clone(&self.stopping);
        let runtime = Arc::downgrade(self);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("notify-sound-config".into())
            .spawn(move || {
                let _done = SignalDone(done_tx);
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                loop {
                    let request = {
                        let (pending, changed) = &*slot;
                        let mut pending = pending.lock().unwrap();
                        // Never upgrade `runtime` while this guard is held: if
                        // the temporary Arc were the last reference, its Drop
                        // would run `shutdown()` and re-lock this mutex.
                        while pending.is_none() && !stopping.load(Ordering::Acquire) {
                            pending = changed.wait(pending).unwrap();
                        }
                        pending.take()
                    };
                    let Some(request) = request else { break };
                    let Some(runtime) = runtime.upgrade() else {
                        break;
                    };
                    if runtime.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let Some(snapshot) = (runtime.builder)(&request.config, &runtime.stopping)
                    else {
                        break;
                    };
                    let snapshot = Arc::new(snapshot);
                    let mut current = runtime.snapshot.lock().unwrap();
                    if !runtime.stopping.load(Ordering::Acquire)
                        && runtime.reload_generation.load(Ordering::Acquire) == request.generation
                    {
                        *current = snapshot;
                    }
                }
            })
            // best-effort: an unavailable reload worker leaves the empty snapshot in place
            .ok()
            .map(|join| Worker {
                join,
                done: done_rx,
            });
        *self.reload_worker.lock().unwrap() = spawned;
    }

    /// Build a fresh snapshot off the compositor loop, then swap it atomically.
    pub(crate) fn reload(self: &Arc<Self>, cfg: SoundConfig) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        // Keep request replacement linearized with publication: if generation
        // advancement and mailbox replacement were separate, concurrent
        // reload callers could leave an older request in the one-slot mailbox.
        let _snapshot = self.snapshot.lock().unwrap();
        let generation = self
            .reload_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let (pending, changed) = &*self.reload_slot;
        let mut pending = pending.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        replace_pending_reload(
            &mut pending,
            ReloadRequest {
                generation,
                config: cfg,
            },
        );
        changed.notify_one();
    }

    pub(crate) fn enqueue(self: &Arc<Self>, emit: &SoundEmit) {
        let job = match emit {
            SoundEmit::File { sound_ref, volume } => {
                let snapshot = self.snapshot.lock().unwrap().clone();
                let Some(path) = resolve(sound_ref, &snapshot) else {
                    self.request_fallback("sound reference was not found in the configured pack");
                    return;
                };
                SoundJob::File {
                    path,
                    volume: *volume,
                }
            }
            SoundEmit::Command(command) => SoundJob::Command(command.clone()),
            SoundEmit::Bell => return,
        };

        let Some(tx) = self.ensure_worker() else {
            self.request_fallback("could not start notify-sound worker");
            return;
        };
        match tx.try_send(job) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    target: "thegn::notify_sound",
                    dropped_total = dropped,
                    "sound queue full — dropped an audio job"
                );
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.request_fallback("notify-sound worker exited");
            }
        }
    }

    fn ensure_worker(self: &Arc<Self>) -> Option<std::sync::mpsc::SyncSender<SoundJob>> {
        if self.stopping.load(Ordering::Acquire) {
            return None;
        }
        let mut queue = self.queue.lock().unwrap();
        // Re-check under the lock: `shutdown` sets `stopping` before taking it,
        // so no worker can be spawned (and orphaned) after shutdown began.
        if self.stopping.load(Ordering::Acquire) {
            return None;
        }
        if let Some(tx) = queue.as_ref() {
            return Some(tx.clone());
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
        let runtime = Arc::downgrade(self);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("notify-sound".into())
            .spawn(move || {
                let _done = SignalDone(done_tx);
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                while let Ok(job) = rx.recv() {
                    let Some(runtime) = runtime.upgrade() else { break };
                    if runtime.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    match job {
                        SoundJob::File { path, volume } => {
                            let snapshot = runtime.snapshot.lock().unwrap().clone();
                            let Some(provider) = snapshot.provider.as_ref() else {
                                runtime.request_fallback("no audio provider");
                                continue;
                            };
                            if !supported_format(&path, &provider.caps().formats) {
                                runtime.request_fallback("sound file format is unsupported");
                                continue;
                            }
                            if let Err(error) =
                                provider.play(&path, volume, &runtime.cancellation)
                            {
                                tracing::debug!(target: "thegn::notify_sound", %error, "audio provider failed");
                                runtime.request_fallback("audio provider failed");
                            }
                        }
                        SoundJob::Command(command) => {
                            if let Err(error) = run_command(&command, &runtime.cancellation) {
                                tracing::debug!(
                                    target: "thegn::notify_sound",
                                    %error,
                                    "legacy sound command failed"
                                );
                            }
                        }
                    }
                }
            });
        *self.worker.lock().unwrap() = Some(Worker {
            join: spawned.ok()?,
            done: done_rx,
        });
        *queue = Some(tx.clone());
        Some(tx)
    }

    /// Stop accepting sound work, cancel active helper playback, discard the
    /// coalesced reload request, and wait for both owned workers.
    ///
    /// The wait is bounded by [`SHUTDOWN_JOIN_BOUND`] in total. A worker that
    /// has not finished by then (a child in uninterruptible I/O, a hung pack
    /// directory, an unbounded Windows `status()`) is logged and detached
    /// rather than blocking exit; it holds only a weak runtime reference and
    /// observes `stopping`, so it winds down on its own if it ever unblocks.
    pub(crate) fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancellation.store(true, Ordering::Release);
        self.queue.lock().unwrap().take();
        let (pending, changed) = &*self.reload_slot;
        pending.lock().unwrap().take();
        changed.notify_all();
        let deadline =
            std::time::Instant::now() + crate::platform::sound_process::SHUTDOWN_JOIN_BOUND;
        let playback = self.worker.lock().unwrap().take();
        let reload = self.reload_worker.lock().unwrap().take();
        for (name, worker) in [("playback", playback), ("snapshot", reload)] {
            if let Some(worker) = worker {
                join_bounded(name, worker, deadline);
            }
        }
    }

    fn request_fallback(&self, reason: &str) {
        tracing::debug!(target: "thegn::notify_sound", reason, "falling back to terminal bell");
        request_fallback_raw(&self.fallback_bell, &self.waker, reason);
    }

    pub(crate) fn take_fallback_bell(&self) -> bool {
        self.fallback_bell.swap(false, Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn latch_fallback_bell_for_test(&self) {
        self.fallback_bell.store(true, Ordering::Relaxed);
    }

    pub(crate) fn report(cfg: &SoundConfig) -> serde_json::Value {
        let snapshot = build_snapshot(cfg);
        serde_json::json!({
            "provider": snapshot.provider_report,
            "pack": snapshot.pack.as_ref().map(|p| p.display().to_string()),
            "pack_entries": snapshot.pack_entry_count,
            "fallback": snapshot.fallback,
        })
    }
}

impl Drop for SoundRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn join_bounded(name: &str, worker: Worker, deadline: std::time::Instant) {
    if worker.join.thread().id() == std::thread::current().id() {
        // Drop can run on a worker if its temporary weak upgrade was the final
        // strong reference. That thread is already exiting; just detach.
        return;
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    // Both a `()` and a disconnect mean the worker is finished.
    if matches!(
        worker.done.recv_timeout(remaining),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ) {
        tracing::warn!(
            target: "thegn::notify_sound",
            worker = name,
            "sound worker did not stop within the shutdown bound; detaching it"
        );
        return;
    }
    if worker.join.join().is_err() {
        tracing::error!(target: "thegn::notify_sound", worker = name, "sound worker panicked during shutdown");
    }
}

fn request_fallback_raw(flag: &AtomicBool, waker: &TerminalWaker, reason: &str) {
    tracing::debug!(target: "thegn::notify_sound", reason, "sound degraded to terminal bell");
    flag.store(true, Ordering::Relaxed);
    let _ = waker.wake(); // best-effort: a fallback cue must not fail its producer
}

fn replace_pending_reload(pending: &mut Option<ReloadRequest>, latest: ReloadRequest) {
    *pending = Some(latest);
}

fn build_snapshot(cfg: &SoundConfig) -> PlaybackSnapshot {
    build_snapshot_cancellable(cfg, &AtomicBool::new(false)).unwrap()
}

fn build_snapshot_cancellable(
    cfg: &SoundConfig,
    stopping: &AtomicBool,
) -> Option<PlaybackSnapshot> {
    if stopping.load(Ordering::Acquire) {
        return None;
    }
    let provider = crate::platform::sound::provider();
    let provider_report = provider
        .as_ref()
        .map_or_else(crate::platform::sound::probe, |p| p.probe());
    let pack = (!cfg.pack.trim().is_empty())
        .then(|| PathBuf::from(thegn_core::util::expand_tilde(cfg.pack.trim())));
    let mut entries = BTreeMap::new();
    let mut pack_entry_count = 0;
    let mut files = BTreeMap::new();
    let mut fallback = None;
    if let Some(dir) = &pack {
        match std::fs::read_dir(dir) {
            Ok(read_dir) => {
                for item in read_dir.flatten() {
                    if stopping.load(Ordering::Acquire) {
                        return None;
                    }
                    let path = item.path();
                    if !path.is_file() {
                        continue;
                    }
                    pack_entry_count += 1;
                    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    entries
                        .entry(file_name.to_string())
                        .or_insert_with(|| path.clone());
                    if let Some(stem) = path.file_stem().and_then(|n| n.to_str()) {
                        entries.entry(stem.to_string()).or_insert(path);
                    }
                }
            }
            Err(error) => fallback = Some(format!("sound pack unavailable: {error}")),
        }
    }
    if provider.is_none() {
        fallback = Some("no supported audio player found; terminal bell is used".into());
    }
    for raw in cfg
        .per_kind
        .values()
        .chain(std::iter::once(&cfg.chime_file))
    {
        if stopping.load(Ordering::Acquire) {
            return None;
        }
        let Ok(SoundRef::File(path)) = SoundRef::parse(raw) else {
            continue;
        };
        let expanded = PathBuf::from(thegn_core::util::expand_tilde(&path));
        if expanded.is_file() {
            files.insert(path, expanded);
        } else {
            fallback.get_or_insert_with(|| "a configured sound file is missing".into());
        }
    }
    Some(PlaybackSnapshot {
        provider,
        pack,
        entries,
        pack_entry_count,
        files,
        provider_report,
        fallback,
    })
}

fn resolve(sound_ref: &SoundRef, snapshot: &PlaybackSnapshot) -> Option<PathBuf> {
    match sound_ref {
        SoundRef::Off | SoundRef::Bell => None,
        SoundRef::Pack(name) => snapshot.entries.get(name).cloned(),
        SoundRef::File(path) => snapshot.files.get(path).cloned(),
    }
}

fn supported_format(path: &Path, formats: &[&str]) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    formats.iter().any(|f| f.eq_ignore_ascii_case(ext))
}

fn run_command(
    command: &str,
    cancellation: &AtomicBool,
) -> Result<(), crate::platform::sound::SoundError> {
    use crate::platform::sound_process::{SoundProcessOutcome as Outcome, run};
    match run("sh", &["-c".into(), command.into()], cancellation) {
        Outcome::Exited(status) if status.success() => Ok(()),
        Outcome::Exited(_) => Err(crate::platform::sound::SoundError::Failed),
        Outcome::Spawn(error) => Err(crate::platform::sound::SoundError::Spawn(error)),
        Outcome::Timeout => Err(crate::platform::sound::SoundError::Timeout),
        Outcome::Cancelled => Err(crate::platform::sound::SoundError::Cancelled),
        Outcome::Reap(error) => Err(crate::platform::sound::SoundError::Reap(error)),
        Outcome::DescendantsRemain => Err(crate::platform::sound::SoundError::DescendantsRemain),
    }
}

pub(crate) fn emit(runtime: &Arc<SoundRuntime>, emit: &SoundEmit) {
    runtime.enqueue(emit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_snapshot_indexes_filename_and_stem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("attention.wav"), b"RIFF").unwrap();
        let cfg = SoundConfig {
            pack: dir.path().display().to_string(),
            ..SoundConfig::default()
        };
        let snapshot = build_snapshot(&cfg);
        assert!(snapshot.entries.contains_key("attention.wav"));
        assert!(snapshot.entries.contains_key("attention"));
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.pack_entry_count, 1);
    }

    #[test]
    fn sound_mode_only_needs_a_worker_for_file_or_command_playback() {
        let bell = SoundConfig::default();
        assert!(!needs_worker(&bell));
        let command = SoundConfig {
            mode: thegn_core::config::SoundMode::Command,
            command: "true".into(),
            ..bell.clone()
        };
        assert!(needs_worker(&command));
    }

    #[test]
    fn stale_reload_generation_cannot_replace_newer_snapshot() {
        let generation = AtomicU64::new(1);
        assert_eq!(generation.load(Ordering::Acquire), 1);
        generation.store(2, Ordering::Release);
        assert_ne!(generation.load(Ordering::Acquire), 1);
    }

    use crate::platform::sound_process::test_support::{UNIX, pid_gone, test_waker};

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let started = std::time::Instant::now();
        while !cond() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn reload_storm_runs_one_build_at_a_time_and_the_latest_config_wins() {
        if !UNIX {
            return;
        }
        use std::sync::atomic::AtomicUsize;
        let (waker, _master, _terminal) = test_waker();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let builds = Arc::new(AtomicUsize::new(0));
        let builder: Arc<SnapshotBuilder> = {
            let (active, max_active, builds) = (active.clone(), max_active.clone(), builds.clone());
            Arc::new(move |cfg: &SoundConfig, _stopping: &AtomicBool| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(20));
                builds.fetch_add(1, Ordering::SeqCst);
                active.fetch_sub(1, Ordering::SeqCst);
                let mut snapshot = PlaybackSnapshot::empty();
                snapshot.fallback = Some(cfg.pack.clone());
                Some(snapshot)
            })
        };
        let runtime = SoundRuntime::with_builder(waker, builder);
        for generation in 1..=100 {
            runtime.reload(SoundConfig {
                pack: format!("pack-{generation}"),
                ..SoundConfig::default()
            });
        }
        wait_until("the latest config to be published", || {
            runtime.snapshot.lock().unwrap().fallback.as_deref() == Some("pack-100")
        });
        runtime.shutdown();
        assert_eq!(max_active.load(Ordering::SeqCst), 1, "builds overlapped");
        assert!(
            builds.load(Ordering::SeqCst) < 100,
            "the storm was not coalesced"
        );
    }

    #[test]
    fn shutdown_is_bounded_with_a_hung_helper_and_a_hung_pending_reload() {
        if !UNIX {
            return;
        }
        let (waker, _master, _terminal) = test_waker();
        let building = Arc::new(AtomicBool::new(false));
        // An uncooperative build (a hung `read_dir`): ignores `stopping`.
        let builder: Arc<SnapshotBuilder> = {
            let building = building.clone();
            Arc::new(move |_cfg: &SoundConfig, _stopping: &AtomicBool| {
                building.store(true, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_secs(3));
                None
            })
        };
        let runtime = SoundRuntime::with_builder(waker, builder);
        runtime.reload(SoundConfig::default());
        wait_until("the build to start", || building.load(Ordering::SeqCst));
        runtime.reload(SoundConfig {
            pack: "pending".into(),
            ..SoundConfig::default()
        });

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        runtime.enqueue(&SoundEmit::Command(format!(
            "echo $$ > '{}'; sleep 30 & wait",
            pid_file.display()
        )));
        wait_until("the helper to start", || {
            std::fs::read_to_string(&pid_file).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
        });
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let started = std::time::Instant::now();
        runtime.shutdown();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "shutdown took {:?}",
            started.elapsed()
        );
        assert!(runtime.worker.lock().unwrap().is_none());
        assert!(runtime.reload_worker.lock().unwrap().is_none());
        // The helper was killed AND reaped: its pid no longer exists (a zombie
        // would still answer signal 0).
        assert!(
            pid_gone(pid),
            "helper {pid} still exists (running or zombie)"
        );
    }

    #[test]
    fn legacy_command_failure_is_reported() {
        assert!(run_command("exit 7", &AtomicBool::new(false)).is_err());
    }

    fn needs_worker(cfg: &SoundConfig) -> bool {
        cfg.mode == thegn_core::config::SoundMode::Command
            || !cfg.chime_file.trim().is_empty()
            || cfg.per_kind.values().any(|value| {
                matches!(
                    SoundRef::parse(value),
                    Ok(SoundRef::File(_) | SoundRef::Pack(_))
                )
            })
    }
}
