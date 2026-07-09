//! Loop-side side effects extracted from `run.rs` (god-file ratchet): spawning a
//! command into a pane/tab, opening a URL in the browser, and the CI action
//! dispatch (AV group) behind the CI badge overlay (`DetailOutcome::Act`) and
//! the panel's `Section::Ci` action keys — drill into a run, open it, re-run,
//! cancel. The event loop hands its mutable state in via [`CiActionCtx`] so the
//! loop keeps only thin call sites.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

use crate::pr_view::{PrView, PrViewData, PrViewOutcome};

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::detail::DetailAction;
use crate::focus::{FocusState, Zone};
use crate::hydrate::{RefreshKind, active_tab_path};
use crate::panes::{Panes, tool_drawer_argv};
use crate::run::SidebarState;
use crate::session::Session;
use superzej_core::store::NotificationStore;

/// Spawn `command` into a brand-new tab in the active group.
pub(crate) fn open_command_tab(
    session: &mut Session,
    panes: &mut Panes,
    command: &str,
    cwd: Option<&std::path::Path>,
    center: Rect,
) {
    let argv = tool_drawer_argv(command);
    let Ok(id) = panes.spawn_argv(&argv, cwd, center) else {
        return;
    };
    if let Some(g) = session.active_group_mut() {
        g.add_tab();
        if let Some(tab) = g.active_tab_mut() {
            tab.center = crate::center::CenterTree::Leaf(id);
            tab.focused_pane = id;
            return;
        }
    }
    panes.table.remove(&id);
}

/// Spawn `command` into a new split beside the focused center pane.
pub(crate) fn open_command_pane(
    session: &mut Session,
    panes: &mut Panes,
    focused: u32,
    command: &str,
    cwd: Option<&std::path::Path>,
    center: Rect,
) {
    let argv = tool_drawer_argv(command);
    let Ok(id) = panes.spawn_argv(&argv, cwd, center) else {
        return;
    };
    if let Some(tab) = session.active_tab_mut()
        && tab.center.split(focused, crate::center::Dir::Row, id)
    {
        tab.focused_pane = id;
        return;
    }
    panes.table.remove(&id);
}

/// Handle a private `OSC 5379` control message the bundled yazi drawer emitted
/// on its own PTY (see [`crate::queries::DrawerCmd`]). This is how the drawer
/// drives the host chrome while yazi keeps ownership of every keystroke, so the
/// loop never has to intercept — and mis-steal — `q`/`Esc` from yazi's inputs.
/// The caller marks the frame for relayout.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_drawer_command(
    cmd: crate::queries::DrawerCmd,
    session: &mut Session,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    drawer_pool: &mut crate::run::DrawerPool,
    drawer_home: &mut Option<std::path::PathBuf>,
    focus: &mut FocusState,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    cfg: &superzej_core::config::Config,
    center: Rect,
) {
    match cmd {
        crate::queries::DrawerCmd::Close => {
            // Hide to the keep-alive pool (position survives reopen); hand the
            // keyboard back to the center.
            crate::escape::close_drawer_to_pool(
                drawer,
                drawer_pool,
                drawer_home,
                session,
                panes,
                cfg,
            );
            if focus.drawer() {
                focus.zone = Zone::Center;
            }
        }
        crate::queries::DrawerCmd::Editor(path) => {
            // Open yazi's hovered file in a fresh center editor tab, reusing the
            // same invocation every panel open path uses. The drawer stays live.
            let cwd = crate::run::active_cwd(session);
            let command = crate::panel_util::editor_open_command(cfg, &path, None);
            open_command_tab(session, panes, &command, cwd.as_deref(), center);
            focus.zone = Zone::Center;
            crate::run::refresh_tab_model(model, session, sb);
        }
    }
}

