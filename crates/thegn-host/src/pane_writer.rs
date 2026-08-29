//! Per-pane PTY stdin writer thread: a bounded FIFO between the event loop (or
//! the daemon's session actor) and a dedicated thread that owns the blocking
//! PTY writer.
//!
//! Why: `write_all` to a PTY blocks once the child stops draining stdin (^S
//! flow control, a stopped/busy process) and the kernel's ~64KB buffer fills —
//! a large paste into a non-reading pane would park the entire compositor.
//! Queuing to a bounded channel keeps the loop non-blocking; overflow drops
//! the chunk (typed as [`StdinSendError::Full`]) rather than reordering or
//! blocking.
//!
//! Ordering is structurally preserved: a single producer (the loop / the
//! actor), a single FIFO channel, and a single consumer thread that owns the
//! writer and completes each `write_all` before the next `recv` — bytes hit
//! the kernel in exactly enqueue order. Drops remove whole chunks and never
//! reorder; [`build_paste_bytes`] makes a paste ONE chunk so a drop can never
//! leave the app inside an open bracketed-paste marker. Note: because the
//! consumer keeps draining, a chunk dropped mid-stream under *sustained*
//! congestion (the 256-slot queue stays full across several sends) is a gap at
//! that chunk boundary, not tail truncation — the surviving chunks keep their
//! relative order, but a keystroke can be missing from the middle of a burst.
//! In practice this only bites a pane whose child has ignored stdin long enough
//! to back up 256 chunks (a visibly wedged pane); pastes are atomic, so only
//! individual keystrokes typed into such a pane can gap.
//!
//! Both PTY owners share this: the compositor's local panes
//! ([`crate::pane::PtyPane`]) and the pane daemon's session actor
//! ([`crate::daemon::session`]) — one implementation, one test suite, two
//! transports.

use std::io::Write;

/// Stdin chunks queued to the PTY writer thread before overflow drops them.
/// Keystrokes are tiny; even paste bursts fit comfortably. NB: the bound is
/// chunk-COUNT, not bytes — a single paste chunk can be large (accepted; same
/// as the daemon has always done).
pub(crate) const STDIN_CHANNEL_CAP: usize = 256;

/// Why a queued stdin send failed. Typed (not stringly) so call sites can
/// branch: `Full` is congestion on a live pane (surface it for user-invoked
/// pastes), `Closed` means the writer is gone (child exited / write error) —
/// the reaper retires the pane on its Exit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StdinSendError {
    /// The bounded queue is full — the child isn't reading stdin.
    Full,
    /// The writer thread is gone: the child's PTY died (write error) or the
    /// pane is being torn down.
    Closed,
}

impl std::fmt::Display for StdinSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StdinSendError::Full => write!(f, "pane input queue full (child not reading)"),
            StdinSendError::Closed => write!(f, "pane input closed (child exited)"),
        }
    }
}

impl std::error::Error for StdinSendError {}

/// Where a writer's logs go. The daemon must keep its `thegn::daemon` target +
/// `session` field (grep-ability of daemon logs); local panes log under
/// `thegn::pane` with the pane id. An enum (not a plain label string) because
/// `tracing` targets must be literals at the macro call site.
#[derive(Clone)]
pub(crate) enum WriterLog {
    /// A compositor-local pane.
    Pane { pane: u32 },
    /// A daemon-owned session actor's PTY.
    DaemonSession { session: String },
}

impl WriterLog {
    fn thread_name(&self) -> String {
        match self {
            WriterLog::Pane { pane } => format!("pty-writer-pane-{pane}"),
            WriterLog::DaemonSession { session } => format!("pty-writer-{session}"),
        }
    }

    fn warn_write_failed(&self, e: &std::io::Error) {
        match self {
            WriterLog::Pane { pane } => {
                tracing::warn!(target: "thegn::pane", pane, "pty write failed: {e}");
            }
            WriterLog::DaemonSession { session } => {
                tracing::warn!(target: "thegn::daemon", session = %session, "pty write failed: {e}");
            }
        }
    }

    fn warn_stalled(&self, e: &StdinSendError) {
        match self {
            WriterLog::Pane { pane } => {
                tracing::warn!(target: "thegn::pane", pane, "pty stdin stalled; dropping input: {e}");
            }
            WriterLog::DaemonSession { session } => {
                tracing::warn!(target: "thegn::daemon", session = %session, "pty stdin stalled; dropping input: {e}");
            }
        }
    }
}

