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

use thegn_core::activity_step::Agentness;
use thegn_core::agent_error::{AgentErrorSignatures, AgentErrorState};
use thegn_core::attention::{AttentionTier, PaneAgentState, pane_agent_state};
use thegn_core::config::Config;
use thegn_core::control_wire::EventFrame;
use thegn_core::graveyard::Graveyard;
use thegn_core::history::{AnsiStripper, HistoryBuffer, feed_bytes_to_history};
use thegn_core::osc_attention::{AttentionSignal, OscAttentionScanner};
use thegn_core::session_activity::{Observation, SessionActivity};
use thegn_core::term_snapshot::{ScreenSnapshot, SnapCell, SnapColor, encode_ansi};
use thegn_svc::control::{
    AttachKind, AttachReply, ControlError, RecordSpec, RecordStatus, SessionActivityEvent,
    SessionInfo,
};

use crate::emulator::{AlacrittyEmulator, CellColor, PaneEmulator};
use crate::pane::PaneEvent;
use crate::pane_pty::PtyHandle;

use super::tombstone::{TOMBSTONE_HISTORY_LINES, Tombstone};

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
        /// Include the scrollback history tail in the snapshot (first attach
        /// of a fresh client emulator); reconnects pass `false` so the tail
        /// isn't duplicated into scrollback the client already holds.
        history: bool,
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
    /// "What is this agent doing *right now*?" — a synchronous level check.
    ///
    /// `wait` asks this after subscribing to the event feed and before blocking
    /// on it. That ordering is the whole correctness argument: subscribe first
    /// so no transition can slip through the gap, then check the level so a
    /// condition that is *already* true resolves immediately instead of waiting
    /// for a transition that will never come.
    Probe {
        reply: oneshot::Sender<ProbeReply>,
    },
    /// Register an output matcher for `wait --until match:<regex>`.
    ///
    /// The actor scans the retained scrollback on receipt — a supervisor that
    /// spawns an agent, does something else, and only then asks to wait must
    /// not deadlock on a line that already scrolled past — and thereafter tests
    /// only newly completed lines.
    WatchOutput {
        re: Box<regex::Regex>,
        reply: oneshot::Sender<u64>,
    },
    /// Start/stop/query this session's asciicast recording. The actor owns the
    /// recorder, so recording continues while every client is detached.
    Record {
        spec: RecordSpec,
        reply: oneshot::Sender<Result<RecordStatus, ControlError>>,
    },
    Kill,
}

/// The answer to [`SessionMsg::Probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeReply {
    pub state: PaneAgentState,
    /// Whether this session has ever been observed busy. `wait --until idle`
    /// reads it so "wait until the agent finishes" is not answered instantly by
    /// a session that has not started working yet.
    pub ever_busy: bool,
}

/// Live, actor-maintained bits of a session's listing row (the static parts
/// live in [`SessionMeta`]).
#[derive(Debug, Default)]
pub(crate) struct LiveMeta {
    pub rows: u16,
    pub cols: u16,
    pub attached: u32,
    /// Path of the `.cast` file while this session is *actively* being
    /// recorded, surfaced in listings so an attached UI can show a recording
    /// indicator. Cleared when recording stops (the finalized path is still
    /// reported by `sessions.record` status and the tombstone).
    pub recording: Option<String>,
}

/// The static identity of a session, fixed at open.
#[derive(Debug, Clone)]
pub(crate) struct SessionMeta {
    pub id: String,
    pub worktree: Option<String>,
    pub program: String,
    pub cwd: Option<String>,
    pub created_at_ms: i64,
    /// The PTY child's pid, exposed in listings so a same-host compositor can
    /// capture the pane's live cwd/foreground command from `/proc`.
    pub pid: Option<u32>,
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
            pid: self.pid,
            exited_at_ms: None,
            exit_code: None,
            final_state: None,
            recording: live.recording.clone(),
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
    /// Observers are excluded from the interactive busy-count that drives the
    /// lease bookkeeping — an `Observer` never holds the relay lease open.
    kind: AttachKind,
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
    /// Interactive subscriber count after the last `after_sub_change`, so only
    /// 0↔nonzero transitions signal the lease bookkeeping (observer churn on a
    /// detached session must not refresh its relay grace).
    prev_interactive: u32,
    /// Monotone per-output-chunk sequence; a snapshot at `seq` folds chunks
    /// `..=seq`, the next delta carries `seq + 1`.
    seq: u64,
    events: broadcast::Sender<Arc<EventFrame>>,
    idle_tx: mpsc::UnboundedSender<IdleTransition>,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, super::service::SessionEntry>>>,
    /// Where this session goes when it dies, so a late `wait`/`snapshot` still
    /// gets an answer instead of a 404.
    tombs: Arc<tokio::sync::Mutex<Graveyard<Tombstone>>>,
    /// For persisting an attention signal as a notification row — the channel
    /// by which a headless agent's raised hand reaches the sidebar.
    db: super::service::SharedDb,
    /// Per-subscriber channel capacity ([`SUB_CHANNEL_CAP`]; shrunk in tests
    /// to exercise the lag/resync path without megabytes of output).
    sub_cap: usize,

    // ── the supervision signals ──────────────────────────────────────────────
    /// This session's observer of the activity FSM.
    activity: SessionActivity,
    /// Thresholds and the recognized-agent list.
    cfg: Arc<Config>,
    /// Whether this session is running something that can *be* an agent. A
    /// plain shell reports `Idle` forever — the same rule the sidebar's dots
    /// apply, so a terminal never claims to be an agent that finished.
    has_agent: bool,
    /// Raised hand: set by an `OSC 9`/`OSC 777` notification, cleared when the
    /// user answers or the agent resumes on its own.
    attention: Option<AttentionSignal>,
    /// Generation of the raised hand, bumped **in order on the actor loop**
    /// every time the hand goes up or comes down. The `session_attention`
    /// writes ride `spawn_blocking`, which orders nothing between tasks: an
    /// answer typed in the same instant as the signal could otherwise run its
    /// DELETE before the INSERT and leave a hand up that nobody is waiting
    /// behind — the un-clearable nag THE-68 is about, reintroduced. The upsert
    /// task carries the generation it was spawned with and skips the write once
    /// the counter has moved past it.
    attention_gen: Arc<std::sync::atomic::AtomicU64>,
    osc: OscAttentionScanner,
    /// Scratch for the OSC scanner, reused so a hot output path allocates
    /// nothing in the overwhelmingly common no-signal case.
    osc_signals: Vec<AttentionSignal>,
    /// The last state broadcast on the feed, so transitions are edge-triggered.
    last_state: PaneAgentState,
    /// Live `wait --until match:<regex>` registrations. Pruned as their waiters
    /// time out and drop the receiving end.
    matchers: Vec<(regex::Regex, oneshot::Sender<u64>)>,

    // ── recording (`sessions.record`) ────────────────────────────────────────
    /// The active asciicast recorder, teed in `on_output`. `None` ⇒ the tee is
    /// a single null check (free when off).
    recorder: Option<super::record::Recorder>,
    /// Path of the current or most-recently-finalized recording, so a `status`
    /// or `stop` call can report where it was written.
    record_last_path: Option<std::path::PathBuf>,
    /// Whether the last recording stopped by hitting `[recording] max_bytes`.
    record_capped: bool,
    /// Why the last recording could not be finalized cleanly, if it couldn't:
    /// the `.cast` on disk is truncated and must not be reported as saved.
    record_truncated: Option<String>,

