//! The budgeted PTY drain — receive, stash, and parse pane output on the event
//! loop under the pure [`crate::loop_policy`] byte/deadline budget. Extracted
//! from the ratchet-pinned `run.rs` (the old inline drain was a chunk-count
//! loop: up to 64 × 8KB of unbounded vt100 parsing per iteration, with input
//! discovered mid-drain waiting out the entire backlog).
//!
//! Shape per iteration:
//! 1. **Receive** (no parsing): `try_recv` every queued [`PaneEvent`] into the
//!    [`PtyBacklog`] stash, stopping at [`crate::loop_policy::BACKLOG_HIGH_WATER`] so
//!    the bounded channel fills and reader threads block — end-to-end
//!    backpressure to the child, exactly what a plain terminal does to
//!    `cat bigfile`. Bytes are NEVER dropped: a dropped chunk can split an
//!    escape sequence (corrupting emulator state) and silently lose
//!    scrollback; backpressure is the correct throttle.
//! 2. **Exits**: a pane's stashed output parses before its Exit is honored, so
//!    final output lands in scrollback before the pane leaves the table.
//! 3. **Parse**: round-robin across panes with backlog, coalescing each
//!    pane's queued chunks into one buffer per [`crate::loop_policy::pane_slice`] —
//!    one emulator feed + one query scan + one OSC pass per merged buffer
//!    instead of per 8KB chunk. Slices are capped (`loop_policy::MAX_SLICE`)
//!    so the deadline check between them has real granularity; splitting a
//!    chunk at the cap is safe because the emulator parses incrementally
//!    (the boundary is no different from the PTY read's own chunking).
//! 4. **Input preemption**: between pane slices a zero-timeout `poll_input`
//!    checks for user input; a Key/Mouse/Paste stamps `input_at`, queues the
//!    event, and aborts the drain — worst-case added input latency is one
//!    pane slice of parsing, not the whole backlog.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use termwiz::terminal::Terminal;
use termwiz::terminal::buffered::BufferedTerminal;
use thegn_core::store::NotificationStore;
use tokio::sync::mpsc as tokio_mpsc;

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::pane::PaneEvent;
use crate::panes::Panes;
use crate::pins::pin_cwd;
use crate::run::{
    DrawerPool, SidebarState, active_cwd, persist_pin_state, prospective_corner_rect,
    update_crash_count,
};

/// A pane that exits within this of being spawned is a "fast crash" —
/// bwrap/sandbox failures write their error to the PTY before dying, so
/// output-based detection would mis-classify them as normal exits.
pub(crate) const CRASH_THRESHOLD: Duration = Duration::from_secs(2);

/// On a fast failed exit of an ssh-reached provider's pane (`machine0`/`fly`/vps),
/// mark that provider unhealthy in the connect-health registry so
/// [`crate::agent::env_halt_reason`] raises the failover-off "cannot connect to the
/// remote" halt on the immediate respawn. Cheap resolution mirroring
/// `env_halt_reason` (DB effective-env; no network); a no-op for local/host panes
/// and for the WSS-native providers (whose in-process relay reports its own health).
fn report_pane_connect_failure(cfg: &thegn_core::config::Config, wt: &str) {
    use std::path::{Path, PathBuf};
    use thegn_core::store::WorkspaceStore;
    if wt.is_empty() {
        return;
    }
    let loc = thegn_core::remote::GitLoc::for_worktree(Path::new(wt));
    // One DB handle for both reads (each `Db::open` re-runs pragmas). This path
    // is order-dependent — the health mark below must land before the respawn's
    // `env_halt_reason` check — so it stays synchronous, but need not open twice.
    let db = thegn_core::db::Db::open().ok();
    let repo_root: PathBuf = db
        .as_ref()
        .and_then(|db| db.repo_root_for(wt).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(Path::new(wt)))
        .unwrap_or_else(|| PathBuf::from(wt));
    let selected = db
        .as_ref()
        .and_then(|db| db.effective_env(wt, &repo_root.to_string_lossy()));
    let env = cfg.resolve_env(&repo_root, &loc, Path::new(wt), selected.as_deref());
    if env.placement.is_local() {
        return;
    }
    let Some(provider) = cfg.env.get(&env.name).map(|e| e.provider.provider.trim()) else {
        return;
    };
    if thegn_core::config::ssh_reached_provider_kind(provider) {
        tracing::warn!(
            target: "thegn::sandbox", %provider, worktree = %wt,
            "remote pane connect failure; marking provider unhealthy"
        );
        crate::agent::native_exec_report(provider, false);
    }
}

/// Raw, unparsed PTY chunks awaiting their parse slice, per pane, FIFO within
/// a pane (cross-pane order is unspecified — same as the shared channel
/// today). Loop-persistent: leftovers carry to the next iteration.
#[derive(Default)]
pub(crate) struct PtyBacklog {
    per_pane: HashMap<u32, VecDeque<Vec<u8>>>,
    /// Round-robin cursor order over panes with backlog.
    rr: VecDeque<u32>,
    /// Total stashed bytes (the high-water gauge).
    total: usize,
}

impl PtyBacklog {
    pub(crate) fn is_empty(&self) -> bool {
        self.total == 0
    }

    fn push(&mut self, id: u32, chunk: Vec<u8>) {
        self.total += chunk.len();
        let q = self.per_pane.entry(id).or_default();
        if q.is_empty() && !self.rr.contains(&id) {
            self.rr.push_back(id);
        }
        q.push_back(chunk);
    }

    /// Coalesce this pane's queued chunks into one buffer of at most `max`
    /// bytes, splitting the final chunk at the cap when needed (the remainder
    /// goes back to the queue front). Splitting is safe: the emulator is an
    /// incremental parser, so an arbitrary byte boundary here is semantically
    /// identical to the arbitrary chunking the PTY read already imposes — and
    /// it's what gives the drain deadline its granularity (an unsplittable
    /// 64KB chunk would pin one slice at ~16ms of feed).
    fn take_slice(&mut self, id: u32, max: usize) -> Vec<u8> {
        let Some(q) = self.per_pane.get_mut(&id) else {
            return Vec::new();
        };
        let max = max.max(1);
        let mut out = Vec::new();
        while out.len() < max {
            let Some(mut chunk) = q.pop_front() else {
                break;
            };
            let room = max - out.len();
            if chunk.len() > room {
                let rest = chunk.split_off(room);
                q.push_front(rest);
            }
            self.total -= chunk.len();
            if out.is_empty() {
                out = chunk;
            } else {
                out.extend_from_slice(&chunk);
            }
        }
        if q.is_empty() {
            self.per_pane.remove(&id);
        }
        out
    }

    /// Everything stashed for `id` (the pre-Exit flush). Removes the pane.
    fn drain_pane(&mut self, id: u32) -> Vec<u8> {
        let Some(q) = self.per_pane.remove(&id) else {
            return Vec::new();
        };
        self.rr.retain(|&p| p != id);
        let mut out = Vec::new();
        for chunk in q {
            self.total -= chunk.len();
            out.extend_from_slice(&chunk);
        }
        out
    }

    /// The next pane in round-robin order that still has backlog; rotates so
    /// a flooding pane yields to its siblings between slices.
    fn next_pane(&mut self) -> Option<u32> {
        while let Some(id) = self.rr.pop_front() {
            if self.per_pane.get(&id).is_some_and(|q| !q.is_empty()) {
                self.rr.push_back(id);
                return Some(id);
            }
        }
        None
    }