/// The sending half of a pane's stdin queue, held where the pane lives (the
/// event loop's pane table / the session actor). `send` never blocks.
pub(crate) struct StdinTx {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    /// Warn-once-per-congestion-episode flag: set on the first failed send,
    /// reset on the next successful one — a wedged pane logs one warning, not
    /// one per keystroke.
    congested: bool,
    log: WriterLog,
}

impl StdinTx {
    /// Queue `bytes` for the writer thread. Non-blocking: a full queue or a
    /// dead writer returns the typed error immediately (and warns once per
    /// congestion episode).
    pub(crate) fn send(&mut self, bytes: Vec<u8>) -> Result<(), StdinSendError> {
        match self.tx.try_send(bytes) {
            Ok(()) => {
                self.congested = false;
                Ok(())
            }
            Err(e) => {
                let err = match e {
                    std::sync::mpsc::TrySendError::Full(_) => StdinSendError::Full,
                    std::sync::mpsc::TrySendError::Disconnected(_) => StdinSendError::Closed,
                };
                if !self.congested {
                    self.congested = true;
                    self.log.warn_stalled(&err);
                }
                Err(err)
            }
        }
    }
}

/// Spawn the dedicated writer thread for one PTY: loops `recv` →
/// `write_all` + `flush`; exits when the sender drops (pane/actor teardown) or
/// the write errors (child's side of the PTY died after exit/kill), after
/// which sends return [`StdinSendError::Closed`].
pub(crate) fn spawn_stdin_writer(mut writer: Box<dyn Write + Send>, log: WriterLog) -> StdinTx {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(STDIN_CHANNEL_CAP);
    let thread_log = log.clone();
    // best-effort: if the thread can't spawn, the receiver drops and stdin
    // degrades to Closed-drop at the send sites.
    // best-effort: stdout write: EPIPE on a closed |head pipe is normal
    let _ = std::thread::Builder::new()
        .name(log.thread_name())
        .spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if let Err(e) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                    thread_log.warn_write_failed(&e);
                    return; // child gone/broken — remaining input has nowhere to go
                }
            }
        });
    StdinTx {
        tx,
        congested: false,
        log,
    }
}

