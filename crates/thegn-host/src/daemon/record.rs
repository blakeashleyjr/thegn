//! The per-session recorder: tees a daemon session's PTY output into an
//! asciicast v2 `.cast` file. File ownership and all encoding/writes live on a
//! named background worker. The actor only makes nonblocking sends into a
//! bounded queue; a full queue truncates recording explicitly.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thegn_core::asciicast::{CastWriter, Header, Utf8Carry};
use thegn_core::config::Config;

const QUEUE_COMMANDS: usize = 64;
const MAX_CHUNK_BYTES: usize = 16 * 1024;
pub(super) const FINISH_TIMEOUT: Duration = Duration::from_secs(2);
const GLOBAL_INFLIGHT_BYTES: usize = 8 * 1024 * 1024;

static RECORDING_INFLIGHT: OnceLock<Arc<AtomicUsize>> = OnceLock::new();

fn global_inflight() -> Arc<AtomicUsize> {
    Arc::clone(RECORDING_INFLIGHT.get_or_init(|| Arc::new(AtomicUsize::new(0))))
}

struct BytePermit {
    budget: Arc<AtomicUsize>,
    bytes: usize,
}

impl BytePermit {
    fn reserve(budget: &Arc<AtomicUsize>, bytes: usize, limit: usize) -> Option<Self> {
        match budget.fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes).filter(|next| *next <= limit)
        }) {
            Ok(_) => Some(Self {
                budget: Arc::clone(budget),
                bytes,
            }),
            Err(_) => None,
        }
    }
}

impl Drop for BytePermit {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

enum Command {
    Output {
        at: f64,
        bytes: Vec<u8>,
        _permit: BytePermit,
    },
    Resize {
        at: f64,
        cols: u16,
        rows: u16,
    },
}

struct WorkerState {
    done: AtomicBool,
    capped: AtomicBool,
    bytes_written: AtomicU64,
    failure: Mutex<Option<String>>,
    completion_wake: Arc<tokio::sync::Notify>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            capped: AtomicBool::new(false),
            bytes_written: AtomicU64::new(0),
            failure: Mutex::new(None),
            completion_wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn fail(&self, reason: String) {
        if let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(reason);
        }
        self.done.store(true, Ordering::Release);
    }

    fn done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn capped(&self) -> bool {
        self.capped.load(Ordering::Acquire)
    }
}

pub(crate) struct Recorder {
    tx: Option<mpsc::SyncSender<Command>>,
    worker: Option<JoinHandle<()>>,
    result: mpsc::Receiver<Finished>,
    state: Arc<WorkerState>,
    inflight: Arc<AtomicUsize>,
    inflight_limit: usize,
    path: PathBuf,
    start: Instant,
}

/// What [`Recorder::finish`] leaves behind: where the recording was written,
/// and whether finalizing it actually succeeded.
pub(crate) struct Finished {
    /// The `.cast` file's path (written whether or not final flush worked).
    pub(crate) path: PathBuf,
    /// Whether the configured recording size cap was reached.
    pub(crate) capped: bool,
    /// `Some(reason)` when recording was incomplete or finalization failed.
    pub(crate) truncated: Option<String>,
}