    fn panes_with_backlog(&self) -> usize {
        self.per_pane.len()
    }
}

/// What one drain pass did — feeds the perf counters and the loop's
/// re-wake/return decisions.
#[derive(Default)]
pub(crate) struct DrainSummary {
    pub chunks: u64,
    pub bytes: u64,
    /// Backlog remains (budget/deadline hit or preempted): the loop arms the
    /// short poll timeout to continue promptly.
    pub budget_exhausted: bool,
    /// The PTY channel closed — the loop tears down.
    pub disconnected: bool,
    /// Input was discovered mid-drain and the drain aborted for it.
    pub preempted: bool,
    /// Panes whose child exited this pass (the onboarding wizard watches for
    /// its spawned `gh auth login` / agent-setup tab closing).
    pub exited: Vec<u32>,
    /// An active tab's sole pane died and its dead leaf was left in the tree
    /// for the off-thread materialize pipeline. The loop's lazy-materialize
    /// block runs BEFORE the drain each turn, so the caller pulses the waker
    /// on this to kick the respawn on the next (immediate) turn instead of
    /// waiting for an incidental wake. Set only in that branch — never for
    /// pins/drawer/corner/terminal exits, non-sole removals, background tabs,
    /// or the keeps-crashing give-up.
    pub left_for_materialize: bool,
}

/// Everything the moved Output/Exit handlers touch, borrowed from the loop.
pub(crate) struct DrainCtx<'a> {
    pub session: &'a mut crate::session::Session,
    pub panes: &'a mut Panes,
    pub model: &'a mut FrameModel,
    pub sb: &'a mut SidebarState,
    pub focus: &'a mut crate::focus::FocusState,
    pub keymap_config: &'a thegn_core::config::Config,
    pub current_config: &'a thegn_core::config::Config,
    pub chrome_center: Rect,
    pub cols: usize,
    pub rows: usize,
    /// Visible pane ids (active tab's center + the corner overlay): only their
    /// output dirties the frame; everything else parses without a repaint.
    pub visible: &'a HashSet<u32>,
    pub dirty_panes: &'a mut HashSet<u32>,
    pub dirty: &'a mut bool,
    pub need_relayout: &'a mut bool,
    pub drawer: &'a mut Option<u32>,
    pub drawer_pool: &'a mut DrawerPool,
    pub drawer_home: &'a mut Option<std::path::PathBuf>,
    pub corner: &'a mut Option<u32>,
    pub corner_name: &'a mut Option<String>,
    pub corner_kitty: bool,
    pub corner_relay: &'a mut crate::kitty_relay::KittyRelay,
    pub corner_gfx: &'a mut Vec<Vec<u8>>,
    pub corner_occluded: &'a mut bool,
    pub supervisor: &'a mut crate::pins::PinSupervisor,
    pub loading_state: &'a mut crate::loading::track::LoadingTracker,
    pub loading_remote: &'a mut HashMap<(String, usize), bool>,
    pub loading_retired: &'a mut HashSet<(String, usize)>,
    pub respawn_crash_count: &'a mut HashMap<(usize, usize), u32>,
    pub center_dormant: &'a mut bool,
    /// The loop's shutdown flag. A terminal shell exiting while this is set is
    /// teardown, not a user close — its `terminals` registry row must be kept so
    /// the terminal survives to the next launch (see [`close_exited_terminal`]).
    pub shutdown: &'a std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub event_bus: &'a thegn_core::event_bus::EventBus,
    pub notify_state: &'a std::sync::Arc<crate::notify::NotifyState>,
    /// All stdout bytes route through the writer thread — a direct write here
    /// could interleave with an in-flight frame.
    pub writer: &'a crate::frame_writer::FrameWriter,
}

/// One budgeted drain pass. See the module docs for the shape.
pub(crate) fn drain<T: Terminal>(
    ctx: &mut DrainCtx<'_>,
    buf: &mut BufferedTerminal<T>,
    rx: &mut tokio_mpsc::Receiver<PaneEvent>,
    backlog: &mut PtyBacklog,
    pending_input: &mut VecDeque<termwiz::input::InputEvent>,
    input_at: &mut Option<Instant>,
) -> DrainSummary {
    let budget = crate::loop_policy::drain_budget(input_at.is_some() || !pending_input.is_empty());
    let t0 = Instant::now();
    let mut summary = DrainSummary::default();

    // 1. Receive — stash raw chunks, no parsing. Stop at the high-water so the
    // bounded channel backpressures the reader threads (and the child).
    let mut exits: Vec<(u32, Option<i32>)> = Vec::new();
    let mut fallbacks: Vec<u32> = Vec::new();
    let mut reattached: Vec<u32> = Vec::new();
    while backlog.total < crate::loop_policy::BACKLOG_HIGH_WATER {
        match rx.try_recv() {
            Ok(PaneEvent::Output(id, chunk)) => {
                summary.chunks += 1;
                summary.bytes += chunk.len() as u64;
                backlog.push(id, chunk);
            }
            Ok(PaneEvent::Exit(id, code)) => exits.push((id, code)),
            Ok(PaneEvent::SessionFallback(id)) => fallbacks.push(id),
            Ok(PaneEvent::Reattached(id)) => reattached.push(id),
            Err(tokio_mpsc::error::TryRecvError::Empty) => break,
            Err(tokio_mpsc::error::TryRecvError::Disconnected) => {
                summary.disconnected = true;
                break;
            }
        }
    }

    // A reattach replays server-side scrollback, which arrives as ordinary
    // output. Mark it solicited BEFORE the chunks below are fed, so the burst
    // can't register as unsolicited agent work and drag the worktree's activity
    // dot busy (or clear a genuine needs-you dot). The pane itself is old, so
    // `agent_output`'s spawn grace cannot cover this case.
    for id in reattached {
        if let Some(p) = ctx.panes.table.get_mut(&id) {
            p.mark_output_solicited();
        }
    }

    // A warm reattach degraded to a fresh session: repaint the persisted
    // scrollback tail + arm the relaunch overlay (before parsing the fresh
    // session's output, so the restored history lands underneath it).
    for id in fallbacks {
        crate::handlers::daemon_lifecycle::handle_session_fallback(ctx, id);
    }

    // 2. Exits — flush the pane's stashed tail into its emulator first, so
    // its final output reaches scrollback before the pane leaves the table.
    summary.exited = exits.iter().map(|(id, _)| *id).collect();
    for (id, code) in exits {
        let tail = backlog.drain_pane(id);
        if !tail.is_empty() {
            handle_output(ctx, id, &tail);
        }
        summary.left_for_materialize |= handle_exit(ctx, id, code);
    }

    // 3+4. Parse round-robin under the byte/deadline budget, with input
    // preemption between pane slices.
    let mut spent = 0usize;
    while !backlog.is_empty() {
        if spent >= budget.max_bytes || t0.elapsed() >= budget.deadline {
            break;
        }
        let slice =
            crate::loop_policy::pane_slice(budget.max_bytes - spent, backlog.panes_with_backlog());
        let Some(id) = backlog.next_pane() else { break };
        let merged = backlog.take_slice(id, slice);
        if merged.is_empty() {
            continue;
        }
        spent += merged.len();
        handle_output(ctx, id, &merged);

        // Input preemption: a keystroke found here aborts the drain — its
        // dispatch (and the frame showing its effect) must not wait out the
        // backlog. Wake/Resized events just queue; they don't abort.
        if let Ok(Some(ev)) = buf.terminal().poll_input(Some(Duration::ZERO)) {
            use termwiz::input::InputEvent;
            let interactive = matches!(
                ev,
                InputEvent::Key(_) | InputEvent::Mouse(_) | InputEvent::Paste(_)
            );
            pending_input.push_back(ev);
            if interactive {
                *input_at = Some(Instant::now());
                summary.preempted = true;
                break;
            }
        }
    }

    summary.budget_exhausted = !backlog.is_empty();
    summary
}

