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
//! **The badge is the only default surface.** A fetch that moves refs refreshes
//! the affected rows ([`refresh_badges`]) and repaints — nothing pops, nothing
//! chimes. The inbox notice in [`notify_behind`] is strictly opt-in behind
//! `[git] auto_fetch_notify`, on the view that an interrupting notification for
//! a routine upstream commit is noise; the number next to the down arrow is
//! what the user asked to be kept current.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::GitConfig;
use thegn_core::remote::GitLoc;

use crate::hydrate::RefreshKind;

/// Notification kind string for "the branch is behind its upstream now".
const KIND_UPSTREAM: &str = "upstream_behind";

const MAX_LOOSE_REMOTE_REF_ENTRIES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FetchOutcome {
    NotRun,
    Unchanged,
    Changed,
}

fn should_refresh(outcomes: &[FetchOutcome]) -> bool {
    outcomes.contains(&FetchOutcome::Changed)
}

fn fetch_outcome(ok: bool, before: Option<u64>, after: Option<u64>) -> FetchOutcome {
    match (ok, before, after) {
        (_, Some(before), Some(after)) if before != after => FetchOutcome::Changed,
        (true, Some(_), Some(_)) => FetchOutcome::Unchanged,
        (true, _, _) => FetchOutcome::Changed,
        (false, _, _) => FetchOutcome::NotRun,
    }
}