impl Recorder {
    /// Start a background writer. File creation and header output happen on
    /// that worker, so even startup filesystem latency cannot stall the actor.
    pub(crate) fn start(session_id: &str, cols: u16, rows: u16, cfg: &Config) -> io::Result<Self> {
        let dir = cfg.recording.resolved_dir();
        let stamp = now_ms();
        let safe: String = session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("{safe}_{stamp}.cast"));
        let max_bytes = cfg.recording.max_bytes;
        let (tx, rx) = mpsc::sync_channel(QUEUE_COMMANDS);
        let (result_tx, result) = mpsc::channel();
        let state = Arc::new(WorkerState::new());
        let inflight = global_inflight();
        let worker_state = Arc::clone(&state);
        let worker_path = path.clone();
        let worker = thread::Builder::new()
            .name(format!("record-{safe}"))
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                let finished = run_worker(
                    rx,
                    &worker_path,
                    cols,
                    rows,
                    max_bytes,
                    stamp,
                    worker_state.as_ref(),
                );
                if let Err(error) = result_tx.send(finished) {
                    tracing::debug!(target: "thegn::daemon", "recording completion receiver dropped: {error}");
                }
                worker_state.completion_wake.notify_one();
            })?;
        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            result,
            state,
            inflight,
            inflight_limit: GLOBAL_INFLIGHT_BYTES,
            path,
            start: Instant::now(),
        })
    }

    /// Enqueue a raw PTY chunk without waiting for the disk.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if self.done() {
            return;
        }
        let at = self.elapsed();
        for chunk in bytes.chunks(MAX_CHUNK_BYTES) {
            let Some(permit) =
                BytePermit::reserve(&self.inflight, chunk.len(), self.inflight_limit)
            else {
                self.state.fail(
                    "recording truncated: global recording buffer budget exhausted".to_string(),
                );
                break;
            };
            self.try_enqueue(Command::Output {
                at,
                bytes: chunk.to_vec(),
                _permit: permit,
            });
            if self.done() {
                break;
            }
        }
    }

    /// Record a geometry change in the same ordered queue as output.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        if self.done() {
            return;
        }
        self.try_enqueue(Command::Resize {
            at: self.elapsed(),
            cols,
            rows,
        });
    }

    fn try_enqueue(&mut self, command: Command) {
        let Some(tx) = self.tx.as_ref() else { return };
        match tx.try_send(command) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => self
                .state
                .fail("recording truncated: bounded writer queue is full".to_string()),
            Err(mpsc::TrySendError::Disconnected(_)) => self
                .state
                .fail("recording truncated: writer stopped unexpectedly".to_string()),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn bytes_written(&self) -> u64 {
        self.state.bytes_written.load(Ordering::Acquire)
    }

    pub(crate) fn done(&self) -> bool {
        self.state.done()
    }

    pub(crate) fn completion_wake(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.state.completion_wake)
    }

    pub(crate) fn capped(&self) -> bool {
        self.state.capped()
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.state
            .failure
            .lock()
            .ok()
            .and_then(|failure| failure.clone())
    }

    /// Close admission and wait at most two seconds for queued writes and flush.
    /// On timeout, the still-running worker retains file ownership; status says
    /// truncated even though the detached worker may finish flushing later.
    pub(crate) fn finish(self) -> Finished {
        self.finish_with_timeout(FINISH_TIMEOUT)
    }

    fn finish_with_timeout(mut self, timeout: Duration) -> Finished {
        self.close_admission();
        self.wait_finished(timeout)
    }

    fn close_admission(&mut self) {
        drop(self.tx.take()); // disconnect after all accepted commands, preserving order
    }

    fn wait_finished(mut self, timeout: Duration) -> Finished {
        let outcome = self.result.recv_timeout(timeout);
        match outcome {
            Ok(mut finished) => {
                if self
                    .worker
                    .take()
                    .is_some_and(|worker| worker.join().is_err())
                    && finished.truncated.is_none()
                {
                    finished.truncated = Some("recording writer panicked".to_string());
                }
                finished
            }
            Err(error) => {
                drop(self.worker.take()); // worker keeps its file and finishes independently
                let reason = if matches!(error, mpsc::RecvTimeoutError::Timeout) {
                    "recording finalization timed out; writer may still be flushing"
                } else {
                    "recording writer stopped without a completion result"
                };
                Finished {
                    path: self.path.clone(),
                    capped: self.state.capped(),
                    truncated: Some(reason.to_string()),
                }
            }
        }
    }

    /// Close admission immediately and perform the bounded wait away from the
    /// session actor. The actor receives completion through its own mailbox.
    pub(crate) fn finish_background(
        mut self,
        completed: tokio::sync::mpsc::UnboundedSender<Finished>,
    ) {
        self.close_admission();
        drop(tokio::task::spawn_blocking(move || {
            let finished = self.wait_finished(FINISH_TIMEOUT);
            if let Err(error) = completed.send(finished) {
                tracing::debug!(target: "thegn::daemon", "recording completion receiver dropped: {error}");
            }
        }));
    }

    fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

fn run_worker(
    rx: mpsc::Receiver<Command>,
    path: &Path,
    cols: u16,
    rows: u16,
    max_bytes: u64,
    stamp: i64,
    state: &WorkerState,
) -> Finished {
    let initialized = open_cast_writer(path, cols, rows, stamp);
    let writer = match initialized {
        Ok(writer) => writer,
        Err(error) => {
            state.fail(format!("recording start failed: {error}"));
            return Finished {
                path: path.to_path_buf(),
                capped: false,
                truncated: Some(format!("recording start failed: {error}")),
            };
        }
    };
    run_writer(rx, path, max_bytes, state, writer)
}