/// Open a URL in the system browser, fully detached (no `gh`/toolchain needed).
pub(crate) fn open_url_detached(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Build a `szhost <args>` command line rooted at this process's own binary
/// (falling back to the `szhost` name on PATH), for spawning a subcommand pane.
pub(crate) fn szhost_cmd(args: &[&str]) -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "szhost".to_string());
    std::iter::once(exe.as_str())
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run a CI mutation (rerun / cancel) off the loop, then pulse a CI refresh so
/// the badge + panel repaint. The provider is resolved inside the blocking task;
/// ops it can't perform are declined with a warning (mirrors `cmd::ci`, keeping
/// the provider the single authority on capabilities). Non-mutation actions are
/// ignored here — they're handled inline on the loop.
pub(crate) fn spawn_ci_action(
    session: &Session,
    cfg: &superzej_core::config::CiConfig,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    action: DetailAction,
) {
    let wt = active_tab_path(session);
    let cfg = cfg.clone();
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let Some(client) = superzej_svc::ci::provider_for(&loc, &cfg) else {
            superzej_core::msg::warn("ci: no provider for this worktree");
            return;
        };
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let caps = client.caps();
        let res = match action {
            DetailAction::CiRerun { run_id, failed } => {
                if !caps.rerun {
                    superzej_core::msg::warn("ci: this provider can't re-run runs");
                    return;
                }
                if failed && !caps.rerun_failed {
                    // Don't silently retry everything when the user asked for
                    // failed-only (GitLab's `retry` has no scope).
                    superzej_core::msg::warn(
                        "ci: this provider can't re-run only failed jobs — use r to retry all",
                    );
                    return;
                }
                let scope = if failed {
                    superzej_core::ci::RerunScope::Failed
                } else {
                    superzej_core::ci::RerunScope::All
                };
                rt.block_on(client.rerun(&loc, &run_id, scope))
            }
            DetailAction::CiCancel { run_id } => {
                if !caps.cancel {
                    superzej_core::msg::warn("ci: this provider can't cancel runs");
                    return;
                }
                rt.block_on(client.cancel(&loc, &run_id))
            }
            // OpenUrl / RunCommand never reach here (handled on the loop).
            _ => return,
        };
        if let Err(e) = res {
            superzej_core::msg::warn(&format!("ci action failed: {e}"));
        }
        // Forced: the user just mutated a run, so the ttl guard must not
        // swallow the follow-up refetch.
        if tx.send(RefreshKind::Ci { force: true }).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// Fetch a CI run's full detail (jobs/steps) + the failing jobs' log tails off
/// the loop, then deliver them into the live modal overlay via a
/// `RefreshKind::CiDetail` on the refresh channel (applied by
/// `crate::detail::apply_ci_detail`). The header already painted from the cached
/// run; this fills the drill. On any fetch error we fall back to the cached run
/// so the modal still shows the header rather than crashing or spawning a pane.
pub(crate) fn spawn_ci_detail(
    session: &Session,
    cfg: &superzej_core::config::CiConfig,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    run: superzej_core::ci::CiRun,
) {
    use superzej_core::ci::CiState;
    let wt = active_tab_path(session);
    let cfg = cfg.clone();
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let Some(client) = superzej_svc::ci::provider_for(&loc, &cfg) else {
            return;
        };
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        // Full run (jobs/steps); on error keep the cached run so the header stays.
        let detail = rt.block_on(client.run_detail(&loc, &run.id)).unwrap_or(run);
        // Failing-job log tails (the "why did it fail"), each tail-capped by
        // `log_tail_lines` and prefixed with the job name. Fetched in small
        // concurrent batches — the provider "async" methods block on a
        // subprocess, so a run with many failed jobs was N serial calls —
        // scoped threads (each with a tiny current-thread runtime) buy real
        // parallelism while chunking keeps display order + bounds the fan-out.
        let cap = cfg.log_tail_lines;
        let failing: Vec<&superzej_core::ci::CiJob> = detail
            .jobs
            .iter()
            .filter(|j| j.state == CiState::Fail)
            .collect();
        let mut log_tail: Vec<String> = Vec::new();
        for chunk in failing.chunks(4) {
            let logs: Vec<Option<superzej_core::ci::CiLog>> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|job| {
                        scope.spawn(|| {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .ok()?;
                            rt.block_on(client.logs(&loc, &detail.id, &job.id)).ok()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().ok().flatten())
                    .collect()
            });
            for (job, log) in chunk.iter().zip(logs) {
                let Some(log) = log else { continue };
                let lines: Vec<&str> = log.text.lines().collect();
                let start = lines
                    .len()
                    .saturating_sub(if cap > 0 { cap } else { lines.len() });
                log_tail.push(format!("\u{2500}\u{2500} {} \u{2500}\u{2500}", job.name));
                log_tail.extend(lines[start..].iter().map(|s| (*s).to_string()));
            }
        }
        let payload = crate::detail::CiDetailPayload {
            run: detail,
            log_tail,
        };
        if tx.send(RefreshKind::CiDetail(Box::new(payload))).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// Run a full-screen PR-view action off the loop, posting an in-progress status
/// and pulsing a `RefreshKind::Pr` on completion (which re-hydrates the panel
/// cache and, if the view is open, re-fetches its diff + conversation). Mirrors
/// `run.rs`'s `spawn_pr_action`; `OpenUrl` is handled inline (no `gh`).
pub(crate) fn run_pr_view_action(
    session: &Session,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    model: &mut FrameModel,
    action: crate::pr_view::PrViewAction,
) {
    use crate::pr_view::PrViewAction as A;
    use superzej_core::github as gh;

    if let A::OpenUrl(url) = &action {
        open_url_detached(url);
        model.status = "Opened PR in the browser".into();
        return;
    }
    let (label, status): (&'static str, &'static str) = match &action {
        A::Merge => ("pr merge", "Merging PR (squash)…"),
        A::Approve => ("pr approve", "Approving PR…"),
        A::Rerun => ("pr rerun-checks", "Re-running failed checks…"),
        A::Comment { .. } => ("pr comment", "Posting comment…"),
        A::Review { .. } => ("pr review", "Submitting review…"),
        A::Reply { .. } => ("pr reply", "Posting reply…"),
        A::LineComment { .. } => ("pr line-comment", "Posting line comment…"),
        A::OpenUrl(_) => unreachable!("handled above"),
    };
    model.status = status.into();
    let wt = active_tab_path(session);
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let res: Result<(), gh::GhError> = match action {
            A::Merge => gh::merge_pr(&loc, gh::MergeMethod::Squash, false, false),
            A::Approve => gh::approve_pr(&loc, None),
            A::Rerun => gh::rerun_failed_checks(&loc).map(|_| ()),
            A::Comment { body } => gh::comment_pr(&loc, &body),
            A::Review { state, body } => gh::submit_review(&loc, state, Some(&body)),
            A::Reply { thread_id, body } => gh::reply_to_thread(&loc, &thread_id, &body),
            A::LineComment {
                owner,
                repo,
                number,
                commit_id,
                path,
                line,
                body,
            } => gh::add_line_comment(&loc, &owner, &repo, number, &commit_id, &path, line, &body),
            A::OpenUrl(_) => Ok(()),
        };
        if let Err(e) = res {
            superzej_core::msg::warn(&format!("{label} failed: {}", gh::describe(&e)));
        }
        if tx.send(RefreshKind::Pr).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// The panel `Section::Pr` action keys (`M` merge, `A` approve, `r` re-run,
/// `o` browser, `c` create). Merge/approve/re-run reuse the PR-view executor;
/// `o`/`c` are handled here. Returns whether the key was claimed.
pub(crate) fn panel_pr_action_key(
    key: char,
    model: &mut FrameModel,
    session: &Session,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) -> bool {
    use crate::pr_view::PrViewAction as A;
    let has_pr = model.panel.pr.is_some();
    match key {
        'M' if has_pr => run_pr_view_action(session, refresh_tx, waker, model, A::Merge),
        'M' => model.status = "No pull request to merge".into(),
        'A' if has_pr => run_pr_view_action(session, refresh_tx, waker, model, A::Approve),
        'A' => model.status = "No pull request to approve".into(),
        'r' => run_pr_view_action(session, refresh_tx, waker, model, A::Rerun),
        'o' if has_pr => {
            let url = model
                .panel
                .pr
                .as_ref()
                .map(|p| p.url.clone())
                .unwrap_or_default();
            run_pr_view_action(session, refresh_tx, waker, model, A::OpenUrl(url));
        }
        'o' => model.status = "No pull request to open".into(),
        'c' if has_pr => model.status = "A pull request already exists".into(),
        'c' => {
            model.status = "Creating PR from branch commits…".into();
            let wt = active_tab_path(session);
            let tx = refresh_tx.clone();
            let waker = waker.clone();
            tokio::task::spawn_blocking(move || {
                use superzej_core::github as gh;
                let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
                let opts = gh::CreateOpts {
                    title: None,
                    body: None,
                    base: None,
                    draft: false,
                    web: false,
                    fill: true,
                };
                if let Err(e) = gh::create_pr(&loc, &opts) {
                    superzej_core::msg::warn(&format!("pr create failed: {}", gh::describe(&e)));
                }
                if tx.send(RefreshKind::Pr).is_ok() {
                    let _ = waker.wake();
                }
            });
        }
        _ => return false,
    }
    true
}

/// Fetch the full-screen PR view's async data (conversation + diff) off the
/// loop and deliver it over `tx`. Single-flight via `generation` — the loop
/// drops deliveries from a stale generation. Best-effort: a failed fetch leaves
/// that half `None` (the view shows "loading" / degrades).
pub(crate) fn spawn_pr_view_fetch(
    session: Session,
    owner: String,
    repo: String,
    number: u64,
    generation: u64,
    tx: &UnboundedSender<PrViewData>,
    waker: &TerminalWaker,
) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let wt = active_tab_path(&session);
        let loc = superzej_core::remote::GitLoc::for_worktree(&wt);
        let conversation = superzej_core::github::conversation(&loc, &owner, &repo, number).ok();
        let diff = superzej_core::github::pr_diff(&loc).ok();
        let data = PrViewData {
            generation,
            conversation,
            diff,
        };
        if tx.send(data).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// Open the full-screen PR view from the panel's cached PR data, kicking its
/// async diff + conversation fetch. `None` (with a status set by the caller)
/// when there's no PR.
pub(crate) fn open_pr_view(
    model: &FrameModel,
    session: &Session,
    gen_ctr: &mut u64,
    tx: &UnboundedSender<PrViewData>,
    waker: &TerminalWaker,
) -> Option<PrView> {
    let pr = model.panel.pr.as_ref()?;
    *gen_ctr += 1;
    let mut v = PrView::open(
        pr,
        &model.panel.checks,
        &model.panel.pr_base,
        &model.panel.pr_head_oid,
        &model.panel.pr_mergeable,
        &model.panel.pr_merge_state,
    );
    v.branch = model.panel.branch.clone();
    v.generation = *gen_ctr;
    if !v.owner.is_empty() {
        spawn_pr_view_fetch(
            session.clone(),
            v.owner.clone(),
            v.repo.clone(),
            v.number,
            *gen_ctr,
            tx,
            waker,
        );
    }
    Some(v)
}

/// Re-kick the open view's fetch (after a write) so new comments/reviews show.
pub(crate) fn refetch_pr_view(
    view: Option<&mut PrView>,
    session: &Session,
    gen_ctr: &mut u64,
    tx: &UnboundedSender<PrViewData>,
    waker: &TerminalWaker,
) {
    if let Some(v) = view
        && !v.owner.is_empty()
    {
        *gen_ctr += 1;
        v.generation = *gen_ctr;
        spawn_pr_view_fetch(
            session.clone(),
            v.owner.clone(),
            v.repo.clone(),
            v.number,
            *gen_ctr,
            tx,
            waker,
        );
    }
}

/// Route a key to the open PR view: close it, do nothing, or run its action.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_pr_view_key(
    view: &mut Option<PrView>,
    key: &KeyCode,
    mods: Modifiers,
    session: &Session,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    model: &mut FrameModel,
) {
    let Some(v) = view.as_mut() else { return };
    match v.handle_key(key, mods) {
        PrViewOutcome::Close => *view = None,
        PrViewOutcome::Pending => {}
        PrViewOutcome::Act(action) => run_pr_view_action(session, refresh_tx, waker, model, action),
    }
}

/// Apply an async delivery to the open view if its generation is current.
pub(crate) fn apply_pr_view_delivery(view: Option<&mut PrView>, data: PrViewData) -> bool {
    if let Some(v) = view
        && data.generation == v.generation
    {
        v.apply_data(data);
        return true;
    }
    false
}

/// A transient status line for a CI mutation about to be spawned.
fn status_for(action: &DetailAction) -> &'static str {
    match action {
        DetailAction::CiRerun { failed: true, .. } => "Re-running failed CI jobs…",
        DetailAction::CiRerun { .. } => "Re-running CI…",
        DetailAction::CiCancel { .. } => "Cancelling CI run…",
        _ => "",
    }
}

/// The mutable slice of event-loop state a CI action touches. Built inline at
/// each call site (Act dispatch, panel Select, panel action keys) so the loop
/// itself carries no CI logic.
pub(crate) struct CiActionCtx<'a> {
    pub session: &'a mut Session,
    pub panes: &'a mut Panes,
    pub model: &'a mut FrameModel,
    pub focus: &'a mut FocusState,
    pub sb: &'a mut SidebarState,
    pub need_relayout: &'a mut bool,
    pub center: Rect,
    pub cfg: &'a superzej_core::config::Config,
    pub refresh_tx: &'a UnboundedSender<RefreshKind>,
    pub waker: &'a TerminalWaker,
}

