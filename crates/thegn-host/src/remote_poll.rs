//! Background remote polling — the thing that makes the `↓N` badge honest
//! without a manual `git fetch`.
//!
//! Every upstream-freshness surface in thegn (the sidebar row's `↓N`, the panel
//! header's `⇣N`, the pull affordance) is derived from `refs/remotes/…` in the
//! LOCAL object store. Those refs only move when something fetches — so with no
//! background fetch the counts render "0 behind" forever, however far the remote
//! has run ahead. This module is the missing fetcher: a `git fetch --prune` (a
//! pure read — it writes remote-tracking refs and nothing else; never a merge,
//! never the working tree) on three triggers:
//!
//! - **startup** — one poll shortly after the first frame, so a session opened
//!   in the morning shows the night's commits;
//! - **periodic** — every `[git] auto_fetch_interval_secs` (default 5m);
//! - **on switch** — when a worktree becomes the active one, so looking at a
//!   worktree is itself a freshness request.
//!
//! Three properties keep that from being a network storm:
//!
//! 1. **Repo-keyed, not worktree-keyed.** Every worktree of a repo shares one
//!    object store, so one fetch updates all of them. Work is deduped by the
//!    repo's `--git-common-dir`.
//! 2. **A hard floor** (`auto_fetch_min_interval_secs`) between two fetches of
//!    the same repo, whatever the trigger — rapid worktree switching cannot
//!    outrun it — plus an in-flight latch so a ticker and a switch can't
//!    double-fetch.
//! 3. **Exponential backoff** on failure (a repo with no network / no
//!    credentials stops being retried every tick) and a hard skip while the
//!    connectivity holder says offline.
//!
//! Everything runs on the `sched` background lane, never the event loop. The
//! fetch is non-interactive by construction (`GIT_TERMINAL_PROMPT=0`, ssh
//! `BatchMode`/`ConnectTimeout`, http low-speed abort), so a repo needing
//! credentials fails fast instead of wedging a pool thread on a prompt.
//!
//! **The badge is the only default surface.** A landed fetch refreshes the
//! affected rows ([`refresh_badges`]) and repaints — nothing pops, nothing
//! chimes. The inbox notice in [`notify_behind`] is strictly opt-in behind
//! `[git] auto_fetch_notify`, on the view that an interrupting notification for
//! a routine upstream commit is noise; the number next to the down arrow is
//! what the user asked to be kept current.

use std::collections::HashMap;
use std::sync::Mutex;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::GitConfig;
use thegn_core::remote::GitLoc;

use crate::hydrate::RefreshKind;

/// Notification kind string for "the branch is behind its upstream now".
const KIND_UPSTREAM: &str = "upstream_behind";

// --- pure policy ------------------------------------------------------------

/// The auto-fetch ticker cadence in 500ms slots, from
/// `[git] auto_fetch_interval_secs`. `None` disables the periodic poll (the
/// startup + on-switch triggers still fire). Clamped to ≥ 30s — every tick is a
/// network round trip per repo. Pure, so it's unit-tested.
pub(crate) fn fetch_every_slots(interval_secs: u64) -> Option<u64> {
    (interval_secs > 0).then(|| (interval_secs.max(30) * 1000) / 500)
}

/// Whether a repo is due for a background fetch: never fetched this session, or
/// last fetched at least `min_interval_secs` ago. This is the floor EVERY
/// trigger passes through, so a burst of worktree switches costs one fetch.
/// Pure, so it's unit-tested.
pub(crate) fn due(last_at: Option<i64>, now: i64, min_interval_secs: u64) -> bool {
    match last_at {
        Some(t) => now.saturating_sub(t) >= min_interval_secs as i64,
        None => true,
    }
}

/// Refetch backoff after `failures` consecutive errors: the poll interval
/// doubled per extra failure, capped at 30 minutes. A repo whose remote is
/// unreachable (or wants credentials we can't supply non-interactively) decays
/// to a twice-hourly retry instead of burning a subprocess every tick. Pure, so
/// it's unit-tested.
pub(crate) fn backoff_secs(failures: u32, interval_secs: u64) -> u64 {
    if failures == 0 {
        return 0;
    }
    let base = interval_secs.clamp(30, 1800);
    base.saturating_mul(1u64 << (failures - 1).min(8)).min(1800)
}