fn open_cast_writer(
    path: &Path,
    cols: u16,
    rows: u16,
    stamp: i64,
) -> io::Result<CastWriter<BufWriter<std::fs::File>>> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    crate::platform::restrict_dir_owner_only(dir);
    let file = crate::platform::create_private_file(path)?;
    let mut header = Header::new(cols, rows);
    header.timestamp = Some(stamp / 1000);
    header.term = Some("thegn".to_string());
    CastWriter::new(BufWriter::new(file), &header)
}

#[cfg(test)]
fn start_with_writer<W: Write + Send + 'static>(writer: CastWriter<W>) -> Recorder {
    start_with_writer_budget(writer, Arc::new(AtomicUsize::new(0)), GLOBAL_INFLIGHT_BYTES)
}

#[cfg(test)]
fn start_with_writer_budget<W: Write + Send + 'static>(
    writer: CastWriter<W>,
    inflight: Arc<AtomicUsize>,
    inflight_limit: usize,
) -> Recorder {
    let (tx, rx) = mpsc::sync_channel(QUEUE_COMMANDS);
    let (result_tx, result) = mpsc::channel();
    let state = Arc::new(WorkerState::new());
    let worker_state = Arc::clone(&state);
    let path = PathBuf::from("injected.cast");
    let worker_path = path.clone();
    let worker = thread::spawn(move || {
        let finished = run_writer(rx, &worker_path, 0, &worker_state, writer);
        if let Err(error) = result_tx.send(finished) {
            tracing::debug!(target: "thegn::daemon", "recording completion receiver dropped: {error}");
        }
        worker_state.completion_wake.notify_one();
    });
    Recorder {
        tx: Some(tx),
        worker: Some(worker),
        result,
        state,
        inflight,
        inflight_limit,
        path,
        start: Instant::now(),
    }
}