impl CiActionCtx<'_> {
    /// The run id at the panel's row cursor (if any).
    fn run_id_at(&self, cursor: usize) -> Option<String> {
        self.model.panel.ci_runs.get(cursor).map(|r| r.id.clone())
    }

    /// Spawn `szhost <args>` in a split beside the focused pane, then focus it.
    fn open_szhost_pane(&mut self, args: &[&str]) {
        let cmd = szhost_cmd(args);
        let focused = self
            .session
            .active_tab()
            .map(|t| t.focused_pane)
            .unwrap_or(0);
        let cwd = crate::run::active_cwd(self.session);
        open_command_pane(
            self.session,
            self.panes,
            focused,
            &cmd,
            cwd.as_deref(),
            self.center,
        );
        self.focus.zone = Zone::Center;
        crate::run::refresh_tab_model(self.model, self.session, self.sb);
        *self.need_relayout = true;
    }

    /// Kick the off-loop fetch that fills a CI-run drill (the overlay already
    /// swapped to the run's header in place). The result lands back in the modal
    /// via `RefreshKind::CiDetail` — no pane is spawned (that one-shot pane was
    /// the "crashed quickly" bug: it printed and exited instantly).
    fn drill_ci_detail(&mut self, run: superzej_core::ci::CiRun) {
        self.model.status = "Fetching CI run detail\u{2026}".into();
        spawn_ci_detail(self.session, &self.cfg.ci, self.refresh_tx, self.waker, run);
    }

    /// Force a CI run-history refetch (the `g` key): bypasses the `[ci]
    /// ttl_secs` guard so the user never stares at data they just asked to
    /// update. The fetch runs off-loop via the normal refresh path.
    fn refresh_ci(&mut self) {
        self.model.status = "Refreshing CI runs\u{2026}".into();
        if self
            .refresh_tx
            .send(RefreshKind::Ci { force: true })
            .is_ok()
        {
            let _ = self.waker.wake();
        }
    }

    /// Fire a CI mutation off the loop after posting an in-progress status.
    fn spawn_mutation(&mut self, action: DetailAction) {
        self.model.status = status_for(&action).into();
        spawn_ci_action(
            self.session,
            &self.cfg.ci,
            self.refresh_tx,
            self.waker,
            action,
        );
    }

    /// Execute a detail-overlay row action, returning the overlay to *retain*
    /// (the CI drill keeps it open to fill in place) or `None` to close it — the
    /// loop assigns the result back to its `bar_detail` slot. Covers the CI badge
    /// (`OpenUrl`/`DrillCiRun`/rerun/cancel) and the notifications badge (worktree
    /// focus, inbox management, log pager, copy).
    pub(crate) fn run_detail_action(
        &mut self,
        action: DetailAction,
        overlay: Option<crate::detail::DetailOverlay>,
    ) -> Option<crate::detail::DetailOverlay> {
        let keep = action.keeps_overlay();
        match action {
            DetailAction::OpenUrl(u) => {
                open_url_detached(&u);
                self.model.status = "Opened CI run in the browser".into();
            }
            DetailAction::DrillCiRun { run } => self.drill_ci_detail(*run),
            DetailAction::CiRerun { .. } | DetailAction::CiCancel { .. } => {
                self.spawn_mutation(action)
            }
            DetailAction::CiRefresh => self.refresh_ci(),
            DetailAction::FocusWorktree(path) => self.focus_worktree(&path),
            DetailAction::DismissNotification { id } => self.mutate_notifications(Some(id)),
            DetailAction::ClearNotifications => self.mutate_notifications(None),
            DetailAction::OpenLogPager => self.open_log_pager(),
            DetailAction::CopyLine(line) => {
                crate::clipboard::copy(&line);
                self.model.status = "Copied log line".into();
            }
            // ShowLog drills in place inside the overlay and never reaches the loop.
            DetailAction::ShowLog(_) => {}
            // Intercepted by the loop's Act arm (it owns the panel locals);
            // unreachable here.
            DetailAction::OpenMergeQueueSection => {}
        }
        // Retain the overlay only for the in-place CI drill; every other action
        // has done its side effect and the modal should close.
        keep.then_some(overlay).flatten()
    }

    /// Switch to the open worktree tab at `path` (the common case for a
    /// "worktree ready" notification, which was just created + opened). If it
    /// isn't an open group, say so rather than silently doing nothing.
    fn focus_worktree(&mut self, path: &str) {
        let idx = self.session.worktrees.iter().position(|g| g.path == path);
        match idx {
            Some(i) => {
                self.session.switch_to(i);
                self.focus.zone = Zone::Center;
                crate::run::refresh_tab_model(self.model, self.session, self.sb);
                *self.need_relayout = true;
            }
            None => self.model.status = "That worktree isn't open".into(),
        }
    }

    /// Mark one (`Some(id)`) or every (`None`) notification read, off the loop,
    /// then pulse a model refresh so the inbox list + badge counts repaint.
    fn mutate_notifications(&mut self, id: Option<i64>) {
        let tx = self.refresh_tx.clone();
        let waker = self.waker.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(db) = superzej_core::db::Db::open() {
                let _ = match id {
                    Some(id) => db.mark_notification_read(id),
                    None => db.mark_all_notifications_read(),
                };
            }
            if tx.send(RefreshKind::Model).is_ok() {
                let _ = waker.wake();
            }
        });
        self.model.status = if id.is_some() {
            "Dismissed notification".into()
        } else {
            "Cleared notifications".into()
        };
    }

    /// Open the raw szhost.log in a pager pane (`$PAGER`, else `less`), scrolled
    /// to the end — fuller scrollback than the modal's bounded tail.
    fn open_log_pager(&mut self) {
        let path = superzej_core::util::xdg_state_home().join("superzej/logs/szhost.log");
        let cmd = format!("${{PAGER:-less}} +G \"{}\"", path.display());
        let focused = self
            .session
            .active_tab()
            .map(|t| t.focused_pane)
            .unwrap_or(0);
        let cwd = crate::run::active_cwd(self.session);
        open_command_pane(
            self.session,
            self.panes,
            focused,
            &cmd,
            cwd.as_deref(),
            self.center,
        );
        self.focus.zone = Zone::Center;
        crate::run::refresh_tab_model(self.model, self.session, self.sb);
        *self.need_relayout = true;
    }

    /// Enter on a panel `Section::Ci` row: drill into the selected run.
    pub(crate) fn open_view_at(&mut self, cursor: usize) {
        if let Some(id) = self.run_id_at(cursor) {
            self.open_szhost_pane(&["ci", "view", &id]);
        }
    }

    /// A `Section::Ci` action key; returns whether it was claimed. `v` drills in,
    /// `o` opens the run page, `r`/`R` re-run (all/failed), `c` cancels,
    /// `g` force-refreshes the run history.
    pub(crate) fn panel_key(&mut self, key: KeyCode, cursor: usize) -> bool {
        match key {
            KeyCode::Char('v') => {
                self.open_view_at(cursor);
                true
            }
            KeyCode::Char('g') => {
                self.refresh_ci();
                true
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.model.panel.ci_runs.get(cursor).map(|r| r.url.clone())
                    && !url.is_empty()
                {
                    open_url_detached(&url);
                    self.model.status = "Opened CI run in the browser".into();
                }
                true
            }
            KeyCode::Char(c @ ('r' | 'R')) => {
                if let Some(run_id) = self.run_id_at(cursor) {
                    self.spawn_mutation(DetailAction::CiRerun {
                        run_id,
                        failed: c == 'R',
                    });
                }
                true
            }
            KeyCode::Char('c') => {
                if let Some(run_id) = self.run_id_at(cursor) {
                    self.spawn_mutation(DetailAction::CiCancel { run_id });
                }
                true
            }
            _ => false,
        }
    }
}