/// Whether an observed `behind` count is worth a notification, given the last
/// count we notified for the same branch. Notifies on the first observation of a
/// non-zero count (so a session that starts behind says so) and on every
/// subsequent INCREASE; a count that shrank (the user pulled, or rebased) or held
/// steady is silent. Pure, so it's unit-tested.
pub(crate) fn should_notify(prev_behind: Option<usize>, behind: usize) -> bool {
    behind > 0 && prev_behind.is_none_or(|p| behind > p)
}

/// The notification/toast line for a branch that fell behind its upstream.
/// Pure, so it's unit-tested.
pub(crate) fn behind_message(branch: &str, upstream: &str, behind: usize) -> String {
    let plural = if behind == 1 { "commit" } else { "commits" };
    format!("{branch}: {behind} new {plural} on {upstream} — pull to update")
}

// --- per-repo poll state ----------------------------------------------------

/// What a repo's background poll knows between rounds. Process-global (same
/// pattern as the CI fetch-health map) so it needs no threading through the
/// trigger sites; the DB stays a cache of git *data*, never of transient poll
/// health.
#[derive(Clone, Default)]
struct RepoPoll {
    /// Epoch seconds of the last completed fetch attempt (`0` = never).
    last_at: i64,
    /// Consecutive failed fetches (reset on success).
    failures: u32,
    /// Epoch seconds before which this repo is not retried.
    backoff_until: i64,
    /// A fetch for this repo is running right now — a second trigger skips
    /// rather than queueing a duplicate network round trip.
    inflight: bool,
}

#[derive(Default)]
struct PollState {
    /// Poll bookkeeping, keyed by the repo's `--git-common-dir`.
    repos: HashMap<String, RepoPoll>,
    /// worktree path → repo key. A worktree never changes repos, so this is a
    /// permanent cache that saves a `rev-parse` subprocess per trigger.
    repo_of: HashMap<String, String>,
    /// worktree path → the last `behind` count we notified about, per branch.
    /// Keyed by `(worktree, branch)` so switching branches re-arms the notice.
    notified: HashMap<(String, String), usize>,
}

fn state() -> &'static Mutex<PollState> {
    static STATE: std::sync::OnceLock<Mutex<PollState>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| Mutex::new(PollState::default()))
}

fn lock() -> std::sync::MutexGuard<'static, PollState> {
    state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Reserve the right to fetch `repo` now: `true` only when it is past the
/// min-interval floor, past any failure backoff, and not already in flight —
/// and in that case the in-flight latch is taken under the same lock, so two
/// concurrent triggers can never both win.
fn claim(repo: &str, now: i64, min_interval_secs: u64) -> bool {
    let mut st = lock();
    let e = st.repos.entry(repo.to_string()).or_default();
    let last = (e.last_at > 0).then_some(e.last_at);
    if e.inflight || now < e.backoff_until || !due(last, now, min_interval_secs) {
        return false;
    }
    e.inflight = true;
    true
}

/// Release the in-flight latch and record the outcome: a success clears the
/// failure streak, a failure arms the exponential backoff.
fn release(repo: &str, now: i64, ok: bool, interval_secs: u64) {
    let mut st = lock();
    let e = st.repos.entry(repo.to_string()).or_default();
    e.inflight = false;
    e.last_at = now;
    if ok {
        e.failures = 0;
        e.backoff_until = 0;
    } else {
        e.failures = e.failures.saturating_add(1);
        e.backoff_until = now + backoff_secs(e.failures, interval_secs) as i64;
    }
}