/// One pane's (possibly coalesced) output buffer: feed the emulator, answer
/// terminal queries, forward OSC passthrough, route the drawer control
/// channel, and mark pane damage. Moved verbatim from the run.rs drain
/// (adapted to `ctx` borrows; per-chunk work now runs once per merged buffer).
fn handle_output(ctx: &mut DrainCtx<'_>, id: u32, b: &[u8]) {
    if let Some(p) = ctx.panes.table.get_mut(&id) {
        // First real output ⇒ this worktree's shell is live; drop its loading
        // splash (by owner, so a background worktree that finished while away
        // shows no stale splash on return). Held while provisioning is still
        // live (the premature-shell guard); only the shell-wait shape clears.
        // `any_clearable_splash` pre-gates the tab scan: the lingering empty
        // markers parked by eager/warm-spare success keep the map non-empty,
        // so gating on `!is_empty()` rescanned + re-logged per output chunk
        // forever (the log storm). See `loading::{any_clearable_splash,
        // should_clear_splash_on_output}`.
        if crate::loading::any_clearable_splash(ctx.loading_state)
            && let Some((gi, ti)) = ctx
                .session
                .iter_tabs()
                .find(|(_, _, t)| t.center.pane_ids().contains(&id))
                .map(|(gi, ti, _)| (gi, ti))
        {
            let key = (ctx.session.worktrees[gi].name.clone(), ti);
            if crate::loading::should_clear_splash_on_output(ctx.loading_state, &key) {
                tracing::debug!(
                    target: "thegn::loading",
                    worktree = %ctx.session.worktrees[gi].name,
                    "first pane output cleared the loading splash (provisioning done, shell live)"
                );
                ctx.loading_state.remove(&key);
                ctx.loading_remote.remove(&key);
                // Shell spoke ⇒ retire: no late splash re-raise.
                ctx.loading_retired.insert(key);
            }
        }
        if Some(id) == *ctx.corner && ctx.corner_kitty {
            // CRISP CORNER VIDEO: split the corner pane's stream — text feeds
            // the emulator (so its cursor tracks the child's placement), kitty
            // image escapes are pulled out, repositioned to the corner rect,
            // and queued for the outer terminal (emitted after the frame
            // flush). See `kitty_relay`.
            let origin = ctx
                .current_config
                .pins
                .iter()
                .find(|pp| Some(pp.name.as_str()) == ctx.corner_name.as_deref())
                .map(|pp| {
                    let c = crate::pins::inset1(prospective_corner_rect(pp, ctx.cols, ctx.rows));
                    (c.y as u16, c.x as u16)
                })
                .unwrap_or((0, 0));
            let mut emu_text: Vec<u8> = Vec::new();
            for piece in ctx.corner_relay.feed(b) {
                match piece {
                    crate::kitty_relay::Piece::Emulator(t) => {
                        p.feed(&t);
                        emu_text.extend_from_slice(&t);
                    }
                    crate::kitty_relay::Piece::GfxDisplay(seq) => {
                        // Cursor reflects the text fed so far (the child homes
                        // right before the image); place there + origin.
                        let cur = p.emulator().cursor();
                        let mut bytes = crate::kitty_relay::cup(origin, cur);
                        bytes.extend_from_slice(&seq);
                        ctx.corner_gfx.push(bytes);
                    }
                    crate::kitty_relay::Piece::GfxOther(seq) => {
                        ctx.corner_gfx.push(seq);
                    }
                    crate::kitty_relay::Piece::GfxAnswer(ans) => {
                        // Must-deliver kitty graphics answer; warn on a rare
                        // Full drop (see the query-reply sites below).
                        if let Err(e) = p.write_reply(&ans) {
                            tracing::warn!(
                                target: "thegn::pane",
                                "dropped a kitty graphics reply ({e}); an inner program may hang"
                            );
                        }
                    }
                }
            }
            // DA/DSR/OSC replies + OSC52 passthrough on the graphics-stripped
            // bytes only (the kitty probe, if any, was answered by the relay).
            if !emu_text.is_empty() {
                let resp = {
                    let (fg, bg) = crate::compositor::pane_colors();
                    let emu = p.emulator();
                    crate::queries::query_responses(
                        &emu_text,
                        emu.cursor(),
                        emu.size(),
                        crate::queries::PaneColors { fg, bg },
                    )
                };
                if !resp.is_empty() {
                    // A terminal-query reply (DA/DSR/kitty) is host-generated and
                    // must-deliver — an inner program (yazi, etc.) warns or times
                    // out without it. It shares the pane's bounded stdin queue, so
                    // a Full drop (child not reading its 256-chunk backlog) is
                    // rare but worth a warn so a hung inner app is diagnosable.
                    if let Err(e) = p.write_reply(&resp) {
                        tracing::warn!(
                            target: "thegn::pane",
                            "dropped a terminal-query reply ({e}); an inner program may hang"
                        );
                    }
                }
                let fwd = crate::queries::osc_passthrough(&emu_text);
                if !fwd.is_empty() {
                    ctx.writer.submit_oob(fwd);
                }
            }
            // Corner is in `visible`; mark it dirty so the render block runs
            // and flushes `corner_gfx`.
            ctx.dirty_panes.insert(id);
        } else {
            p.feed(b);
            // Answer terminal queries (DA/DSR/OSC color, kitty probes) the app
            // just sent — without a reply, programs like yazi warn or time out.
            let resp = {
                let (fg, bg) = crate::compositor::pane_colors();
                let emu = p.emulator();
                crate::queries::query_responses(
                    b,
                    emu.cursor(),
                    emu.size(),
                    crate::queries::PaneColors { fg, bg },
                )
            };
            if !resp.is_empty() {
                let _ = p.write_reply(&resp);
            }
            // Clipboard sets (OSC 52) from inner apps go VERBATIM to the outer
            // terminal — vim's "+y inside a pane reaches the system clipboard
            // like in a plain terminal.
            let fwd = crate::queries::osc_passthrough(b);
            if !fwd.is_empty() {
                ctx.writer.submit_oob(fwd);
            }
            if ctx.visible.contains(&id) {
                // Pane-content-only damage: recompose just this pane, not the
                // chrome (see render_plan).
                ctx.dirty_panes.insert(id);
            }
        }
    }
    // Private drawer→host control channel (OSC 5379): a file manager whose caps
    // declare a control channel signals close/open-in-editor here so it keeps
    // ownership of every key (no host key-stealing). Decoded through the
    // file_manager seam — a capless manager (custom) is never scanned.
    if *ctx.drawer == Some(id)
        && let Some(cmd) = thegn_core::file_manager::decode_control(ctx.current_config, b)
    {
        crate::actions::dispatch_drawer_command(
            cmd,
            ctx.session,
            ctx.panes,
            ctx.drawer,
            ctx.drawer_pool,
            ctx.drawer_home,
            ctx.focus,
            ctx.model,
            ctx.sb,
            ctx.keymap_config,
            ctx.chrome_center,
        );
        *ctx.need_relayout = true;
        *ctx.dirty = true;
    }
}