/// Strip bracketed-paste markers embedded in pasted content. Without this, a
/// clipboard payload containing `ESC[201~` closes the paste bracket early and
/// its tail is interpreted as keystrokes (bracketed-paste command injection); a
/// stray start marker is dropped too so the payload can't re-open a nested
/// bracket.
pub(crate) fn neutralize_paste_markers(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains("\x1b[201~") || text.contains("\x1b[200~") {
        std::borrow::Cow::Owned(text.replace("\x1b[201~", "").replace("\x1b[200~", ""))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// Build the byte chunk for one paste: `ESC[200~` + payload + `ESC[201~` when
/// the app requested bracketed paste, bare payload otherwise. ONE buffer so
/// the whole paste is queued or dropped as a unit — a congestion drop can
/// never leave the app inside an open paste bracket, and no keystroke can land
/// between the markers.
///
/// The bracketed path ALWAYS neutralizes embedded markers
/// ([`neutralize_paste_markers`]) — including for the terminal
/// `InputEvent::Paste` path, which historically wrote the payload raw (a
/// deliberate hardening change: an embedded `ESC[201~` must not close the
/// bracket early). The non-bracketed path stays raw: without brackets there is
/// no bracket to break out of.
pub(crate) fn build_paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let payload = neutralize_paste_markers(text);
        let mut out = Vec::with_capacity(payload.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(payload.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// A `Write` sink that records everything and optionally sleeps per write
    /// (slow consumer) — shared handle for assertions.
    #[derive(Clone)]
    struct Recorder {
        buf: Arc<Mutex<Vec<u8>>>,
        delay: Duration,
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.buf.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn wait_for_len(buf: &Arc<Mutex<Vec<u8>>>, want: usize, deadline: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if buf.lock().unwrap().len() >= want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn ordered_under_slow_consumer() {
        // A slow consumer must not reorder: a large "paste" chunk followed by
        // keystroke chunks drains in exactly enqueue order.
        let buf = Arc::new(Mutex::new(Vec::new()));
        let rec = Recorder {
            buf: buf.clone(),
            delay: Duration::from_millis(1),
        };
        let mut tx = spawn_stdin_writer(Box::new(rec), WriterLog::Pane { pane: 1 });
        let paste = vec![b'P'; 64 * 1024];
        let mut expected = Vec::new();
        tx.send(paste.clone()).unwrap();
        expected.extend_from_slice(&paste);
        for k in [b"a".to_vec(), b"b".to_vec(), b"\r".to_vec()] {
            tx.send(k.clone()).unwrap();
            expected.extend_from_slice(&k);
        }
        assert!(
            wait_for_len(&buf, expected.len(), Duration::from_secs(5)),
            "writer thread did not drain the queue"
        );
        assert_eq!(
            *buf.lock().unwrap(),
            expected,
            "byte order must match enqueue order"
        );
    }

    /// A `Write` sink parked on a condvar until released; records after.
    struct Gated {
        buf: Arc<Mutex<Vec<u8>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Write for Gated {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cv) = &*self.gate;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            drop(open);
            self.buf.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn full_queue_returns_full_without_blocking() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let sink = Gated {
            buf: buf.clone(),
            gate: gate.clone(),
        };
        let mut tx = spawn_stdin_writer(Box::new(sink), WriterLog::Pane { pane: 2 });
        // Fill: the writer thread takes chunks off the queue but parks in
        // write(), so after CAP + a-few sends the queue must report Full.
        // Chunks are single distinct bytes so order is assertable after.
        let start = Instant::now();
        let mut sent = Vec::new();
        let mut got_full = false;
        for i in 0..(STDIN_CHANNEL_CAP + 8) {
            let b = vec![(i % 251) as u8];
            match tx.send(b.clone()) {
                Ok(()) => sent.extend_from_slice(&b),
                Err(StdinSendError::Full) => {
                    got_full = true;
                    break;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(got_full, "queue never reported Full");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "send must fail fast, not block: {:?}",
            start.elapsed()
        );
        // Release the consumer: surviving chunks drain in enqueue order.
        {
            let (lock, cv) = &*gate;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        assert!(
            wait_for_len(&buf, sent.len(), Duration::from_secs(5)),
            "surviving chunks did not drain"
        );
        assert_eq!(
            *buf.lock().unwrap(),
            sent,
            "surviving chunks must keep relative order"
        );
    }

    /// A `Write` sink that errors on every write.
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("child gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn closed_after_write_error() {
        let mut tx = spawn_stdin_writer(Box::new(Broken), WriterLog::Pane { pane: 3 });
        // First send queues fine; the thread then hits the write error and
        // exits, dropping the receiver — later sends must see Closed.
        tx.send(b"x".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match tx.send(b"y".to_vec()) {
                Err(StdinSendError::Closed) => break, // pass
                Ok(()) | Err(StdinSendError::Full) => {
                    // Thread hasn't observed the error yet — bounded retry.
                    assert!(
                        Instant::now() < deadline,
                        "sends never turned Closed after the write error"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    #[test]
    fn build_paste_bytes_bracketed_is_one_wrapped_buffer() {
        let out = build_paste_bytes("hello\nworld", true);
        assert_eq!(out, b"\x1b[200~hello\nworld\x1b[201~".to_vec());
    }

    #[test]
    fn build_paste_bytes_bracketed_neutralizes_embedded_markers() {
        // Bracketed-paste injection: an embedded end marker must not close the
        // bracket early (hardened for BOTH the register and terminal paths).
        let out = build_paste_bytes("ls\x1b[201~\nrm -rf ~\n", true);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b[200~") && s.ends_with("\x1b[201~"));
        let inner = &s["\x1b[200~".len()..s.len() - "\x1b[201~".len()];
        assert!(!inner.contains("\x1b[201~"), "embedded end marker survived");
        assert!(
            inner.contains("rm -rf ~"),
            "content preserved (just defused)"
        );
    }

    #[test]
    fn build_paste_bytes_plain_is_raw_payload() {
        // No bracket, nothing to break out of: payload passes through raw.
        let text = "a\x1b[200~b";
        assert_eq!(build_paste_bytes(text, false), text.as_bytes().to_vec());
    }

    #[test]
    fn neutralize_paste_markers_strips_embedded_brackets() {
        // A clipboard payload that tries to close the paste bracket early and
        // inject a command must have its markers removed before it is written
        // into the pane.
        let hostile = "ls\x1b[201~\nrm -rf ~\n";
        let safe = neutralize_paste_markers(hostile);
        assert!(!safe.contains("\x1b[201~"), "end marker survived: {safe:?}");
        assert!(
            safe.contains("rm -rf ~"),
            "content preserved (just defused)"
        );

        // A stray start marker is dropped too.
        let with_start = "a\x1b[200~b";
        assert_eq!(neutralize_paste_markers(with_start), "ab");

        // Clean text is passed through by reference (no allocation, no change).
        let clean = "plain text\nline two";
        assert!(matches!(
            neutralize_paste_markers(clean),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(neutralize_paste_markers(clean), clean);
    }
}