/// The repo key for a worktree, cached forever after the first resolve.
/// `None` when the path isn't a git worktree.
///
/// The memo is keyed by the HOST worktree path, never `loc.path()` — for a
/// provider worktree that is the in-sandbox path (`/workspace` for every
/// worktree of an env), which made the first resolve's repo answer for all of
/// them, sharing one min-interval/backoff bucket across unrelated repos. The
/// key VALUE is the `--git-common-dir`, prefixed with the location identity
/// for off-host worktrees — two different repos in two different envs can
/// both report `/workspace/.git`, and they must not merge into one bucket.
fn repo_key(host_path: &str, loc: &GitLoc) -> Option<String> {
    if let Some(k) = lock().repo_of.get(host_path) {
        return Some(k.clone());
    }
    let common = loc.git_out(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let key = match loc {
        GitLoc::Local(_) => common,
        GitLoc::Remote { ssh, .. } => format!("ssh:{}:{}:{common}", ssh.host, ssh.port),
        GitLoc::Provider { control_prefix, .. } => {
            format!("prov:{}:{common}", control_prefix.join(" "))
        }
    };
    lock().repo_of.insert(host_path.to_string(), key.clone());
    Some(key)
}

// --- the fetch itself -------------------------------------------------------

/// The remote to poll for the checked-out branch: its configured
/// `branch.<name>.remote`, else `origin` when the repo has one, else the first
/// remote. `None` for a repo with no remotes at all (nothing to poll).
fn remote_for(loc: &GitLoc) -> Option<String> {
    if let Some(branch) = loc.git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
        && branch != "HEAD"
        && let Some(r) = loc.git_out(&["config", "--get", &format!("branch.{branch}.remote")])
    {
        return Some(r);
    }
    let remotes = loc.git_out(&["remote"])?;
    let mut names = remotes.lines().map(str::trim).filter(|s| !s.is_empty());
    if remotes.lines().any(|l| l.trim() == "origin") {
        return Some("origin".to_string());
    }
    names.next().map(str::to_string)
}

/// Run the background fetch. Non-interactive by construction: no terminal
/// credential prompt, ssh in `BatchMode` with a connect timeout, and an http
/// low-speed abort — so an unreachable or credential-hungry remote fails in
/// seconds instead of parking a background-lane thread indefinitely. Returns
/// `Ok(())` or the first stderr line.
fn run_fetch(loc: &GitLoc, remote: &str) -> Result<(), String> {
    let mut cmd = loc.git_command_env(
        &[
            // Never stop for a username/password prompt on a background fetch.
            ("GIT_TERMINAL_PROMPT", "0"),
            // …and abort an http transfer that has stalled rather than hanging.
            ("GIT_HTTP_LOW_SPEED_LIMIT", "1000"),
            ("GIT_HTTP_LOW_SPEED_TIME", "20"),
        ],
        &[
            "-c",
            "core.sshCommand=ssh -o BatchMode=yes -o ConnectTimeout=10",
            "fetch",
            "--quiet",
            "--prune",
            remote,
        ],
    );
    // off-loop: every caller is inside `sched::spawn_bg` (a spawn_blocking task).
    #[expect(clippy::disallowed_methods)]
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(err
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("git fetch failed")
        .to_string())
}

/// Whether a fetch error looks like a lost network link (offline evidence for
/// the connectivity holder) rather than a repo-specific problem (bad
/// credentials, missing remote branch), which says nothing about connectivity.
fn is_transport_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    [
        "could not resolve host",
        "connection timed out",
        "connection refused",
        "network is unreachable",
        "no route to host",
        "temporary failure in name resolution",
        "operation timed out",
        "failed to connect",
    ]
    .iter()
    .any(|p| e.contains(p))
}

/// Resolve a path for comparison against the absolute paths `git worktree list`
/// reports — a session path can carry symlinks (`/home` → `/System/Volumes/…`
/// on macOS, a symlinked worktrees dir on any platform) that would otherwise
/// never string-match. Falls back to the path as given when it can't be
/// canonicalized (a remote worktree's path doesn't exist locally).
fn canon(p: &str) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p))
}