fn run_writer<W: Write>(
    rx: mpsc::Receiver<Command>,
    path: &Path,
    max_bytes: u64,
    state: &WorkerState,
    mut writer: CastWriter<W>,
) -> Finished {
    let mut carry = Utf8Carry::default();
    let mut last_at = 0.0;
    let mut failure = None;
    while let Ok(command) = rx.recv() {
        let result = match command {
            Command::Output { at, bytes, .. } => {
                last_at = at;
                let text = carry.push(&bytes);
                if text.is_empty() {
                    Ok(())
                } else {
                    writer.output(at, &text)
                }
            }
            Command::Resize { at, cols, rows } => writer.resize(at, cols, rows),
        };
        if let Err(error) = result {
            failure = Some(format!("recording write failed: {error}"));
            if let Some(reason) = failure.as_ref() {
                state.fail(reason.clone());
            }
            break;
        }
        state
            .bytes_written
            .store(writer.bytes_written(), Ordering::Release);
        if max_bytes > 0 && writer.bytes_written() >= max_bytes {
            state.capped.store(true, Ordering::Release);
            state.done.store(true, Ordering::Release);
            if let Err(error) = writer.flush() {
                failure = Some(format!("recording cap flush failed: {error}"));
                if let Some(reason) = failure.as_ref() {
                    state.fail(reason.clone());
                }
            }
            break;
        }
    }
    if failure.is_none() {
        match state.failure.lock() {
            Ok(guard) => {
                if let Some(reason) = guard.clone() {
                    failure = Some(reason);
                }
            }
            Err(_) => failure = Some("recording failure state was poisoned".to_string()),
        }
    }
    if !state.capped() {
        let tail = carry.flush();
        if !tail.is_empty()
            && let Err(error) = writer.output(last_at, &tail)
        {
            failure.get_or_insert_with(|| format!("final write failed: {error}"));
        }
    }
    if let Err(error) = writer.flush() {
        failure.get_or_insert_with(|| format!("final flush failed: {error}"));
    }
    state
        .bytes_written
        .store(writer.bytes_written(), Ordering::Release);
    Finished {
        path: path.to_path_buf(),
        capped: state.capped(),
        truncated: if state.capped() && failure.is_none() {
            None
        } else {
            failure
        },
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| thegn_core::time_policy::saturating_i64(d.as_millis()))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_and_finish_write_a_parseable_cast() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.recording.dir = tmp.path().to_string_lossy().into_owned();
        let mut rec = Recorder::start("sess-1", 80, 24, &cfg).expect("start");
        rec.feed(b"hello ");
        rec.resize(120, 40);
        rec.feed("wörld".as_bytes());
        let fin = rec.finish();
        assert!(fin.truncated.is_none(), "clean finish is not truncated");
        let text = std::fs::read_to_string(&fin.path).unwrap();
        let mut lines = text.lines();
        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 80);
        let events: Vec<serde_json::Value> = lines
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 3, "output, resize, output retain queue order");
        assert_eq!(events[1][1], "r");
    }

    #[test]
    fn feed_splits_a_64k_chunk_and_recording_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.recording.dir = tmp.path().to_string_lossy().into_owned();
        let mut rec = Recorder::start("sess-large", 80, 24, &cfg).expect("start");
        rec.feed(&vec![b'a'; 64 * 1024]);
        assert!(
            !rec.done(),
            "a PTY sized 64 KiB chunk is split, not rejected"
        );
        rec.feed(b"continues");
        let fin = rec.finish();
        assert!(fin.truncated.is_none());
        let text = std::fs::read_to_string(&fin.path).unwrap();
        assert_eq!(
            text.lines().count(),
            6,
            "header, four chunks, and later output"
        );
    }

    #[test]
    fn asynchronous_start_failure_marks_recorder_done() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("not-a-directory");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let mut cfg = Config::default();
        cfg.recording.dir = blocker.to_string_lossy().into_owned();
        let rec = Recorder::start("sess-fail", 80, 24, &cfg).expect("worker starts");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !rec.done() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(
            rec.done(),
            "startup I/O failure is visible while output is idle"
        );
        assert!(
            rec.failure()
                .as_deref()
                .is_some_and(|reason| reason.contains("recording start failed"))
        );
        let finished = rec.finish();
        assert!(finished.truncated.is_some());
    }

    #[test]
    fn slow_sink_does_not_block_admission_and_overflow_is_bounded() {
        struct SlowWriter {
            started: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            stalled: bool,
        }
        impl Write for SlowWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.stalled {
                    self.started
                        .send(())
                        .map_err(|_| io::Error::other("test writer observer dropped"))?;
                    self.release
                        .recv()
                        .map_err(|_| io::Error::other("test writer stall released"))?;
                    self.stalled = false;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut header = Header::new(80, 24);
        header.timestamp = Some(0);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = CastWriter::new(
            BufWriter::new(SlowWriter {
                started: started_tx,
                release: release_rx,
                stalled: true,
            }),
            &header,
        )
        .expect("buffered header");
        let mut rec = start_with_writer(writer);
        rec.feed(&vec![b'a'; MAX_CHUNK_BYTES]);
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("slow sink was entered");
        let started = Instant::now();
        for _ in 0..=QUEUE_COMMANDS {
            rec.feed(&vec![b'x'; MAX_CHUNK_BYTES]);
        }
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(rec.done(), "feed marks the recorder done on queue overflow");
        release_tx.send(()).unwrap();
        let finished = rec.finish();
        assert!(
            finished
                .truncated
                .as_deref()
                .is_some_and(|reason| reason.contains("queue is full")),
            "feed overflow must report queue-full truncation"
        );
        assert_eq!(QUEUE_COMMANDS * MAX_CHUNK_BYTES, 1024 * 1024);
    }

    #[test]
    fn recorders_share_a_global_inflight_byte_budget() {
        struct SlowWriter {
            started: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            stalled: bool,
        }
        impl Write for SlowWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.stalled {
                    self.started
                        .send(())
                        .map_err(|_| io::Error::other("test observer dropped"))?;
                    self.release
                        .recv()
                        .map_err(|_| io::Error::other("test stall released"))?;
                    self.stalled = false;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        fn slow_recorder(
            budget: &Arc<AtomicUsize>,
            started: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        ) -> Recorder {
            let mut header = Header::new(80, 24);
            header.timestamp = Some(0);
            let writer = CastWriter::new(
                BufWriter::new(SlowWriter {
                    started,
                    release,
                    stalled: true,
                }),
                &header,
            )
            .expect("buffered header");
            start_with_writer_budget(writer, Arc::clone(budget), MAX_CHUNK_BYTES)
        }

        let budget = Arc::new(AtomicUsize::new(0));
        let (first_started_tx, first_started_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();
        let (second_started_tx, _second_started_rx) = mpsc::channel();
        let (second_release_tx, second_release_rx) = mpsc::channel();
        let mut first = slow_recorder(&budget, first_started_tx, first_release_rx);
        let mut second = slow_recorder(&budget, second_started_tx, second_release_rx);

        first.feed(&vec![b'a'; MAX_CHUNK_BYTES]);
        first_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first slow sink entered");
        second.feed(b"second recorder still has queue slots");
        assert!(second.done());
        assert!(
            second
                .failure()
                .as_deref()
                .is_some_and(|reason| reason.contains("global recording buffer budget"))
        );
        assert_eq!(budget.load(Ordering::Acquire), MAX_CHUNK_BYTES);

        first_release_tx.send(()).unwrap();
        second_release_tx.send(()).unwrap();
        let first_finished = first.finish();
        let second_finished = second.finish();
        assert!(first_finished.truncated.is_none());
        assert!(
            second_finished
                .truncated
                .as_deref()
                .is_some_and(|reason| reason.contains("global recording buffer budget"))
        );
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[test]
    fn finish_timeout_reports_truncation() {
        struct StalledWriter(mpsc::Receiver<()>);
        impl Write for StalledWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0
                    .recv()
                    .map_err(|_| io::Error::other("stall released"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0
                    .recv()
                    .map_err(|_| io::Error::other("stall released"))
            }
        }
        let mut header = Header::new(80, 24);
        header.timestamp = Some(0);
        let (release_tx, release_rx) = mpsc::channel();
        let writer = CastWriter::new(BufWriter::new(StalledWriter(release_rx)), &header)
            .expect("buffered header");
        let mut rec = start_with_writer(writer);
        rec.feed(b"block on final flush");
        let finished = rec.finish_with_timeout(Duration::from_millis(20));
        assert!(
            finished
                .truncated
                .as_deref()
                .is_some_and(|reason| reason.contains("timed out")),
            "a stalled worker must report the finish timeout"
        );
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
    }

    #[test]
    fn failed_sink_is_reported_by_worker() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("injected sink failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("injected sink failure"))
            }
        }
        let path = PathBuf::from("injected.cast");
        let mut header = Header::new(80, 24);
        header.timestamp = Some(0);
        let writer =
            CastWriter::new(BufWriter::new(FailingWriter), &header).expect("header is buffered");
        let state = WorkerState::new();
        let budget = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::sync_channel(2);
        for bytes in [b"fail".as_slice(), b"queued".as_slice()] {
            let permit = BytePermit::reserve(&budget, bytes.len(), 64).unwrap();
            tx.send(Command::Output {
                at: 0.1,
                bytes: bytes.to_vec(),
                _permit: permit,
            })
            .unwrap();
        }
        assert_eq!(budget.load(Ordering::Acquire), 10);
        drop(tx);
        let fin = run_writer(rx, &path, 0, &state, writer);
        assert!(
            fin.truncated.is_some(),
            "sink failure must not report a clean save"
        );
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn worker_completion_pulses_the_wake() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("injected sink failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("injected sink failure"))
            }
        }
        let mut header = Header::new(80, 24);
        header.timestamp = Some(0);
        let writer =
            CastWriter::new(BufWriter::new(FailingWriter), &header).expect("header is buffered");
        let mut rec = start_with_writer(writer);
        let wake = rec.completion_wake();
        rec.feed(b"fail");
        rec.close_admission();

        tokio::time::timeout(Duration::from_secs(1), wake.notified())
            .await
            .expect("worker completion wakes its observer");
        assert!(
            rec.finish().truncated.is_some(),
            "the failing sink must finish with a reported error"
        );
    }

    #[test]
    fn size_cap_finalizes_rather_than_growing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.recording.dir = tmp.path().to_string_lossy().into_owned();
        cfg.recording.max_bytes = 200;
        let mut rec = Recorder::start("sess-cap", 80, 24, &cfg).expect("start");
        for _ in 0..100 {
            let before = rec.bytes_written();
            rec.feed(b"a lot of output to blow the cap\n");
            let deadline = Instant::now() + Duration::from_secs(1);
            while rec.bytes_written() == before && !rec.done() && Instant::now() < deadline {
                thread::yield_now();
            }
            assert!(
                rec.bytes_written() > before || rec.done(),
                "worker should process each admitted event"
            );
            if rec.done() {
                break;
            }
        }
        let fin = rec.finish();
        assert!(fin.truncated.is_none(), "cap stop leaves a complete cast");
        let text = std::fs::read_to_string(&fin.path).unwrap();
        let header: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
    }
}