/// Hash the remote-tracking refs in a local common dir without invoking git.
/// Any uncertainty returns `None`; a successful fetch conservatively refreshes
/// in that case.
fn remote_ref_fingerprint(loc: &GitLoc, common_dir: &std::path::Path) -> Option<u64> {
    if !matches!(loc, GitLoc::Local(_)) {
        return None;
    }

    let mut entries = Vec::new();
    let packed_refs = common_dir.join("packed-refs");
    match std::fs::read(&packed_refs) {
        Ok(contents) => entries.push((std::path::PathBuf::from("packed-refs"), contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }

    /// The cap is 4096 directory entries, including directories as well as ref files.
    fn collect_refs(
        common_dir: &std::path::Path,
        refs_root: &std::path::Path,
        dir: &std::path::Path,
        entries: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
        visited: &mut usize,
    ) -> Option<()> {
        let children = match std::fs::read_dir(dir) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && dir == refs_root => {
                return Some(());
            }
            Err(_) => return None,
        };
        for child in children {
            if *visited >= MAX_LOOSE_REMOTE_REF_ENTRIES {
                return None;
            }
            *visited += 1;
            let child = child.ok()?;
            let path = child.path();
            let kind = child.file_type().ok()?;
            if kind.is_dir() {
                collect_refs(common_dir, refs_root, &path, entries, visited)?;
            } else if kind.is_file() {
                entries.push((
                    path.strip_prefix(common_dir).ok()?.to_path_buf(),
                    std::fs::read(path).ok()?,
                ));
            } else {
                return None;
            }
        }
        Some(())
    }

    let refs_root = common_dir.join("refs/remotes");
    let mut visited = 0;
    collect_refs(
        common_dir,
        &refs_root,
        &refs_root,
        &mut entries,
        &mut visited,
    )?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (path, contents) in entries {
        path.hash(&mut hasher);
        contents.hash(&mut hasher);
    }
    Some(hasher.finish())
}

// --- pure policy ------------------------------------------------------------

/// The auto-fetch ticker cadence in 500ms slots, from
/// `[git] auto_fetch_interval_secs`. `None` disables the periodic poll (the
/// startup + on-switch triggers still fire). Clamped to ≥ 30s — every tick is a
/// network round trip per repo. Pure, so it's unit-tested.
pub(crate) fn fetch_every_slots(interval_secs: u64) -> Option<u64> {
    (interval_secs > 0)
        .then(|| thegn_core::time_policy::cadence_slots(interval_secs, 30, 500).get())
}

/// Whether a repo is due for a background fetch: never fetched this session, or
/// last fetched at least `min_interval_secs` ago. This is the floor EVERY
/// trigger passes through, so a burst of worktree switches costs one fetch.
/// Pure, so it's unit-tested.
pub(crate) fn due(last_at: Option<i64>, now: i64, min_interval_secs: u64) -> bool {
    !thegn_core::time_policy::is_fresh(now, last_at, min_interval_secs)
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
        e.backoff_until =
            thegn_core::time_policy::deadline_seconds(now, backoff_secs(e.failures, interval_secs))
                .unwrap_or(i64::MAX);
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
    thegn_core::util::note_core_git_spawn();
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
/// single-checkout repo costs. Entries with no branch (detached or bare) are
/// retained so their cache rows can be invalidated when the session lists the
/// path. Checkouts this session does not show are dropped.
fn session_checkouts(loc: &GitLoc, session_worktrees: &[String]) -> Vec<(String, Option<String>)> {
    let Some(porcelain) = loc.git_out(&["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    let session: HashMap<std::path::PathBuf, &String> =
        session_worktrees.iter().map(|w| (canon(w), w)).collect();
    thegn_core::util::parse_worktree_branches(&porcelain)
        .into_iter()
        .filter_map(|(path, branch)| {
            let wt = session.get(&canon(&path))?;
            Some(((*wt).clone(), branch))
        })
        .collect()
}

/// The default, always-on half of a successful fetch: make the **badge** honest.
///
/// The fetch just moved `refs/remotes/…` under every worktree of this repo, so
/// each one's `↓N` is now wrong by construction. Patch cached rows with the
/// fetched counts while preserving their status data and scan timestamp. Rows
/// missing from cache, on a different branch, or stamped after the ref read are
/// invalidated so hydration reconstructs them; the timestamp guard prevents an
/// older ref read from overwriting a newer scan. A failed ref read also
/// invalidates all relevant rows.
fn refresh_badges(
    checkouts: &[(String, Option<String>)],
    branches: &[thegn_core::gitrefs::BranchInfo],
    read_at: std::time::Instant,
) {
    let (patches, evictions) = ahead_behind_patches(checkouts, branches);
    for (path, branch, ahead, behind) in patches {
        if crate::hydrate::patch_glyph_ahead_behind(&path, &branch, ahead, behind, read_at)
            != crate::hydrate::GlyphPatch::Patched
        {
            crate::hydrate::invalidate_glyphs(&[path]);
        }
    }
    crate::hydrate::invalidate_glyphs(&evictions);
}

/// Match each checked-out branch to the repo-wide ref read. Detached checkouts
/// and branches absent from the read cannot be patched, so invalidate only
/// those rows and let normal hydration reconstruct them.
/// (path, branch, ahead, behind) cache patches for checkouts whose upstream moved.
type AheadBehindPatch = (String, String, usize, usize);

fn ahead_behind_patches(
    checkouts: &[(String, Option<String>)],
    branches: &[thegn_core::gitrefs::BranchInfo],
) -> (Vec<AheadBehindPatch>, Vec<String>) {
    let mut patches = Vec::new();
    let mut evictions = Vec::new();
    for (path, branch) in checkouts {
        let Some(info) = branch
            .as_ref()
            .and_then(|name| branches.iter().find(|b| &b.name == name))
        else {
            evictions.push(path.clone());
            continue;
        };
        let (ahead, behind) = if info.upstream_gone {
            (0, 0)
        } else {
            (info.ahead, info.behind)
        };
        patches.push((path.clone(), info.name.clone(), ahead, behind));
    }
    (patches, evictions)
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
    checkouts: &[(String, Option<String>)],
    branches: &[thegn_core::gitrefs::BranchInfo],
    notify: &crate::notify::NotifyState,
) {
    // Opened lazily: a repo that is fully up to date must not pay a DB open.
    let mut db: Option<thegn_core::db::Db> = None;
    for (wt, branch) in checkouts {
        let Some(branch) = branch else {
            continue;
        };
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
            db = thegn_core::db::Db::open().ok(); // best-effort: cache: the None path below reports and degrades
        }
        let Some(db) = db.as_ref() else {
            return; // no DB, no inbox — nothing left to do this round
        };
        let msg = behind_message(branch, upstream, behind);
        let _ = crate::notify::record(db, notify, KIND_UPSTREAM, upstream, &msg, wt);
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
    crate::sched::spawn_bg_replaceable(move || {
        let mut outcomes = Vec::with_capacity(targets.len());
        for target in targets {
            outcomes.push(poll_one(&target, &all, &cfg, &notify));
        }
        if should_refresh(&outcomes) {
            // The fetch moved `refs/remotes/…`, so every cached upstream count
            // is stale. The ref fs-watcher normally catches this, but it only
            // watches the ACTIVE local worktree — drop the (tiny, in-memory)
            // branch cache directly so a swept background repo and a remote
            // worktree converge too. Cheap + lock-only; safe off-loop.
            crate::branch_cache::invalidate_all();
            // …then re-hydrate so the sidebar's ahead/behind markers and the
            // panel header repaint at once instead of on the next ticker beat.
            let _ = refresh_tx.send(RefreshKind::Model); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Fetch one worktree's repo (if due) and raise any behind-notifications for the
/// session worktrees that share it. Returns whether the fetched refs changed.
/// Blocking — background lane only.
fn poll_one(
    worktree: &str,
    all_worktrees: &[String],
    cfg: &GitConfig,
    notify: &crate::notify::NotifyState,
) -> FetchOutcome {
    let path = std::path::Path::new(worktree);
    if !path.is_dir() {
        return FetchOutcome::NotRun;
    }
    let loc = GitLoc::for_worktree(path);
    let Some(repo) = repo_key(worktree, &loc) else {
        return FetchOutcome::NotRun;
    };
    // Stay out of a jj-colocated repo's way: jj's docs warn a background
    // `git fetch` can interleave with its auto-snapshot, so skip unless the user
    // opted in with `[git] auto_fetch_colocated`. The repo root (main checkout)
    // is the `.git` common dir's parent — that's where a `.jj/` sibling lives.
    let common_dir = thegn_core::util::git_common_dir(path);
    let repo_root = common_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| path.to_path_buf());
    if !thegn_core::jj::should_auto_fetch(
        true,
        thegn_core::jj::is_colocated(&repo_root),
        cfg.auto_fetch_colocated,
    ) {
        return FetchOutcome::NotRun;
    }
    let now = thegn_core::util::now();
    if !claim(&repo, now, cfg.auto_fetch_min_interval_secs) {
        return FetchOutcome::NotRun;
    }
    let result = match remote_for(&loc) {
        Some(remote) => {
            let before = remote_ref_fingerprint(&loc, &common_dir);
            let result = run_fetch(&loc, &remote);
            let ok = result.is_ok();
            let after = remote_ref_fingerprint(&loc, &common_dir);
            (result, fetch_outcome(ok, before, after))
        }
        // No remotes configured: nothing to poll, but treat it as a success so
        // the repo just rides the normal interval instead of backing off.
        None => (Ok(()), FetchOutcome::NotRun),
    };
    let (result, outcome) = result;
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
            let branches = if !checkouts.is_empty()
                && (outcome == FetchOutcome::Changed || cfg.auto_fetch_notify)
            {
                let read_at = std::time::Instant::now();
                match crate::git_handle::get().branches_full(&loc) {
                    Ok(branches) => Some((branches, read_at)),
                    Err(error) => {
                        if outcome == FetchOutcome::Changed {
                            tracing::debug!(target: "thegn::git", repo = %repo, %error, "could not read branch counts after auto-fetch; invalidating cached glyphs");
                            crate::hydrate::invalidate_glyphs(
                                &checkouts
                                    .iter()
                                    .map(|(path, _)| path.clone())
                                    .collect::<Vec<_>>(),
                            );
                        }
                        None
                    }
                }
            } else {
                None
            };
            if outcome == FetchOutcome::Changed
                && let Some((branches, read_at)) = branches.as_ref()
            {
                refresh_badges(&checkouts, branches, *read_at);
            }
            if cfg.auto_fetch_notify
                && let Some((branches, _)) = branches.as_ref()
            {
                notify_behind(&checkouts, branches, notify);
            }
        }
        Err(e) => {
            if is_transport_error(&e) {
                thegn_core::connectivity::report_failure();
            }
            tracing::debug!(target: "thegn::git", repo = %repo, error = %e, "auto-fetch failed");
        }
    }
    outcome
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
    #[expect(
        clippy::disallowed_methods,
        reason = "initialize the isolated local git fixture"
    )]
    fn gitloc_git_out_counts_a_single_spawn() {
        let dir = tempfile::tempdir().unwrap();
        thegn_core::fsperm::restrict_dir_to_owner(dir.path()).unwrap();
        let init = thegn_core::util::git_cmd(dir.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(init.success());
        let loc = GitLoc::Local(dir.path().to_path_buf());
        let before = thegn_core::util::core_git_spawn_count();
        assert!(loc.git_out(&["rev-parse", "--git-dir"]).is_some());
        assert_eq!(thegn_core::util::core_git_spawn_count() - before, 1);
    }

    #[test]
    fn remote_ref_fingerprint_tracks_loose_and_packed_refs() {
        let dir = tempfile::tempdir().unwrap();
        let refs = dir.path().join("refs/remotes/origin");
        std::fs::create_dir_all(&refs).unwrap();

        // Missing packed-refs is valid, and identical files have a stable hash.
        let loc = GitLoc::Local(dir.path().to_path_buf());
        std::fs::write(refs.join("main"), "1111111\n").unwrap();
        let first = remote_ref_fingerprint(&loc, dir.path()).unwrap();
        assert_eq!(remote_ref_fingerprint(&loc, dir.path()), Some(first));

        // A loose remote-tracking ref update changes the fingerprint.
        std::fs::write(refs.join("main"), "2222222\n").unwrap();
        let loose_updated = remote_ref_fingerprint(&loc, dir.path()).unwrap();
        assert_ne!(loose_updated, first);

        // Packed refs participate too, even when the loose refs are unchanged.
        std::fs::write(
            dir.path().join("packed-refs"),
            "# pack-refs\n3333333 refs/remotes/origin/release\n",
        )
        .unwrap();
        let packed_added = remote_ref_fingerprint(&loc, dir.path()).unwrap();
        assert_ne!(packed_added, loose_updated);
        std::fs::write(
            dir.path().join("packed-refs"),
            "# pack-refs\n4444444 refs/remotes/origin/release\n",
        )
        .unwrap();
        assert_ne!(remote_ref_fingerprint(&loc, dir.path()), Some(packed_added));
    }

    #[test]
    fn remote_ref_fingerprint_returns_unknown_past_loose_ref_cap() {
        let dir = tempfile::tempdir().unwrap();
        let refs = dir.path().join("refs/remotes/origin");
        std::fs::create_dir_all(&refs).unwrap();
        for i in 0..=MAX_LOOSE_REMOTE_REF_ENTRIES {
            std::fs::write(refs.join(format!("ref-{i}")), i.to_string()).unwrap();
        }
        let loc = GitLoc::Local(dir.path().to_path_buf());
        assert_eq!(remote_ref_fingerprint(&loc, dir.path()), None);
    }

    #[test]
    fn only_changed_fetch_outcomes_request_refresh() {
        let cases = [
            (&[][..], false),
            (&[FetchOutcome::NotRun][..], false),
            (&[FetchOutcome::Unchanged][..], false),
            (&[FetchOutcome::NotRun, FetchOutcome::Unchanged][..], false),
            (&[FetchOutcome::Changed][..], true),
            (&[FetchOutcome::Unchanged, FetchOutcome::Changed][..], true),
        ];
        for (outcomes, expected) in cases {
            assert_eq!(should_refresh(outcomes), expected, "{outcomes:?}");
        }
    }

    #[test]
    fn checkout_ref_mapping_patches_known_branches_and_evicts_unmapped_rows() {
        let branches = vec![
            thegn_core::gitrefs::BranchInfo {
                name: "main".into(),
                is_head: true,
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 5,
                upstream_gone: false,
                sha: "abc".into(),
                date: 0,
                subject: String::new(),
            },
            thegn_core::gitrefs::BranchInfo {
                name: "gone-upstream".into(),
                is_head: false,
                upstream: None,
                ahead: 0,
                behind: 0,
                upstream_gone: true,
                sha: "def".into(),
                date: 0,
                subject: String::new(),
            },
        ];
        let checkouts = vec![
            ("/wt/main".into(), Some("main".into())),
            ("/wt/missing-branch".into(), Some("removed".into())),
            ("/wt/detached".into(), None),
            ("/wt/gone-upstream".into(), Some("gone-upstream".into())),
        ];

        assert_eq!(
            ahead_behind_patches(&checkouts, &branches),
            (
                vec![
                    ("/wt/main".into(), "main".into(), 2, 5),
                    ("/wt/gone-upstream".into(), "gone-upstream".into(), 0, 0)
                ],
                vec!["/wt/missing-branch".into(), "/wt/detached".into()],
            )
        );
    }

    #[test]
    fn refresh_invalidates_newer_scans_and_rows_from_another_branch() {
        let read_at = std::time::Instant::now();
        let newer_path = format!("/test/newer-refresh-{}", std::process::id());
        let branch_path = format!("/test/branch-refresh-{}", std::process::id());
        let new_scan = read_at
            .checked_add(std::time::Duration::from_millis(1))
            .unwrap();
        let old_scan = read_at - std::time::Duration::from_secs(1);
        for (path, branch, scanned_at) in [
            (newer_path.as_str(), "main", new_scan),
            (branch_path.as_str(), "old-branch", old_scan),
        ] {
            crate::hydrate::glyph_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    path.to_string(),
                    (
                        crate::glyph_types::GlyphRow {
                            dirty: true,
                            ahead: 4,
                            behind: 6,
                            branch: Some(branch.into()),
                            repo_root: "/repo".into(),
                            add: thegn_svc::git::DiffCount::Exact(3),
                            del: thegn_svc::git::DiffCount::Exact(2),
                            branch_diff: None,
                            submodule_dirty: false,
                        },
                        scanned_at,
                    ),
                );
        }
        let checkouts = vec![
            (newer_path.clone(), Some("main".into())),
            (branch_path.clone(), Some("main".into())),
        ];
        let branches = vec![thegn_core::gitrefs::BranchInfo {
            name: "main".into(),
            is_head: true,
            upstream: Some("origin/main".into()),
            ahead: 2,
            behind: 7,
            upstream_gone: false,
            sha: "abc".into(),
            date: 0,
            subject: String::new(),
        }];

        refresh_badges(&checkouts, &branches, read_at);

        let cache = crate::hydrate::glyph_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(!cache.contains_key(&newer_path));
        assert!(!cache.contains_key(&branch_path));
    }

    #[test]
    fn fetch_outcome_refreshes_only_known_ref_moves_after_failure() {
        let cases = [
            (true, Some(1), Some(1), FetchOutcome::Unchanged),
            (true, Some(1), Some(2), FetchOutcome::Changed),
            (true, None, Some(2), FetchOutcome::Changed),
            (true, Some(1), None, FetchOutcome::Changed),
            (false, None, None, FetchOutcome::NotRun),
            (false, Some(1), Some(1), FetchOutcome::NotRun),
            (false, Some(1), Some(2), FetchOutcome::Changed),
        ];
        for (ok, before, after, expected) in cases {
            assert_eq!(fetch_outcome(ok, before, after), expected);
        }
    }

    #[test]
    fn enormous_fetch_floor_cannot_narrow_into_an_immediate_retry() {
        assert!(!due(Some(1), 1000, u64::MAX));
        assert!(!due(Some(i64::MAX), i64::MIN, 1));
        assert!(due(Some(1000), 1000, 0));
    }

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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
        let b = crate::git_handle::get()
            .branches_full(loc)
            .ok()?
            .into_iter()
            .find(|b| b.name == branch)?;
        Some((b.upstream?, b.behind))
    }

    #[test]
    fn fetch_moves_the_behind_count_a_stale_clone_could_not_see() {
        let fixture = tempfile::tempdir().unwrap();
        let origin = fixture.path().join("origin");
        let clone = fixture.path().join("clone");
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
            vec![(wt.clone(), Some("main".to_string()))],
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
        let git_spawns_before_fetch = thegn_core::util::core_git_spawn_count();
        run_fetch(&loc, "origin").unwrap();
        assert_eq!(
            thegn_core::util::core_git_spawn_count() - git_spawns_before_fetch,
            1,
            "remote_poll fetch must contribute exactly one git spawn"
        );
        let (upstream, behind) = tracked(&loc, "main").unwrap();
        assert_eq!(behind, 1);

        // The targeted ref read patches only the counts and preserves the
        // cached status row and its timestamp, so hydration does not full-scan.
        let scanned_at = std::time::Instant::now() - std::time::Duration::from_secs(1);
        crate::hydrate::glyph_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                wt.clone(),
                (
                    crate::glyph_types::GlyphRow {
                        dirty: false,
                        ahead: 0,
                        behind: 0,
                        branch: Some("main".into()),
                        repo_root: String::new(),
                        add: thegn_svc::git::DiffCount::Exact(0),
                        del: thegn_svc::git::DiffCount::Exact(0),
                        branch_diff: None,
                        submodule_dirty: false,
                    },
                    scanned_at,
                ),
            );
        let read_at = std::time::Instant::now();
        let branches = crate::git_handle::get().branches_full(&loc).unwrap();
        refresh_badges(&session_checkouts(&loc, &session), &branches, read_at);
        let cache = crate::hydrate::glyph_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (row, timestamp) = cache.get(&wt).unwrap();
        assert_eq!((row.ahead, row.behind), (0, 1));
        assert_eq!(*timestamp, scanned_at);
        assert!(!crate::hydrate::should_rescan_glyphs(
            Some(std::time::Instant::now().saturating_duration_since(*timestamp)),
            std::time::Duration::from_secs(120),
        ));
        drop(cache);

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
        let bare = fixture.path().join("solo");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "-b", "main"]);
        assert_eq!(remote_for(&GitLoc::Local(bare.clone())), None);
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
