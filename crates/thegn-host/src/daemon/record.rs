//! The per-session recorder: tees a daemon session's PTY output into an
//! asciicast v2 `.cast` file. File ownership and all encoding/writes live on a
//! named background worker. The actor only makes nonblocking sends into a
//! bounded queue; a full queue truncates recording explicitly.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thegn_core::asciicast::{CastWriter, Header, Utf8Carry};
use thegn_core::config::Config;

const QUEUE_COMMANDS: usize = 64;
const MAX_CHUNK_BYTES: usize = 16 * 1024;
const FINISH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum Command {
    Output { at: f64, bytes: Vec<u8> },
    Resize { at: f64, cols: u16, rows: u16 },
}

struct WorkerState {
    done: AtomicBool,
    capped: AtomicBool,
    bytes_written: AtomicU64,
    failure: Mutex<Option<String>>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            capped: AtomicBool::new(false),
            bytes_written: AtomicU64::new(0),
            failure: Mutex::new(None),
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
                    worker_state.as_ref(),
                );
                if let Err(error) = result_tx.send(finished) {
                    tracing::debug!(target: "thegn::daemon", "recording completion receiver dropped: {error}");
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            result,
            state,
            path,
            start: Instant::now(),
        })
    }

    /// Enqueue a raw PTY chunk without waiting for the disk. Chunks larger than
    /// the fixed payload limit and full queues both stop recording explicitly.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if self.done() {
            return;
        }
        if bytes.len() > MAX_CHUNK_BYTES {
            self.state.fail(format!(
                "recording truncated: PTY chunk exceeded {MAX_CHUNK_BYTES} byte limit"
            ));
            return;
        }
        self.try_enqueue(Command::Output {
            at: self.elapsed(),
            bytes: bytes.to_vec(),
        });
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
        self.state.done.load(Ordering::Acquire)
    }

    /// Close admission and wait at most two seconds for queued writes and flush.
    /// On timeout, the still-running worker retains file ownership; status says
    /// truncated even though the detached worker may finish flushing later.
    pub(crate) fn finish(mut self) -> Finished {
        drop(self.tx.take()); // disconnect after all accepted commands, preserving order
        let outcome = self.result.recv_timeout(FINISH_TIMEOUT);
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
            Err(_) => {
                drop(self.worker.take()); // worker keeps its file and finishes independently
                let reason = if matches!(outcome, Err(mpsc::RecvTimeoutError::Timeout)) {
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
    state: &WorkerState,
) -> Finished {
    let initialized = (|| -> io::Result<CastWriter<BufWriter<std::fs::File>>> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        crate::platform::restrict_dir_owner_only(dir);
        let file = crate::platform::create_private_file(path)?;
        let mut header = Header::new(cols, rows);
        let stamp = now_ms();
        header.timestamp = Some(stamp / 1000);
        header.term = Some("thegn".to_string());
        CastWriter::new(BufWriter::new(file), &header)
    })();
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

#[cfg(test)]
fn run_worker_with_writer<W: Write>(
    rx: mpsc::Receiver<Command>,
    path: &Path,
    max_bytes: u64,
    state: &WorkerState,
    writer: CastWriter<W>,
) -> Finished {
    run_writer(rx, path, max_bytes, state, writer)
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
            Command::Output { at, bytes } => {
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
        let tmp = std::env::temp_dir().join(format!("thegn-rec-test-{}", now_ms()));
        let mut cfg = Config::default();
        cfg.recording.dir = tmp.to_string_lossy().into_owned();
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
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn slow_sink_does_not_block_admission_and_overflow_is_bounded() {
        struct SlowWriter(mpsc::Sender<()>);
        impl Write for SlowWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0
                    .send(())
                    .map_err(|_| io::Error::other("test writer observer dropped"))?;
                std::thread::sleep(Duration::from_millis(10));
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                std::thread::sleep(Duration::from_millis(10));
                Ok(())
            }
        }
        let path = PathBuf::from("slow.cast");
        let mut header = Header::new(80, 24);
        header.timestamp = Some(0);
        let (started_tx, started_rx) = mpsc::channel();
        let writer = CastWriter::new(BufWriter::new(SlowWriter(started_tx)), &header)
            .expect("buffered header");
        let state = Arc::new(WorkerState::new());
        let worker_state = Arc::clone(&state);
        let (tx, rx) = mpsc::sync_channel(QUEUE_COMMANDS);
        let worker = thread::spawn(move || run_writer(rx, &path, 0, &worker_state, writer));
        tx.send(Command::Output {
            at: 0.1,
            bytes: vec![b'a'; MAX_CHUNK_BYTES],
        })
        .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("slow sink was entered");
        let started = Instant::now();
        for _ in 0..QUEUE_COMMANDS {
            assert!(
                tx.try_send(Command::Output {
                    at: 0.2,
                    bytes: vec![b'x'; MAX_CHUNK_BYTES]
                })
                .is_ok()
            );
        }
        assert!(matches!(
            tx.try_send(Command::Output {
                at: 0.3,
                bytes: vec![b'x'; MAX_CHUNK_BYTES]
            }),
            Err(mpsc::TrySendError::Full(_))
        ));
        state.fail("recording truncated: bounded writer queue is full".to_string());
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(QUEUE_COMMANDS * MAX_CHUNK_BYTES, 1024 * 1024);
        drop(tx);
        let finished = worker.join().unwrap();
        assert!(
            finished.truncated.is_some(),
            "overflow must be observable as truncation"
        );
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
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Command::Output {
            at: 0.1,
            bytes: b"fail".to_vec(),
        })
        .unwrap();
        drop(tx);
        let fin = run_worker_with_writer(rx, &path, 0, &state, writer);
        assert!(
            fin.truncated.is_some(),
            "sink failure must not report a clean save"
        );
    }

    #[test]
    fn size_cap_finalizes_rather_than_growing() {
        let tmp = std::env::temp_dir().join(format!("thegn-rec-cap-{}", now_ms()));
        let mut cfg = Config::default();
        cfg.recording.dir = tmp.to_string_lossy().into_owned();
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
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