/// Pure exit classification for [`handle_exit`]: an *agent session exit* is a
/// daemon-backed pane running a non-routine, non-wrapper program
/// (`pane::is_routine_pane` — interactive shells and unnamed panes are
/// routine; `pane::is_runtime_wrapper` — bwrap/ssh/systemd-run/… — is what a
/// SANDBOXED or remote pane's spawn argv names, so without the wrapper
/// exclusion every shell exit on such a worktree would misread as an agent).
/// Plain daemon shells keep today's exit behavior exactly; only agent /
/// non-routine programs take the keep-cmd + honest-status path below.
fn is_daemon_agent_exit(daemon_backed: bool, program: &str) -> bool {
    daemon_backed
        && !crate::pane::is_routine_pane(program)
        && !crate::pane::is_runtime_wrapper(program)
}

/// A pane's PTY closed: drawer/pool/corner/pin routing, then the owning-tab
/// respawn-or-remove logic with fast-crash detection and process-exit
/// notification routing. Moved verbatim from the run.rs drain (`continue`s
/// became early returns).
///
/// Returns `true` when an active tab's sole pane died and its dead leaf was
/// left in the tree for the off-thread materialize pipeline (see
/// [`DrainSummary::left_for_materialize`]). No spawn — and no sandbox
/// resolution — happens here on the loop.
fn handle_exit(ctx: &mut DrainCtx<'_>, id: u32, exit_code: Option<i32>) -> bool {
    // Program name is needed for attention routing after the pane leaves the
    // table (item 524).
    let exited_program = ctx.panes.table.get(&id).map(|p| p.program().to_string());
    // The daemon session behind this pane, grabbed before it leaves the table:
    // a daemon-routed pane (and therefore every adopted `sessions.open` agent)
    // is a `Stream` pane whose server announces its session id. This is the
    // dispatch row's IDENTITY for the roster stamp below — with a pipeline
    // running several stages in one worktree, the path alone cannot say which
    // row just died. `None` for a plain local PTY pane, which falls back to the
    // most-recent-active row for the worktree.
    let exited_session = ctx.panes.table.get(&id).and_then(|p| p.session_id());
    // Grab the dying pane's last output BEFORE it leaves the table — a
    // sandbox/exec failure writes its error here, and a fast crash would
    // otherwise discard it (the pane just vanishes).
    let crash_tail = ctx
        .panes
        .table
        .get(&id)
        .map(|p| p.history_tail(12))
        .unwrap_or_default();
    // A longer tail for a sole worktree pane's respawn scrollback refresh
    // (mirrors the persist-time capture in `snapshot.rs`, including its
    // server-side-replay skip) — must also be grabbed before the pane leaves
    // the table. Cheap: exits are rare and the ring is bounded.
    let respawn_tail = ctx
        .panes
        .table
        .get(&id)
        .filter(|p| p.is_daemon_backed() || p.provider_session().is_none())
        .map(|p| p.history_tail(ctx.current_config.session.scrollback_lines as usize));
    // An *agent session exit* (daemon-backed, non-routine program — e.g. an
    // attached worktree agent finishing its run), classified while the pane is
    // still in the table. Consumed by the removal arms below: the sole leaf
    // keeps its remembered command even on a clean exit (Enter relaunches the
    // agent) and the status names the program + code instead of the generic
    // "restarting shell…"; a plain daemon shell keeps today's behavior.
    let daemon_agent_exit = ctx
        .panes
        .table
        .get(&id)
        .is_some_and(|p| is_daemon_agent_exit(p.is_daemon_backed(), p.program()));
    ctx.panes.table.remove(&id);
    // Set only in the sole-pane leave-for-materialize branch below.
    let mut left_for_materialize = false;
    // The visible drawer manager's process ended. Clear it, mark the worktree's
    // drawer closed, hand focus back to the center, and relayout to reclaim
    // the bottom slice.
    if *ctx.drawer == Some(id) {
        *ctx.drawer = None;
        if let Some(dir) = ctx.drawer_home.take().or_else(|| active_cwd(ctx.session)) {
            crate::drawer_state::set_flag(&dir, false);
        }
        // A clean exit is the normal `q`-quit path — stay quiet. Only an
        // abnormal exit (e.g. the contained scope hit the drawer memory
        // limit) gets a hint.
        if exit_code != Some(0) {
            ctx.model.status = "Files drawer exited unexpectedly; if image \
                previews were on it may have hit the drawer memory limit."
                .into();
        }
        if ctx.focus.drawer() {
            ctx.focus.zone = crate::focus::Zone::Center;
        }
        *ctx.need_relayout = true;
        *ctx.dirty = true;
        return false;
    }
    // A pooled (hidden) drawer manager exited; just forget it.
    if ctx.drawer_pool.remove_id(id) {
        *ctx.dirty = true;
        return false;
    }
    // The corner overlay pin died (e.g. mpv quit on `q`). It's a supervised
    // pin, so still drive `on_exit` for the chip/health + restart policy, but
    // respawn into the corner rect (not the center) and re-occupy the single
    // corner slot. A clean exit stays down unless `restart = always`.
    if *ctx.corner == Some(id) {
        *ctx.corner = None;
        let name = ctx.corner_name.take();
        // The child is gone; clear its last image off the outer terminal and
        // reset the relay state.
        if ctx.corner_kitty {
            ctx.writer
                .submit_oob(crate::kitty_relay::delete_all().to_vec());
        }
        ctx.corner_relay.reset();
        ctx.corner_gfx.clear();
        *ctx.corner_occluded = false;
        if ctx.focus.corner() {
            ctx.focus.zone = crate::focus::Zone::Center;
        }
        if let Some(name) = name {
            let respawn = matches!(
                ctx.supervisor.on_exit(id, exit_code == Some(0)),
                crate::pins::RestartDecision::Respawn
            );
            if respawn
                && let Some(pin) = ctx
                    .current_config
                    .pins
                    .iter()
                    .find(|p| p.name == name)
                    .cloned()
            {
                let active_dir = active_cwd(ctx.session);
                let content =
                    crate::pins::inset1(prospective_corner_rect(&pin, ctx.cols, ctx.rows));
                let argv = crate::pins::PinSupervisor::argv(&pin);
                let env: Vec<(String, String)> = crate::pins::PinSupervisor::spawn_env(&pin)
                    .into_iter()
                    .collect();
                let cwd = pin_cwd(&pin, active_dir);
                if let Ok(fresh) = ctx
                    .panes
                    .spawn_argv_env_local(&argv, Some(&cwd), &env, content)
                {
                    ctx.supervisor.reattach(&name, fresh);
                    *ctx.corner = Some(fresh);
                    *ctx.corner_name = Some(name);
                    // Corner panes parse on the loop (kitty relay feeds
                    // text pieces at exact cursor positions).
                    if let Some(p) = ctx.panes.table.get(&fresh) {
                        p.set_loop_fed(true);
                    }
                }
            }
        }
        persist_pin_state(ctx.supervisor, &ctx.session.id);
        *ctx.need_relayout = true;
        *ctx.dirty = true;
        return false;
    }
    // Pin panes are supervised separately from tab panes: the supervisor
    // applies the restart policy. A clean exit (code 0) is reported as such so
    // `restart = on-failure` pins stay down on a normal stop; an unknown code
    // (None) is treated as a failure.
    if let Some(inst) = ctx.supervisor.instance_of_pane(id) {
        let name = inst.name.clone();
        match ctx.supervisor.on_exit(id, exit_code == Some(0)) {
            crate::pins::RestartDecision::Respawn => {
                let active_dir = active_cwd(ctx.session);
                let pin = ctx
                    .current_config
                    .pins
                    .iter()
                    .find(|p| p.name == name)
                    .cloned();
                if let Some(pin) = pin {
                    let argv = crate::pins::PinSupervisor::argv(&pin);
                    let env: Vec<(String, String)> = crate::pins::PinSupervisor::spawn_env(&pin)
                        .into_iter()
                        .collect();
                    let cwd = pin_cwd(&pin, active_dir);
                    if let Ok(fresh) =
                        ctx.panes
                            .spawn_argv_env_local(&argv, Some(&cwd), &env, ctx.chrome_center)
                    {
                        ctx.supervisor.reattach(&name, fresh);
                    }
                }
            }
            crate::pins::RestartDecision::Leave => {}
        }
        persist_pin_state(ctx.supervisor, &ctx.session.id);
        *ctx.need_relayout = true;
        *ctx.dirty = true;
        return false;
    }
    // Find the owning (group, tab) and either drop the pane from its split or,
    // if its only shell died, keep the tab and respawn a fresh shell. Explicit
    // close-pane/worktree actions remove the pane from the session before the
    // PTY exit event arrives, so this path is for external child death.
    let owner = ctx
        .session
        .iter_tabs()
        .find(|(_, _, t)| t.center.pane_ids().contains(&id))
        .map(|(gi, ti, t)| (gi, ti, t.center.pane_ids().len() == 1));
    if let Some((gi, ti, sole)) = owner {
        // A standalone terminal's sole shell exited: close the terminal rather
        // than respawning a fresh shell (a terminal's whole purpose IS that one
        // shell). Worktrees keep respawning below; terminals divert here (before
        // the crash/respawn/notification handling, which a terminal's empty
        // `path` skips anyway) so the `terminals` registry row + sidebar entry
        // are removed. The dead pane is already out of `panes.table` (top of fn).
        if sole && ctx.session.worktrees[gi].kind == crate::session::GroupKind::Terminal {
            // Delete the `terminals` registry row ONLY on a genuine interactive
            // close (Ctrl-D on a live terminal). Keep it — so the terminal
            // survives to the next launch — when the exit is teardown or a
            // non-interactive death:
            //  - shutting down: the exit is the app quitting, not a user close;
            //  - fast crash (age < CRASH_THRESHOLD): a resurrected terminal whose
            //    sandbox/remote backend is gone at launch exits immediately, and
            //    reaping its row would silently drop a persisted terminal.
            let age = ctx.panes.pane_age(id).unwrap_or_default();
            ctx.panes.forget_spawn_time(id);
            let shutting_down = ctx.shutdown.load(std::sync::atomic::Ordering::Relaxed);
            let interactive_close = !shutting_down && age >= CRASH_THRESHOLD;
            close_exited_terminal(ctx, gi, ti, interactive_close);
            return false;
        }
        let is_active_tab = gi == ctx.session.active && ti == ctx.session.worktrees[gi].active_tab;
        // A pane that exits within CRASH_THRESHOLD of being spawned is a
        // "fast crash" — bwrap/sandbox failures write their error to the PTY
        // before dying, so output-based detection would mis-classify them as
        // normal exits. Count consecutive fast crashes; reset when a pane
        // lives long enough (normal exit).
        let age = ctx.panes.pane_age(id).unwrap_or_default();
        ctx.panes.forget_spawn_time(id);
        let crash_key = (gi, ti);
        let crashes = update_crash_count(ctx.respawn_crash_count, crash_key, age, CRASH_THRESHOLD);
        // Prefer the real exit code; fall back to the fast-crash heuristic
        // when the child status couldn't be reaped. A failed exit arms the
        // relaunch overlay on the respawned shell.
        let failed = match exit_code {
            Some(c) => c != 0,
            None => crashes > 0,
        };
        // A remote env's interactive pane is a `*-ssh` self-bridge subprocess, so a
        // fast failed exit is a connect/resolve failure the compositor can't observe
        // any other way. Mark the provider unhealthy so the respawn's off-thread
        // `env_halt_reason` check (in the materialize worker) halts — surfacing the
        // "cannot connect" modal via `drain_specs`' Err arm — instead of silently
        // husking pane after pane. Order-dependent: the mark must land before the
        // materialize worker reads it, which the mark-here / read-later-off-thread
        // sequencing guarantees.
        if failed && age < CRASH_THRESHOLD {
            // Resolve with the SAME config the respawn's `env_halt_reason` uses
            // (`keymap_config`), so the mark and the halt check agree.
            report_pane_connect_failure(ctx.keymap_config, &ctx.session.worktrees[gi].path);
        }
        {
            let wt = ctx.session.worktrees[gi].path.clone();
            if !wt.is_empty() {
                let program = exited_program.clone().unwrap_or_default();
                let is_shell = crate::pane::is_routine_pane(&program);
                let policy = thegn_core::event_bus::ProcessExitPolicy::parse(
                    &ctx.current_config.notifications.process_exit,
                );
                let outcome =
                    thegn_core::event_bus::classify_process_exit(exit_code, is_shell, policy);
                // Explicit derefs: `.clone()` on the `&T` fields would clone
                // the *reference* (which can't cross into 'static). These are
                // the owned EventBus clone + the Arc<NotifyState> handle.
                let bus = (*ctx.event_bus).clone();
                let nstate = std::sync::Arc::clone(ctx.notify_state);
                let exited_session = exited_session.clone();
                tokio::task::spawn_blocking(move || {
                    let Ok(db) = thegn_core::db::Db::open() else {
                        return;
                    };
                    // Agent panes keep their dedicated agent_done/failed path;
                    // everything else routes through item-524 process attention.
                    //
                    // Which row: `dispatch_for_exit` resolves by daemon session
                    // id first and only then falls back to the most recent
                    // ACTIVE row for the worktree — never a terminal one. So a
                    // shell opened later in an ex-agent worktree no longer
                    // re-stamps (and re-notifies) work that finished days ago,
                    // and two stages sharing a worktree each get their own row.
                    //
                    // DIVISION OF LABOUR: this handler only stamps dispatches
                    // whose worker is a *pane*. A headless session (`session
                    // open` without `--adopt`) has no pane to exit here — its
                    // Done/Failed is written by the supervising agent after
                    // `sessions.wait`, which is the only observer that sees it
                    // finish.
                    if let Ok(Some((dispatch_id, issue_id))) =
                        db.dispatch_for_exit(&wt, exited_session.as_deref())
                    {
                        let kind = if failed { "agent_failed" } else { "agent_done" };
                        let base = thegn_core::util::basename(&wt);
                        let msg = format!(
                            "agent {} in {base}",
                            if failed { "crashed" } else { "finished" }
                        );
                        // Routing gate: a rule may drop this from the inbox; a
                        // sound fires per the decision (agent panes have no
                        // desktop event, matching prior behavior).
                        let (dec, _) =
                            crate::notify::record(&db, &nstate, kind, &issue_id, &msg, &wt);
                        nstate.emit_sound(&dec);
                        // Write the roster status through the TYPED enum, not a
                        // free string: the old `"failed"`/`"done"` string writes
                        // were not members of the parseable set a supervisor
                        // resumes from (`AgentDispatchStatus`), so exactly the
                        // rows that mattered were unreadable. `Done`/`Failed`
                        // round-trip through `AgentDispatchStatus::parse`.
                        use thegn_core::issue::AgentDispatchStatus;
                        let _ = db.update_dispatch_status(
                            dispatch_id,
                            if failed {
                                AgentDispatchStatus::Failed
                            } else {
                                AgentDispatchStatus::Done
                            },
                        );
                        // Tell the pipeline board its roster moved. A flag, not
                        // a channel send: this task holds no refresh sender, and
                        // the exit has already dirtied the frame — so the board
                        // re-samples on the next loop turn with no wake source
                        // of its own (`monitor_pipeline::take_roster_dirty`).
                        crate::monitor_pipeline::mark_roster_dirty();
                        return;
                    }
                    // Non-agent pane: route per policy.
                    let Some(outcome) = outcome else {
                        return;
                    };
                    use thegn_core::event_bus::ProcessOutcome;
                    let kind = match outcome {
                        ProcessOutcome::Failed => "process_failed",
                        ProcessOutcome::TaskDone => "process_exited",
                    };
                    let label = if program.is_empty() {
                        "process"
                    } else {
                        &program
                    };
                    let msg = match (outcome, exit_code) {
                        (ProcessOutcome::Failed, Some(c)) => {
                            format!("{label} failed (exit {c})")
                        }
                        (ProcessOutcome::Failed, None) => format!("{label} crashed"),
                        (ProcessOutcome::TaskDone, _) => format!("{label} finished"),
                    };
                    // Diagnostic hook: with routine shells + unreapable `None`
                    // exits suppressed in `classify_process_exit`, a
                    // `process_failed` here is a genuine task-pane failure. If a
                    // spurious pile ever recurs, `THEGN_LOG=thegn::attention=debug`
                    // pins the worktree + exit_code + program that minted it
                    // (e.g. to confirm a remote-bridge relay-lost trigger).
                    if matches!(outcome, ProcessOutcome::Failed) {
                        tracing::debug!(
                            target: "thegn::attention",
                            worktree = %wt,
                            program = %program,
                            ?exit_code,
                            "recording process_failed notification"
                        );
                    }
                    // Routing gate: record (unless dropped), then desktop
                    // toast + sound only when the decision allows (rules /
                    // DND / modes).
                    let (dec, _) = crate::notify::record(&db, &nstate, kind, &program, &msg, &wt);
                    let event = thegn_core::event_bus::Event::ProcessExited {
                        worktree: wt.clone(),
                        program: program.clone(),
                        exit_code,
                        failed: matches!(outcome, ProcessOutcome::Failed),
                    };
                    // Desktop urgency gating still applies in the notifier
                    // thread; the decision decides whether it is eligible at
                    // all.
                    if dec.desktop {
                        bus.publish_with_notification(&event);
                    } else {
                        bus.publish(&event);
                    }
                    nstate.emit_sound(&dec);
                });
            }
        }
        if sole {
            // NO respawn happens here. The dead id already left `panes.table`
            // (top of fn) and its leaf stays in `tab.center`, so it is now a
            // "missing leaf" the loop's off-thread materialize pipeline
            // (`maybe_materialize` → spec channel → `materialize_with_specs`)
            // respawns — sandbox resolution (DB open, container ensure:
            // seconds to minutes on a wedged podman) runs on a blocking task,
            // never on the loop. A sandbox halt (`env_halt_reason`) surfaces
            // via the same pipeline's Err arm (`drain_specs` raises the
            // once-per-key modal), replacing the inline handling that lived
            // here. The bookkeeping runs for EVERY sole exit — active tab or
            // not — so a background tab's switch-back materialize sees the
            // same prepared state (no dead-daemon-session reattach, no stale
            // relaunch after a clean exit).
            // Was a relaunchable foreground command captured for this leaf?
            // `pane_cmds` is persist-time capture (workspace switch / quit /
            // rename) — an agent watched straight through its run may have
            // none, and Enter could not retype anything, so the status must
            // not promise it. (A captured command also implies the host
            // backend: `foreground_command` skips wrapper children, so this
            // agrees with the overlay's host-only arming rule.) Computed
            // before `prep_leaf_for_respawn`, whose `keep_cmd` decision
            // preserves exactly the entries that exist here.
            let relaunchable = ctx
                .session
                .tab_mut(gi, ti)
                .is_some_and(|t| t.pane_cmds.contains_key(&id));
            if let Some(tab) = ctx.session.tab_mut(gi, ti) {
                // `exit_code == None` is a transport-loss exit (the relay's
                // reconnect ladder exhausted, pane.rs) — the daemon/provider
                // session may still be alive. Keep its record so switch-back
                // materialize can warm-reattach instead of orphaning it.
                let transport_loss = exit_code.is_none();
                crate::handlers::crash::prep_leaf_for_respawn(
                    tab,
                    id,
                    failed,
                    transport_loss,
                    // An agent's clean exit still keeps the remembered
                    // command: the respawned shell must arm the
                    // Enter-to-relaunch overlay the status line promises.
                    daemon_agent_exit,
                    respawn_tail,
                );
            }
            if is_active_tab {
                match crate::handlers::crash::respawn_action(crashes, failed) {
                    crate::handlers::crash::RespawnAction::GiveUp => {
                        // Crashing on every startup — stop respawning and surface
                        // the pane's real last error (e.g. a container/exec
                        // failure) so it isn't a silent black hole. `center_dormant`
                        // also gates the materialize block off.
                        tracing::error!(
                            worktree = %ctx.session.worktrees[gi].name,
                            tail = %crash_tail,
                            "sandbox pane kept crashing; not respawning"
                        );
                        ctx.loading_state
                            .remove(&(ctx.session.worktrees[gi].name.clone(), ti));
                        ctx.model.load_steps.clear();
                        *ctx.center_dormant = true;
                        ctx.model.status =
                            crate::handlers::crash::keeps_crashing_status(&crash_tail);
                    }
                    crate::handlers::crash::RespawnAction::LeaveForMaterialize { .. } => {
                        // The materialize half owns the relaunch offer: it arms
                        // `set_pending_relaunch` (host backend + remembered cmd
                        // only), so no Enter-to-relaunch promise here — except
                        // for an attached agent's exit with a captured
                        // command, which `keep_cmd` preserved precisely so
                        // that promise holds (no capture — e.g. the common
                        // open-and-watch flow, which never persisted — means
                        // the bare line only). Note: on a native-exec provider
                        // worktree the respawn relaunches the worktree's
                        // remembered AGENT (materialize's `db.worktree_agent`
                        // rule), not the forced plain shell the old inline
                        // path spawned — an agent crash loop stays bounded by
                        // the 3-crash give-up above.
                        ctx.model.status = if daemon_agent_exit {
                            crate::handlers::crash::agent_exit_status(
                                exited_program.as_deref().unwrap_or(""),
                                exit_code,
                                relaunchable,
                            )
                        } else if failed {
                            "Pane crashed; restarting shell…".into()
                        } else {
                            "Pane exited; restarting shell…".into()
                        };
                        left_for_materialize = true;
                        *ctx.need_relayout = true;
                    }
                }
            }
        } else if let Some(tab) = ctx.session.tab_mut(gi, ti) {
            tab.center.remove(id);
            if tab.focused_pane == id
                && let Some(first) = tab.center.pane_ids().first()
            {
                tab.focused_pane = *first;
            }
            // An attached agent finishing inside a split still removes its leaf
            // (a fan-out tab must not accumulate one husk per finished stage —
            // design §D3's documented tradeoff) but is announced on the active
            // tab's status line, never a silent vanish. No Enter/Esc hint:
            // the leaf is gone — there is no respawned shell for the overlay
            // to intercept in.
            if daemon_agent_exit && is_active_tab {
                ctx.model.status = crate::handlers::crash::agent_exit_status(
                    exited_program.as_deref().unwrap_or(""),
                    exit_code,
                    false,
                );
            }
            *ctx.need_relayout = true;
        }
    }
    *ctx.dirty = true;
    left_for_materialize
}

