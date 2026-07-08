//! One daemon-owned session: a PTY + the **authoritative** emulator + a
//! bounded history ring + the subscriber fan-out, all owned by a single actor
//! task.
//!
//! The actor is the sole consumer of both the PTY reader channel and the
//! control mailbox, which is what makes the warm-attach ordering guarantee
//! trivial: an `Attach` is processed *between* output chunks, so the snapshot
//! it takes (tagged `seq`) and the subscriber insertion are atomic — the
//! subscriber's first delta is exactly `seq + 1`, no gap, no overlap.
//!
//! Backpressure: each subscriber has a bounded frame channel fed with
//! `try_send`. A full channel marks the subscriber *lagged* and drops further
//! deltas **for it only** (the PTY reader and the authoritative emulator never
//! block on a slow client); once its channel drains below half, it gets a
//! fresh snapshot (`Resync` semantics — an idempotent full repaint) and
//! resumes deltas.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, mpsc, oneshot};

use superzej_core::control_wire::EventFrame;
use superzej_core::history::{AnsiStripper, HistoryBuffer, feed_bytes_to_history};
use superzej_core::term_snapshot::{ScreenSnapshot, SnapCell, SnapColor, encode_ansi};
use superzej_svc::control::{AttachKind, AttachReply, ControlError, SessionInfo};

use crate::emulator::{AlacrittyEmulator, CellColor, PaneEmulator};
use crate::pane::PaneEvent;
use crate::pane_pty::PtyHandle;

/// Per-subscriber frame-channel capacity. At the 8 KB PTY read size this
/// bounds a slow client to ~2 MB of queued output before it degrades to
/// snapshot-resync.
const SUB_CHANNEL_CAP: usize = 256;

/// History lines folded into a warm-attach snapshot (scrollback context).
const SNAPSHOT_HISTORY_LINES: usize = 2_000;

/// The actor's control mailbox.
pub(crate) enum SessionMsg {
    Attach {
        client_id: String,
        kind: AttachKind,
        rows: u16,
        cols: u16,
        reply: oneshot::Sender<Result<AttachReply, ControlError>>,
    },
    Detach {
        client_id: String,
    },
    Stdin(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
    Snapshot {
        reply: oneshot::Sender<EventFrame>,
    },
    Kill,
}

/// Live, actor-maintained bits of a session's listing row (the static parts
/// live in [`SessionMeta`]).
#[derive(Debug, Default)]
pub(crate) struct LiveMeta {
    pub rows: u16,
    pub cols: u16,
    pub attached: u32,
}

/// The static identity of a session, fixed at open.
#[derive(Debug, Clone)]
pub(crate) struct SessionMeta {
    pub id: String,
    pub worktree: Option<String>,
    pub program: String,
    pub cwd: Option<String>,
    pub created_at_ms: i64,
}

impl SessionMeta {
    pub(crate) fn info(&self, live: &LiveMeta, lease_expires_at: Option<i64>) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            worktree: self.worktree.clone(),
            program: self.program.clone(),
            cwd: self.cwd.clone(),
            rows: live.rows,
            cols: live.cols,
            created_at_ms: self.created_at_ms,
            attached_clients: live.attached,
            lease_expires_at,
        }
    }
}

/// Sub-count transitions the daemon's lease bookkeeping listens to: `idle`
/// (last subscriber left — open a relay lease) and busy (first subscriber in —
/// release it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdleTransition {
    pub session: String,
    pub idle: bool,
}

struct Subscriber {
    client_id: String,
    tx: mpsc::Sender<EventFrame>,
    lagged: bool,
}

pub(crate) struct SessionActor {
    meta: SessionMeta,
    live: Arc<Mutex<LiveMeta>>,
    pty: PtyHandle,
    emulator: Box<dyn PaneEmulator>,
    history: HistoryBuffer,
    history_partial: Vec<u8>,
    history_stripper: AnsiStripper,
    subs: Vec<Subscriber>,
    /// Monotone per-output-chunk sequence; a snapshot at `seq` folds chunks
    /// `..=seq`, the next delta carries `seq + 1`.
    seq: u64,
    events: broadcast::Sender<Arc<EventFrame>>,
    idle_tx: mpsc::UnboundedSender<IdleTransition>,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, super::service::SessionEntry>>>,
    /// Per-subscriber channel capacity ([`SUB_CHANNEL_CAP`]; shrunk in tests
    /// to exercise the lag/resync path without megabytes of output).
    sub_cap: usize,
}