    // ── harness-failure classification (THE-89) ─────────────────────────────
    /// The session's current harness-failure state. Set when a completed
    /// history line matches one of [`Self::error_signatures`]; cleared the
    /// next time a chunk completes with no match (the agent resumed).
    error_state: AgentErrorState,
    /// The configured signature list. Defaults are shipped in core; the
    /// operator can empty the list to disable text-based detection entirely
    /// (the daemon's `cfg.notifications.agent_error_signatures`).
    error_signatures: AgentErrorSignatures,
    /// The last `error_active` value that went out on the broadcast feed.
    /// Tracked separately from `last_state` so an error-state transition
    /// publishes on its own (the activity FSM's state word is orthogonal
    /// to whether a banner is showing).
    last_published_error_active: bool,
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
        tombs: Arc<tokio::sync::Mutex<Graveyard<Tombstone>>>,
        db: super::service::SharedDb,
        cfg: Arc<Config>,
    ) -> Self {
        let has_agent = is_agent_program(&meta.program, &cfg);
        let error_signatures = AgentErrorSignatures {
            signatures: cfg.notifications.agent_error_signatures.clone(),
        };
        Self {
            emulator: Box::new(AlacrittyEmulator::new(rows, cols, 10_000)),
            history: HistoryBuffer::new(10_000),
            history_partial: Vec::new(),
            history_stripper: AnsiStripper::default(),
            subs: Vec::new(),
            prev_interactive: 0,
            seq: 0,
            events,
            idle_tx,
            sessions,
            tombs,
            db,
            sub_cap: SUB_CHANNEL_CAP,
            activity: SessionActivity::new(unix_now_secs()),
            has_agent,
            cfg,
            attention: None,
            attention_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            osc: OscAttentionScanner::new(),
            osc_signals: Vec::new(),
            last_state: PaneAgentState::Idle,
            matchers: Vec::new(),
            recorder: None,
            record_last_path: None,
            record_capped: false,
            record_truncated: None,
            error_state: AgentErrorState::default(),
            error_signatures,
            last_published_error_active: false,
            meta,
            live,
            pty,
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
        // PTY stdin rides a dedicated writer thread: `write_all` blocks once
        // the child stops draining stdin and the kernel's ~64KB PTY buffer
        // fills, and a blocked actor can neither fan out output nor process
        // Kill (the compositor's keep-blocking-I/O-off-the-loop rule applies
        // to the actor task too). Bounded + `try_send` keeps the mailbox
        // live; overflow drops the chunk with one warning per congestion
        // episode — the pane is wedged until the child reads anyway. The
        // thread exits when the sender drops (actor teardown) or the child's
        // side of the PTY dies (write error after kill/exit). Shared with
        // local compositor panes (`crate::pane_writer`); the `DaemonSession`
        // log context keeps `target: "thegn::daemon"` + the session id.
        let mut stdin_tx = {
            let writer = std::mem::replace(&mut self.pty.writer, Box::new(std::io::sink()));
            crate::pane_writer::spawn_stdin_writer(
                writer,
                crate::pane_writer::WriterLog::DaemonSession {
                    session: self.meta.id.clone(),
                },
            )
        };

        // `child_exited` distinguishes a natural child exit (the reader thread
        // already reaped it) from a teardown while the child may still be alive
        // (Kill / mailbox closed) — the latter must actively terminate the PTY
        // child (see below).
        // The activity observer's next deadline, or `None` when the session is
        // settled. This is the daemon's half of the ~0%-idle contract: a
        // finished agent arms no timer at all, so a fleet of idle sessions
        // costs exactly nothing until one of them speaks again.
        let mut next_tick = self.activity_deadline();

        let (exit_code, child_exited): (Option<i32>, bool) = loop {
            tokio::select! {
                ev = pane_rx.recv() => match ev {
                    Some(PaneEvent::Output(_, bytes)) => self.on_output(&bytes),
                    Some(PaneEvent::Exit(_, code)) => break (code, true),
                    // Compositor-relay-only events; a PTY reader never emits them.
                    Some(PaneEvent::SessionFallback(_) | PaneEvent::Reattached(_)) => {}
                    None => break (None, true), // reader gone ⇒ child already EOF'd
                },
                () = sleep_until_opt(next_tick) => self.observe_activity(),
                msg = msg_rx.recv() => match msg {
                    Some(SessionMsg::Attach { client_id, kind, rows, cols, history, reply }) => {
                        let r = self.on_attach(client_id, kind, rows, cols, history);
                        let _ = reply.send(r); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    }
                    Some(SessionMsg::Detach { client_id }) => self.on_detach(&client_id),
                    // best-effort: Full (child not reading) or Closed (child's
                    // PTY died) drops the input — `StdinTx` warns once per
                    // congestion episode under the daemon's log target.
                    Some(SessionMsg::Stdin(bytes)) => {
                        self.on_input();
                        let _ = stdin_tx.send(bytes); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    }
                    Some(SessionMsg::Resize { rows, cols }) => self.on_resize(rows, cols),
                    Some(SessionMsg::Snapshot { reply }) => {
                        // One-shot capture (`thegn session snapshot`): full
                        // context, history included.
                        let _ = reply.send(self.snapshot_frame(true)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    }
                    Some(SessionMsg::Probe { reply }) => {
                        // Fold an observation first: a level check must reflect
                        // the clock, not the last time something happened to
                        // wake the actor.
                        self.observe_activity();
                        // best-effort: the waiter timed out and dropped its half.
                        let _ = reply.send(ProbeReply {
                            state: self.state(),
                            ever_busy: self.activity.ever_busy(),
                        });
                    }
                    Some(SessionMsg::WatchOutput { re, reply }) => self.on_watch(*re, reply),
                    Some(SessionMsg::Record { spec, reply }) => {
                        let _ = reply.send(self.on_record(spec)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                    }
                    Some(SessionMsg::Kill) | None => break (None, false),
                },
            }
            next_tick = self.activity_deadline();
        };

        // Teardown while the child may still be alive (Kill / mailbox closed):
        // actively terminate the PTY child. Dropping the master alone won't hang
        // up the tty — the reader thread holds a cloned fd, so the pty stays
        // open until the child exits — and a daemon-persistent bwrap pane no
        // longer carries `--die-with-parent`, so nothing else reaps the
        // sandbox. bwrap is PID 1 of its namespace, so terminating it collapses
        // the whole namespace. Best-effort; skipped after a natural exit, whose
        // pid the reader thread already reaped (avoids a pid-reuse hazard).
        if !child_exited && let Some(pid) = self.pty.pid {
            crate::platform::terminate_pid(pid);
        }

        // Finalize any active recording FIRST, so the tombstone can carry the
        // finished `.cast` path and `session list` reports it briefly after
        // death. Recording is owned by the actor, so a session exiting is the
        // last thing that stops it (see the spec: stop on exit).
        self.finalize_recording();

        // Bury the corpse BEFORE anything observable. The ordering is the whole
        // fix for a real, reproduced bug: a `wait` woken by the exit event
        // re-queries the session table, and if the entry has already been
        // removed it gets `NotFound` and the run's exit code — and its entire
        // output — are lost. Inserting the tombstone first means the id is
        // never absent from *both* maps, so no observer can look between them
        // and see nothing. This is closed by construction, not by timing.
        let tomb = self.build_tombstone(exit_code);
        self.tombs
            .lock()
            .await
            .insert(self.meta.id.clone(), tomb, now_ms());

        // Any waiter still holding a matcher will never be satisfied now; drop
        // the senders so their `wait` resolves as exited rather than hanging
        // until its timeout.
        self.matchers.clear();

        // Terminal: tell subscribers (then close their channels by dropping),
        // tell the feed, and remove this session from the daemon's table.
        let exit = EventFrame::SessionExit {
            session: self.meta.id.clone(),
            code: exit_code,
        };
        for sub in &self.subs {
            let _ = sub.tx.try_send(exit.clone()); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        }
        let _ = self.events.send(Arc::new(exit)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = self.events.send(Arc::new(EventFrame::Sessions)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        self.sessions.lock().await.remove(&self.meta.id);
        // The session is gone entirely — no lease should outlive it.
        // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = self.idle_tx.send(IdleTransition {
            session: self.meta.id.clone(),
            idle: false,
        });
        // A killed pane must not leave a permanently-raised hand: the session
        // that could answer it is gone (THE-68).
        self.clear_attention_row();
        // The session's agent-error state belongs to a corpse now: a
        // subscription-bridge consumer (or any direct reader) must not see
        // "this session is still raising" once the session is gone. Best
        // effort: the lock is short, and a stale entry costs only one
        // false-positive pass until the next session-list refresh drops it.
        super::agent_error_cache::clear(&self.meta.id);
        tracing::debug!(target: "thegn::daemon", session = %self.meta.id, code = ?exit_code, "session ended");
    }

    /// Fold one PTY chunk into the authoritative state and fan it out.
    ///
    /// This is the single per-session byte funnel, so it is also where every
    /// supervision signal is derived: an `OSC 9`/`OSC 777` raised hand, the
    /// activity FSM's busy stamp, and any registered output matcher. All three
    /// are cheap and short-circuit on the common case (ordinary output, nobody
    /// waiting), because this function runs for every chunk the agent emits.
    fn on_output(&mut self, bytes: &[u8]) {
        // The scanner never consumes or rewrites, so the sequence still reaches
        // the emulator and the scrollback exactly as it arrived.
        self.osc.feed(bytes, &mut self.osc_signals);
        if !self.osc_signals.is_empty() {
            // A chunk carrying several signals: the last one is what the agent
            // most recently asked for.
            let latest = self.osc_signals.drain(..).next_back();
            if let Some(sig) = latest {
                self.on_attention(sig);
            }
        }

        self.emulator.advance(bytes);
        let pushed_before = self.history.total_pushed();
        feed_bytes_to_history(
            bytes,
            &mut self.history,
            &mut self.history_partial,
            &mut self.history_stripper,
        );
        self.seq += 1;

        // THE-89: classify the just-completed lines against the configured
        // harness-failure signature list. One match per chunk is enough —
        // the rest are at best redundant (we already raised) and at worst
        // turn the cache write into per-line work. A chunk with `pushed > 0`
        // and zero matches clears the state: the agent resumed.
        let pushed = self.history.total_pushed() - pushed_before;
        if pushed > 0 && !self.error_signatures.is_empty() {
            let len = self.history.len();
            let start = len.saturating_sub(pushed as usize);
            let mut hit: Option<&str> = None;
            for i in start..len {
                if let Some(line) = self.history.get(i)
                    && thegn_core::agent_error::classify_error_line(line, &self.error_signatures)
                        .is_some()
                {
                    hit = Some(line);
                    break;
                }
            }
            match hit {
                Some(line) => self.error_state.note_error(line),
                None => self.error_state.clear_on_resume(),
            }
            // Mirror the new state into the host-side cache. Cheap in-process
            // write; the cross-process path is a subscription bridge that
            // decodes the same state from the broadcast `Activity` frame.
            super::agent_error_cache::set(
                &self.meta.id,
                self.meta.worktree.clone(),
                self.error_state.error_active,
            );
            // Re-publish on a transition so the broadcast feed reflects the
            // new state (the `Activity` frame is the wire contract for both
            // the in-process reader and the host subscription bridge).
            self.publish_state();
        }

        // Tee to the recorder (the raw chunk, exactly as it arrived). A single
        // null check when off; finalize inline if this chunk crossed the cap.
        if let Some(rec) = self.recorder.as_mut() {
            rec.feed(bytes);
            if rec.done() {
                self.finalize_recording();
            }
        }

        // Output is the busy signal. Fold it now rather than waiting for the
        // tick, so a `Probe` between chunks sees `working` immediately.
        self.activity.note_output(unix_now_secs());
        self.observe_activity();
        self.check_matchers(self.history.total_pushed() - pushed_before);

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
            // Resync is a repaint of an emulator that already holds the
            // scrollback — omit the history tail or it duplicates on every
            // lag recovery.
            let frame = self.snapshot_frame(false);
            for sub in self
                .subs
                .iter()
                .filter(|s| recovered.iter().any(|c| c == &s.client_id))
            {
                let _ = sub.tx.try_send(frame.clone()); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
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
        history: bool,
    ) -> Result<AttachReply, ControlError> {
        // Last interactive writer wins the PTY size; observers never resize.
        if kind == AttachKind::Interactive {
            self.on_resize(rows, cols);
        }
        // A reattach replays scrollback and a full-screen program repaints:
        // output we asked for, not agent work. And a human attaching is the ack
        // edge — the same one focusing a tab applies in the compositor.
        self.activity.note_solicited(unix_now_secs());
        if kind == AttachKind::Interactive {
            self.activity.mark_read();
            self.publish_state();
        }
        let snapshot = self.snapshot_frame(history);
        let (tx, rx) = mpsc::channel(self.sub_cap);
        // Replace a stale subscription from the same client (reconnect).
        self.subs.retain(|s| s.client_id != client_id);
        self.subs.push(Subscriber {
            client_id,
            kind,
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

    /// Maintain the live listing row + signal interactive idle/busy
    /// transitions for the lease bookkeeping. The listing count includes
    /// observers, but the idle signal counts INTERACTIVE subscribers only (an
    /// `Observer` never holds the relay lease open), and only 0↔nonzero
    /// transitions are signaled — observer churn on a detached session must
    /// not refresh (re-open and extend) its relay grace.
    fn after_sub_change(&mut self) {
        let attached = self.subs.len() as u32;
        if let Ok(mut live) = self.live.lock() {
            live.attached = attached;
        }
        let interactive = self
            .subs
            .iter()
            .filter(|s| s.kind == AttachKind::Interactive)
            .count() as u32;
        if (interactive == 0) != (self.prev_interactive == 0) {
            // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            let _ = self.idle_tx.send(IdleTransition {
                session: self.meta.id.clone(),
                idle: interactive == 0,
            });
        }
        self.prev_interactive = interactive;
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
            tracing::warn!(target: "thegn::daemon", session = %self.meta.id, "pty resize failed: {e}");
        }
        self.emulator.resize(rows, cols);
        // Record the geometry change so the cast replays at the right size.
        if let Some(rec) = self.recorder.as_mut() {
            rec.resize(cols, rows);
        }
        // The SIGWINCH repaint that follows is solicited, not agent work.
        self.activity.note_solicited(unix_now_secs());
        if let Ok(mut live) = self.live.lock() {
            live.rows = rows;
            live.cols = cols;
        }
    }

    /// Serialize the authoritative screen as an ANSI repaint frame at the
    /// current `seq` (the warm-attach snapshot). `include_history` folds the
    /// scrollback tail in — wanted by a fresh client emulator, skipped on
    /// resyncs/reconnects whose emulator already holds it.
    fn snapshot_frame(&self, include_history: bool) -> EventFrame {
        let history_lines = if include_history {
            SNAPSHOT_HISTORY_LINES
        } else {
            0
        };
        self.snapshot_frame_with(history_lines)
    }

    /// [`Self::snapshot_frame`] with an explicit scrollback budget — the
    /// tombstone keeps a smaller tail than a live warm attach.
    fn snapshot_frame_with(&self, history_lines: usize) -> EventFrame {
        let snap = snapshot_of(
            self.emulator.as_ref(),
            &self.history,
            history_lines,
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

    // ── supervision ─────────────────────────────────────────────────────────

    /// Whether a real agent is bound to this session, in the FSM's three-valued
    /// vocabulary. A plain shell is a positive `Absent`, not `Unknown`: the
    /// daemon knows what it launched.
    fn agentness(&self) -> Agentness {
        if self.has_agent {
            Agentness::Present
        } else {
            Agentness::Absent
        }
    }

    /// This session's state in the four-word vocabulary a supervisor reasons
    /// about, via the same pure projection the sidebar uses.
    fn state(&self) -> PaneAgentState {
        let tier = if self.attention.is_some() {
            AttentionTier::Blocked
        } else {
            AttentionTier::Idle
        };
        pane_agent_state(self.activity.kind(), tier, self.has_agent)
    }

    /// Fold one observation and broadcast the result if it changed.
    fn observe_activity(&mut self) {
        let now = unix_now_secs();
        self.activity.observe(
            Observation {
                now,
                agent: self.agentness(),
                cpu_busy: false,
            },
            &self.cfg.activity,
        );
        self.publish_state();
    }

    /// When the observer next wants waking, as a tokio deadline.
    fn activity_deadline(&self) -> Option<tokio::time::Instant> {
        let secs = self
            .activity
            .next_tick(&self.cfg.activity, unix_now_secs())?;
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs_f64(secs))
    }

    /// Broadcast a state transition. **Edge-triggered**: a working agent
    /// redrawing its spinner must not put a frame on the feed per chunk.
    fn publish_state(&mut self) {
        let state = self.state();
        if state == self.last_state
            && self.error_state.error_active == self.last_published_error_active
        {
            return;
        }
        self.last_state = state;
        self.last_published_error_active = self.error_state.error_active;
        let ev = SessionActivityEvent {
            session: self.meta.id.clone(),
            worktree: self.meta.worktree.clone(),
            state: state_str(state).to_string(),
            activity: activity_str(self.activity.kind()).to_string(),
            since_ms: self
                .activity
                .state_since()
                .map(|s| (s * 1000.0) as i64)
                .unwrap_or_else(now_ms),
            seq: self.seq,
            message: self.attention.as_ref().map(|a| a.body.clone()),
            // THE-89: the harness-failure banner flag, tracked separately
            // from the activity FSM's four-word state.
            error_active: self.error_state.error_active,
        };
        match serde_json::to_string(&ev) {
            // best-effort: a feed with no subscribers is the normal case.
            Ok(json) => {
                let _ = self.events.send(Arc::new(EventFrame::Activity { json })); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            }
            Err(e) => {
                tracing::warn!(target: "thegn::daemon", session = %self.meta.id, "activity event encode failed: {e}");
            }
        }
    }

    /// A process raised its hand.
    fn on_attention(&mut self, sig: AttentionSignal) {
        tracing::debug!(
            target: "thegn::daemon",
            session = %self.meta.id,
            body = %sig.body,
            "attention signal",
        );
        // A raised hand supersedes a harness banner. Keep the live attention
        // reason singular: once the session is blocked, the user should see
        // the input request rather than a stale failure bit (THE-89).
        self.clear_error_state();
        let message = match &sig.title {
            Some(t) if !t.is_empty() => format!("{t} — {}", sig.body),
            _ => sig.body.clone(),
        };
        let title = sig.title.clone().unwrap_or_default();
        let body = sig.body.clone();
        self.attention = Some(sig);
        self.publish_state();

        // A raised hand is LIVE STATE, not an inbox event: upsert it in
        // `session_attention` (one row per session, deleted the moment the user
        // answers) instead of appending an `agent_attention` notification per
        // agent turn. Claude Code and friends emit one at the end of EVERY
        // turn, so the old write filled the inbox with rows that no "clear all"
        // could retire and that kept the worktree Blocked after it was answered
        // (THE-68). The sidebar still lights up — `attention_status` reads this
        // table into the same `AgentNeedsInput` evidence.
        //
        // A session with no worktree writes nothing: an unattributed hand can
        // light no sidebar row. The live feed state is unchanged either way.
        let Some(worktree) = self.meta.worktree.clone().filter(|w| !w.is_empty()) else {
            return;
        };
        let db = self.db.clone();
        let source = self.meta.id.clone();
        let inbox_row = self.cfg.notifications.agent_attention_inbox;
        // Claimed on the actor loop, checked under the DB lock: see
        // [`Self::attention_gen`].
        let gen_at_raise = self
            .attention_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let attention_gen = self.attention_gen.clone();
        tokio::task::spawn_blocking(move || {
            use thegn_core::store::NotificationStore;
            let db = db.lock().expect("daemon db lock");
            let row = thegn_core::osc_attention::SessionAttention {
                session: source.clone(),
                worktree_path: worktree.clone(),
                title,
                body,
                since: thegn_core::util::now(),
            };
            // Skip the state write if the hand has already come down (the user
            // answered, or the session ended) while this task queued: raising it
            // now would outlive the answer with nothing left to lower it. The
            // opt-in audit row below is unaffected — the agent DID ask, and that
            // trail is meant to record the ask, not the pending state.
            if attention_gen.load(std::sync::atomic::Ordering::SeqCst) == gen_at_raise
                && let Err(e) = db.put_session_attention(&row)
            {
                tracing::warn!(target: "thegn::daemon", "attention state write failed: {e}");
            }
            // Opt-in audit trail (`[notifications] agent_attention_inbox`, off
            // by default). Delete-then-insert per session, so the inbox holds
            // the CURRENT hand rather than one row per agent turn. This path
            // deliberately bypasses `notify::record`: the daemon may be a
            // separate process with no `NotifyState` to route through.
            if inbox_row {
                // Retire this session's previous hand through the existing
                // per-row `delete_notification` rather than growing the trait.
                // It must DELETE, not mark read: the inbox lists read rows too
                // (`get_all_notifications`), so merely marking them read would
                // still leave one row per agent turn in the list — the very
                // pile THE-68 is about, just greyed. For the same reason the
                // sweep is over ALL rows, not just unread ones: a superseded row
                // the user had already marked read (per-row `x`, or `a`) is
                // still IN the list, so scanning only the unread set would let
                // the pile grow one row per turn again after any clear.
                let stale = db.get_all_notifications(usize::MAX).unwrap_or_default();
                for n in stale.iter().filter(|n| {
                    n.kind == thegn_core::notification::NotificationKind::AgentAttention
                        && n.source_ref == source
                }) {
                    if let Err(e) = db.delete_notification(n.id) {
                        tracing::warn!(target: "thegn::daemon", "attention row retire failed: {e}");
                    }
                }
                if let Err(e) = db.put_notification("agent_attention", &source, &message, &worktree)
                {
                    tracing::warn!(target: "thegn::daemon", "attention notification failed: {e}");
                }
            }
        });
    }

    /// Lower this session's raised hand in the shared `session_attention`
    /// table. Off-thread for the same reason the write is: the byte funnel and
    /// the actor loop must never block on SQLite.
    fn clear_attention_row(&self) {
        // Claim a generation on the actor loop BEFORE the delete is queued, so a
        // raise still waiting for a blocking slot sees the counter has moved and
        // declines to write. See [`Self::attention_gen`].
        self.attention_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let db = self.db.clone();
        let session = self.meta.id.clone();
        tokio::task::spawn_blocking(move || {
            use thegn_core::store::NotificationStore;
            let db = db.lock().expect("daemon db lock");
            if let Err(e) = db.clear_session_attention(&session) {
                tracing::warn!(target: "thegn::daemon", "attention state clear failed: {e}");
            }
        });
    }

    /// Input reached the child: suppress the echo that follows, and treat it as
    /// the user answering a raised hand.
    fn on_input(&mut self) {
        self.activity.note_input(unix_now_secs());
        let had_attention = self.attention.take().is_some();
        let had_error = self.clear_error_state();
        if had_attention || had_error {
            self.publish_state();
        }
        if had_attention {
            // The user answered — lower the hand in the shared table too, or
            // the worktree stays Blocked forever (the old notification row did
            // exactly that, against this capability's own spec).
            self.clear_attention_row();
        }
    }

    /// Clear the transient harness-error bit and mirror the transition to the
    /// host-side cache. Returns whether a live error was actually cleared so
    /// callers can publish an otherwise activity-neutral state transition.
    fn clear_error_state(&mut self) -> bool {
        if !self.error_state.error_active {
            return false;
        }
        self.error_state.clear_on_resume();
        super::agent_error_cache::set(
            &self.meta.id,
            self.meta.worktree.clone(),
            self.error_state.error_active,
        );
        true
    }

    /// Register an output matcher, firing at once if the pattern is already in
    /// the retained scrollback.
    fn on_watch(&mut self, re: regex::Regex, reply: oneshot::Sender<u64>) {
        let start = self
            .history
            .len()
            .saturating_sub(TOMBSTONE_HISTORY_LINES.max(SNAPSHOT_HISTORY_LINES));
        let hit = (start..self.history.len())
            .filter_map(|i| self.history.get(i))
            .any(|line| re.is_match(line));
        if hit {
            // best-effort: the waiter may already have timed out.
            let _ = reply.send(self.seq);
            return;
        }
        self.matchers.push((re, reply));
    }

    /// Test the lines this chunk completed against every live matcher, and drop
    /// matchers whose waiter has gone away.
    fn check_matchers(&mut self, pushed: u64) {
        if self.matchers.is_empty() {
            return;
        }
        let fresh: Vec<String> = if pushed == 0 {
            Vec::new()
        } else {
            let len = self.history.len();
            let start = len.saturating_sub(pushed as usize);
            (start..len)
                .filter_map(|i| self.history.get(i))
                .map(str::to_string)
                .collect()
        };
        let seq = self.seq;
        let mut kept = Vec::with_capacity(self.matchers.len());
        for (re, reply) in std::mem::take(&mut self.matchers) {
            if reply.is_closed() {
                continue; // the waiter timed out — this is the leak guard
            }
            if fresh.iter().any(|line| re.is_match(line)) {
                let _ = reply.send(seq); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            } else {
                kept.push((re, reply));
            }
        }
        self.matchers = kept;
    }

    /// Handle a `sessions.record` request (start/stop/status). Owned by the
    /// actor so recording is unaffected by client attach/detach.
    fn on_record(&mut self, spec: RecordSpec) -> Result<RecordStatus, ControlError> {
        match spec {
            RecordSpec::Start => {
                if self.recorder.is_some() {
                    return Ok(self.record_status()); // already recording — idempotent
                }
                let (rows, cols) = self.emulator.size();
                match super::record::Recorder::start(&self.meta.id, cols, rows, &self.cfg) {
                    Ok(rec) => {
                        let path = rec.path().to_string_lossy().into_owned();
                        if let Ok(mut live) = self.live.lock() {
                            live.recording = Some(path);
                        }
                        self.record_last_path = Some(rec.path().to_path_buf());
                        self.record_capped = false;
                        self.record_truncated = None;
                        self.recorder = Some(rec);
                        // Refresh listings + any attached UI recording chip.
                        let _ = self.events.send(Arc::new(EventFrame::Sessions)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                        Ok(self.record_status())
                    }
                    Err(e) => Err(ControlError::Internal(e.into())),
                }
            }
            RecordSpec::Stop => {
                self.finalize_recording();
                Ok(self.record_status())
            }
            RecordSpec::Status => Ok(self.record_status()),
        }
    }

    /// Stop and finalize the active recording, if any — flushing the file and
    /// clearing the live "recording" flag. Called on stop, size-cap, and exit.
    fn finalize_recording(&mut self) {
        if let Some(rec) = self.recorder.take() {
            self.record_capped = rec.capped();
            let fin = rec.finish();
            // A recording that could not be flushed is truncated; say so on the
            // status reply (and in the log) instead of reporting it as saved.
            if let Some(reason) = &fin.truncated {
                tracing::warn!(
                    target: "thegn::daemon",
                    session = %self.meta.id,
                    path = %fin.path.display(),
                    reason = %reason,
                    "recording could not be finalized — the .cast file is truncated"
                );
            }
            self.record_truncated = fin.truncated;
            self.record_last_path = Some(fin.path);
            if let Ok(mut live) = self.live.lock() {
                live.recording = None;
            }
            let _ = self.events.send(Arc::new(EventFrame::Sessions)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        }
    }

    /// The current recording state for a `sessions.record` reply — status and
    /// path only, never the recorded contents.
    fn record_status(&self) -> RecordStatus {
        RecordStatus {
            recording: self.recorder.is_some(),
            path: self
                .record_last_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            bytes: self
                .recorder
                .as_ref()
                .map(|r| r.bytes_written())
                .unwrap_or(0),
            capped: self.record_capped,
            truncated: self.record_truncated.clone(),
        }
    }

    /// Capture everything worth keeping about a session that is about to end.
    fn build_tombstone(&self, exit_code: Option<i32>) -> Tombstone {
        let len = self.history.len();
        let start = len.saturating_sub(TOMBSTONE_HISTORY_LINES);
        let (rows, cols, attached) = self
            .live
            .lock()
            .map(|l| (l.rows, l.cols, l.attached))
            .unwrap_or((24, 80, 0));
        Tombstone {
            meta: self.meta.clone(),
            exit_code,
            exited_at_ms: now_ms(),
            final_screen: self.snapshot_frame_with(TOMBSTONE_HISTORY_LINES),
            history_tail: (start..len)
                .filter_map(|i| self.history.get(i))
                .map(str::to_string)
                .collect(),
            last_state: self.state(),
            // Who was watching at death — the retry observer's scope gate.
            attached,
            rows,
            cols,
            recording: self.record_last_path.clone(),
        }
    }
}

/// Sleep until `at`, or forever when there is nothing to wait for.
///
/// `pending()` is what makes "a settled session arms no timer" literal: the
/// `select!` arm is simply never ready, so it costs one parked future and no
/// wakeups at all.
async fn sleep_until_opt(at: Option<tokio::time::Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

fn unix_now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn now_ms() -> i64 {
    (unix_now_secs() * 1000.0) as i64
}

/// Whether a session's launched program can be an agent, by the same rules the
/// sidebar applies: a real (non-placeholder) name, or one of the recognized
/// `[activity] agent_programs` / configured `[[agents]]` commands.
fn is_agent_program(program: &str, cfg: &Config) -> bool {
    if program.is_empty() || cfg.tool_command(program).is_some() {
        return false;
    }
    cfg.activity.is_agent_program(program)
        || cfg
            .agents
            .iter()
            .any(|a| crate::pane::agent_program_name(&a.command, &a.name) == program)
        || (thegn_core::activity::is_real_agent(program)
            && !crate::pane::is_interactive_shell(program))
}

pub(crate) fn state_str(s: PaneAgentState) -> &'static str {
    match s {
        PaneAgentState::Blocked => "blocked",
        PaneAgentState::Working => "working",
        PaneAgentState::Done => "done",
        PaneAgentState::Idle => "idle",
    }
}

fn activity_str(k: thegn_core::attention::ActivityKind) -> &'static str {
    use thegn_core::attention::ActivityKind as A;
    match k {
        A::None => "none",
        A::Active => "active",
        A::Loading => "loading",
        A::Waiting => "waiting",
        A::Read => "read",
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
/// (`thegn_core::term_snapshot`). The daemon's emulator is never scrolled,
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
        // The emulator trait doesn't expose cursor visibility (a rare, transient
        // state); default to visible — the client's own emulator corrects it
        // from the very next live delta if the app had hidden it.
        cursor_visible: true,
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
    use thegn_core::control_wire::EventFrame;

    fn meta_running(id: &str, program: &str) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            worktree: Some("/wt/a".into()),
            program: program.into(),
            cwd: None,
            created_at_ms: 0,
            pid: None,
        }
    }

    struct Harness {
        msg_tx: mpsc::Sender<SessionMsg>,
        live: Arc<Mutex<LiveMeta>>,
        idle_rx: mpsc::UnboundedReceiver<IdleTransition>,
        tombs: Arc<tokio::sync::Mutex<Graveyard<Tombstone>>>,
        /// A receiver subscribed BEFORE the actor is spawned. `open_pty` starts
        /// the child immediately, so its first output — an `OSC 9`, or a fast
        /// `exit` — is already queued when `actor.run` is spawned: a test that
        /// subscribes on the line after `spawn_actor*` returns can be
        /// descheduled just long enough for the actor to publish the very frame
        /// it is about to wait for, and `broadcast` delivers nothing sent before
        /// a subscribe. Use this instead of `events.subscribe()` for any frame a
        /// session can emit on its own.
        feed: broadcast::Receiver<Arc<EventFrame>>,
        db: super::super::service::SharedDb,
    }

    /// Spawn a real PTY session actor running `script` under `/bin/sh -c`.
    fn spawn_actor(script: &str, sub_cap: Option<usize>) -> Harness {
        spawn_actor_as(script, sub_cap, "sh")
    }

    /// As [`spawn_actor`], with an explicit launched program — `"claude"` makes
    /// the session agent-bearing, which is what the activity projection gates on.
    fn spawn_actor_as(script: &str, sub_cap: Option<usize>, program: &str) -> Harness {
        spawn_actor_cfg(script, sub_cap, program, Config::default())
    }

    /// As [`spawn_actor_as`], with an explicit config — the OSC attention path
    /// branches on `[notifications] agent_attention_inbox`.
    fn spawn_actor_cfg(
        script: &str,
        sub_cap: Option<usize>,
        program: &str,
        cfg: Config,
    ) -> Harness {
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
            None, // no grid in the daemon — no off-thread feed sink
        )
        .expect("open pty");
        let (events, _keep) = broadcast::channel(64);
        std::mem::forget(_keep); // keep the feed open for the actor's lifetime
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        let tombs = Arc::new(tokio::sync::Mutex::new(Graveyard::new(
            super::super::tombstone::MAX_TOMBSTONES,
            super::super::tombstone::TOMBSTONE_TTL_MS,
        )));
        let db = Arc::new(Mutex::new(
            thegn_core::db::Db::open_memory().expect("in-memory db"),
        ));
        let sessions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let live = Arc::new(Mutex::new(LiveMeta {
            rows: 24,
            cols: 80,
            attached: 0,
            ..Default::default()
        }));
        let mut actor = SessionActor::new(
            meta_running("s1", program),
            live.clone(),
            pty,
            24,
            80,
            events.clone(),
            idle_tx,
            sessions,
            tombs.clone(),
            db.clone(),
            Arc::new(cfg),
        );
        if let Some(cap) = sub_cap {
            actor.set_sub_cap(cap);
        }
        let (msg_tx, msg_rx) = mpsc::channel(16);
        // Subscribe BEFORE the actor can publish anything — see `Harness::feed`.
        let feed = events.subscribe();
        tokio::spawn(actor.run(pane_rx, msg_rx));
        Harness {
            msg_tx,
            live,
            idle_rx,
            tombs,
            feed,
            db,
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
                history: true,
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

    /// Observer attaches/detaches must not signal idle/busy transitions: an
    /// `Observer` never holds the relay lease open, and observer churn on a
    /// detached session must not refresh its relay grace. The last INTERACTIVE
    /// subscriber leaving signals idle even with observers still attached.
    #[tokio::test(flavor = "multi_thread")]
    async fn observers_do_not_drive_idle_transitions() {
        let mut h = spawn_actor("cat", None);
        let _obs = attach(&h, "obs", AttachKind::Observer, 24, 80).await;
        // The attach reply already round-tripped the actor, so any transition
        // it were going to send would be queued by now.
        assert!(
            h.idle_rx.try_recv().is_err(),
            "observer attach must not signal a transition"
        );
        let _int = attach(&h, "int", AttachKind::Interactive, 24, 80).await;
        assert_eq!(
            h.idle_rx.recv().await,
            Some(IdleTransition {
                session: "s1".into(),
                idle: false
            }),
            "first interactive in signals busy"
        );
        h.msg_tx
            .send(SessionMsg::Detach {
                client_id: "int".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            h.idle_rx.recv().await,
            Some(IdleTransition {
                session: "s1".into(),
                idle: true
            }),
            "last interactive out signals idle even with an observer attached"
        );
        // The remaining observer leaving must not re-signal idle (that would
        // release-and-replace — i.e. refresh — the relay lease).
        h.msg_tx
            .send(SessionMsg::Detach {
                client_id: "obs".into(),
            })
            .await
            .unwrap();
        let _snap = attach(&h, "obs2", AttachKind::Observer, 24, 80).await; // round-trip the actor
        assert!(
            h.idle_rx.try_recv().is_err(),
            "observer churn while idle must not signal transitions"
        );
    }

    /// The `include_history` seam: a no-history snapshot omits the scrollback
    /// tail entirely while the with-history snapshot carries it — the
    /// resync/reconnect duplication fix.
    #[test]
    fn no_history_snapshot_omits_the_scrollback_tail() {
        let mut emu = AlacrittyEmulator::new(4, 20, 100);
        emu.advance(b"screen\r\n");
        let mut history = HistoryBuffer::new(100);
        let mut partial = Vec::new();
        let mut stripper = AnsiStripper::default();
        feed_bytes_to_history(
            b"old-line\nnewer-line\n",
            &mut history,
            &mut partial,
            &mut stripper,
        );
        let with = snapshot_of(&emu, &history, 2_000, 1);
        assert!(
            with.history_tail.contains("old-line"),
            "full snapshot carries the tail: {:?}",
            with.history_tail
        );
        let without = snapshot_of(&emu, &history, 0, 1);
        assert!(
            without.history_tail.is_empty(),
            "no-history snapshot must omit the tail: {:?}",
            without.history_tail
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
                    let _ = h.msg_tx.send(SessionMsg::Stdin(b"x\n".to_vec())).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                }
                Ok(None) => break,
                Err(_) => {
                    let _ = h.msg_tx.send(SessionMsg::Stdin(b"y\n".to_vec())).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
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

    // ---- supervision: the primitives a fleet drives ------------------------
    //
    // Everything below pins a contract a supervisor depends on being true.
    // They use `spawn_actor_as(.., "claude")` because the activity projection
    // is agent-gated: a plain `sh` session is `Idle` forever by design.

    /// Ask the actor what it is doing right now.
    async fn probe(h: &Harness) -> ProbeReply {
        let (tx, rx) = oneshot::channel();
        h.msg_tx
            .send(SessionMsg::Probe { reply: tx })
            .await
            .expect("actor alive");
        rx.await.expect("probe reply")
    }

    /// Register an output matcher, returning its reply channel unawaited.
    async fn watch(h: &Harness, pattern: &str) -> oneshot::Receiver<u64> {
        let (tx, rx) = oneshot::channel();
        h.msg_tx
            .send(SessionMsg::WatchOutput {
                re: Box::new(regex::Regex::new(pattern).expect("valid pattern")),
                reply: tx,
            })
            .await
            .expect("actor alive");
        rx
    }

    /// Read `Activity` frames off the feed until one reports `state`, or time
    /// out. Returns that frame's `message`.
    async fn await_state(
        rx: &mut broadcast::Receiver<Arc<EventFrame>>,
        state: &str,
    ) -> Option<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let Ok(Ok(frame)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
            else {
                continue;
            };
            if let EventFrame::Activity { json } = frame.as_ref() {
                let ev: SessionActivityEvent =
                    serde_json::from_str(json).expect("activity event decodes");
                if ev.state == state {
                    return Some(ev.message.unwrap_or_default());
                }
            }
        }
        None
    }

    /// The ordering that closes the lost-exit-code race by construction: the
    /// tombstone is inserted BEFORE `SessionExit` reaches the feed, so an
    /// observer woken by the exit can never look between the two maps and find
    /// the session in neither.
    #[tokio::test(flavor = "multi_thread")]
    async fn tombstone_is_buried_before_the_exit_event() {
        let mut h = spawn_actor_as("echo last-words; exit 3", None, "claude");
        let feed = &mut h.feed;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_exit = false;
        while std::time::Instant::now() < deadline && !saw_exit {
            if let Ok(Ok(frame)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), feed.recv()).await
                && let EventFrame::SessionExit { session, code } = frame.as_ref()
            {
                assert_eq!(session, "s1");
                assert_eq!(*code, Some(3), "the child's exit code reaches the feed");
                saw_exit = true;
            }
        }
        assert!(saw_exit, "the session must announce its exit");

        // No sleep: if burial were not ordered before the announcement, this
        // is exactly where the race would show up.
        let tombs = h.tombs.lock().await;
        let tomb = tombs
            .get("s1", now_ms())
            .expect("the corpse is readable the instant the exit is observable");
        assert_eq!(tomb.exit_code, Some(3));
        assert_eq!(tomb.meta.id, "s1");
        assert_eq!((tomb.rows, tomb.cols), (24, 80), "geometry survives death");
        assert!(
            tomb.history_tail.iter().any(|l| l.contains("last-words")),
            "the agent's last words are retained: {:?}",
            tomb.history_tail
        );
    }

    /// A harness banner raises the live error bit even when the activity state
    /// is unchanged, and the next ordinary output clears it again.
    #[tokio::test(flavor = "multi_thread")]
    async fn error_state_lifecycle() {
        let mut harness = spawn_actor_as(
            "printf 'Weekly limit reached\\n'; sleep 0.2; printf 'normal output\\n'; sleep 0.2; cat",
            None,
            "claude",
        );
        let feed = &mut harness.feed;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut raised = false;
        let mut cleared = false;
        while std::time::Instant::now() < deadline {
            let Ok(Ok(frame)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), feed.recv()).await
            else {
                continue;
            };
            let EventFrame::Activity { json } = frame.as_ref() else {
                continue;
            };
            let event: SessionActivityEvent =
                serde_json::from_str(&json).expect("activity event decodes");
            if event.error_active {
                raised = true;
            } else if raised {
                cleared = true;
                break;
            }
        }
        assert!(raised, "the harness banner must raise error_active");
        assert!(cleared, "normal output must clear error_active");
    }

    /// `wait --until idle` must not be answered instantly by an agent that has
    /// not started working yet — that would make "wait for it to finish"
    /// return before it began. The guard is `ever_busy`, which the service's
    /// `satisfied()` requires for the `Idle` condition.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_freshly_spawned_agent_is_not_yet_ever_busy() {
        let h = spawn_actor_as("sleep 30", None, "claude");
        let fresh = probe(&h).await;
        assert!(
            !fresh.ever_busy,
            "a session that has produced no output has never been busy"
        );

        // Produce output; the session becomes busy and stays flagged.
        let h2 = spawn_actor_as("echo working; sleep 30", None, "claude");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ever = false;
        while std::time::Instant::now() < deadline && !ever {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            ever = probe(&h2).await.ever_busy;
        }
        assert!(ever, "output marks the agent as having worked");
    }

    /// A supervisor that spawns an agent, goes away, and only later asks to
    /// wait for a line must not deadlock on a line that already scrolled by.
    #[tokio::test(flavor = "multi_thread")]
    async fn output_match_fires_on_already_retained_scrollback() {
        let h = spawn_actor_as("echo READY-TOKEN; sleep 30", None, "claude");

        // Let the marker land and scroll into history before anyone watches.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut hit = None;
        while std::time::Instant::now() < deadline && hit.is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let rx = watch(&h, "READY-TOKEN").await;
            hit = tokio::time::timeout(std::time::Duration::from_millis(200), rx)
                .await
                .ok()
                .and_then(Result::ok);
        }
        assert!(
            hit.is_some(),
            "a pattern already in the scrollback resolves immediately"
        );
    }

    /// A matcher that will never be satisfied must resolve when the session
    /// dies rather than hanging until the caller's timeout — the actor drops
    /// the senders during teardown.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pending_matcher_resolves_when_the_session_dies() {
        let h = spawn_actor_as("sleep 0.2; exit 0", None, "claude");
        let rx = watch(&h, "NEVER-APPEARS").await;
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("the matcher must not outlive the session");
        assert!(
            outcome.is_err(),
            "the sender is dropped, so the waiter sees a closed channel, not a match"
        );
    }

    /// `OSC 9` is the agent saying "I need you" — the one signal that
    /// distinguishes *blocked on a human* from *finished*, which are
    /// indistinguishable from CPU and output alone. Answering it clears it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_osc_attention_signal_blocks_and_input_clears_it() {
        let mut h = spawn_actor_as(r"printf '\033]9;pick a branch\007'; cat", None, "claude");

        let message = await_state(&mut h.feed, "blocked")
            .await
            .expect("OSC 9 must raise a blocked state");
        assert!(
            message.contains("pick a branch"),
            "the agent's question rides along: {message:?}"
        );
        assert_eq!(probe(&h).await.state, PaneAgentState::Blocked);

        // The human answers.
        h.msg_tx
            .send(SessionMsg::Stdin(b"main\n".to_vec()))
            .await
            .expect("actor alive");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut cleared = false;
        while std::time::Instant::now() < deadline && !cleared {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cleared = probe(&h).await.state != PaneAgentState::Blocked;
        }
        assert!(cleared, "stdin answers the raised hand");
    }

    /// Count this harness's `session_attention` rows and unread inbox rows.
    fn attention_rows(h: &Harness) -> (usize, usize) {
        use thegn_core::store::NotificationStore;
        let db = h.db.lock().expect("db lock");
        (
            db.list_session_attention().unwrap_or_default().len(),
            db.get_unread_notifications().unwrap_or_default().len(),
        )
    }

    /// Poll until `f` holds, or give up — the DB writes ride `spawn_blocking`,
    /// so they land shortly after the feed state does.
    async fn await_rows(h: &Harness, f: impl Fn((usize, usize)) -> bool) -> (usize, usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let rows = attention_rows(h);
            if f(rows) || std::time::Instant::now() >= deadline {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// THE-68: a raised hand is live state, not an inbox event. By default the
    /// OSC path writes ZERO notification rows — Claude Code emits one at the end
    /// of every turn, and those rows buried the inbox while never clearing.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_osc_signal_writes_state_not_an_inbox_row() {
        let mut h = spawn_actor_as(r"printf '\033]9;pick a branch\007'; cat", None, "claude");
        await_state(&mut h.feed, "blocked")
            .await
            .expect("OSC 9 must raise a blocked state");

        let (hands, unread) = await_rows(&h, |(hands, _)| hands == 1).await;
        assert_eq!(hands, 1, "the hand is one row of live state");
        assert_eq!(unread, 0, "and NOT an inbox row, by default");

        // The human answers: the hand goes down in the shared table too, or the
        // worktree stays Blocked forever.
        h.msg_tx
            .send(SessionMsg::Stdin(b"main\n".to_vec()))
            .await
            .expect("actor alive");
        let (hands, unread) = await_rows(&h, |(hands, _)| hands == 0).await;
        assert_eq!(
            (hands, unread),
            (0, 0),
            "stdin lowers the hand, inbox quiet"
        );
    }

    /// With the opt-in audit trail on, repeated signals from one session leave
    /// ONE current row — delete-then-insert, not one row per agent turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_opt_in_inbox_row_is_one_per_session_not_one_per_turn() {
        let mut cfg = Config::default();
        cfg.notifications.agent_attention_inbox = true;
        let h = spawn_actor_cfg(
            r"printf '\033]9;first\007'; sleep 1; printf '\033]9;second\007'; cat",
            None,
            "claude",
            cfg,
        );

        // Wait for the SECOND turn's row, so the first one has had every chance
        // to still be sitting there.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut arrived = false;
        while std::time::Instant::now() < deadline && !arrived {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            use thegn_core::store::NotificationStore;
            arrived =
                h.db.lock()
                    .expect("db lock")
                    .get_unread_notifications()
                    .unwrap_or_default()
                    .iter()
                    .any(|n| n.message == "second");
        }
        assert!(arrived, "the second turn's audit row must land");

        let (hands, unread) = attention_rows(&h);
        assert_eq!(hands, 1, "still exactly one live hand");
        assert_eq!(
            unread, 1,
            "and exactly one unread audit row, not one per turn"
        );
        // TOTAL rows, not just unread: the inbox lists read rows too, so the
        // previous turn's row must be DELETED rather than marked read — else
        // the list still grows one entry per turn, only greyed.
        use thegn_core::store::NotificationStore;
        let total =
            h.db.lock()
                .expect("db lock")
                .get_all_notifications(usize::MAX)
                .unwrap_or_default()
                .len();
        assert_eq!(total, 1, "the superseded row is retired, not just read");
    }

    /// Poll until an audit row carrying `body` exists (read or unread), or give
    /// up. Returns whether it arrived.
    async fn await_audit_row(h: &Harness, body: &str, secs: u64) -> bool {
        use thegn_core::store::NotificationStore;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let hit =
                h.db.lock()
                    .expect("db lock")
                    .get_all_notifications(usize::MAX)
                    .unwrap_or_default()
                    .iter()
                    .any(|n| n.message == body);
            if hit {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// The retire sweep must cover READ rows too. The inbox lists read rows, so
    /// a superseded row the user had already retired by hand (`x`, or a "clear
    /// all") would otherwise survive — and the opt-in trail would grow one row
    /// per turn again, exactly the pile THE-68 is about, one clear later.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_audit_row_is_still_retired_by_the_next_turn() {
        let mut cfg = Config::default();
        cfg.notifications.agent_attention_inbox = true;
        let h = spawn_actor_cfg(
            r"printf '\033]9;first\007'; sleep 3; printf '\033]9;second\007'; cat",
            None,
            "claude",
            cfg,
        );

        assert!(
            await_audit_row(&h, "first", 10).await,
            "the first turn's audit row must land"
        );
        // The user reads it — the state the old unread-only sweep could not see.
        {
            use thegn_core::store::NotificationStore;
            let db = h.db.lock().expect("db lock");
            assert!(
                !db.get_all_notifications(usize::MAX)
                    .unwrap_or_default()
                    .iter()
                    .any(|n| n.message == "second"),
                "the second turn must not have landed yet, or this proves nothing",
            );
            db.mark_all_notifications_read().expect("mark read");
        }

        assert!(
            await_audit_row(&h, "second", 15).await,
            "the second turn's audit row must land"
        );
        use thegn_core::store::NotificationStore;
        let rows =
            h.db.lock()
                .expect("db lock")
                .get_all_notifications(usize::MAX)
                .unwrap_or_default();
        assert_eq!(rows.len(), 1, "the read row is retired too: {rows:?}");
    }
}