/// A standalone terminal's last shell exited: tear the terminal down instead of
/// respawning. Mirrors `handlers::sidebar_actions::close_terminal` for the drain
/// context — a multi-tab terminal loses just the dead tab, and when its last tab
/// goes the whole group is removed and the sidebar rebuilt so it stops rendering.
///
/// `delete_registry` controls whether the durable `terminals` row is also
/// removed: `true` for a genuine interactive close (Ctrl-D on a live terminal),
/// `false` when the exit is teardown/shutdown or a non-interactive fast crash —
/// there the terminal must persist to the next launch, so the row stays and the
/// dead group is simply dropped from the live session (it re-materializes on
/// activation). Best-effort DB delete: the live session + model are the source
/// of truth here; the DB is a cache.
fn close_exited_terminal(ctx: &mut DrainCtx<'_>, gi: usize, ti: usize, delete_registry: bool) {
    // Bound before the detach: on the multi-tab path only tab `ti` is removed,
    // and the loop's `(group, tab index)` / `(group index, tab index)` keyed
    // state has to be re-keyed with it (see `handlers::tab_keys`). DrainCtx
    // carries the splash/crash-count slice of that state; the rest is re-keyed
    // by the interactive close path.
    let group_name = ctx.session.worktrees.get(gi).map(|g| g.name.clone());
    let closed_group = detach_exited_terminal(
        ctx.session,
        &mut ctx.model.sidebar_db_terminals,
        gi,
        ti,
        delete_registry,
    );
    if closed_group.is_none()
        && let Some(group) = group_name
    {
        // Only the dead TAB left the group: shift the tab-index-keyed state.
        ctx.loading_state.on_tab_closed(&group, ti);
        crate::handlers::tab_keys::shift_named_map(ctx.loading_remote, &group, ti);
        crate::handlers::tab_keys::shift_named_set(ctx.loading_retired, &group, ti);
        crate::handlers::tab_keys::shift_indexed_map(ctx.respawn_crash_count, gi, ti);
    }
    if let Some((name, db_id)) = closed_group {
        // Keep-case leaves the registry row in the snapshot (above), so the
        // persisted terminal keeps rendering — now as an inactive, materializable
        // entry — after its dead group is dropped from the live session.
        ctx.model.status = if delete_registry {
            format!("Closed terminal \"{name}\"")
        } else {
            format!("Terminal \"{name}\" exited")
        };
        if let Some(id) = db_id
            && delete_registry
        {
            tokio::task::spawn_blocking(move || {
                use thegn_core::store::WorkspaceStore;
                // best-effort: cache-only; the group is already gone from the
                // live session + model above.
                if let Ok(db) = thegn_core::db::Db::open() {
                    let _ = db.del_terminal(id);
                }
            });
        }
    }
    crate::run::persist_session_layout(ctx.session, ctx.panes);
    crate::run::refresh_tab_model(ctx.model, ctx.session, ctx.sb);
    ctx.sb.focus_active_row(ctx.model);
    *ctx.need_relayout = true;
    *ctx.dirty = true;
}