impl SessionActor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        meta: SessionMeta,
        live: Arc<Mutex<LiveMeta>>,
        pty: PtyHandle,
        rows: u16,
        cols: u16,
        events: broadcast::Sender<Arc<EventFrame>>,
        idle_tx: mpsc::UnboundedSender<IdleTransition>,
        sessions: Arc<tokio::sync::Mutex<HashMap<String, super::service::SessionEntry>>>,
    ) -> Self {
        Self {
            meta,
            live,
            pty,
            emulator: Box::new(AlacrittyEmulator::new(rows, cols, 10_000)),
            history: HistoryBuffer::new(10_000),
            history_partial: Vec::new(),
            history_stripper: AnsiStripper::default(),
            subs: Vec::new(),
            seq: 0,
            events,
            idle_tx,
            sessions,
            sub_cap: SUB_CHANNEL_CAP,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_sub_cap(&mut self, cap: usize) {
        self.sub_cap = cap;
    }

    /// The actor task: sole consumer of the PTY reader channel and the control
    /// mailbox until the child exits or the session is killed.
    pub(crate) async fn run(
        mut self,
        mut pane_rx: mpsc::Receiver<PaneEvent>,
        mut msg_rx: mpsc::Receiver<SessionMsg>,
    ) {
        let exit_code: Option<i32> = loop {
            tokio::select! {
                ev = pane_rx.recv() => match ev {
                    Some(PaneEvent::Output(_, bytes)) => self.on_output(&bytes),
                    Some(PaneEvent::Exit(_, code)) => break code,
                    None => break None, // reader thread gone without an Exit
                },
                msg = msg_rx.recv() => match msg {
                    Some(SessionMsg::Attach { client_id, kind, rows, cols, reply }) => {
                        let r = self.on_attach(client_id, kind, rows, cols);
                        let _ = reply.send(r);
                    }
                    Some(SessionMsg::Detach { client_id }) => self.on_detach(&client_id),
                    Some(SessionMsg::Stdin(bytes)) => {
                        use std::io::Write;
                        if let Err(e) = self.pty.writer.write_all(&bytes) {
                            tracing::warn!(target: "szhost::daemon", session = %self.meta.id, "pty write failed: {e}");
                        }
                        let _ = self.pty.writer.flush();
                    }
                    Some(SessionMsg::Resize { rows, cols }) => self.on_resize(rows, cols),
                    Some(SessionMsg::Snapshot { reply }) => {
                        let _ = reply.send(self.snapshot_frame());
                    }
                    Some(SessionMsg::Kill) | None => break None,
                },
            }
        };

        // Terminal: tell subscribers (then close their channels by dropping),
        // tell the feed, and remove this session from the daemon's table.
        let exit = EventFrame::SessionExit {
            session: self.meta.id.clone(),
            code: exit_code,
        };
        for sub in &self.subs {
            let _ = sub.tx.try_send(exit.clone());
        }
        let _ = self.events.send(Arc::new(exit));
        let _ = self.events.send(Arc::new(EventFrame::Sessions));
        self.sessions.lock().await.remove(&self.meta.id);
        // The session is gone entirely — no lease should outlive it.
        let _ = self.idle_tx.send(IdleTransition {
            session: self.meta.id.clone(),
            idle: false,
        });
        tracing::debug!(target: "szhost::daemon", session = %self.meta.id, code = ?exit_code, "session ended");
    }

    /// Fold one PTY chunk into the authoritative state and fan it out.
    fn on_output(&mut self, bytes: &[u8]) {
        self.emulator.advance(bytes);
        feed_bytes_to_history(
            bytes,
            &mut self.history,
            &mut self.history_partial,
            &mut self.history_stripper,
        );
        self.seq += 1;
        let session = self.meta.id.clone();
        let seq = self.seq;
        // Deliver to live subscribers; note lagged ones that have drained
        // enough to recover (they get a fresh snapshot instead of this delta —
        // the snapshot already folds it, since the emulator advanced above).
        let mut recovered: Vec<String> = Vec::new();
        let mut pruned = false;
        self.subs.retain_mut(|sub| {
            if sub.lagged {
                if sub.tx.capacity() >= self.sub_cap.div_ceil(2) {
                    sub.lagged = false;
                    recovered.push(sub.client_id.clone());
                }
                return true; // recovered subs resync below; still-lagged drop this delta
            }
            match sub.tx.try_send(EventFrame::PaneDelta {
                session: session.clone(),
                seq,
                bytes: bytes.to_vec(),
            }) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    sub.lagged = true;
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    pruned = true;
                    false // client vanished without detach
                }
            }
        });
        if !recovered.is_empty() {
            let frame = self.snapshot_frame();
            for sub in self
                .subs
                .iter()
                .filter(|s| recovered.iter().any(|c| c == &s.client_id))
            {
                let _ = sub.tx.try_send(frame.clone());
            }
        }
        if pruned {
            self.after_sub_change();
        }
    }

    fn on_attach(
        &mut self,
        client_id: String,
        kind: AttachKind,
        rows: u16,
        cols: u16,
    ) -> Result<AttachReply, ControlError> {
        // Last interactive writer wins the PTY size; observers never resize.
        if kind == AttachKind::Interactive {
            self.on_resize(rows, cols);
        }
        let snapshot = self.snapshot_frame();
        let (tx, rx) = mpsc::channel(self.sub_cap);
        // Replace a stale subscription from the same client (reconnect).
        self.subs.retain(|s| s.client_id != client_id);
        self.subs.push(Subscriber {
            client_id,
            tx,
            lagged: false,
        });
        self.after_sub_change();
        Ok(AttachReply {
            snapshot,
            frames: rx,
        })
    }

    fn on_detach(&mut self, client_id: &str) {
        let before = self.subs.len();
        self.subs.retain(|s| s.client_id != client_id);
        if self.subs.len() != before {
            self.after_sub_change();
        }
    }

    /// Maintain the live listing row + signal idle/busy transitions for the
    /// lease bookkeeping.
    fn after_sub_change(&mut self) {
        let attached = self.subs.len() as u32;
        if let Ok(mut live) = self.live.lock() {
            live.attached = attached;
        }
        let _ = self.idle_tx.send(IdleTransition {
            session: self.meta.id.clone(),
            idle: attached == 0,
        });
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        let (cur_rows, cur_cols) = self.emulator.size();
        if (cur_rows, cur_cols) == (rows, cols) {
            return; // no-op resize: don't SIGWINCH the child
        }
        if let Err(e) = self.pty.master.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            tracing::warn!(target: "szhost::daemon", session = %self.meta.id, "pty resize failed: {e}");
        }
        self.emulator.resize(rows, cols);
        if let Ok(mut live) = self.live.lock() {
            live.rows = rows;
            live.cols = cols;
        }
    }

    /// Serialize the authoritative screen as an ANSI repaint frame at the
    /// current `seq` (the warm-attach snapshot).
    fn snapshot_frame(&self) -> EventFrame {
        let snap = snapshot_of(
            self.emulator.as_ref(),
            &self.history,
            SNAPSHOT_HISTORY_LINES,
            self.seq,
        );
        EventFrame::PaneSnapshot {
            session: self.meta.id.clone(),
            seq: snap.seq,
            cols: snap.cols,
            rows: snap.rows,
            bytes: encode_ansi(&snap),
        }
    }
}

