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
use crate::panes::{Panes, replace_single_dead_center_pane};
use crate::pins::pin_cwd;
use crate::run::{
    DrawerPool, SidebarState, active_cwd, group_cwd, persist_pin_state, prospective_corner_rect,
    spawn_worktree_shell_pane, update_crash_count,
};

/// A pane that exits within this of being spawned is a "fast crash" —
/// bwrap/sandbox failures write their error to the PTY before dying, so
/// output-based detection would mis-classify them as normal exits.
pub(crate) const CRASH_THRESHOLD: Duration = Duration::from_secs(2);

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
    while backlog.total < crate::loop_policy::BACKLOG_HIGH_WATER {
        match rx.try_recv() {
            Ok(PaneEvent::Output(id, chunk)) => {
                summary.chunks += 1;
                summary.bytes += chunk.len() as u64;
                backlog.push(id, chunk);
            }
            Ok(PaneEvent::Exit(id, code)) => exits.push((id, code)),
            Ok(PaneEvent::SessionFallback(id)) => fallbacks.push(id),
            Err(tokio_mpsc::error::TryRecvError::Empty) => break,
            Err(tokio_mpsc::error::TryRecvError::Disconnected) => {
                summary.disconnected = true;
                break;
            }
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
    for (id, code) in exits {
        let tail = backlog.drain_pane(id);
        if !tail.is_empty() {
            handle_output(ctx, id, &tail);
        }
        handle_exit(ctx, id, code);
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
                        let _ = p.write_reply(&ans);
                    }
                }
            }
            // DA/DSR/OSC replies + OSC52 passthrough on the graphics-stripped
            // bytes only (the kitty probe, if any, was answered by the relay).
            if !emu_text.is_empty() {
                let resp = {
                    let emu = p.emulator();
                    crate::queries::query_responses(&emu_text, emu.cursor(), emu.size())
                };
                if !resp.is_empty() {
                    let _ = p.write_reply(&resp);
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
                let emu = p.emulator();
                crate::queries::query_responses(b, emu.cursor(), emu.size())
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
    // Private drawer→host control channel (OSC 5379): the bundled yazi signals
    // close/open-in-editor here so it keeps ownership of every key (no host
    // key-stealing).
    if *ctx.drawer == Some(id)
        && let Some(cmd) = crate::queries::drawer_command(b)
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

/// A pane's PTY closed: drawer/pool/corner/pin routing, then the owning-tab
/// respawn-or-remove logic with fast-crash detection and process-exit
/// notification routing. Moved verbatim from the run.rs drain (`continue`s
/// became early returns).
fn handle_exit(ctx: &mut DrainCtx<'_>, id: u32, exit_code: Option<i32>) {
    // Program name is needed for attention routing after the pane leaves the
    // table (item 524).
    let exited_program = ctx.panes.table.get(&id).map(|p| p.program().to_string());
    // Grab the dying pane's last output BEFORE it leaves the table — a
    // sandbox/exec failure writes its error here, and a fast crash would
    // otherwise discard it (the pane just vanishes).
    let crash_tail = ctx
        .panes
        .table
        .get(&id)
        .map(|p| p.history_tail(12))
        .unwrap_or_default();
    ctx.panes.table.remove(&id);
    // The visible yazi drawer's process ended. Clear it, mark the worktree's
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
        return;
    }
    // A pooled (hidden) drawer's yazi exited; just forget it.
    if ctx.drawer_pool.remove_id(id) {
        *ctx.dirty = true;
        return;
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
        return;
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
        return;
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
        // What this pane was last running (captured at persist time) —
        // offered for relaunch after a crash. Grabbed before the pane's tab
        // is mutated.
        let remembered = ctx
            .session
            .tab_mut(gi, ti)
            .and_then(|t| t.pane_cmds.get(&id))
            .map(|c| c.display())
            .filter(|s| !s.is_empty());
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
                tokio::task::spawn_blocking(move || {
                    let Ok(db) = thegn_core::db::Db::open() else {
                        return;
                    };
                    // Agent panes (worktree has a dispatch) keep their
                    // dedicated agent_done/failed path; everything else routes
                    // through item-524 process attention.
                    if let Ok(Some((dispatch_id, issue_id))) = db.dispatch_info_for_worktree(&wt) {
                        let kind = if failed { "agent_failed" } else { "agent_done" };
                        let base = wt.rsplit('/').next().unwrap_or(&wt);
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
                        let _ = db.update_dispatch_status(
                            dispatch_id,
                            if failed { "failed" } else { "done" },
                        );
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
            if is_active_tab {
                if crashes >= 3 {
                    // Crashing on every startup — stop respawning and surface the
                    // pane's real last error (e.g. a container/exec failure) so it
                    // isn't a silent black hole.
                    tracing::error!(
                        worktree = %ctx.session.worktrees[gi].name,
                        tail = %crash_tail,
                        "sandbox pane kept crashing; not respawning"
                    );
                    ctx.loading_state
                        .remove(&(ctx.session.worktrees[gi].name.clone(), ti));
                    ctx.model.load_steps.clear();
                    *ctx.center_dormant = true;
                    ctx.model.status = crate::handlers::crash::keeps_crashing_status(&crash_tail);
                } else {
                    // Worktree dir first, then current_dir, then $HOME.
                    let cwd = group_cwd(&ctx.session.worktrees[gi])
                        .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from));
                    match spawn_worktree_shell_pane(
                        ctx.panes,
                        ctx.keymap_config,
                        cwd.as_deref(),
                        ctx.chrome_center,
                        false,
                        None,
                        "",
                    ) {
                        Ok(fresh) => {
                            if let Some(tab) = ctx.session.tab_mut(gi, ti) {
                                replace_single_dead_center_pane(tab, id, fresh);
                            }
                            // On a crash, offer to relaunch what was running
                            // (if known) over the fresh shell; a clean exit
                            // just lands at a prompt.
                            if failed && let Some(cmd) = remembered.clone() {
                                if let Some(p) = ctx.panes.table.get_mut(&fresh) {
                                    p.set_pending_relaunch(Some(cmd));
                                }
                                ctx.model.status = "Pane crashed; press Enter to relaunch \
                                     (Esc for a shell)"
                                    .into();
                            } else {
                                ctx.model.status = "Pane exited; spawned a fresh shell".into();
                            }
                            *ctx.need_relayout = true;
                        }
                        Err(err) => {
                            let k = (ctx.session.worktrees[gi].name.clone(), ti);
                            ctx.loading_state.remove(&k);
                            ctx.loading_remote.remove(&k);
                            ctx.model.load_steps.clear();
                            *ctx.center_dormant = true;
                            ctx.model.status = format!("Respawn failed: {err:#}");
                        }
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
            *ctx.need_relayout = true;
        }
    }
    *ctx.dirty = true;
}

#[cfg(test)]
mod tests {
    use super::*;

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