/// Detach an exited terminal's dead tab from the live `session` + (optionally)
/// the sidebar's `db_terminals` registry snapshot, restoring focus to the
/// pre-exit active group when it survives. A multi-tab terminal loses only tab
/// `ti` and returns `None`; the last tab removes the whole group and returns
/// `Some((name, db_row_id))`. `remove_from_snapshot` drops the row from the
/// sidebar snapshot (the interactive-close path, which also deletes the DB row);
/// when `false` the row is left in place so the persisted terminal keeps
/// rendering after a non-interactive/teardown exit. Pure session/model
/// bookkeeping so the last-tab vs multi-tab split is unit-tested without a
/// `DrainCtx`.
fn detach_exited_terminal(
    session: &mut crate::session::Session,
    db_terminals: &mut Vec<thegn_core::models::TerminalRow>,
    gi: usize,
    ti: usize,
    remove_from_snapshot: bool,
) -> Option<(String, Option<i64>)> {
    // Keep focus on whatever group the user was on (the terminal may have been a
    // background one); restore it by name after the remove shifts indices.
    let prior_active = session.active_group().map(|g| g.name.clone());

    let closed = if session.worktrees[gi].tabs.len() > 1 {
        // Other tabs are still alive — close just this one, keep the terminal.
        let g = &mut session.worktrees[gi];
        g.tabs.remove(ti);
        if g.active_tab >= g.tabs.len() {
            g.active_tab = g.tabs.len().saturating_sub(1);
        }
        None
    } else {
        // Last tab: drop the group. On an interactive close also remove the
        // registry row from the snapshot (optimistic, so the rebuild stops
        // listing it); otherwise leave it so the persisted terminal keeps
        // rendering as an inactive entry.
        let name = session.worktrees[gi].name.clone();
        let pos = db_terminals.iter().position(|t| t.name == name);
        let db_id = if remove_from_snapshot {
            pos.map(|i| db_terminals.remove(i).id)
        } else {
            pos.map(|i| db_terminals[i].id)
        };
        session.switch_to(gi);
        session.close_active_group();
        Some((name, db_id))
    };

    // Restore the pre-exit active group if it survived the index shift (a closed
    // active terminal won't be found — `close_active_group` already clamped).
    if let Some(name) = prior_active
        && let Some(idx) = session.worktrees.iter().position(|g| g.name == name)
    {
        session.switch_to(idx);
    }

    closed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    fn term_row(id: i64, name: &str) -> thegn_core::models::TerminalRow {
        thegn_core::models::TerminalRow {
            id,
            name: name.to_string(),
            kind: "local".into(),
            connection_string: String::new(),
            folder_id: None,
            created_at: 0,
            last_active: 0,
            position: 0,
            sandbox_backend: String::new(),
            observed_backend: String::new(),
            env_name: String::new(),
        }
    }

    #[test]
    fn detach_exited_terminal_last_tab_removes_group_and_returns_db_id() {
        // A single-tab terminal whose sole shell died: the whole group leaves the
        // session, its registry snapshot row is dropped, and its DB id comes back
        // for deletion — so the sidebar (built from that snapshot) stops showing it.
        let mut session = Session::default();
        session.add_group(WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"));
        session.add_group(WorktreeGroup::terminal("local")); // gi 1, single "main" tab
        session.switch_to(1);
        let mut db_terminals = vec![term_row(7, "local")];

        let closed = detach_exited_terminal(&mut session, &mut db_terminals, 1, 0, true);

        assert_eq!(closed, Some(("local".to_string(), Some(7))));
        assert!(session.worktrees.iter().all(|g| g.name != "local"));
        assert_eq!(session.worktrees.len(), 1, "only the home worktree remains");
        assert!(db_terminals.is_empty(), "registry snapshot row removed");
    }

    #[test]
    fn detach_exited_terminal_keep_case_drops_group_but_retains_registry_row() {
        // Non-interactive / teardown exit (`remove_from_snapshot = false`): the
        // dead group leaves the live session, but the registry snapshot row stays
        // so the persisted terminal keeps rendering (as an inactive, re-openable
        // entry) and its DB row is NOT deleted on the next launch.
        let mut session = Session::default();
        session.add_group(WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"));
        session.add_group(WorktreeGroup::terminal("local")); // gi 1
        session.switch_to(1);
        let mut db_terminals = vec![term_row(7, "local")];

        let closed = detach_exited_terminal(&mut session, &mut db_terminals, 1, 0, false);

        // Still reports the closed terminal (for status), but keeps the snapshot.
        assert_eq!(closed, Some(("local".to_string(), Some(7))));
        assert!(
            session.worktrees.iter().all(|g| g.name != "local"),
            "dead group dropped from the live session"
        );
        assert_eq!(db_terminals.len(), 1, "registry snapshot row retained");
        assert_eq!(db_terminals[0].id, 7);
    }

    #[test]
    fn detach_exited_terminal_multi_tab_closes_only_the_dead_tab() {
        // A terminal with a second tab keeps living when one tab's shell exits:
        // just that tab goes, the group and its DB row stay.
        let mut session = Session::default();
        session.add_group(WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"));
        let mut term = WorktreeGroup::terminal("local");
        term.add_tab(); // now 2 tabs
        session.add_group(term); // gi 1
        session.switch_to(1);
        let mut db_terminals = vec![term_row(7, "local")];

        let closed = detach_exited_terminal(&mut session, &mut db_terminals, 1, 0, true);

        assert_eq!(closed, None, "the terminal survives, so nothing to delete");
        let g = session
            .worktrees
            .iter()
            .find(|g| g.name == "local")
            .expect("terminal still present");
        assert_eq!(g.tabs.len(), 1, "only the dead tab was closed");
        assert_eq!(db_terminals.len(), 1, "DB registry row untouched");
    }

    #[test]
    fn detach_exited_terminal_keeps_focus_on_the_prior_active_group() {
        // A background terminal's shell dies while the user is on the home
        // worktree: focus must stay on home, not jump to whatever slid into the
        // removed slot.
        let mut session = Session::default();
        session.add_group(WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"));
        session.add_group(WorktreeGroup::terminal("local")); // gi 1 (background)
        session.switch_to(0);
        let mut db_terminals = vec![term_row(7, "local")];

        detach_exited_terminal(&mut session, &mut db_terminals, 1, 0, true);

        assert_eq!(
            session.active_group().map(|g| g.name.as_str()),
            Some("app/home"),
            "focus restored to the pre-exit active group"
        );
    }

    #[test]
    fn daemon_agent_exit_classifies_attached_agent_panes() {
        // Daemon-backed + non-routine program (an attached agent): the new
        // keep-cmd + honest-status path.
        // A non-routine program NOT behind the daemon (plain PTY pane) is not
        // an agent session exit either. A runtime wrapper (bwrap/ssh/…) is —
        // it's what a sandboxed/remote pane's spawn argv NAMES, and its exits
        // are ordinary shell exits.
        assert!(is_daemon_agent_exit(true, "claude"));
        // Interactive shells — and unnamed panes — keep today's behavior.
        assert!(!is_daemon_agent_exit(true, "bash"));
        assert!(!is_daemon_agent_exit(true, "zsh"));
        assert!(!is_daemon_agent_exit(true, ""));
        // A non-routine program NOT behind the daemon (plain PTY pane) is not
        // an agent session exit either.
        assert!(!is_daemon_agent_exit(false, "claude"));
        // Sandbox/remote transports: the argv label is the wrapper, not an
        // agent — misclassifying these would turn every shell exit on a
        // sandboxed or remote worktree into a fake "agent bwrap exited" line.
        assert!(!is_daemon_agent_exit(true, "bwrap"));
        assert!(!is_daemon_agent_exit(true, "ssh"));
        assert!(!is_daemon_agent_exit(true, "systemd-run"));
    }

    #[test]
    fn backlog_take_fills_the_slice_exactly_and_carries_the_rest() {
        let mut b = PtyBacklog::default();
        b.push(1, vec![0u8; 8 * 1024]);
        b.push(1, vec![1u8; 8 * 1024]);
        b.push(1, vec![2u8; 8 * 1024]);
        assert_eq!(b.total, 24 * 1024);
        // A 12KB slice takes the first chunk whole plus half the second (the
        // remainder returns to the queue front, order preserved).
        let s = b.take_slice(1, 12 * 1024);
        assert_eq!(s.len(), 12 * 1024);
        assert_eq!((s[0], s[11 * 1024]), (0u8, 1u8));
        assert_eq!(b.total, 12 * 1024);
        // The remainder coalesces and carries over FIFO, byte-exact.
        let s2 = b.take_slice(1, usize::MAX);
        assert_eq!(s2.len(), 12 * 1024);
        assert_eq!((s2[0], s2[s2.len() - 1]), (1u8, 2u8), "FIFO order");
        assert!(b.is_empty());
    }

    #[test]
    fn backlog_splits_an_oversized_chunk_at_the_cap() {
        let mut b = PtyBacklog::default();
        b.push(7, vec![0u8; 64 * 1024]);
        // The slice cap bounds one feed even when a single chunk exceeds it —
        // that's what gives the drain deadline its granularity.
        let s = b.take_slice(7, 8 * 1024);
        assert_eq!(s.len(), 8 * 1024);
        assert_eq!(b.total, 56 * 1024);
        let s2 = b.take_slice(7, usize::MAX);
        assert_eq!(s2.len(), 56 * 1024);
        assert!(b.is_empty());
    }

    #[test]
    fn backlog_round_robin_rotates_across_panes() {
        let mut b = PtyBacklog::default();
        b.push(1, vec![0u8; 1024]);
        b.push(2, vec![0u8; 1024]);
        b.push(1, vec![0u8; 1024]);
        let first = b.next_pane().unwrap();
        let _ = b.take_slice(first, 512); // takes one whole chunk
        let second = b.next_pane().unwrap();
        assert_ne!(first, second, "the second slice goes to the other pane");
    }

    #[test]
    fn backlog_drain_pane_returns_everything_for_exit() {
        let mut b = PtyBacklog::default();
        b.push(3, vec![0u8; 100]);
        b.push(3, vec![1u8; 100]);
        b.push(4, vec![2u8; 100]);
        let tail = b.drain_pane(3);
        assert_eq!(tail.len(), 200);
        assert_eq!(b.total, 100, "other panes' backlog is untouched");
        assert!(b.drain_pane(3).is_empty());
    }
}
