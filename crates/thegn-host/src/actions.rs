//! Loop-side side effects extracted from `run.rs` (god-file ratchet): spawning a
//! command into a pane/tab, opening a URL in the browser, and the CI action
//! dispatch (AV group) behind the CI badge overlay (`DetailOutcome::Act`) and
//! the panel's `Section::Ci` action keys — drill into a run, open it, re-run,
//! cancel. The event loop hands its mutable state in via [`CiActionCtx`] so the
//! loop keeps only thin call sites.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

use crate::diff_view::{DiffView, DiffViewData, DiffViewOutcome};
use crate::pr_view::{PrView, PrViewData, PrViewOutcome};

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::detail::DetailAction;
use crate::focus::{FocusState, Zone};
use crate::hydrate::{RefreshKind, active_tab_path};
use crate::panes::{Panes, tool_drawer_argv};
use crate::run::SidebarState;
use crate::session::Session;
use thegn_core::store::{CacheStore, NotificationStore};

/// Spawn `command` into a brand-new tab in the active group.
pub(crate) fn open_command_tab(
    session: &mut Session,
    panes: &mut Panes,
    command: &str,
    cwd: Option<&std::path::Path>,
    center: Rect,
) {
    let _ = open_command_tab_id(session, panes, command, cwd, center);
}

/// [`open_command_tab`], returning the spawned pane's id (the onboarding
/// wizard watches its login/agent tab for exit).
pub(crate) fn open_command_tab_id(
    session: &mut Session,
    panes: &mut Panes,
    command: &str,
    cwd: Option<&std::path::Path>,
    center: Rect,
) -> Option<u32> {
    let argv = tool_drawer_argv(command);
    let Ok(id) = panes.spawn_argv(&argv, cwd, center) else {
        return None;
    };
    if let Some(g) = session.active_group_mut() {
        g.add_tab();
        if let Some(tab) = g.active_tab_mut() {
            tab.center = crate::center::CenterTree::Leaf(id);
            tab.focused_pane = id;
            return Some(id);
        }
    }
    panes.table.remove(&id);
    None
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

/// Handle a private `OSC 5379` control message the drawer's file manager
/// emitted on its own PTY (see [`thegn_core::file_manager::DrawerCmd`]). This is
/// how the drawer drives the host chrome while the manager keeps ownership of
/// every keystroke, so the loop never has to intercept — and mis-steal —
/// `q`/`Esc` from the manager's inputs. The caller marks the frame for relayout.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_drawer_command(
    cmd: thegn_core::file_manager::DrawerCmd,
    session: &mut Session,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    drawer_pool: &mut crate::run::DrawerPool,
    drawer_home: &mut Option<std::path::PathBuf>,
    focus: &mut FocusState,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    cfg: &thegn_core::config::Config,
    center: Rect,
) {
    match cmd {
        thegn_core::file_manager::DrawerCmd::Close => {
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
        thegn_core::file_manager::DrawerCmd::Editor(path) => {
            // Open the manager's hovered file in a fresh center editor tab (via
            // the editor seam), reusing the same invocation every panel open
            // path uses. The drawer stays live.
            let cwd = crate::run::active_cwd(session);
            if crate::panel_util::open_editor(
                session,
                panes,
                cfg,
                &path,
                None,
                cwd.as_deref(),
                center,
                None,
            ) {
                focus.zone = Zone::Center;
            }
            crate::run::refresh_tab_model(model, session, sb);
        }
    }
}

/// Spawn `cmd` fully detached and hand its `Child` to a reaper thread that
/// `wait()`s on it (audit run.rs:13296). thegn is long-lived; without the wait,
/// every short-lived helper (xdg-open, external editor, profile window) that
/// exits would leave a `<defunct>` zombie for the rest of the session, since
/// Rust drops `Child` without reaping and the host installs no global SIGCHLD
/// handler. Returns whether the spawn itself succeeded.
pub(crate) fn spawn_detached_reaped(mut cmd: std::process::Command) -> bool {
    match cmd.spawn() {
        Ok(mut child) => {
            // best-effort reap: the thread lives only until the child exits.
            std::thread::spawn(move || {
                // off-loop: this runs on a dedicated reaper thread, never the
                // event loop — the whole point is to reap the zombie async.
                #[expect(
                    clippy::disallowed_methods,
                    reason = "reaper thread, off the event loop"
                )]
                let _ = child.wait(); // best-effort: teardown: the child may already have exited or been reaped
            });
            true
        }
        Err(_) => false,
    }
}