/// The session worktrees that live in `loc`'s repo, paired with the branch each
/// one has checked out. One `git worktree list --porcelain` for the WHOLE repo —
/// a twenty-worktree repo (the shape thegn is built for) costs what a
/// single-checkout repo costs. Detached and bare entries carry no branch and are
/// dropped; so is any checkout this session doesn't show.
fn session_checkouts(loc: &GitLoc, session_worktrees: &[String]) -> Vec<(String, String)> {
    let Some(porcelain) = loc.git_out(&["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    let session: HashMap<std::path::PathBuf, &String> =
        session_worktrees.iter().map(|w| (canon(w), w)).collect();
    thegn_core::util::parse_worktree_branches(&porcelain)
        .into_iter()
        .filter_map(|(path, branch)| {
            let wt = session.get(&canon(&path))?;
            Some(((*wt).clone(), branch?))
        })
        .collect()
}

/// The default, always-on half of a successful fetch: make the **badge** honest.
///
/// The fetch just moved `refs/remotes/…` under every worktree of this repo, so
/// each one's `↓N` is now wrong by construction. Background sidebar rows are
/// glyph-TTL-cached (rightly — a rescan is a `git status` fan-out per row), which
/// would leave the badge stale for up to one TTL window after an event we KNOW
/// changed it. Evicting those rows makes the very next hydration rescan them.
/// The active row already rescans unconditionally, and the panel header reads
/// ahead/behind live, so this is what closes the loop for the other rows.
fn refresh_badges(checkouts: &[(String, String)]) {
    let paths: Vec<String> = checkouts.iter().map(|(wt, _)| wt.clone()).collect();
    crate::hydrate::invalidate_glyphs(&paths);
}

/// The opt-in half (`[git] auto_fetch_notify`): an inbox entry for every worktree
/// whose branch just fell (further) behind. Off by default — the badge is the
/// ambient surface, and an interrupting notice for a routine upstream commit is
/// noise.
///
/// `branches_full` carries every local branch's upstream + behind count in one
/// read (recomputed from the refs the fetch just moved), so this stays one
/// subprocess for the whole repo. Dispatch goes through the single notification
/// chokepoint (`notify::record`), so `[notifications]` rules, DND and modes
/// govern it exactly like every other kind — never a hand-rolled toast that
/// dodges routing.
fn notify_behind(
    loc: &GitLoc,
    checkouts: &[(String, String)],
    notify: &crate::notify::NotifyState,
) {
    use thegn_svc::git::GitBackend;
    let Ok(branches) = thegn_svc::git::GixGit::new().branches_full(loc) else {
        return;
    };
    // Opened lazily: a repo that is fully up to date must not pay a DB open.
    let mut db: Option<thegn_core::db::Db> = None;
    for (wt, branch) in checkouts {
        let Some((upstream, behind)) = branches
            .iter()
            .find(|b| &b.name == branch)
            .and_then(|b| b.upstream.as_deref().map(|u| (u, b.behind)))
        else {
            continue; // no upstream configured (or the branch vanished)
        };
        let key = (wt.clone(), branch.clone());
        if !should_notify(lock().notified.get(&key).copied(), behind) {
            // A pull (or a rebase onto the new tip) drops the count back to 0;
            // forget it so the NEXT divergence notifies again.
            if behind == 0 {
                lock().notified.remove(&key);
            }
            continue;
        }
        lock().notified.insert(key, behind);
        if db.is_none() {
            db = thegn_core::db::Db::open().ok();
        }
        let Some(db) = db.as_ref() else {
            return; // no DB, no inbox — nothing left to do this round
        };
        let msg = behind_message(branch, upstream, behind);
        let (dec, _) = crate::notify::record(db, notify, KIND_UPSTREAM, upstream, &msg, wt);
        notify.emit_sound(&dec);
    }
}

// --- triggers ---------------------------------------------------------------

/// One background poll round. `sweep` (the periodic ticker) also advances a
/// round-robin cursor over the OTHER worktrees so every repo in the session
/// converges, not just the active one; the event-driven triggers (startup,
/// worktree switch) pass `false` and poll only what the user is looking at.
///
/// Cheap and safe to call on any trigger: a repo that isn't due, is backing off,
/// or already has a fetch in flight costs one map lookup. Off-loop throughout.
pub(crate) fn poll(
    session: &crate::session::Session,
    cfg: &GitConfig,
    notify: std::sync::Arc<crate::notify::NotifyState>,
    refresh_tx: &tokio::sync::mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    sweep: bool,
) {
    if !cfg.auto_fetch || thegn_core::connectivity::is_offline() {
        return;
    }
    let active = crate::hydrate::active_tab_path(session);
    let mut targets = vec![active.to_string_lossy().into_owned()];
    if sweep && let Some(other) = next_sweep_target(session, &active) {
        targets.push(other);
    }
    let all: Vec<String> = session.worktrees.iter().map(|g| g.path.clone()).collect();
    let cfg = cfg.clone();
    let refresh_tx = refresh_tx.clone();
    let waker = waker.clone();
    crate::sched::spawn_bg(move || {
        let mut any = false;
        for target in targets {
            any |= poll_one(&target, &all, &cfg, &notify);
        }
        if any {
            // The fetch moved `refs/remotes/…`, so every cached upstream count
            // is stale. The ref fs-watcher normally catches this, but it only
            // watches the ACTIVE local worktree — drop the (tiny, in-memory)
            // branch cache directly so a swept background repo and a remote
            // worktree converge too. Cheap + lock-only; safe off-loop.
            crate::branch_cache::invalidate_all();
            // …then re-hydrate so the sidebar's ahead/behind markers and the
            // panel header repaint at once instead of on the next ticker beat.
            let _ = refresh_tx.send(RefreshKind::Model);
            let _ = waker.wake();
        }
    });
}

/// Fetch one worktree's repo (if due) and raise any behind-notifications for the
/// session worktrees that share it. Returns whether a fetch actually ran.
/// Blocking — background lane only.
fn poll_one(
    worktree: &str,
    all_worktrees: &[String],
    cfg: &GitConfig,
    notify: &crate::notify::NotifyState,
) -> bool {
    let path = std::path::Path::new(worktree);
    if !path.is_dir() {
        return false;
    }
    let loc = GitLoc::for_worktree(path);
    let Some(repo) = repo_key(worktree, &loc) else {
        return false;
    };
    let now = thegn_core::util::now();
    if !claim(&repo, now, cfg.auto_fetch_min_interval_secs) {
        return false;
    }
    let result = match remote_for(&loc) {
        Some(remote) => run_fetch(&loc, &remote),
        // No remotes configured: nothing to poll, but treat it as a success so
        // the repo just rides the normal interval instead of backing off.
        None => Ok(()),
    };
    let ok = result.is_ok();
    release(
        &repo,
        thegn_core::util::now(),
        ok,
        cfg.auto_fetch_interval_secs,
    );
    match result {
        Ok(()) => {
            // A git round trip got through — online evidence for the app-wide holder.
            thegn_core::connectivity::report_success();
            // Scoped to THIS repo's own checkouts: a fetch says nothing about an
            // unrelated repo's branches, so neither the badge refresh nor the
            // (opt-in) notice can ever touch an unrelated worktree.
            let checkouts = session_checkouts(&loc, all_worktrees);
            refresh_badges(&checkouts);
            if cfg.auto_fetch_notify {
                notify_behind(&loc, &checkouts, notify);
            }
        }
        Err(e) => {
            if is_transport_error(&e) {
                thegn_core::connectivity::report_failure();
            }
            tracing::debug!(target: "thegn::git", repo = %repo, error = %e, "auto-fetch failed");
        }
    }
    ok
}

/// Round-robin cursor over background worktrees, advanced once per sweep so
/// every repo in the session gets a turn.
static SWEEP_CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The next non-active worktree to poll on a sweep tick, or `None` when the
/// session has only the active one. The repo-level `claim` guard makes picking a
/// worktree whose repo was just fetched a free no-op, so this stays a plain
/// rotation with no cross-referencing.
fn next_sweep_target(
    session: &crate::session::Session,
    active: &std::path::Path,
) -> Option<String> {
    let others: Vec<&str> = session
        .worktrees
        .iter()
        .map(|g| g.path.as_str())
        .filter(|p| !p.is_empty() && std::path::Path::new(p) != active)
        .collect();
    if others.is_empty() {
        return None;
    }
    let i = SWEEP_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(others[i % others.len()].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_honors_config_and_clamps() {
        // `[git] auto_fetch_interval_secs` maps to 500ms ticker slots…
        assert_eq!(fetch_every_slots(300), Some(600));
        assert_eq!(fetch_every_slots(60), Some(120));
        // …is clamped to >= 30s (a network round trip per tick)…
        assert_eq!(fetch_every_slots(1), Some(60));
        assert_eq!(fetch_every_slots(30), Some(60));
        // …and 0 disables the periodic poll entirely (event triggers survive).
        assert_eq!(fetch_every_slots(0), None);
    }

    #[test]
    fn min_interval_floor_coalesces_bursts() {
        // Never fetched this session → always due (covers the startup trigger).
        assert!(due(None, 1_000, 60));
        // Inside the floor → skipped, however many switches trigger it.
        assert!(!due(Some(1_000), 1_030, 60));
        assert!(!due(Some(1_000), 1_059, 60));
        // At/past the floor → due again.
        assert!(due(Some(1_000), 1_060, 60));
        assert!(due(Some(1_000), 9_999, 60));
        // A floor of 0 means every trigger fetches.
        assert!(due(Some(1_000), 1_000, 0));
        // A clock that went backwards must not wedge the poller forever.
        assert!(!due(Some(2_000), 1_000, 60));
    }

    #[test]
    fn failure_backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(0, 300), 0);
        assert_eq!(backoff_secs(1, 300), 300);
        assert_eq!(backoff_secs(2, 300), 600);
        assert_eq!(backoff_secs(3, 300), 1200);
        // Capped at 30 minutes however long the streak…
        assert_eq!(backoff_secs(4, 300), 1800);
        assert_eq!(backoff_secs(99, 300), 1800);
        // …and a degenerate interval is clamped into the sane band first.
        assert_eq!(backoff_secs(1, 0), 30);
        assert_eq!(backoff_secs(1, 100_000), 1800);
    }

    #[test]
    fn notify_on_first_sighting_and_on_growth_only() {
        // First observation of a non-zero count notifies (the startup case).
        assert!(should_notify(None, 3));
        // Growth notifies…
        assert!(should_notify(Some(3), 5));
        // …a steady or shrinking count does not (no repeat every 5 minutes).
        assert!(!should_notify(Some(3), 3));
        assert!(!should_notify(Some(5), 1));
        // Caught up: never a notification, from any prior state.
        assert!(!should_notify(None, 0));
        assert!(!should_notify(Some(4), 0));
    }

    #[test]
    fn behind_message_reads_as_a_call_to_action() {
        assert_eq!(
            behind_message("main", "origin/main", 1),
            "main: 1 new commit on origin/main — pull to update"
        );
        assert_eq!(
            behind_message("tg/feat", "origin/tg/feat", 12),
            "tg/feat: 12 new commits on origin/tg/feat — pull to update"
        );
    }

    #[test]
    fn transport_errors_are_told_apart_from_repo_errors() {
        // Lost link → offline evidence for the connectivity holder.
        assert!(is_transport_error(
            "fatal: unable to access 'https://x/': Could not resolve host: x"
        ));
        assert!(is_transport_error(
            "ssh: connect to host x: Connection timed out"
        ));
        assert!(is_transport_error(
            "ssh: connect to host x: Network is unreachable"
        ));
        // Repo-specific failures say nothing about connectivity — the remote
        // was reached fine, it just refused us.
        assert!(!is_transport_error(
            "fatal: Authentication failed for 'https://x/'"
        ));
        assert!(!is_transport_error(
            "fatal: 'upstream' does not appear to be a git repository"
        ));
        assert!(!is_transport_error("Permission denied (publickey)."));
    }

    // ── real git: the fetch → behind-count pipeline over a file:// remote ────
    // The point of the whole module is that `↓behind` only moves when something
    // fetches. This proves the actual plumbing (remote resolution, the
    // non-interactive fetch argv, the ahead/behind read) does move it — with a
    // local clone standing in for the network.
    #[expect(clippy::disallowed_methods)] // test-only git plumbing, never on the loop
    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = thegn_core::util::git_cmd(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
    }

    fn identity(dir: &std::path::Path) {
        git(dir, &["config", "user.name", "t"]);
        git(dir, &["config", "user.email", "t@e"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    /// The `(upstream, behind)` the notify pass would read for `branch` — the
    /// same `branches_full` read `notify_behind` makes, so the test drives the
    /// real pipeline rather than a parallel implementation of it.
    fn tracked(loc: &GitLoc, branch: &str) -> Option<(String, usize)> {
        use thegn_svc::git::GitBackend;
        let b = thegn_svc::git::GixGit::new()
            .branches_full(loc)
            .ok()?
            .into_iter()
            .find(|b| b.name == branch)?;
        Some((b.upstream?, b.behind))
    }

    #[test]
    fn fetch_moves_the_behind_count_a_stale_clone_could_not_see() {
        let tag = std::process::id();
        let origin = std::env::temp_dir().join(format!("thegn-rp-origin-{tag}"));
        let clone = std::env::temp_dir().join(format!("thegn-rp-clone-{tag}"));
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        std::fs::create_dir_all(&origin).unwrap();

        git(&origin, &["init", "-q", "-b", "main"]);
        identity(&origin);
        std::fs::write(origin.join("base.txt"), "base\n").unwrap();
        git(&origin, &["add", "-A"]);
        git(&origin, &["commit", "-q", "-m", "c0"]);

        git(
            origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                &origin.to_string_lossy(),
                &clone.to_string_lossy(),
            ],
        );
        identity(&clone);
        let loc = GitLoc::Local(clone.clone());

        // The clone tracks `origin/main` and starts level with it.
        assert_eq!(remote_for(&loc).as_deref(), Some("origin"));
        assert_eq!(tracked(&loc, "main"), Some(("origin/main".into(), 0)));
        // …and the post-fetch pass maps the checkout back to its branch, which is
        // what scopes both the badge refresh and the opt-in notice to this repo.
        let wt = clone.to_string_lossy().into_owned();
        let session = vec![wt.clone(), "/somewhere/else".to_string()];
        assert_eq!(
            session_checkouts(&loc, &session),
            vec![(wt.clone(), "main".to_string())],
            "only THIS repo's session checkouts, paired with their branch"
        );

        // The remote runs ahead. Nothing local has changed, so the clone still
        // reads 0 behind — exactly the stale state the poller exists to fix.
        std::fs::write(origin.join("new.txt"), "new\n").unwrap();
        git(&origin, &["add", "-A"]);
        git(&origin, &["commit", "-q", "-m", "c1"]);
        assert_eq!(
            tracked(&loc, "main"),
            Some(("origin/main".into(), 0)),
            "pre-fetch: blind to the remote"
        );

        // One background fetch and the count behind the badge is honest.
        run_fetch(&loc, "origin").unwrap();
        let (upstream, behind) = tracked(&loc, "main").unwrap();
        assert_eq!(behind, 1);

        // …and the row is evicted from the glyph cache, so the next hydration
        // rescans it instead of serving a TTL-fresh `↓0`. This is the default
        // surface: no notification, just a badge that stops lying.
        crate::hydrate::glyph_cache().lock().unwrap().insert(
            wt.clone(),
            (
                (false, 0, 0, None, String::new(), 0, 0, None),
                std::time::Instant::now(),
            ),
        );
        refresh_badges(&session_checkouts(&loc, &session));
        assert!(
            !crate::hydrate::glyph_cache()
                .lock()
                .unwrap()
                .contains_key(&wt),
            "a landed fetch must invalidate the fetched repo's cached badges"
        );

        // The opt-in notice reads the same count off the same branch data.
        assert!(should_notify(None, behind));
        assert_eq!(
            behind_message("main", &upstream, behind),
            "main: 1 new commit on origin/main — pull to update"
        );

        // A fetch is a pure read: the working tree and HEAD are untouched, so
        // the user's uncommitted work can never be disturbed by the poller.
        assert!(!clone.join("new.txt").exists());

        // A repo with no remotes at all resolves to nothing to poll.
        let bare = std::env::temp_dir().join(format!("thegn-rp-solo-{tag}"));
        let _ = std::fs::remove_dir_all(&bare);
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "-b", "main"]);
        assert_eq!(remote_for(&GitLoc::Local(bare.clone())), None);

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn claim_is_exclusive_and_release_arms_backoff() {
        let repo = format!("/tmp/remote-poll-test-{}/.git", std::process::id());
        // First claim wins; a second (a switch racing the ticker) is refused
        // while the first is still in flight.
        assert!(claim(&repo, 1_000, 60));
        assert!(!claim(&repo, 1_000, 60));
        // A failure releases the latch but arms the backoff window…
        release(&repo, 1_000, false, 300);
        assert!(!claim(&repo, 1_100, 0), "inside the backoff window");
        assert!(claim(&repo, 1_400, 0), "past the backoff window");
        // …and a success clears the streak, leaving only the min-interval floor.
        release(&repo, 1_400, true, 300);
        assert!(!claim(&repo, 1_410, 60), "inside the min-interval floor");
        assert!(claim(&repo, 1_500, 60));
        release(&repo, 1_500, true, 300);
    }
}