fn snap_color(c: CellColor) -> SnapColor {
    match c {
        CellColor::Default => SnapColor::Default,
        CellColor::Indexed(n) => SnapColor::Indexed(n),
        CellColor::Rgb(r, g, b) => SnapColor::Rgb(r, g, b),
    }
}

/// Lower the emulator grid + history tail into the pure snapshot model
/// (`superzej_core::term_snapshot`). The daemon's emulator is never scrolled,
/// so `cell()` reads the live screen.
pub(crate) fn snapshot_of(
    emu: &dyn PaneEmulator,
    history: &HistoryBuffer,
    history_lines: usize,
    seq: u64,
) -> ScreenSnapshot {
    let (rows, cols) = emu.size();
    let alt_screen = emu.alt_screen();
    let mut cells = Vec::with_capacity(rows as usize * cols as usize);
    for row in 0..rows {
        for col in 0..cols {
            let cell = emu.cell(row, col).unwrap_or_default();
            let wide = unicode_width::UnicodeWidthStr::width(cell.text.as_str()) >= 2;
            cells.push(SnapCell {
                text: cell.text,
                fg: snap_color(cell.fg),
                bg: snap_color(cell.bg),
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline,
                inverse: cell.inverse,
                wide,
            });
        }
    }
    let history_tail = if alt_screen {
        String::new()
    } else {
        let total = history.len();
        let start = total.saturating_sub(history_lines);
        let mut lines: Vec<&str> = (start..total).filter_map(|i| history.get(i)).collect();
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    };
    ScreenSnapshot {
        rows,
        cols,
        cursor: emu.cursor(),
        cursor_visible: emu.cursor_visible(),
        alt_screen,
        app_cursor: emu.application_cursor(),
        bracketed_paste: emu.bracketed_paste(),
        history_tail,
        cells,
        seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::control_wire::EventFrame;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            worktree: None,
            program: "sh".into(),
            cwd: None,
            created_at_ms: 0,
        }
    }

    struct Harness {
        msg_tx: mpsc::Sender<SessionMsg>,
        live: Arc<Mutex<LiveMeta>>,
        idle_rx: mpsc::UnboundedReceiver<IdleTransition>,
    }

    /// Spawn a real PTY session actor running `script` under `/bin/sh -c`.
    fn spawn_actor(script: &str, sub_cap: Option<usize>) -> Harness {
        let (pane_tx, pane_rx) = mpsc::channel(256);
        let pty = crate::pane_pty::open_pty(
            0,
            &["/bin/sh".into(), "-c".into(), script.into()],
            None,
            &[],
            24,
            80,
            pane_tx,
            None,
        )
        .expect("open pty");
        let (events, _keep) = broadcast::channel(64);
        std::mem::forget(_keep); // keep the feed open for the actor's lifetime
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let live = Arc::new(Mutex::new(LiveMeta {
            rows: 24,
            cols: 80,
            attached: 0,
        }));
        let mut actor = SessionActor::new(
            meta("s1"),
            live.clone(),
            pty,
            24,
            80,
            events,
            idle_tx,
            sessions,
        );
        if let Some(cap) = sub_cap {
            actor.set_sub_cap(cap);
        }
        let (msg_tx, msg_rx) = mpsc::channel(16);
        tokio::spawn(actor.run(pane_rx, msg_rx));
        Harness {
            msg_tx,
            live,
            idle_rx,
        }
    }

    async fn attach(
        h: &Harness,
        client: &str,
        kind: AttachKind,
        rows: u16,
        cols: u16,
    ) -> AttachReply {
        let (tx, rx) = oneshot::channel();
        h.msg_tx
            .send(SessionMsg::Attach {
                client_id: client.into(),
                kind,
                rows,
                cols,
                reply: tx,
            })
            .await
            .expect("actor alive");
        rx.await.expect("reply").expect("attach ok")
    }

    fn snapshot_parts(frame: &EventFrame) -> (u64, String) {
        match frame {
            EventFrame::PaneSnapshot { seq, bytes, .. } => {
                (*seq, String::from_utf8_lossy(bytes).into_owned())
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    /// The control-plane spec's "Pane survives client detach" +
    /// "Reattach restores live screen" scenarios, plus the seq contract:
    /// output produced while NO client was attached is present in the
    /// reattach snapshot, and the first live delta is exactly snapshot.seq+1.
    #[tokio::test(flavor = "multi_thread")]
    async fn pane_survives_detach_and_warm_reattaches() {
        let h = spawn_actor("echo marker1; sleep 0.3; echo marker2; cat", None);
        let first = attach(&h, "c1", AttachKind::Interactive, 24, 80).await;
        let (seq0, _) = snapshot_parts(&first.snapshot);
        h.msg_tx
            .send(SessionMsg::Detach {
                client_id: "c1".into(),
            })
            .await
            .unwrap();
        // The child keeps writing while detached (marker2 after 300ms).
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;

        let mut second = attach(&h, "c2", AttachKind::Interactive, 24, 80).await;
        let (seq1, screen) = snapshot_parts(&second.snapshot);
        assert!(
            screen.contains("marker1") && screen.contains("marker2"),
            "detached-period output must be in the snapshot: {screen:?}"
        );
        assert!(seq1 > seq0, "output advanced the sequence while detached");

        // Live continuity: stdin echoes back through `cat`; the first delta
        // after the snapshot carries seq+1.
        h.msg_tx
            .send(SessionMsg::Stdin(b"hello\n".to_vec()))
            .await
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), second.frames.recv())
            .await
            .expect("delta within 5s")
            .expect("stream open");
        match frame {
            EventFrame::PaneDelta { seq, .. } => assert_eq!(seq, seq1 + 1),
            other => panic!("expected first delta after snapshot, got {other:?}"),
        }
    }

    /// Idle/busy transitions drive the lease bookkeeping: last-out signals
    /// idle, first-in signals busy.
    #[tokio::test(flavor = "multi_thread")]
    async fn idle_transitions_on_attach_detach() {
        let mut h = spawn_actor("cat", None);
        let _r = attach(&h, "c1", AttachKind::Interactive, 24, 80).await;
        assert_eq!(
            h.idle_rx.recv().await,
            Some(IdleTransition {
                session: "s1".into(),
                idle: false
            })
        );
        h.msg_tx
            .send(SessionMsg::Detach {
                client_id: "c1".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            h.idle_rx.recv().await,
            Some(IdleTransition {
                session: "s1".into(),
                idle: true
            })
        );
    }

    /// Resize policy: observers never resize the PTY; interactive attaches do.
    #[tokio::test(flavor = "multi_thread")]
    async fn observer_never_resizes() {
        let h = spawn_actor("cat", None);
        let _obs = attach(&h, "obs", AttachKind::Observer, 10, 40).await;
        {
            let live = h.live.lock().unwrap();
            assert_eq!((live.rows, live.cols), (24, 80), "observer must not resize");
        }
        let _int = attach(&h, "int", AttachKind::Interactive, 30, 100).await;
        // Resize is applied by the actor task; poll until visible.
        for _ in 0..50 {
            {
                let live = h.live.lock().unwrap();
                if (live.rows, live.cols) == (30, 100) {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("interactive attach did not resize the PTY");
    }

    /// A slow subscriber degrades to snapshot-resync instead of blocking the
    /// PTY: flood output while never draining, then drain and expect a fresh
    /// snapshot to arrive (not the dropped deltas).
    #[tokio::test(flavor = "multi_thread")]
    async fn lagged_subscriber_gets_snapshot_resync() {
        let h = spawn_actor("cat", Some(2));
        let mut r = attach(&h, "slow", AttachKind::Interactive, 24, 80).await;
        // Generate more chunks than the capacity-2 channel can hold. Each
        // line echo is at least one PTY chunk.
        for i in 0..40 {
            h.msg_tx
                .send(SessionMsg::Stdin(format!("line{i}\n").into_bytes()))
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Drain everything queued; keep reading until a resync snapshot lands
        // (the actor sends it once the channel has drained below half).
        let mut saw_snapshot = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(200), r.frames.recv()).await
            {
                Ok(Some(EventFrame::PaneSnapshot { .. })) => {
                    saw_snapshot = true;
                    break;
                }
                Ok(Some(_)) => {
                    // Deltas drain the channel; nudge more output so the actor
                    // notices the recovery and emits the resync.
                    let _ = h.msg_tx.send(SessionMsg::Stdin(b"x\n".to_vec())).await;
                }
                Ok(None) => break,
                Err(_) => {
                    let _ = h.msg_tx.send(SessionMsg::Stdin(b"y\n".to_vec())).await;
                }
            }
        }
        assert!(
            saw_snapshot,
            "lagged subscriber must receive a resync snapshot"
        );
    }

    /// Round-trip: an encoded snapshot fed to a fresh emulator reproduces the
    /// source grid cell-for-cell (the pure golden tests in core can't do this
    /// cross-check — it needs a real emulator).
    #[test]
    fn snapshot_ansi_round_trips_through_an_emulator() {
        let mut a = AlacrittyEmulator::new(12, 40, 1000);
        a.advance(b"plain \x1b[1;31mbold-red\x1b[0m tail\r\n");
        a.advance(b"\x1b[44mblue-bg\x1b[0m and wide: \xe6\xbc\xa2 done\r\n");
        a.advance(b"third line");
        let history = HistoryBuffer::new(100);
        let snap = snapshot_of(&a, &history, 0, 7);
        assert_eq!(snap.seq, 7);
        let bytes = encode_ansi(&snap);

        let mut b = AlacrittyEmulator::new(12, 40, 1000);
        b.advance(&bytes);
        for row in 0..12u16 {
            for col in 0..40u16 {
                let ca = a.cell(row, col).unwrap_or_default();
                let cb = b.cell(row, col).unwrap_or_default();
                // Blank and empty are visually identical.
                let norm = |c: &crate::emulator::GridCell| {
                    let mut c = c.clone();
                    if c.text == " " {
                        c.text = String::new();
                    }
                    c
                };
                assert_eq!(
                    norm(&ca),
                    norm(&cb),
                    "cell ({row},{col}) diverged after round-trip"
                );
            }
        }
        assert_eq!(a.cursor(), b.cursor());
    }
}