/// The platform's "open this URL with whatever handles it" command.
///
/// `$BROWSER` first (the POSIX convention, and what `[forward] browser`'s docs
/// promise as the fallback), then the OS opener: `open` on macOS, `xdg-open` on
/// Linux/BSD. `$BROWSER` may hold a colon-separated list — take the first entry,
/// which is what every other consumer of the variable does.
fn url_opener() -> String {
    if let Some(b) = std::env::var_os("BROWSER") {
        let b = b.to_string_lossy();
        let first = b.split(':').next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    if cfg!(target_os = "macos") {
        "open".to_string()
    } else if cfg!(target_os = "windows") {
        // Windows has no `xdg-open`. `explorer <url>` hands the URL to the
        // registered protocol handler and takes exactly one argument like the
        // other two, so `open_url_detached` needs no special case. (`cmd /c
        // start` would need an extra empty-title argument to avoid swallowing
        // a quoted URL as the window title.)
        "explorer".to_string()
    } else {
        "xdg-open".to_string()
    }
}

/// Open a URL in the system browser, fully detached (no `gh`/toolchain needed).
///
/// Callers that want an explicit command use `[forward] browser`; this is the
/// path taken when that key is empty.
pub(crate) fn open_url_detached(url: &str) {
    let mut cmd = std::process::Command::new(url_opener());
    cmd.arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    spawn_detached_reaped(cmd);
}

/// Build a `thegn <args>` command line rooted at this process's own binary
/// (falling back to the `thegn` name on PATH), for spawning a subcommand pane.
pub(crate) fn thegn_cmd(args: &[&str]) -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "thegn".to_string());
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
    cfg: &thegn_core::config::CiConfig,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    action: DetailAction,
) {
    let wt = active_tab_path(session);
    let cfg = cfg.clone();
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let loc = thegn_core::remote::GitLoc::for_worktree(&wt);
        let Some(client) = thegn_svc::ci::provider_for(&loc, &cfg) else {
            thegn_core::msg::warn("ci: no provider for this worktree");
            return;
        };
        let caps = client.caps();
        let res = match action {
            DetailAction::CiRerun { run_id, failed } => {
                if !caps.rerun {
                    thegn_core::msg::warn("ci: this provider can't re-run runs");
                    return;
                }
                if failed && !caps.rerun_failed {
                    // Don't silently retry everything when the user asked for
                    // failed-only (GitLab's `retry` has no scope).
                    thegn_core::msg::warn(
                        "ci: this provider can't re-run only failed jobs — use r to retry all",
                    );
                    return;
                }
                let scope = if failed {
                    thegn_core::ci::RerunScope::Failed
                } else {
                    thegn_core::ci::RerunScope::All
                };
                client.rerun(&loc, &run_id, scope)
            }
            DetailAction::CiCancel { run_id } => {
                if !caps.cancel {
                    thegn_core::msg::warn("ci: this provider can't cancel runs");
                    return;
                }
                client.cancel(&loc, &run_id)
            }
            // OpenUrl / RunCommand never reach here (handled on the loop).
            _ => return,
        };
        if let Err(e) = res {
            thegn_core::msg::warn(&format!("ci action failed: {e}"));
        }
        // Forced: the user just mutated a run, so the ttl guard must not
        // swallow the follow-up refetch.
        if tx.send(RefreshKind::Ci { force: true }).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
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
    cfg: &thegn_core::config::CiConfig,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    run: thegn_core::ci::CiRun,
) {
    use thegn_core::ci::CiState;
    let wt = active_tab_path(session);
    let cfg = cfg.clone();
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let loc = thegn_core::remote::GitLoc::for_worktree(&wt);
        let Some(client) = thegn_svc::ci::provider_for(&loc, &cfg) else {
            return;
        };
        // Full run (jobs/steps); on error keep the cached run so the header stays.
        let detail = client.run_detail(&loc, &run.id).unwrap_or(run);
        // Failing-job log tails (the "why did it fail"), each tail-capped by
        // `log_tail_lines` and prefixed with the job name. Fetched in small
        // concurrent batches — the provider "async" methods block on a
        // subprocess, so a run with many failed jobs was N serial calls —
        // scoped threads (each with a tiny current-thread runtime) buy real
        // parallelism while chunking keeps display order + bounds the fan-out.
        let cap = cfg.log_tail_lines;
        let failing: Vec<&thegn_core::ci::CiJob> = detail
            .jobs
            .iter()
            .filter(|j| j.state == CiState::Fail)
            .collect();
        let mut log_tail: Vec<String> = Vec::new();
        for chunk in failing.chunks(4) {
            let logs: Vec<Option<thegn_core::ci::CiLog>> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|job| scope.spawn(|| client.logs(&loc, &detail.id, &job.id).ok()))
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
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Gather AI-account usage off the loop (each account's local credential home:
/// Codex rollup files offline, Claude/Antigravity via their on-disk OAuth token
/// plus a live fetch when `[usage] allow_network`) and deliver it via
/// `RefreshKind::Usage` — into the model, which feeds the statusbar badge and the
/// panel section, and into the overlay when it's open. `thegn_svc::usage::gather`
/// never errors: unreadable accounts come back `Unavailable`.
///
/// `interactive` distinguishes the user opening the overlay (which already
/// painted a loading shell and is waiting on this) from the periodic poll. It
/// only affects logging: both run on the blocking pool directly.
///
/// Deliberately NOT through [`crate::sched::spawn_bg`]. That lane *silently
/// skips* work when it is saturated, on the assumption that a periodic trigger
/// will retry shortly — which is true of the 2s model refresh and false here.
/// The lane is busiest during startup, which is exactly when the one-shot first
/// poll fires, so the badge stayed empty until the next tick a full
/// `poll_interval_secs` (300s by default) later. The thing the lane protects
/// against is a background refresh starving interactive hydration; one
/// network-bound task every five minutes, out of 32 blocking threads, is not
/// that.
/// Rolls up the last 7 days of model-proxy audit rows for the usage panel's
/// spend block. Runs off-loop (inside the usage `spawn_blocking`); a DB failure
/// yields `None` (no block), never an error on the loop.
fn proxy_spend_rollup() -> Option<thegn_core::proxy::stats::Rollup> {
    use thegn_core::store::ModelProxyStore;
    let db = thegn_core::db::Db::open().ok()?;
    let since_ms = chrono::Utc::now().timestamp_millis() - 7 * 86_400_000;
    let rows = db.model_proxy_requests_since(since_ms, 50_000).ok()?;
    if rows.is_empty() {
        return None;
    }
    Some(thegn_core::proxy::stats::rollup(&rows))
}

pub(crate) fn spawn_usage(
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    cfg: thegn_core::config::UsageConfig,
    interactive: bool,
    proxy_enabled: bool,
) {
    let tx = refresh_tx.clone();
    let cfg_for_rollup = cfg.clone();
    let waker_for_work = waker.clone();
    let work = move || {
        let waker = waker_for_work;
        let started = std::time::Instant::now();
        let accounts = thegn_svc::usage::gather(&cfg);
        // Errors surface, never swallow: `gather` degrades every failure to an
        // `Unavailable` row by contract, so without this a missing badge is a
        // mystery. `THEGN_LOG=thegn::usage=debug` explains it — the same lesson
        // the media watcher records in its module doc.
        if tracing::enabled!(target: "thegn::usage", tracing::Level::DEBUG) {
            for a in &accounts {
                tracing::debug!(
                    target: "thegn::usage",
                    provider = %a.provider,
                    account = %a.account_label,
                    state = ?a.state,
                    windows = a.windows.len(),
                    note = a.note.as_deref().unwrap_or(""),
                    home = ?a.home,
                    "usage account"
                );
            }
        }
        let ok = accounts
            .iter()
            .filter(|a| a.state == thegn_core::usage::UsageState::Ok)
            .count();
        tracing::debug!(
            target: "thegn::usage",
            accounts = accounts.len(),
            readable = ok,
            interactive,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "usage gather"
        );
        let history = record_usage_history(&cfg, &accounts);
        // Proxy spend rolls up from the audit tables off-loop, on this same
        // cadence. Best-effort: a DB error just yields no block, never a stall.
        let proxy_spend = proxy_enabled.then(proxy_spend_rollup).flatten();
        let payload = crate::detail::UsagePayload {
            accounts,
            history,
            proxy_spend,
        };
        if tx.send(RefreshKind::Usage(Box::new(payload))).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    };
    tokio::task::spawn_blocking(work);
    // The transcript rollup is a SEPARATE task on purpose. It reads up to two
    // thousand files, and running it before the send above meant the windows —
    // the whole point of the feature — waited on a scan that outlasted the
    // first minute, leaving the badge blank. Nothing the gauge shows depends on
    // it, so it must never be in front of it.
    spawn_usage_rollup(refresh_tx, waker, cfg_for_rollup);
}

/// Refresh the host-wide transcript token rollup, at most once per
/// [`ROLLUP_INTERVAL`]. Delivered on its own `RefreshKind` so a slow scan can
/// never delay the per-account windows.
fn spawn_usage_rollup(
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    cfg: thegn_core::config::UsageConfig,
) {
    if !cfg.token_rollups || !usage_rollup_due() {
        return;
    }
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let Some(r) = thegn_svc::usage::token_rollup(&cfg) else {
            return;
        };
        tracing::debug!(
            target: "thegn::usage",
            records = r.rollup.records,
            skipped = r.skipped,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "usage token rollup"
        );
        let view = crate::detail::TokenRollupView {
            rollup: r.rollup,
            skipped: r.skipped,
        };
        if tx.send(RefreshKind::UsageTokens(Box::new(view))).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Minimum wall-clock gap between transcript rollup scans. Far longer than the
/// usage poll: the scan reads thousands of files, and token totals move slowly
/// enough that an hour-old number is not misleading.
const ROLLUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Whether this gather should also refresh the transcript rollup. True the first
/// time, then once per [`ROLLUP_INTERVAL`]. Process-global rather than per-call
/// so an interactive refresh and the ticker share one budget — otherwise
/// opening the overlay repeatedly would rescan every time.
fn usage_rollup_due() -> bool {
    use std::sync::Mutex;
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return false; // poisoned: skip the expensive work, never panic the task
    };
    let now = std::time::Instant::now();
    match *last {
        Some(t) if now.duration_since(t) < ROLLUP_INTERVAL => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

/// Persist this gather's windows and read back the recent history for each.
///
/// Best-effort throughout: the DB is a cache here (the provider is the source of
/// truth), so a failed open or write costs a sparkline, never the gather. Runs
/// off-loop, inside the same background task as the gather itself.
fn record_usage_history(
    cfg: &thegn_core::config::UsageConfig,
    accounts: &[thegn_core::usage::AccountUsage],
) -> std::collections::BTreeMap<String, Vec<(i64, f32)>> {
    use thegn_core::store::{UsageSample, UsageStore};
    let mut out = std::collections::BTreeMap::new();
    if cfg.history_days == 0 {
        return out;
    }
    let Ok(db) = thegn_core::db::Db::open() else {
        return out;
    };
    let now = thegn_core::util::now();
    let samples: Vec<UsageSample> = accounts
        .iter()
        // Only readable accounts are observations. Recording a zero for an
        // account we simply could not reach would draw a cliff in its trend and
        // then read as a "reset" to the forecast.
        .filter(|a| a.state == thegn_core::usage::UsageState::Ok)
        .flat_map(|a| {
            a.windows.iter().map(move |w| UsageSample {
                account_key: a.key.clone(),
                window: w.label.clone(),
                used_percent: w.used_percent,
                resets_at: w.resets_at,
                sampled_at: now,
            })
        })
        .collect();
    // best-effort: history is a nicety; a write failure must not fail the poll.
    let _ = db.put_usage_samples(&samples);
    let since = now - i64::from(cfg.history_days) * 86_400;
    let _ = db.prune_usage_samples(since); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    for s in &samples {
        let hist = db
            .usage_history(&s.account_key, &s.window, since)
            .unwrap_or_default();
        out.insert(
            crate::detail::history_key(&s.account_key, &s.window),
            hist.into_iter()
                .map(|h| (h.sampled_at, h.used_percent))
                .collect(),
        );
    }
    out
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
    use thegn_core::forge::model::MergeMethod;
    use thegn_core::forge::{ForgeError, LineComment, PrRef, RepoRef};

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
        A::Handoff(_) => ("pr review handoff", "Passing review feedback…"),
        A::OpenUrl(_) => unreachable!("handled above"),
    };
    model.status = status.into();
    let wt = active_tab_path(session);
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let loc = thegn_core::remote::GitLoc::for_worktree(&wt);
        let forges = crate::forge_handle::get();
        let forge = forges.for_loc(&loc);
        let cur = PrRef::Current;
        let res: Result<(), ForgeError> = match action {
            A::Merge => forge.merge_pr(&loc, cur, MergeMethod::Squash, false, false),
            A::Approve => forge.submit_review(
                &loc,
                cur,
                thegn_core::forge::model::ReviewState::Approve,
                None,
            ),
            A::Rerun => forge.rerun_failed(&loc, cur).map(|_| ()),
            A::Comment { body } => forge.comment(&loc, cur, &body),
            A::Review { state, body } => forge.submit_review(&loc, cur, state, Some(&body)),
            A::Reply { thread_id, body } => forge.reply_thread(&loc, &thread_id, &body),
            A::LineComment {
                owner,
                repo,
                number,
                commit_id,
                path,
                line,
                body,
            } => forge.add_line_comment(
                &loc,
                LineComment {
                    repo: &RepoRef { owner, repo },
                    number,
                    commit_id: &commit_id,
                    path: &path,
                    line,
                    body: &body,
                },
            ),
            A::Handoff(_) => Ok(()),
            A::OpenUrl(_) => Ok(()),
        };
        if let Err(e) = res {
            thegn_core::msg::warn(&format!("{label} failed: {}", e.describe()));
        }
        if tx.send(RefreshKind::Pr).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
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
                let loc = thegn_core::remote::GitLoc::for_worktree(&wt);
                let opts = thegn_core::forge::model::CreateOpts {
                    title: None,
                    body: None,
                    base: None,
                    draft: false,
                    web: false,
                    fill: true,
                };
                let forges = crate::forge_handle::get();
                if let Err(e) = forges.for_loc(&loc).create_pr(&loc, &opts) {
                    thegn_core::msg::warn(&format!("pr create failed: {}", e.describe()));
                }
                if tx.send(RefreshKind::Pr).is_ok() {
                    let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
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
/// that half `None` (the view shows "loading" / degrades) and logs the reason.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_pr_view_fetch(
    session: Session,
    owner: String,
    repo: String,
    number: u64,
    branch: String,
    head_oid: String,
    generation: u64,
    tx: &UnboundedSender<PrViewData>,
    waker: &TerminalWaker,
    refresh_tx: &UnboundedSender<RefreshKind>,
) {
    let tx = tx.clone();
    let waker = waker.clone();
    let refresh_tx = refresh_tx.clone();
    tokio::task::spawn_blocking(move || {
        let wt = active_tab_path(&session);
        let loc = thegn_core::remote::GitLoc::for_worktree(&wt);
        let cache_key = thegn_core::remote::GitLoc::worktree_cache_key(&wt);
        let cached = thegn_core::db::Db::open()
            .ok()
            .and_then(|db| db.get_pr_review_cache(&cache_key).ok().flatten())
            .filter(|snapshot| {
                snapshot.branch == branch
                    && snapshot.pr_number == number
                    && snapshot.head_oid == head_oid
            });
        let forges = crate::forge_handle::get();
        let forge = forges.for_loc(&loc);
        let repo_ref = thegn_core::forge::RepoRef { owner, repo };
        let conversation = match forge.conversation(&loc, &repo_ref, number) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(target: "thegn::panel", error = %e, "PR conversation fetch failed; the view opens without it");
                None
            }
        };
        let diff = match forge.pr_diff(&loc, thegn_core::forge::PrRef::Current) {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!(target: "thegn::panel", error = %e, "PR diff fetch failed; the view opens without it");
                None
            }
        };
        let live_complete = conversation.is_some() && diff.is_some();
        let review = match (conversation, diff) {
            (Some(conversation), Some(diff)) => {
                let snapshot = thegn_core::review::PrReviewSnapshot {
                    worktree_key: cache_key,
                    branch,
                    pr_number: number,
                    head_oid,
                    fetched_at: thegn_core::util::now(),
                    conversation,
                    diff,
                };
                // Branch/head are filled by the panel identity in the loop;
                // an incomplete identity is intentionally not cached or
                // presented as a current snapshot.
                Some(snapshot)
            }
            _ => cached.clone(),
        };
        let data = PrViewData {
            generation,
            conversation: review.as_ref().map(|r| r.conversation.clone()),
            diff: review.as_ref().map(|r| r.diff.clone()),
            review,
            review_status: if live_complete {
                None
            } else if cached.is_some() {
                Some("showing cached PR review".into())
            } else {
                Some("PR review unavailable or unsupported".into())
            },
        };
        // A complete remote result is delivered to the modal. The identity
        // fields are stamped by the caller's current PR facts before the
        // cache write, so a transient/partial fetch leaves the old row intact.
        if live_complete
            && let Some(snapshot) = data
                .review
                .clone()
                .filter(|s| !s.branch.is_empty() && !s.head_oid.is_empty())
            && let Ok(db) = thegn_core::db::Db::open()
        {
            let _ = db.put_pr_review_cache(&snapshot);
            if refresh_tx.send(RefreshKind::Model).is_ok() {
                let _ = waker.wake();
            }
        }
        if tx.send(data).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
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
    refresh_tx: &UnboundedSender<RefreshKind>,
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
            v.branch.clone(),
            v.head_sha.clone(),
            *gen_ctr,
            tx,
            waker,
            refresh_tx,
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
    refresh_tx: &UnboundedSender<RefreshKind>,
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
            v.branch.clone(),
            v.head_sha.clone(),
            *gen_ctr,
            tx,
            waker,
            refresh_tx,
        );
    }
}

/// Route a key to the open PR view: close it, do nothing, or run its action.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_pr_view_key(
    view: &mut Option<PrView>,
    key: &KeyCode,
    mods: Modifiers,
    session: &mut Session,
    panes: &mut crate::panes::Panes,
    focus: &mut crate::focus::FocusState,
    cfg: &thegn_core::config::Config,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    model: &mut FrameModel,
) {
    let Some(v) = view.as_mut() else { return };
    match v.handle_key(key, mods) {
        PrViewOutcome::Close => *view = None,
        PrViewOutcome::Pending => {}
        PrViewOutcome::Act(crate::pr_view::PrViewAction::Handoff(selection)) => {
            if let Some(snapshot) = v.review.clone() {
                crate::review_handoff::dispatch(
                    session, panes, focus, cfg, model, refresh_tx, waker, snapshot, selection,
                    &v.title, &v.url, &v.base,
                );
            } else {
                v.status = Some("review feedback is still loading".into());
            }
        }
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

/// Fetch the worktree's branch-point diff (the `thegn diff` range, incl.
/// uncommitted work) off the loop and deliver it over `tx`. Single-flight via
/// `generation`; a failed read delivers an empty diff (the view shows "no
/// changes" rather than hanging on "loading").
pub(crate) fn spawn_diff_view_fetch(
    session: Session,
    generation: u64,
    tx: &UnboundedSender<DiffViewData>,
    waker: &TerminalWaker,
    structural: Option<(String, crate::structural_diff::CaptureOpts)>,
) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let wt = active_tab_path(&session);
        let loc = thegn_core::remote::GitLoc::for_worktree(&wt);
        let base = crate::cmd::diff::default_branch(&loc);
        let target = loc
            .git_out(&["merge-base", &base, "HEAD"])
            .unwrap_or_else(|| "HEAD".to_string());
        let raw = loc
            .git_out(&["diff", "--no-color", &target])
            .unwrap_or_default();
        let diff = thegn_core::forge::model::parse_unified_diff(&raw);
        // Structural render (best-effort): a failure becomes a fallback notice.
        let structural = structural.map(|(difft, opts)| {
            crate::structural_diff::capture(&loc, &target, None, &difft, &opts)
                .map_err(|e| format!("difft unavailable — showing internal diff ({e})"))
        });
        let data = DiffViewData {
            generation,
            diff: Some(diff),
            structural,
            review: None,
            review_status: None,
        };
        if tx.send(data).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Open the in-app diff viewer for the active worktree, kicking its async load.
/// Honors `[git] structural_diff`: when structural and difft resolves, the view
/// requests a structural render (delivered alongside the internal diff).
pub(crate) fn open_diff_view(
    cfg: &thegn_core::config::Config,
    model: &FrameModel,
    session: &Session,
    gen_ctr: &mut u64,
    tx: &UnboundedSender<DiffViewData>,
    waker: &TerminalWaker,
) -> DiffView {
    *gen_ctr += 1;
    let wt = active_tab_path(session);
    let title = wt
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| format!("{n} · diff"))
        .unwrap_or_else(|| "diff".to_string());
    let mode = cfg.repo_git(&wt).structural_diff;
    let structural = crate::structural_diff::choose(cfg, mode).map(|difft| {
        let light_bg = thegn_core::theme::relative_luminance(&cfg.palette().bg0) > 0.5;
        let opts = crate::structural_diff::CaptureOpts {
            light_bg,
            ..crate::structural_diff::CaptureOpts::default()
        };
        (difft, opts)
    });
    let want_structural = structural.is_some();
    spawn_diff_view_fetch(session.clone(), *gen_ctr, tx, waker, structural);
    let mut view = DiffView::with_structural(title, *gen_ctr, want_structural);
    let review = model.panel.review_snapshot.clone().filter(|snapshot| {
        model.panel.pr.as_ref().is_some_and(|pr| {
            snapshot.branch == model.panel.branch
                && snapshot.pr_number == pr.number
                && snapshot.head_oid == model.panel.pr_head_oid
        })
    });
    view.set_review(review, model.panel.review_snapshot_status.clone());
    view
}

/// Route a key to the open diff viewer: close it, or consume it (read-only).
pub(crate) fn dispatch_diff_view_key(view: &mut Option<DiffView>, key: &KeyCode, mods: Modifiers) {
    let Some(v) = view.as_mut() else { return };
    match v.handle_key(key, mods) {
        DiffViewOutcome::Close => *view = None,
        DiffViewOutcome::Pending => {}
    }
}

/// Apply an async delivery to the open viewer if its generation is current.
pub(crate) fn apply_diff_view_delivery(view: Option<&mut DiffView>, data: DiffViewData) -> bool {
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
    pub cfg: &'a thegn_core::config::Config,
    pub refresh_tx: &'a UnboundedSender<RefreshKind>,
    pub waker: &'a TerminalWaker,
}

impl CiActionCtx<'_> {
    /// The run id at the panel's row cursor (if any). Resolves through the
    /// section's `display_runs` (one row per workflow), NOT raw `ci_runs`, so
    /// the action hits exactly the run the user sees on the cursor row.
    fn run_id_at(&self, cursor: usize) -> Option<String> {
        crate::panel::sections::ci::display_runs(&self.model.panel)
            .get(cursor)
            .map(|r| r.id.clone())
    }

    /// Spawn `thegn <args>` in a split beside the focused pane, then focus it.
    fn open_thegn_pane(&mut self, args: &[&str]) {
        let cmd = thegn_cmd(args);
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
    fn drill_ci_detail(&mut self, run: thegn_core::ci::CiRun) {
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
            let _ = self.waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    }

    /// Fetch one month's calendar events off the loop.
    ///
    /// The popup has already painted the month; only the day markers and the
    /// agenda are waiting on this, so nothing here is on a latency path and it
    /// deliberately posts no status message.
    fn spawn_calendar_fetch(
        &mut self,
        year: i32,
        month: u32,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) {
        crate::hydrate_calendar::spawn_month_fetch(
            self.cfg.calendar.clone(),
            year,
            month,
            from,
            to,
            self.refresh_tx.clone(),
            self.waker.clone(),
        );
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
        mut overlay: Option<crate::detail::DetailOverlay>,
    ) -> Option<crate::detail::DetailOverlay> {
        let keep = action.keeps_overlay();
        match action {
            DetailAction::OpenUrl(u) => {
                open_url_detached(&u);
                self.model.status = "Opened CI run in the browser".into();
            }
            // Intercepted by the loop's Act arm, which owns the monitor slot
            // and the saved prefs this needs. Unreachable here.
            DetailAction::OpenMonitor { .. } => {}
            DetailAction::DrillCiRun { run } => self.drill_ci_detail(*run),
            DetailAction::CiRerun { .. } | DetailAction::CiCancel { .. } => {
                self.spawn_mutation(action)
            }
            DetailAction::CiRefresh => self.refresh_ci(),
            DetailAction::FetchCalendar {
                year,
                month,
                from,
                to,
            } => self.spawn_calendar_fetch(year, month, from, to),
            DetailAction::FocusWorktree(path) => self.focus_worktree(&path),
            // Intercepted by the loop's Act arm (it owns the workspace-pool /
            // drawer locals this ctx lacks); unreachable here.
            DetailAction::ActivateTarget(_) => {}
            DetailAction::AckAttention {
                path,
                reason,
                since,
                episode,
            } => {
                self.ack_attention(path, reason, since, episode);
                // Drop the row in place: the quieted worktree leaves the list
                // where `x` was pressed (the static snapshot won't rebuild
                // until reopen, and the chip recomputes on the refresh pulse).
                if let Some(ov) = overlay.as_mut() {
                    ov.remove_selected();
                }
            }
            // "Clear all" means one thing everywhere: the inbox's `a`, the
            // overlay's `a`/`R`, and `Alt Shift R` all land in `mark_all_read`,
            // which marks notifications read *and* acks the live needs-you set.
            DetailAction::AckAllAttention => {
                crate::handlers::attention::mark_all_read(self.model, self.refresh_tx, self.waker)
            }
            DetailAction::DismissNotification { id } => {
                self.mutate_notifications(id);
                if let Some(ov) = overlay.as_mut() {
                    ov.remove_selected();
                }
            }
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
            // Intercepted by the loop's Act arm (it owns the fold/drive locals
            // that `CiActionCtx` lacks); unreachable here.
            DetailAction::MergeQueueAction { .. } => {}
        }
        // Retain the overlay only for the in-place actions (CI drill, per-row
        // `x`); every other action has done its side effect and the modal closes.
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

    /// Mark one notification read, off the loop, then pulse a model refresh so
    /// the inbox list + badge counts repaint. ("Clear all" is
    /// [`DetailAction::AckAllAttention`] → `mark_all_read`, the one total-clear
    /// path every surface shares.)
    fn mutate_notifications(&mut self, id: i64) {
        let tx = self.refresh_tx.clone();
        let waker = self.waker.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(db) = thegn_core::db::Db::open() {
                let _ = db.mark_notification_read(id); // best-effort: DB is a cache
            }
            if tx.send(RefreshKind::Model).is_ok() {
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        });
        // Optimistic: the chip drops on the next frame, not after the rehydrate.
        self.model.panel.mark_read_where(|n| n.id == id);
        self.model.status = "Dismissed notification".into();
    }

    /// Acknowledge (quiet) one worktree's live "Needs you" signal off the loop,
    /// then refresh so the badge / popup drop it. UPSERT → idempotent.
    fn ack_attention(
        &mut self,
        path: String,
        reason: thegn_core::attention::AttentionReason,
        since: Option<i64>,
        episode: thegn_core::attention::Episode,
    ) {
        let reason = serde_json::to_string(&reason).unwrap_or_default();
        if reason.is_empty() {
            return;
        }
        let quieted = path.clone();
        let tx = self.refresh_tx.clone();
        let waker = self.waker.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(db) = thegn_core::db::Db::open() {
                // best-effort: DB is a cache
                let _ = db.put_attention_ack(&path, &reason, since, episode);
                // The worktree's inbox rows are the same item seen from the
                // other side; leaving them unread made a quieted needs-you
                // entry reappear under Alerts with the ⚑ count unchanged.
                let _ = db.mark_notifications_read_for_worktree(&path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                // Same item from the other side: a quieted needs-you worktree
                // must also lower its live raised hand, or the demand returns
                // on the very next hydration.
                // best-effort: DB is a cache
                let _ = db.clear_session_attention_for_worktree(&path);
            }
            if tx.send(RefreshKind::Model).is_ok() {
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        });
        // Optimistic: quiet the worktree in the model now (the rehydrate
        // re-derives the same ack from the DB).
        self.model
            .panel
            .mark_read_where(|n| n.worktree_path == quieted);
        self.model.sidebar_status.acked.insert(quieted);
    }

    /// Open the raw thegn.log in a pager pane (`$PAGER`, else `less`), scrolled
    /// to the end — fuller scrollback than the modal's bounded tail.
    fn open_log_pager(&mut self) {
        let path = thegn_core::util::xdg_state_home().join("thegn/logs/thegn.log");
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
            self.open_thegn_pane(&["ci", "view", &id]);
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
                if let Some(url) = crate::panel::sections::ci::display_runs(&self.model.panel)
                    .get(cursor)
                    .map(|r| r.url.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_opener_prefers_browser_then_the_os_default() {
        // $BROWSER wins outright.
        {
            let _env = crate::testenv::EnvVarGuard::set(&[("BROWSER", "firefox")]);
            assert_eq!(url_opener(), "firefox");
        }
        // A colon-separated list takes its first entry (the POSIX convention).
        {
            let _env = crate::testenv::EnvVarGuard::set(&[("BROWSER", "w3m:lynx")]);
            assert_eq!(url_opener(), "w3m");
        }
        // Empty/whitespace $BROWSER falls through to the OS opener rather than
        // trying to spawn "".
        {
            let _env = crate::testenv::EnvVarGuard::set(&[("BROWSER", "  ")]);
            let expect = if cfg!(target_os = "macos") {
                "open"
            } else if cfg!(target_os = "windows") {
                "explorer"
            } else {
                "xdg-open"
            };
            assert_eq!(url_opener(), expect);
        }
    }

    #[test]
    fn thegn_cmd_joins_args_after_the_exe() {
        let cmd = thegn_cmd(&["pr", "list", "--json"]);
        // The exe path varies per environment, but the argv tail is fixed and
        // space-joined with no trailing/leading padding.
        assert!(cmd.ends_with(" pr list --json"), "cmd: {cmd}");
        assert!(!cmd.starts_with(' '));
        // No args ⇒ just the exe, no trailing space.
        let bare = thegn_cmd(&[]);
        assert!(!bare.ends_with(' '), "bare: {bare}");
    }

    #[test]
    fn status_for_describes_ci_mutations_only() {
        assert_eq!(
            status_for(&DetailAction::CiRerun {
                run_id: "1".into(),
                failed: true
            }),
            "Re-running failed CI jobs…"
        );
        assert_eq!(
            status_for(&DetailAction::CiRerun {
                run_id: "1".into(),
                failed: false
            }),
            "Re-running CI…"
        );
        assert_eq!(
            status_for(&DetailAction::CiCancel { run_id: "1".into() }),
            "Cancelling CI run…"
        );
        // A non-mutation action has no transient status.
        assert_eq!(status_for(&DetailAction::CiRefresh), "");
    }
}
