//! Startup session load + background model hydration: resurrect the persisted
//! tab list, paint a cheap first frame, then rebuild the full sidebar/panel
//! model (git status, PR cache) on worker threads — with the refresh ticker
//! and the per-worktree diff fs-watcher pulsing the loop to repaint.

use std::path::Path;
use std::time::{Duration, Instant};

use notify::{Event, RecursiveMode, Watcher, recommended_watcher};
use tokio::sync::mpsc as tokio_mpsc;
use tokio::task;

use termwiz::terminal::TerminalWaker;

use crate::chrome::{FrameModel, LoadStep};
use crate::run::now_secs;

/// Default for [`model_refresh_interval`]. Matches `bg_glyph_ttl`'s 5s default
/// (the ticker's only job is refreshing background glyphs + the activity FSM);
/// must stay a multiple of the 500ms base that divides `PR_REFRESH_INTERVAL` so
/// the ticker keeps emitting `RefreshKind::Pr` (see the cadence-invariant test).
const DEFAULT_MODEL_REFRESH_MS: u64 = 5000;

/// Safety-net cadence for the background model re-hydration ticker. The *active*
/// worktree's panel + git glyphs already update in real time off the diff
/// fs-watcher (`retarget_diff_watcher`), so this tick exists only to (a) refresh
/// *background* worktrees' sidebar glyphs — themselves capped to the
/// `bg_glyph_ttl` (5s) staleness window, so ticking faster does no extra git
/// work — and (b) advance the activity-dot FSM (`activity::poll_and_save`, which
/// is wall-normalized and so stays correct at any cadence; dots just react up to
/// one tick later). The default therefore matches that 5s TTL.
///
/// It was 1s, which rebuilt the whole model — a ~0.3-0.4s `git` fan-out — every
/// second even when fully idle. `FrameModel::hydration_eq` drops the idle
/// *frame*, but NOT the wasted *build CPU*; on this thread that redundant rebuild
/// was the dominant idle/agent-active hydration cost. Override with
/// `SUPERZEJ_MODEL_REFRESH_MS` (lower = snappier dots/glyphs, more background git
/// work). Clamped to a multiple of the 500ms ticker base, min 500ms.
fn model_refresh_interval() -> Duration {
    let ms = std::env::var("SUPERZEJ_MODEL_REFRESH_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MODEL_REFRESH_MS)
        .max(500);
    Duration::from_millis((ms / 500) * 500)
}
const PR_REFRESH_INTERVAL: Duration = Duration::from_secs(20);
const ISSUE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Cached git-glyph row for one worktree: `(dirty, ahead, behind, branch,
/// repo_root)`. Computing it runs a full `git status` (50-150ms), so only the
/// *active* worktree pays that every Model tick; background worktrees reuse the
/// last value until it goes stale (see [`should_rescan_glyphs`]).
pub(crate) type GlyphRow = (bool, usize, usize, Option<String>, String);

/// Process-global staleness cache for background-worktree git glyphs. Mirrors
/// the global-state pattern of the sibling `activity` subsystem, so it needs no
/// threading through `spawn_model_hydration`'s ~dozen call sites. The `Mutex`
/// covers the (rare) case of overlapping hydrations; it's just a cache, so a
/// racing miss only costs a redundant scan.
fn glyph_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, (GlyphRow, Instant)>>
{
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (GlyphRow, Instant)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Staleness window for background-worktree git glyphs. The active worktree is
/// always rescanned; others reuse the cache until this elapses. Default 5s,
/// override with `SUPERZEJ_BG_MODEL_REFRESH_MS` (`0` = always rescan, i.e. the
/// old every-worktree-every-tick behavior).
fn bg_glyph_ttl() -> Duration {
    let ms = std::env::var("SUPERZEJ_BG_MODEL_REFRESH_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5000);
    Duration::from_millis(ms)
}

/// Decide whether a worktree's git glyphs must be rescanned now, or can be
/// served from cache. Pure, so it's unit-tested. The active worktree always
/// rescans (the user is looking at it, and its diff fs-watcher already forces
/// immediate refreshes); a background worktree rescans only when it has no
/// cached row yet or the cached row is older than `ttl`.
pub(crate) fn should_rescan_glyphs(
    is_active: bool,
    cached_age: Option<Duration>,
    ttl: Duration,
) -> bool {
    if is_active {
        return true;
    }
    match cached_age {
        None => true,
        Some(age) => age >= ttl,
    }
}

/// Merge a freshly-attempted git scan against the worktree's last-known-good
/// row. Pure, so it's unit-tested. A live `gix` read (dirty / ahead-behind /
/// branch) can return `Err` when it races a concurrent `.git` mutation — the
/// user committing/fetching in the pane, or hydration's own index rewrite. That
/// transient failure must NOT collapse a real glyph to zero/clean; each errored
/// field reuses the prior cached value instead. A genuine `Ok(None)` from
/// `ahead_behind` (no upstream configured) is the real "no arrows" state and is
/// kept as `(0, 0)`. The returned `bool` is `true` only when every read
/// succeeded — a degraded row must not overwrite the cache (else it would poison
/// background reuse for up to the TTL). `Err` is modelled as `()` so the helper
/// stays free of the git backend's error type.
#[allow(clippy::type_complexity)]
pub(crate) fn merge_glyph_scan(
    prior: Option<&GlyphRow>,
    dirty: std::result::Result<bool, ()>,
    ahead_behind: std::result::Result<Option<(usize, usize)>, ()>,
    branch: std::result::Result<Option<String>, ()>,
    repo_root: String,
) -> (GlyphRow, bool) {
    let mut clean = true;
    let dirty = match dirty {
        Ok(d) => d,
        Err(()) => {
            clean = false;
            prior.map(|p| p.0).unwrap_or(false)
        }
    };
    let (ahead, behind) = match ahead_behind {
        Ok(Some((a, b))) => (a, b),
        Ok(None) => (0, 0),
        Err(()) => {
            clean = false;
            prior.map(|p| (p.1, p.2)).unwrap_or((0, 0))
        }
    };
    let branch = match branch {
        Ok(b) => b,
        Err(()) => {
            clean = false;
            prior.and_then(|p| p.3.clone())
        }
    };
    ((dirty, ahead, behind, branch, repo_root), clean)
}

/// A refresh request delivered to the event loop. `Model` rehydrates the
/// sidebar/panel/diff (cheap, gix-backed, off-thread); `Pr` additionally kicks
/// the GitHub PR-cache refresh; `Issues` kicks the issue-tracker cache refresh.
/// All arrive event-driven (worktree fs-watch, tab switch) and on low-frequency
/// safety-net intervals.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RefreshKind {
    Model,
    Pr,
    Issues,
    /// CI run-history cache refresh (AV group) — same cadence as `Pr`.
    Ci,
    /// Per-worktree disk-size scan (off-loop `du`, cached in the DB). Slow, so
    /// it runs on a long cadence and the scan itself coalesces by `fetched_at`.
    Disk,
}

const CONTAINER_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Disk-scan tick cadence. The scan is `du`-heavy, so this is a coarse backstop
/// (the per-worktree scan further skips entries refreshed within the configured
/// `[disk].scan_interval_secs`). A whole multiple of the 500ms half-tick.
const DISK_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Background ticker: emits a `Model` refresh every [`model_refresh_interval`]
/// and a `Pr` refresh every `PR_REFRESH_INTERVAL`, pulsing the waker so an idle loop
/// wakes to service it. This is the staleness backstop; fs-watch + on-switch
/// refresh handle the common, latency-sensitive cases.
///
/// Also refreshes the container list on a 5s cadence (sent on `container_tx`),
/// keeping the sandbox panel live without blocking the hydration path.
///
/// Runs on a dedicated OS thread (not `tokio::spawn`) so it can never be starved
/// by the main loop blocking a runtime worker in `poll_input(None)` — true even
/// on a single-core runtime. The thread sleeps in 500ms half-ticks: fine enough
/// for the Telemetry section's live graphs (`stats_live` set while it's open)
/// while the model/PR cadences (default 1s/20s, model tunable via
/// `SUPERZEJ_MODEL_REFRESH_MS`) stay whole multiples of the half-tick.
pub(crate) fn spawn_refresh_ticker(
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    stats_tx: tokio_mpsc::UnboundedSender<superzej_metrics::StatsSnapshot>,
    container_tx: tokio_mpsc::UnboundedSender<Vec<superzej_core::sandbox::ContainerInfo>>,
    stats_interval_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stats_live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    disk_path: std::path::PathBuf,
    waker: TerminalWaker,
) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let tick = Duration::from_millis(500);
        let model_every = (model_refresh_interval().as_millis() as u64 / 500).max(1);
        let pr_every = PR_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let issue_every = ISSUE_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let container_every = CONTAINER_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let disk_every = DISK_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let mut ticks: u64 = 0;
        // System stats for the top bar ride the same thread/cadence — the
        // /proc reads never touch the event loop.
        let mut sampler = superzej_metrics::StatsSampler::new(disk_path);
        let _ = stats_tx.send(sampler.sample()); // prime counters for rate deltas
        let mut last_stats = Instant::now();
        loop {
            std::thread::sleep(tick);
            ticks += 1;
            let mut wake = false;
            if ticks.is_multiple_of(model_every) {
                let kind = if ticks.is_multiple_of(pr_every) {
                    RefreshKind::Pr
                } else {
                    RefreshKind::Model
                };
                if tx.send(kind).is_err() {
                    break; // loop gone
                }
                wake = true;
            }
            // CI run-history rides the PR cadence (AV group).
            if ticks.is_multiple_of(pr_every) && tx.send(RefreshKind::Ci).is_err() {
                break;
            }
            if ticks.is_multiple_of(issue_every) {
                if tx.send(RefreshKind::Issues).is_err() {
                    break;
                }
                wake = true;
            }
            if ticks.is_multiple_of(disk_every) {
                if tx.send(RefreshKind::Disk).is_err() {
                    break;
                }
                wake = true;
            }
            // Live mode (telemetry layer open) samples every half-tick;
            // otherwise the user-cycled rate (1/2/5/10s) is honored.
            let interval =
                Duration::from_millis(stats_interval_ms.load(Ordering::Relaxed).max(500));
            if stats_live.load(Ordering::Relaxed) || last_stats.elapsed() >= interval {
                last_stats = Instant::now();
                let snap = {
                    let _g = crate::perf::measure(crate::perf::Subsys::Stats);
                    sampler.sample()
                };
                if stats_tx.send(snap).is_err() {
                    break;
                }
                wake = true;
            }
            // Container list refresh: runs OCI `ps` subprocesses, so keep it on
            // its own cadence (5s) rather than tying it to the fast stats tick.
            if ticks.is_multiple_of(container_every) {
                let containers = {
                    let _g = crate::perf::measure(crate::perf::Subsys::Container);
                    superzej_core::sandbox::running_containers()
                };
                if container_tx.send(containers).is_err() {
                    break;
                }
                wake = true;
            }
            if wake {
                let _ = waker.wake();
            }
        }
    });
}

/// Resurrect the persisted tab list, seeding a single Home tab for the current
/// worktree if the session is empty (and persisting it so the next launch
/// restores it). The native host owns this — it's the resurrect path that
/// replaced zellij's session serialization.
///
/// The `bool` is true when the session was freshly SEEDED (first launch / new
/// workspace) rather than resurrected — the launch splash shows only then.
pub(crate) fn load_or_seed_session(cwd: &std::path::Path) -> (crate::session::Session, bool) {
    let _span = tracing::info_span!("load_or_seed_session").entered();
    use crate::session::{GroupKind, Session, WorktreeGroup};

    let sess = superzej_core::db::session();
    let base = cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());

    let mut env_session = std::env::var("SUPERZEJ_SESSION").ok();
    if let Some(ref s) = env_session
        && s == "superzej"
    {
        // Ignore the old legacy default
        env_session = None;
    }

    let cwd_str = cwd.to_string_lossy().into_owned();
    // One DB handle for both the workspace lookup and the resurrect below —
    // every `open` re-runs pragmas + migration checks, so don't repeat it.
    // `XDG_STATE_HOME` selects the explicit DB in test/bench scenarios.
    let db = if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        let path = std::path::Path::new(&state_home).join("superzej/superzej.db");
        superzej_core::db::Db::open_at(&path)
    } else {
        superzej_core::db::Db::open()
    };

    // sj is directory-agnostic: the launch directory never selects (or
    // creates) a workspace. Resolution order:
    //   1. An inherited SUPERZEJ_SESSION (so child shells stay in the session).
    //   2. The explicit "active workspace" pointer — the workspace the user was
    //      actually in at the last switch — provided its dir still exists.
    //   3. The most-recently-active workspace by `workspaces()` (last_active
    //      DESC) as a fallback for pre-pointer state.
    //   4. A genuine first run (no env, no DB history) falls back to the cwd.
    // The pointer is separate from `last_active` on purpose: that column also
    // orders the sidebar tree, which must not reshuffle on every switch.
    let session_name = env_session
        .clone()
        .or_else(|| {
            db.as_ref().ok().and_then(|db| {
                db.active_workspace()
                    .ok()
                    .flatten()
                    .filter(|p| std::path::Path::new(p).is_dir())
            })
        })
        .or_else(|| {
            db.as_ref().ok().and_then(|db| {
                db.workspaces()
                    .ok()
                    .and_then(|ws| ws.into_iter().next())
                    .map(|w| w.repo_path)
            })
        })
        .unwrap_or(cwd_str);

    let Ok(db) = db else {
        // No DB — synthesize an ephemeral single-worktree session. Best-effort
        // slug (no DB to consult): the slugified basename matches what
        // `slug_for_repo` would assign absent a collision.
        let slug = {
            let s = superzej_core::util::slugify(&base);
            if s.is_empty() { "repo".to_string() } else { s }
        };
        return (
            Session {
                id: sess.to_string(),
                worktrees: vec![WorktreeGroup::new(
                    superzej_core::repo::home_tab(&slug),
                    GroupKind::Home,
                    cwd.to_string_lossy().into_owned(),
                )],
                active: 0,
            },
            true,
        );
    };

    let mut session = Session::resurrect(&db, &session_name).unwrap_or_default();

    // git is the source of truth for worktrees on disk: drop resurrected
    // groups whose local dir vanished (deleted/moved outside superzej), and
    // forget their registry rows so nothing re-adopts them. Remote worktrees
    // (a `location` in the registry) are exempt — their path isn't local.
    let remote: std::collections::HashSet<String> = db
        .worktrees()
        .map(|rows| {
            rows.into_iter()
                .filter(|w| !w.location.is_empty())
                .map(|w| w.worktree)
                .collect()
        })
        .unwrap_or_default();
    let active_name = session.active_group().map(|g| g.name.clone());
    let before = session.worktrees.len();
    let dead: Vec<crate::session::WorktreeGroup> = {
        let (live, dead) =
            session
                .worktrees
                .drain(..)
                .partition(|g: &crate::session::WorktreeGroup| {
                    g.path.is_empty() || remote.contains(&g.path) || Path::new(&g.path).is_dir()
                });
        session.worktrees = live;
        dead
    };
    if session.worktrees.len() != before {
        for g in &dead {
            let _ = db.del_worktree(&g.path);
        }
        session.active = active_name
            .and_then(|n| session.worktrees.iter().position(|g| g.name == n))
            .unwrap_or(0);
        let _ = session.persist(&db, &session_name, now_secs());
        tracing::info!(
            target: "szhost::startup",
            pruned = dead.len(),
            "stale worktrees pruned (dirs gone from disk)"
        );
    }

    let mut seeded = false;
    if session.worktrees.is_empty() {
        // Key the home group by the canonical DB slug (`{slug}/home`), never
        // the raw basename — the sidebar dedupes workspaces by this prefix.
        let slug = superzej_core::repo::repo_slug_with(&db, std::path::Path::new(&session_name));
        // Directory-agnostic: anchor the home group at the session's own path
        // (the resolved workspace), not the launch cwd.
        let home_path = if Path::new(&session_name).is_dir() {
            session_name.clone()
        } else {
            cwd.to_string_lossy().into_owned()
        };
        session.worktrees.push(WorktreeGroup::new(
            superzej_core::repo::home_tab(&slug),
            GroupKind::Home,
            home_path,
        ));
        session.active = 0;
        seeded = true;
        let _ = session.persist(&db, &session_name, now_secs());
    }
    session.id = session_name; // Need to add id to session
    // Register the resolved workspace so it survives switches: without a
    // `workspaces` row it exists only as a live fallback in `workspace_list`
    // (empty repo_path) and vanishes from the sidebar the moment another
    // workspace becomes active. Unconditional — it also self-heals installs
    // whose bootstrap workspace predates this registration. Safe upsert:
    // `put_workspace` assigns `position` (sidebar order) only on first insert.
    if Path::new(&session.id).is_dir() {
        let name = Path::new(&session.id)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".into());
        // A path that resolves to a git main-worktree is a "repo" workspace;
        // anything else is a plain "dir" workspace (mirrors switch_to_workspace).
        let kind = if superzej_core::repo::main_worktree(Path::new(&session.id)).is_some() {
            "repo"
        } else {
            "dir"
        };
        // best-effort: the DB is a cache; git is the source of truth
        let _ = db.put_workspace(&session.id, &name, kind);
        let _ = db.touch_repo(&session.id, &name);
    }
    // Record the resolved workspace as the active pointer so the next cold
    // start reopens it even on a first run (where no switch has happened yet).
    let _ = db.set_active_workspace(&session.id);
    (session, seeded)
}

pub(crate) fn active_tab_path(session: &crate::session::Session) -> std::path::PathBuf {
    session
        .active_group()
        .and_then(|g| {
            (!g.path.is_empty() && std::path::Path::new(&g.path).is_dir())
                .then(|| std::path::PathBuf::from(&g.path))
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| ".".into())
}

/// The worktree dirs immediately above and below the active one in the sidebar
/// order — the prefetch targets so moving to a neighbor is already warm. Skips
/// the active worktree, empties, and paths that no longer exist on disk.
pub(crate) fn neighbor_worktree_paths(
    session: &crate::session::Session,
) -> Vec<std::path::PathBuf> {
    let active = session.active;
    [active.wrapping_sub(1), active + 1]
        .into_iter()
        .filter(|&i| i != active)
        .filter_map(|i| session.worktrees.get(i))
        .filter(|g| !g.path.is_empty())
        .map(|g| std::path::PathBuf::from(&g.path))
        .filter(|p| p.is_dir())
        .collect()
}

/// The tabbar strip for the active worktree: (worktree label, tab chip titles,
/// active chip index).
pub(crate) fn tab_strip(session: &crate::session::Session) -> (String, Vec<String>, usize) {
    match session.active_group() {
        Some(g) => (
            g.name.clone(),
            g.tabs.iter().map(|t| t.title.clone()).collect(),
            g.active_tab,
        ),
        None => (String::new(), Vec::new(), 0),
    }
}

/// The ordered `(slug, display, kind)` workspace list backing the tree: every
/// workspace known to the DB (stable slug; `kind` = "repo" | "dir"), plus any
/// live tab's repo prefix not yet in the DB. The structured tree is then built
/// by [`crate::sidebar::build_rows`].
pub(crate) fn workspace_list(
    session: &crate::session::Session,
    db: Option<&superzej_core::db::Db>,
) -> Vec<(String, String, String, String)> {
    let mut db_backed: Vec<(String, String, String, String)> = Vec::new();
    if let Some(db) = db
        && let Ok(rows) = db.workspaces()
    {
        for w in rows {
            let display = if w.name.trim().is_empty() {
                std::path::Path::new(&w.repo_path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| w.repo_path.clone())
            } else {
                w.name.clone()
            };
            let base = superzej_core::util::slugify(&display);
            let slug = db
                .slug_for_repo(&w.repo_path, &base)
                .unwrap_or_else(|_| base.clone());
            if !db_backed.iter().any(|(s, _, _, _)| *s == slug) {
                db_backed.push((slug, display, w.kind.clone(), w.repo_path.clone()));
            }
        }
    }
    let mut live: Vec<(String, String, String, String)> = Vec::new();
    for g in &session.worktrees {
        if let Some((repo, _)) = crate::sidebar::split_tab(&g.name)
            && !live.iter().any(|(s, _, _, _)| *s == repo)
        {
            // Live worktrees always belong to a git repo workspace. The empty
            // repo_path marks this as a live fallback (no DB row yet).
            live.push((repo.clone(), repo, "repo".to_string(), String::new()));
        }
    }
    merge_workspace_lists(db_backed, live)
}

/// Merge DB-backed workspace entries (authoritative; order preserved) with
/// live-session fallback entries, keyed by canonical slug. Entries with an
/// empty `repo_path` in `db_backed` are live fallbacks from a previous merge —
/// they are dropped and re-derived from `live`, so a stale fallback (e.g. left
/// behind by a workspace switch) can never accumulate or duplicate.
pub(crate) fn merge_workspace_lists(
    db_backed: Vec<(String, String, String, String)>,
    live: Vec<(String, String, String, String)>,
) -> Vec<(String, String, String, String)> {
    let mut out = db_backed;
    out.retain(|(_, _, _, path)| !path.is_empty());
    for entry in live {
        if !out.iter().any(|(slug, _, _, _)| *slug == entry.0) {
            out.push(entry);
        }
    }
    out
}

/// Worktrees registered in the DB, ready for the sidebar's cross-workspace
/// rows: one entry per registry row whose dir still exists (or is remote).
pub(crate) fn db_worktree_list(db: &superzej_core::db::Db) -> Vec<crate::sidebar::DbWorktree> {
    let mut out = Vec::new();
    for w in db.worktrees().unwrap_or_default() {
        // git is the source of truth: a local registry row whose dir vanished
        // (deleted outside superzej) is dead — delete it here (we're on the
        // hydration thread) instead of merely hiding it, so deceased
        // worktrees stop resurfacing in the tree. Remote rows are exempt.
        if w.location.is_empty() && !std::path::Path::new(&w.worktree).is_dir() {
            let _ = db.del_worktree(&w.worktree);
            continue;
        }
        let Some((slug, branch)) = crate::sidebar::split_tab(&w.tab_name) else {
            continue;
        };
        out.push(crate::sidebar::DbWorktree {
            slug,
            branch,
            repo_path: w.repo_root.clone(),
            tab_name: w.tab_name.clone(),
            path: w.worktree.clone(),
            folder_id: w.folder_id,
            sandbox_backend: w.sandbox_backend.clone(),
            env_name: w.env_name.clone(),
        });
    }
    out
}

/// Gather per-worktree git/agent/activity status for every tab in the session.
/// Runs on the hydration thread (git can be slow); the event loop merges this
/// into the tree at render time. Also advances the activity FSM in-process.
/// Compute the entity-level summary of a worktree's pending changes from
/// `git diff HEAD` (semantic git layer). Runs on the hydration thread: one diff
/// subprocess + a tree-sitter parse per changed file. Capped at 50 files so a
/// sprawling change never balloons hydration; `None` when there's nothing to
/// show or git/parse yields no entities.
fn compute_entity_summary(
    loc: &superzej_core::remote::GitLoc,
    diff_entries: &[superzej_svc::git::DiffEntry],
) -> Option<superzej_core::semantic::EntitySummary> {
    use superzej_core::semantic::{EntitySummary, Lang, entities_for_diff};
    if diff_entries.is_empty() || diff_entries.len() > 50 {
        return None;
    }
    // Same sanitized flags the git backend uses (see svc SANITIZED_DIFF) so the
    // patch parses cleanly: no color/ext-diff/renames, 3 lines of context.
    let diff = loc.git_out(&[
        "-c",
        "diff.noprefix=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-renames",
        "-U3",
        "HEAD",
    ])?;
    let root = loc.path();
    let mut per_file = Vec::new();
    for f in superzej_core::patch::parse_patch(&diff) {
        let Some(lang) = Lang::from_path(&f.new_path) else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(std::path::Path::new(&root).join(&f.new_path)) else {
            continue;
        };
        let changes = entities_for_diff(&src, lang, &f.hunks);
        if !changes.is_empty() {
            per_file.push((f.new_path.clone(), changes));
        }
    }
    let summary = EntitySummary::new(per_file);
    (!summary.per_file.is_empty()).then_some(summary)
}

fn collect_sidebar_status(
    session: &crate::session::Session,
    db: &superzej_core::db::Db,
    alert_kinds: &[&str],
    counted_kinds: &[&str],
    // The budget-governed warm/lifecycle policy: reconciles the warm set (drops
    // idle bridges so sandboxes suspend) and gates remote git-glyph scans so a
    // suspended sandbox is never woken just to refresh the sidebar.
    lifecycle: &superzej_core::config::LifecycleConfig,
) -> crate::sidebar::SidebarStatus {
    use superzej_core::remote::GitLoc;
    use superzej_svc::git::{GitBackend, GixGit};
    let mut status = crate::sidebar::SidebarStatus::default();
    let t0 = std::time::Instant::now();

    // Advance the activity state machine over ALL registered worktrees,
    // then read the fresh states (keyed by tab name). This keeps background
    // agents in other workspaces ticking.
    let mut managed_map = std::collections::BTreeMap::new();
    if let Ok(db_wts) = db.worktrees() {
        for wt in db_wts {
            if !wt.worktree.is_empty() {
                managed_map.insert(
                    wt.worktree.clone(),
                    superzej_core::activity::ManagedWorktree {
                        worktree: wt.worktree.clone(),
                        tab: wt.tab_name.clone(),
                    },
                );
            }
        }
    }
    // Overlay the active session (might have unpersisted fresh worktrees)
    for g in &session.worktrees {
        if !g.path.is_empty() {
            managed_map.insert(
                g.path.clone(),
                superzej_core::activity::ManagedWorktree {
                    worktree: g.path.clone(),
                    tab: g.name.clone(),
                },
            );
        }
    }
    let managed: Vec<_> = managed_map.into_values().collect();
    // Remote/provider worktrees: their processes run in the env, not on this
    // host, so the local /proc scan never sees them. For each that has a live
    // resident bridge, fetch its in-env jiffies via `proc.list` and inject them
    // (authoritative, overriding the local scan). Blocking RPC is fine — this is
    // the hydration thread, never the loop. Empty (zero behaviour change) when
    // no worktree is remote / no bridge is connected.
    let mut activity_extra = std::collections::BTreeMap::new();
    for w in &managed {
        let loc = GitLoc::for_worktree(std::path::Path::new(&w.worktree));
        if !loc.is_remote() {
            continue;
        }
        if let Some(bridge) = superzej_svc::bridge::for_loc(&loc) {
            let workdir = loc.path();
            if let Ok(m) = bridge.proc_list(std::slice::from_ref(&workdir)) {
                activity_extra.insert(w.worktree.clone(), m.get(&workdir).copied().unwrap_or(0));
            }
        }
    }
    superzej_core::activity::poll_and_save_with(&managed, &activity_extra);
    status.activity = superzej_core::activity::read_states()
        .into_iter()
        .map(|(tab, st)| (tab, crate::sidebar::ActivityState::from_str(&st)))
        .collect();

    // Reconcile the warm set now (after fresh activity): drop resident bridges for
    // idle, over-budget remote sandboxes so they suspend — BEFORE the glyph scan
    // below, so the just-suspended ones serve cache instead of being woken.
    crate::lifecycle::reconcile(session, lifecycle);
    let gate_remote_scans = lifecycle.enabled && lifecycle.serve_cached_glyphs;

    // Badge counts (item 28): unread + alert notifications grouped by worktree.
    status.unread_counts = db
        .get_unread_counts_by_worktree(counted_kinds)
        .unwrap_or_default();
    status.alert_counts = db
        .get_alert_counts_by_worktree(alert_kinds)
        .unwrap_or_default();
    // Per-worktree disk sizes from the off-loop scan's cache (pure DB read).
    status.disk_sizes = db.all_worktree_disk().unwrap_or_default();

    // Populate agent and PR badges for ALL registered worktrees from the DB.
    // This ensures non-session workspaces still show their agent/PR status
    // when they are rendered as collapsed/switchable sidebar rows.
    if let Ok(db_wts) = db.worktrees() {
        for wt in db_wts {
            if !wt.agent.is_empty() {
                status.agent.insert(wt.worktree.clone(), wt.agent.clone());
            }
            if !wt.branch.is_empty()
                && !wt.repo_root.is_empty()
                && let Ok(counts) = db.get_open_pr_counts_by_branch(&wt.repo_root)
                && let Some(&n) = counts.get(&wt.branch)
                && n > 0
            {
                status.pr_counts.insert(wt.worktree.clone(), n);
            }
        }
    }

    // git glyphs + agent + PR badge per distinct worktree path. `is_dirty` does a
    // full `git status` scan (50-150ms), so scanning every worktree every Model
    // tick was the dominant hydration cost (cpu_hydrate scaled with worktree
    // count). Tier it: the *active* worktree always rescans (and its diff
    // fs-watcher forces immediate refreshes), while background worktrees reuse a
    // cached glyph row until it goes stale. The remaining scans still fan out
    // across scoped threads; DB-keyed inserts (agent, PR counts) stay on this
    // thread since `Db` isn't `Send`.
    let mut seen = std::collections::HashSet::new();
    let paths: Vec<String> = session
        .worktrees
        .iter()
        .filter(|g| !g.path.is_empty())
        .map(|g| g.path.clone())
        .filter(|p| seen.insert(p.clone()) && std::path::Path::new(p).is_dir())
        .collect();

    // Partition into paths that must be rescanned now vs. served from cache.
    let active_path: Option<String> = session.active_group().map(|g| g.path.clone());
    let ttl = bg_glyph_ttl();
    let now = Instant::now();
    let mut to_scan: Vec<String> = Vec::new();
    let mut reused: Vec<(String, GlyphRow)> = Vec::new();
    // Last-known-good rows for the paths we're about to rescan, so a scan that
    // hits a transient gix error can reuse the prior value instead of dropping
    // the glyph to zero/clean (see `merge_glyph_scan`).
    let mut prior_for_scan: std::collections::HashMap<String, GlyphRow> =
        std::collections::HashMap::new();
    {
        let cache = glyph_cache().lock().unwrap();
        for p in &paths {
            let is_active = active_path.as_deref() == Some(p.as_str());
            let cached = cache.get(p);
            // Budget gate: never wake a suspended provider sandbox just to refresh
            // the sidebar. A remote worktree that isn't active and has no live
            // bridge is suspended — serve its last-known glyphs (or a placeholder)
            // rather than running an in-sandbox `git status` that wakes it. The
            // active worktree (and any warm one) still live-scans.
            if gate_remote_scans {
                let loc = GitLoc::for_worktree(std::path::Path::new(p));
                let is_remote = loc.is_remote();
                let warm = is_remote && superzej_svc::bridge::for_loc(&loc).is_some();
                if !superzej_core::lifecycle::should_live_scan(is_remote, warm, is_active) {
                    let row = cached.map(|(row, _)| row.clone()).unwrap_or((
                        false,
                        0,
                        0,
                        None,
                        String::new(),
                    ));
                    reused.push((p.clone(), row));
                    continue;
                }
            }
            let age = cached.map(|(_, ts)| now.saturating_duration_since(*ts));
            if should_rescan_glyphs(is_active, age, ttl) {
                if let Some((row, _)) = cached {
                    prior_for_scan.insert(p.clone(), row.clone());
                }
                to_scan.push(p.clone());
            } else if let Some((row, _)) = cached {
                reused.push((p.clone(), row.clone()));
            } else {
                to_scan.push(p.clone());
            }
        }
    }

    // (path, GlyphRow, clean) — git only, no DB access in the scope. `repo_root`
    // is the main-worktree root shared by every linked worktree of the repo; it
    // keys the repo-wide `pr_branch_cache` (item 28). `clean` is false when any
    // read errored (and reused its prior value) — those rows must not overwrite
    // the cache. See `merge_glyph_scan`.
    let prior_for_scan = &prior_for_scan;
    let scanned: Vec<(String, GlyphRow, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = to_scan
            .iter()
            .map(|p| {
                s.spawn(move || {
                    let wt = std::path::Path::new(p);
                    let loc = GitLoc::for_worktree(wt);
                    let git = GixGit::new();
                    let dirty = git.is_dirty(&loc).map_err(|_| ());
                    let ahead_behind = git.ahead_behind(&loc).map_err(|_| ());
                    let branch = git.current_branch(&loc).map(Some).map_err(|_| ());
                    let repo_root = superzej_core::repo::main_worktree(wt)
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.clone());
                    let (row, clean) = merge_glyph_scan(
                        prior_for_scan.get(p),
                        dirty,
                        ahead_behind,
                        branch,
                        repo_root,
                    );
                    (p.clone(), row, clean)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Refresh the cache with the fresh rows and drop entries for worktrees that
    // are no longer present (bounds growth across the process lifetime). A
    // degraded row (a transient read error that reused its prior value) is left
    // out so the existing cache entry is preserved rather than poisoned.
    {
        let mut cache = glyph_cache().lock().unwrap();
        for (p, row, clean) in &scanned {
            if *clean {
                cache.insert(p.clone(), (row.clone(), now));
            }
        }
        cache.retain(|k, _| paths.iter().any(|p| p == k));
    }

    let scanned_n = scanned.len();
    let git_rows = scanned
        .into_iter()
        .map(|(p, row, _clean)| (p, row))
        .chain(reused)
        .map(|(p, (dirty, ahead, behind, branch, repo_root))| {
            (p, dirty, ahead, behind, branch, repo_root)
        });
    for (path, dirty, ahead, behind, branch, repo_root) in git_rows {
        status.git.insert(
            path.clone(),
            crate::sidebar::GitGlyphs {
                dirty,
                ahead,
                behind,
            },
        );
        if let Ok(Some(agent)) = db.worktree_agent(&path) {
            status.agent.insert(path.clone(), agent);
        }
        // PR badge: open PRs for this worktree's current branch, joined from the
        // repo-wide `pr_branch_cache` (keyed by repo root, so every worktree of
        // the repo — not just the active one — resolves its branch's count).
        if let Some(branch) = branch
            && let Ok(counts) = db.get_open_pr_counts_by_branch(&repo_root)
            && let Some(&n) = counts.get(&branch)
            && n > 0
        {
            status.pr_counts.insert(path.clone(), n);
        }
    }
    tracing::debug!(
        target: "szhost::hydrate",
        status_ms = t0.elapsed().as_millis() as u64,
        worktrees = paths.len(),
        scanned = scanned_n,
        cached = paths.len().saturating_sub(scanned_n),
        "sidebar status collected"
    );
    status
}

/// tokei line count for `path`, cached in `loc_cache` (hydration thread —
/// tokei walks the whole tree). Stale cache (>5 min) refreshes in place;
/// missing tokei yields `None` and the widget hides.
fn worktree_loc(
    db: &superzej_core::db::Db,
    path: &std::path::Path,
) -> Option<superzej_core::loc::LocReport> {
    use superzej_core::loc::LocReport;
    const TTL_SECS: i64 = 300;
    let key = path.to_string_lossy().into_owned();
    if let Ok(Some((json, fetched_at))) = db.get_loc_cache_entry(&key)
        && now_secs() - fetched_at < TTL_SECS
        && let Ok(report) = serde_json::from_str::<LocReport>(&json)
    {
        return Some(report);
    }
    let report = crate::loc_scan::scan(path);
    if let Ok(json) = serde_json::to_string(&report) {
        let _ = db.put_loc_cache(&key, report.total_code, &json);
    }
    Some(report)
}

/// A cheap first-frame model: no git, no diff, no DB recents. It gives the
/// user immediate chrome/status while the expensive model hydrates in the
/// background. Sidebar workspaces are populated from the already-loaded session
/// (no DB, no git) so the tree is non-blank on frame 1.
/// Build the cheap first frame. Pass the already-open `db` from
/// `load_or_seed_session` so the sidebar workspace list is populated from
/// the DB on the very first frame — no waiting for the hydration worker.
pub(crate) fn build_initial_model(
    session: &crate::session::Session,
    db: Option<&superzej_core::db::Db>,
) -> FrameModel {
    let active_name = session
        .active_group()
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "workspace/home".into());
    let cwd = active_tab_path(session);
    let (worktree, tabs, active_tab) = tab_strip(session);
    // Use the DB if available (it's already open from load_or_seed_session)
    // so the sidebar shows all registered workspaces on the very first frame
    // instead of only the live session entries.
    let sidebar_workspaces = workspace_list(session, db);
    FrameModel {
        worktree,
        tabs,
        active_tab,
        sidebar_workspaces,
        active_container_name: superzej_core::sandbox::container_name(&cwd.to_string_lossy()),
        panel: crate::panel::PanelData {
            branch: active_name,
            ..Default::default()
        },
        panel_focused: false,
        status: format!(
            "Starting szhost (build: {})… panes usable while git status hydrates",
            env!("SZHOST_BUILD_TIME")
        ),
        load_steps: vec![
            LoadStep::pending("sandbox"),
            LoadStep::pending("container"),
            LoadStep::pending("shell"),
        ],
        accent: superzej_core::theme::TEAL.to_string(),
        ..Default::default()
    }
}

/// What the open panel needs from this hydration pass — lets `build_model`
/// skip work for closed sections (the git log, the file count).
#[derive(Debug, Clone, Default)]
pub(crate) struct HydrateHints {
    pub open: crate::panel::Section,
    pub expanded: bool,
    /// Active profile slug for per-profile container naming (empty = default).
    pub profile: String,
}

impl HydrateHints {
    fn wants_commits(&self) -> bool {
        self.open == crate::panel::Section::Commits || (self.expanded && self.open.is_git_family())
    }

    fn visible_commit_limit(&self) -> usize {
        if self.expanded { 80 } else { 20 }
    }
}

// Short TTL: the Commits list is only built while a commits / expanded-git
// section is on screen, and a `git log -80` is cheap, so a tight window keeps
// the list close behind pane-driven commits without re-running git every wake.
// (Working-tree fields refresh every tick already; commits had lagged a further
// 30s on top — the most visible half of the "panel out of sync" report.)
const COMMIT_CACHE_TTL_SECS: i64 = 3;

fn commit_cache_needs_refresh(cache: Option<&(String, i64)>) -> bool {
    let Some((json, fetched_at)) = cache else {
        return true;
    };
    serde_json::from_str::<Vec<crate::panel::CommitRow>>(json).is_err()
        || superzej_core::util::now().saturating_sub(*fetched_at) >= COMMIT_CACHE_TTL_SECS
}

fn refresh_commit_cache(db: &superzej_core::db::Db, session: &crate::session::Session) -> bool {
    use superzej_core::remote::GitLoc;
    use superzej_svc::git::{CliGit, GitBackend};

    let cwd = active_tab_path(session);
    if !cwd.is_dir() {
        return false;
    }
    let loc = GitLoc::for_worktree(&cwd);
    let Ok(rows) = CliGit.log_commits(&loc, 80) else {
        return false;
    };
    let rows: Vec<crate::panel::CommitRow> = rows
        .into_iter()
        .map(|c| crate::panel::CommitRow {
            sha: c.sha,
            short: c.short,
            subject: c.subject,
            author: c.author,
            date: c.date,
            refs: c.refs,
            parents: c.parents,
        })
        .collect();
    serde_json::to_string(&rows)
        .ok()
        .and_then(|json| db.put_commit_cache(&loc.path(), &json).ok())
        .is_some()
}

/// Map the typed PR cache into the panel's pr/checks/threads/issues fields.
fn apply_pr_cache(panel: &mut crate::panel::PanelData, cached: superzej_core::github::PrPanel) {
    use superzej_core::github::{Bucket, PanelState, check_bucket};
    let now = superzej_core::util::now();
    match cached.state {
        PanelState::Pr(pr) => {
            panel.pr = Some(crate::panel::PrSummary {
                number: pr.number,
                title: pr.title.clone(),
                state: pr.state.clone(),
                url: pr.url.clone(),
                is_draft: pr.is_draft,
                review_decision: pr.review_decision.clone(),
            });
            panel.pr_base = pr.base_ref_name.clone();
            panel.checks = pr
                .status_check_rollup
                .iter()
                .map(|c| crate::panel::CheckLine {
                    name: c.name.clone(),
                    state: match check_bucket(c) {
                        Bucket::Pass => crate::panel::CheckState::Pass,
                        Bucket::Fail => crate::panel::CheckState::Fail,
                        Bucket::Pending => crate::panel::CheckState::Pending,
                    },
                    duration_secs: c.duration_secs(now),
                    details_url: c.details_url.clone(),
                })
                .collect();
        }
        PanelState::NoGh => panel.pr_note = Some("gh CLI not installed".into()),
        PanelState::NotAuthenticated => panel.pr_note = Some("gh not authenticated".into()),
        PanelState::NoPr => panel.pr_note = Some("no pull request".into()),
        PanelState::RateLimited => panel.pr_note = Some("GitHub rate limited".into()),
        PanelState::Offline => panel.pr_note = Some("GitHub unreachable".into()),
        PanelState::Error { message } => panel.pr_note = Some(message),
    }
    panel.threads = cached.threads;
    panel.issues = cached.issues;
}

/// The header's "resolved X/Y" denominator: the first-seen unresolved count
/// of the current merge, persisted per worktree and cleared when the merge
/// ends. `None` (no bar) until a count is known.
fn merge_total(
    db: &superzej_core::db::Db,
    worktree: &str,
    in_merge: bool,
    unresolved: usize,
) -> Option<usize> {
    let key = format!("merge_total:{worktree}");
    if !in_merge {
        let _ = db.set_ui_state("panel", &key, "");
        return None;
    }
    let stored = db
        .get_ui_state("panel", &key)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<usize>().ok());
    match stored {
        Some(total) if total >= unresolved.max(1) => Some(total),
        _ if unresolved > 0 => {
            let _ = db.set_ui_state("panel", &key, &unresolved.to_string());
            Some(unresolved)
        }
        other => other,
    }
}

/// Build the chrome model from the resurrected session + the current worktree's
/// git state (best-effort — the host stays up even with no repo / no DB). This
/// is the in-process data flow the chrome relies on: read core + svc directly,
/// no IPC. This can be slow on large repos, so launch calls it on a background
/// worker after the first frame is already possible.
pub(crate) fn build_model(
    session: &crate::session::Session,
    db: &superzej_core::db::Db,
    hints: HydrateHints,
) -> FrameModel {
    use superzej_core::remote::GitLoc;

    let t0 = std::time::Instant::now();
    let cwd = active_tab_path(session);
    let loc = GitLoc::for_worktree(&cwd);
    // Record the active worktree's log tag so the Logs section can filter the
    // shared szhost.log tail to this worktree's + host-global lines by default.
    crate::panel::scope::set_active_wt_tag(&superzej_core::log_trace::wt_slug(&cwd));

    // Single layered-config load reused for notification priority + tasks below.
    let app_cfg = superzej_core::config::Config::try_load_layered(
        &superzej_core::config::ProcessEnv,
        &[],
        None,
    )
    .unwrap_or_default();
    let alert_kinds = app_cfg.notifications.alert_kind_names();
    let counted_kinds = app_cfg.notifications.counted_unread_kind_names();

    let sidebar_workspaces = workspace_list(session, Some(db));
    // Folders for every workspace shown in the sidebar (not just the active
    // tab's): the sidebar filters this list per-workspace by `repo_path`, so a
    // worktree filed into a folder stays visible whichever tab is active.
    let sidebar_db_folders: Vec<superzej_core::models::FolderRow> = sidebar_workspaces
        .iter()
        .filter(|(_, _, _, repo)| !repo.is_empty())
        .flat_map(|(_, _, _, repo)| db.folders_for_workspace(repo).unwrap_or_default())
        .collect();
    let sidebar_db_worktrees = db_worktree_list(db);
    let sidebar_db_terminals = db.terminals().unwrap_or_default();
    // One-shot at process start: collapse any stale running/active activity dot
    // (a session killed mid-run) to a settled state before the sidebar first
    // paints, so a phantom forever-running dot never survives resurrection. The
    // live FSM re-derives the true state from fresh CPU deltas on the next poll.
    {
        use std::sync::Once;
        static RESTORE_COERCE: Once = Once::new();
        RESTORE_COERCE.call_once(|| {
            let grace_ms = app_cfg.session.restore_grace_secs.saturating_mul(1000);
            superzej_core::activity::coerce_stale_states(grace_ms);
        });
    }
    let sidebar_status = collect_sidebar_status(
        session,
        db,
        &alert_kinds,
        &counted_kinds,
        &app_cfg.lifecycle,
    );
    let loc_count = worktree_loc(db, &cwd);

    // Terse placement kind (ssh/mosh/k8s/<provider>) for the active worktree, for
    // the center tab bar. Resolved from config (pure, no I/O); `None` when local.
    // Reuse the canonical repo_root from the sidebar list so the kind matches the
    // full label shown in the sidebar detail line.
    let active_path = loc.path();
    let active_repo = sidebar_db_worktrees
        .iter()
        .find(|w| w.path == active_path)
        .map(|w| w.repo_path.clone())
        .unwrap_or_else(|| active_path.clone());
    let active_env = app_cfg.resolve_env(
        std::path::Path::new(&active_repo),
        &loc,
        std::path::Path::new(&active_path),
        db.worktree_env(&active_path).ok().flatten().as_deref(),
    );
    let active_placement_kind =
        (!active_env.placement.is_local()).then(|| active_env.placement.kind());
    let active_placement_label =
        (!active_env.placement.is_local()).then(|| active_env.placement.label());

    let panel = build_panel(&cwd, db, &hints, &app_cfg);

    // Decorate the tab-bar placement chip with the backing host's readiness
    // (hosts-as-resources): `[ssh]` stays clean when the host is ready,
    // `[ssh ~<step>]` mid-provision, `[ssh !]` when it failed.
    let active_placement_kind = active_placement_kind.map(|kind| {
        let status = crate::host_ui::env_host_status(&app_cfg, &active_env.name, &panel.hosts);
        crate::host_ui::decorate_placement_kind(&kind, status.as_deref())
    });

    tracing::debug!(
        target: "szhost::hydrate",
        build_model_ms = t0.elapsed().as_millis() as u64,
        diff_files = panel.files.len(),
        changes = panel.changes.len(),
        merging = panel.merge.is_some(),
        tracker_issues = panel.tracker_issues.len(),
        "model hydrated"
    );
    let (worktree, tabs, active_tab) = tab_strip(session);
    FrameModel {
        worktree,
        tabs,
        active_tab,
        sidebar_workspaces,
        sidebar_db_worktrees,
        sidebar_db_folders,
        sidebar_db_terminals,
        disk_warn_threshold_gb: app_cfg.disk.warn_threshold_gb,
        active_worktree_disk: sidebar_status
            .disk_sizes
            .get(cwd.to_string_lossy().as_ref())
            .map(|&(total, _)| total.max(0) as u64),
        sidebar_status,
        loc: loc_count,
        active_container_name: superzej_core::sandbox::container_name_with_profile(
            &loc.path(),
            if hints.profile.is_empty() {
                None
            } else {
                Some(&hints.profile)
            },
        ),
        active_sandbox_backend: db
            .worktree_sandbox(&loc.path())
            .ok()
            .flatten()
            .unwrap_or_default(),
        active_placement_kind,
        active_placement_label,
        // containers is populated by the dedicated container refresh ticker
        // (run.rs) rather than inline here, to avoid blocking model hydration
        // on `podman ps` subprocess calls.
        containers: vec![],
        container_events: db.container_events(&loc.path(), 10).unwrap_or_default(),
        // Unified timeline: sandbox audit + proxy spend, merged newest-first.
        // Two small off-loop reads on the hydration thread (never the event loop).
        timeline: superzej_core::models::merge_timeline(
            &db.container_events(&loc.path(), 20).unwrap_or_default(),
            &db.proxy_requests(&loc.path(), 20).unwrap_or_default(),
            20,
        ),
        panel,
        panel_focused: false,
        status: format!(
            "Ctrl-Space menu   Alt-w worktree   Alt-o switch   Ctrl-q quit  [build {}]",
            env!("SZHOST_BUILD_TIME")
        ),
        accent: superzej_core::theme::TEAL.to_string(),
        ..Default::default()
    }
}

/// Build just the right-side panel for a worktree directory. This is the
/// path-keyed core of model hydration: it touches only `cwd`/`db`/`hints`,
/// never the session, so a background task can warm a not-yet-focused
/// worktree's panel into the switch cache before the user lands on it.
pub(crate) fn build_panel(
    cwd: &std::path::Path,
    db: &superzej_core::db::Db,
    hints: &HydrateHints,
    app_cfg: &superzej_core::config::Config,
) -> crate::panel::PanelData {
    use superzej_core::remote::GitLoc;
    use superzej_svc::git::{GitBackend, GixGit};

    let loc = GitLoc::for_worktree(cwd);

    // Section-gate flags precomputed as plain `Copy` values so the fan-out
    // closures below never capture `&HydrateHints` (keeps them trivially `Send`).
    let want_log = hints.open == crate::panel::Section::Pr;
    let log_n = if hints.expanded { 12 } else { 6 };
    // The Full git frame shows every list, so any open git-family section at
    // Full hydrates branches + stashes too.
    let git_family_full = hints.expanded && hints.open.is_git_family();
    let want_branches = hints.open == crate::panel::Section::Branches || git_family_full;
    let want_stashes = hints.open == crate::panel::Section::Stash || git_family_full;
    let want_lsfiles = hints.open == crate::panel::Section::Files;

    // Fan the independent, read-only git reads out across scoped threads: each
    // builds its own (trivial) `GixGit`, borrows `&loc` (read-only; `git -C` so
    // no chdir hazard) and applies the SAME error fallback inline, so a join
    // yields an already-defaulted value and `PanelData` is field-for-field
    // identical to the serial version. This collapses the sum of the git
    // subprocess latencies to roughly the slowest single one. No DB access in
    // here (`Db` is not `Send`); the DB-backed joins run after the scope.
    let t_git = std::time::Instant::now();
    let (
        branch,
        diff_entries,
        entities,
        status,
        ahead_behind,
        merge_info,
        stash_count,
        log,
        branches_raw,
        stashes_raw,
        ls_files,
    ) = std::thread::scope(|s| {
        let h_branch = s.spawn(|| {
            GixGit::new()
                .current_branch(&loc)
                .unwrap_or_else(|_| "—".into())
        });
        // diff + the semantic entity summary share the diff result and need only
        // `loc`, so they ride one thread (entity parsing is CPU, kept off the rest).
        let h_diff = s.spawn(|| {
            let entries = GixGit::new().diff_files(&loc, "HEAD").unwrap_or_default();
            let entities = compute_entity_summary(&loc, &entries);
            (entries, entities)
        });
        let h_status = s.spawn(|| GixGit::new().status(&loc).unwrap_or_default());
        let h_ahead = s.spawn(|| GixGit::new().ahead_behind(&loc).ok().flatten());
        let h_merge = s.spawn(|| GixGit::new().merge_state(&loc).ok().flatten());
        let h_stash_count = s.spawn(|| GixGit::new().stash_count(&loc).unwrap_or(0));
        // Section-gated heavy reads: spawned only when their section is open, so
        // an idle panel pays nothing. The branch PR-badge join is DB-backed and
        // stays on the main thread below; only the raw `branches_full` runs here.
        let h_log =
            want_log.then(|| s.spawn(|| GixGit::new().log_graph(&loc, log_n).unwrap_or_default()));
        let h_branches = want_branches
            .then(|| s.spawn(|| GixGit::new().branches_full(&loc).unwrap_or_default()));
        let h_stashes =
            want_stashes.then(|| s.spawn(|| GixGit::new().stash_list(&loc).unwrap_or_default()));
        // off-loop: build_panel only runs on hydration workers
        // (spawn_model_hydration / spawn_panel_prefetch spawn_blocking).
        #[expect(clippy::disallowed_methods)]
        let h_ls = want_lsfiles.then(|| {
            s.spawn(|| {
                loc.git_command(&["ls-files"])
                    .output()
                    .ok()
                    .and_then(|out| {
                        out.status.success().then(|| {
                            String::from_utf8_lossy(&out.stdout)
                                .lines()
                                .filter(|l| !l.is_empty())
                                .map(|l| l.to_string())
                                .collect::<Vec<_>>()
                        })
                    })
            })
        });

        let (diff_entries, entities) = h_diff.join().unwrap();
        (
            h_branch.join().unwrap(),
            diff_entries,
            entities,
            h_status.join().unwrap(),
            h_ahead.join().unwrap(),
            h_merge.join().unwrap(),
            h_stash_count.join().unwrap(),
            h_log.map(|h| h.join().unwrap()).unwrap_or_default(),
            h_branches.map(|h| h.join().unwrap()).unwrap_or_default(),
            h_stashes.map(|h| h.join().unwrap()).unwrap_or_default(),
            h_ls.and_then(|h| h.join().unwrap()),
        )
    });
    tracing::debug!(
        target: "szhost::hydrate",
        panel_git_ms = t_git.elapsed().as_millis() as u64,
        "panel git fan-out done"
    );

    let mut panel = crate::panel::PanelData {
        branch,
        ..Default::default()
    };

    // The typed PR cache: summary + checks + review threads + issues.
    if let Ok(Some((json, _))) = db.get_pr_cache(&loc.path())
        && let Ok(cached) = serde_json::from_str::<superzej_core::github::PrPanel>(&json)
    {
        apply_pr_cache(&mut panel, cached);
    }

    // The CI run-history cache feeds the `Ci` section rollup (AV group).
    if let Ok(Some((json, _))) = db.get_ci_cache(&loc.path())
        && let Ok(runs) = serde_json::from_str::<Vec<superzej_core::ci::CiRun>>(&json)
    {
        panel.ci_runs = runs;
    }

    // The local merge queue (fold-actor) — a tiny table, read every model build
    // (no dedicated RefreshKind). Feeds the `MergeQueue` section + statusbar badge.
    panel.merge_queue = db.list_merge_queue().unwrap_or_default();

    // Cross-worktree attention stream (the `Across` section): every worktree's
    // failing CI, from the CI cache. Cheap DB reads only, off the event loop.
    panel.across = build_across(db);

    // Hosts-as-resources: per-[host.*] display snapshots for the System ▸ Hosts
    // section, the sidebar HOSTS block, and the wizard badges. Small DB reads;
    // empty (and free) when no [host.*] is configured. The loop live-merges
    // HostRuntime progress on top after each drain.
    panel.hosts = crate::host_ui::host_snapshots(app_cfg, db);

    panel.files = diff_entries
        .iter()
        .map(|f| crate::panel::DiffFile {
            status: f.path.chars().next().unwrap_or('M'),
            path: f.path.clone(),
            added: f.added,
            deleted: f.deleted,
        })
        .collect();

    // Changes section: porcelain status joined with the diffstat.
    panel.changes = crate::panel::build_change_rows(&status, &diff_entries);
    // Semantic git layer (items 311/313/317): entity-level view of the changes.
    panel.entities = entities;

    // Header zone: upstream divergence + merge-in-progress banner.
    panel.ahead_behind = ahead_behind;
    let unresolved = superzej_svc::git::conflict_count(&status);
    let total = merge_total(db, &loc.path(), merge_info.is_some(), unresolved);
    panel.merge = merge_info.map(|m| crate::panel::MergeBanner {
        label: m.kind.label().to_string(),
        onto: m.onto,
        unresolved,
        total,
    });
    panel.stash_count = stash_count;
    panel.log = log;

    if hints.wants_commits() {
        let cached = db.get_commit_cache(&loc.path()).ok().flatten();
        if let Some((json, _)) = cached.as_ref()
            && let Ok(mut rows) = serde_json::from_str::<Vec<crate::panel::CommitRow>>(json)
        {
            rows.truncate(hints.visible_commit_limit());
            panel.commits = rows;
        }
        panel.commits_loading = commit_cache_needs_refresh(cached.as_ref());
    }
    if want_branches {
        // The per-repo open-PR cache joins onto branch rows by head ref.
        let badges: Vec<superzej_core::github::PrHeader> = db
            .get_pr_branch_cache(&loc.path())
            .ok()
            .flatten()
            .map(|(json, _)| superzej_core::github::parse_pr_headers(&json))
            .unwrap_or_default();
        panel.branches =
            branches_raw
                .into_iter()
                .map(|b| {
                    let pr = badges.iter().find(|p| p.head_ref == b.name).map(|p| {
                        crate::panel::PrBadge {
                            number: p.number,
                            state: p.state.clone(),
                            is_draft: p.is_draft,
                            url: p.url.clone(),
                        }
                    });
                    crate::panel::BranchRow {
                        name: b.name,
                        is_head: b.is_head,
                        upstream: b.upstream,
                        ahead: b.ahead,
                        behind: b.behind,
                        upstream_gone: b.upstream_gone,
                        sha: b.sha,
                        date: b.date,
                        subject: b.subject,
                        pr,
                    }
                })
                .collect();
    }
    if want_stashes {
        panel.stashes = stashes_raw
            .into_iter()
            .map(|s| crate::panel::StashRow {
                index: s.index,
                sha: s.sha,
                date: s.date,
                message: s.message,
            })
            .collect();
    }

    // Tests section snapshot from the cache (summary + failures + history).
    if let Ok(Some((json, _))) = db.get_test_cache(&loc.path())
        && let Ok(cache) = serde_json::from_str::<crate::testkit::model::TestCache>(&json)
    {
        panel.tests = Some(crate::panel::tests_lite(&cache));
    }

    // Tracked-file list for the Files accordion — fetched in the fan-out above
    // (only while Files is open; `git ls-files` isn't free on big repos every 2s).
    if let Some(files) = ls_files {
        panel.file_count = Some(files.len() as u64);
        panel.all_files = files;
    }

    // Issue tracker cache — reads directly from the DB (no network; the
    // background `spawn_issue_cache_refresh` keeps the cache warm). Loads every
    // cached provider for this repo and concatenates, so multiple trackers
    // (e.g. Linear + Jira) aggregate into one list.
    if let Ok(cached) = db.get_all_issue_cache(&loc.path()) {
        for (_provider, json) in cached {
            if let Ok(mut issues) = serde_json::from_str::<Vec<superzej_core::issue::Issue>>(&json)
            {
                panel.tracker_issues.append(&mut issues);
            }
        }
    }
    if let Ok(links) = db.linked_issues(&cwd.to_string_lossy()) {
        panel.tracker_links = links;
    }
    // The active worktree's repo root — the default scoping unit for the panel's
    // otherwise-global sections (My Work, notifications).
    let repo_root = superzej_core::repo::main_worktree(cwd).unwrap_or_else(|| cwd.to_path_buf());
    // Unified "My Work" feed. Default: the active repo's scoped cache row (keyed
    // by repo root); under the Mine "all repos" toggle: the cross-repo
    // `ALL_SCOPE` row.
    let my_work_scope = if crate::panel::scope::mine_all() {
        superzej_core::work::ALL_SCOPE.to_string()
    } else {
        repo_root.to_string_lossy().into_owned()
    };
    if let Ok(Some((json, _))) = db.get_my_work_cache(&my_work_scope)
        && let Ok(rows) = serde_json::from_str::<Vec<superzej_core::work::WorkRow>>(&json)
    {
        panel.my_work = rows;
    }
    // Full notification list for the inbox panel; badge counts are derived from it
    // by effective priority (Info kinds never count; the red flag is Alert-only).
    // Scoped to this repo's own worktrees by default (host-global notifications,
    // with an empty `worktree_path`, always show); the System-tab "all" toggle
    // reveals every worktree's — so a sibling repo's error doesn't leak here or
    // light the badge.
    if let Ok(mut notifications) = db.get_all_notifications(50) {
        use superzej_core::notification::Priority;
        if !crate::panel::scope::system_all() {
            let repo_paths = repo_worktree_paths(db, &repo_root);
            notifications
                .retain(|n| n.worktree_path.is_empty() || repo_paths.contains(&n.worktree_path));
        }
        let unread = notifications.iter().filter(|n| !n.read);
        let (mut alert, mut counted) = (0usize, 0usize);
        for n in unread {
            match app_cfg.notifications.priority_of(n.kind) {
                Priority::Alert => {
                    alert += 1;
                    counted += 1;
                }
                Priority::Notice => counted += 1,
                Priority::Info => {}
            }
        }
        panel.alert_notifications = alert;
        panel.unread_notifications = counted;
        panel.notifications = notifications;
    }
    // Tasks section: populate task specs from config + auto-discovery (reusing the
    // single layered-config load above). Configured tasks win by name; discovered
    // tasks from manifests fill gaps.
    {
        let configured = app_cfg.tasks.clone();
        let discovered = crate::task::discover_all_tasks(cwd);
        panel.task_specs = crate::task::merge_tasks(configured, discovered);
    }

    // Logs section: tail the szhost log file.
    // Always scan for new ERRORs to surface as notifications; full tail only
    // when the section is open (to avoid reading 5 MB on every tick).
    let log_path = superzej_core::util::xdg_state_home().join("superzej/logs/szhost.log");
    if log_path.exists()
        && let Ok(bytes) = std::fs::read(&log_path)
    {
        let content = String::from_utf8_lossy(bytes.as_ref());
        let all_lines: Vec<_> = content
            .lines()
            .filter_map(superzej_core::log_view::parse_log_line)
            .collect();

        // Surface new ERROR lines as a notification (at most once per 5 min).
        let now_ms = superzej_core::util::now();
        let five_min_ms: i64 = 5 * 60 * 1_000;
        let has_recent_log_error = panel
            .notifications
            .iter()
            .any(|n| n.source_ref == "log:szhost" && now_ms - n.created_at_ms < five_min_ms);
        if !has_recent_log_error {
            let error_count = all_lines
                .iter()
                .filter(|l| l.level == superzej_core::log_view::LogLevel::Error)
                .count();
            if error_count > 0 {
                let msg = format!(
                    "{} error{} in szhost.log",
                    error_count,
                    if error_count == 1 { "" } else { "s" }
                );
                let _ = db.put_notification("log_error", "log:szhost", &msg, "");
            }
        }

        if hints.open == crate::panel::Section::Logs {
            let start = all_lines.len().saturating_sub(500);
            panel.log_lines = all_lines[start..].to_vec();
        }
    }
    panel
}

/// `gen` tags the result so the event loop can drop models that were spawned
/// before a workspace/worktree switch but land after it (spawn_blocking tasks
/// complete out of order; a stale model would resurrect the old sidebar).
pub(crate) fn spawn_model_hydration(
    tx: tokio_mpsc::UnboundedSender<(u64, FrameModel)>,
    generation: u64,
    session: crate::session::Session,
    waker: Option<TerminalWaker>,
    hints: HydrateHints,
) {
    task::spawn_blocking(move || {
        let Ok(db) = superzej_core::db::Db::open() else {
            return;
        };
        let first = {
            let _g = crate::perf::measure(crate::perf::Subsys::Hydrate);
            build_model(&session, &db, hints.clone())
        };
        let refresh_commits = first.panel.commits_loading;
        if tx.send((generation, first)).is_ok()
            && let Some(w) = &waker
        {
            let _ = w.wake();
        }

        // `git log` can be expensive on large repos. Run it only after the
        // cache-backed model has already landed, then send a second model from
        // the refreshed cache. Generation tagging in the event loop drops this
        // safely if the user switched worktrees meanwhile.
        if refresh_commits
            && refresh_commit_cache(&db, &session)
            && tx
                .send((generation, build_model(&session, &db, hints)))
                .is_ok()
            && let Some(w) = &waker
        {
            let _ = w.wake();
        }
    });
}

/// Warm a not-yet-focused worktree's panel into the switch cache. Builds only
/// the path-keyed [`build_panel`] data (no sidebar/tab work, no `git log`
/// refresh) on a blocking worker and ships `(worktree_path, panel)` back so the
/// event loop can serve it instantly when the user switches to that worktree.
/// Unlike [`spawn_model_hydration`] this is fire-and-forget background warming —
/// the result never replaces the live frame, only seeds the cache.
pub(crate) fn spawn_panel_prefetch(
    tx: tokio_mpsc::UnboundedSender<(std::path::PathBuf, crate::panel::PanelData)>,
    cwd: std::path::PathBuf,
    hints: HydrateHints,
    waker: Option<TerminalWaker>,
) {
    task::spawn_blocking(move || {
        if !cwd.is_dir() {
            return;
        }
        let Ok(db) = superzej_core::db::Db::open() else {
            return;
        };
        let app_cfg = superzej_core::config::Config::try_load_layered(
            &superzej_core::config::ProcessEnv,
            &[],
            None,
        )
        .unwrap_or_default();
        let panel = build_panel(&cwd, &db, &hints, &app_cfg);
        if tx.send((cwd, panel)).is_ok()
            && let Some(w) = &waker
        {
            let _ = w.wake();
        }
    });
}

pub(crate) fn spawn_pr_cache_refresh(
    session: crate::session::Session,
    cfg: superzej_core::config::IssuesConfig,
    disk_cfg: superzej_core::config::DiskConfig,
    waker: Option<TerminalWaker>,
) {
    let branch_session = session.clone();
    let branch_waker = waker.clone();
    task::spawn_blocking(move || {
        let cwd = active_tab_path(&session);
        if !cwd.is_dir() {
            return;
        }
        let loc = superzej_core::remote::GitLoc::for_worktree(&cwd);
        let Ok(db) = superzej_core::db::Db::open() else {
            return;
        };

        // Snapshot the old PR state BEFORE overwriting the cache.
        let old_pr_state: Option<String> = db
            .get_pr_cache(&loc.path())
            .ok()
            .flatten()
            .and_then(|(json, _)| {
                serde_json::from_str::<superzej_core::github::PrPanel>(&json).ok()
            })
            .and_then(|p| match p.state {
                superzej_core::github::PanelState::Pr(pr) => Some(pr.state),
                _ => None,
            });

        // The full feed: PR + checks + review threads + issues (extras are
        // best-effort and never fail the panel).
        let panel = superzej_core::github::pr_status_full(&loc);
        let Ok(json) = serde_json::to_string(&panel) else {
            return;
        };
        let _ = db.put_pr_cache(&loc.path(), &panel.branch, &json);

        // Emit a notification when the PR transitions between states
        // (e.g. OPEN → MERGED). Only fires when there was a prior known state
        // to diff against — avoids spurious notifications on first fetch.
        if let superzej_core::github::PanelState::Pr(ref pr) = panel.state
            && let Some(old) = &old_pr_state
            && old != &pr.state
        {
            let pr_ref = format!("pr:{}", pr.number);
            let msg = format!("PR #{} {} → {}", pr.number, old, pr.state);
            let wt = cwd.to_string_lossy();
            let _ = db.put_notification("pr_state_changed", &pr_ref, &msg, &wt);

            // Lifecycle automation: on merge, move this worktree's linked
            // issue(s) to Done on their tracker (opt-in via `[issues].move_on_merge`).
            if cfg.move_on_merge
                && pr.state == "MERGED"
                && let Ok(linked) = db.linked_issues(&wt)
                && !linked.is_empty()
            {
                let router = superzej_svc::issue::IssueRouter::from_config(&cfg);
                if router.is_configured()
                    && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                {
                    let patch = superzej_core::issue::IssuePatch {
                        status: Some(superzej_core::issue::IssueStatus::Done),
                        ..Default::default()
                    };
                    for id in &linked {
                        let _ = rt.block_on(router.update_issue(id, &patch));
                    }
                }
            }
        }

        // PR cache landing should surface via a model rehydrate; pulse the waker
        // so an idle loop repaints promptly.
        if let Some(w) = &waker {
            let _ = w.wake();
        }
    });
    // Sibling feed: the repo's open-PR headers (`pr_branch_cache`) join onto
    // branch rows as PR badges and back the branches view's open-in-browser.
    // GhBackend::pr_list is async (octocrab native, gh-CLI fallback), so it
    // runs on its own blocking thread under a throwaway current-thread
    // runtime — neither the subprocess fallback nor the HTTP wait can ever
    // touch the event loop.
    task::spawn_blocking(move || {
        let cwd = active_tab_path(&branch_session);
        if !cwd.is_dir() {
            return;
        }
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let loc = superzej_core::remote::GitLoc::for_worktree(&cwd);
        let prs = rt.block_on(async {
            use superzej_svc::gh::{GhBackend, GhNative};
            GhNative::new().pr_list(&loc).await
        });
        if let Ok(prs) = prs
            && let Ok(json) = serde_json::to_string(&prs)
            && let Ok(db) = superzej_core::db::Db::open()
        {
            // `pr_list` returns the repo's open PRs (branch-independent), so key
            // the cache by repo root — every worktree of the repo reads the same
            // entry to resolve its own branch's badge (item 28).
            let repo_root = superzej_core::repo::main_worktree(&cwd)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_else(|| loc.path());

            // On-merge auto-clean (background worktrees only): a branch that had
            // an open PR last round but is gone from the open set now has
            // transitioned (merged or closed). Resolve the precise state and, if
            // it matches the configured policy, reclaim that worktree's
            // `target/`. The active worktree is never touched (you may still be
            // working in it), nor one with a superzej-spawned build in flight.
            if disk_cfg.auto_clean_on_merge || disk_cfg.clean_on_pr_closed {
                maybe_clean_merged_worktrees(&db, &loc, &cwd, &repo_root, &prs, &disk_cfg);
            }

            let _ = db.put_pr_branch_cache(&repo_root, &json);
            if let Some(w) = &branch_waker {
                let _ = w.wake();
            }
        }
    });
}

/// Auto-clean `target/` for worktrees whose open PR has just transitioned away
/// (merged / closed-without-merge), gated by `[disk]` policy. Compares the
/// previously-cached open branches against the current open set; for each
/// branch that dropped out and maps to a known *background* worktree (not the
/// active one, no running build), resolves the precise PR state via a targeted
/// `gh pr view` and cleans on a policy match. Best-effort and silent on error.
fn maybe_clean_merged_worktrees(
    db: &superzej_core::db::Db,
    loc: &superzej_core::remote::GitLoc,
    active: &std::path::Path,
    repo_root: &str,
    open_now: &[superzej_core::github::PrHeader],
    cfg: &superzej_core::config::DiskConfig,
) {
    use std::collections::HashSet;

    // Branches with an open PR in the prior cache.
    let prev_open: HashSet<String> = db
        .get_pr_branch_cache(repo_root)
        .ok()
        .flatten()
        .and_then(|(json, _)| {
            serde_json::from_str::<Vec<superzej_core::github::PrHeader>>(&json).ok()
        })
        .into_iter()
        .flatten()
        .filter(|p| p.state == "OPEN")
        .map(|p| p.head_ref)
        .collect();
    if prev_open.is_empty() {
        return; // first fetch — nothing to diff against
    }
    let open_now: HashSet<&str> = open_now
        .iter()
        .filter(|p| p.state == "OPEN")
        .map(|p| p.head_ref.as_str())
        .collect();

    // Map branch → worktree path for this repo's worktrees.
    let Ok(rows) = db.worktrees() else {
        return;
    };
    let active = active.to_string_lossy();
    for row in rows {
        if row.repo_root != repo_root || row.branch.is_empty() {
            continue;
        }
        // Dropped out of the open set since last round?
        if !prev_open.contains(&row.branch) || open_now.contains(row.branch.as_str()) {
            continue;
        }
        let path = std::path::PathBuf::from(&row.worktree);
        if !path.is_dir() || row.worktree == active || crate::task::slot_active(&path) {
            continue;
        }
        // Resolve the precise outcome (merged vs closed) against policy.
        let merged = matches!(
            superzej_core::github::pr_state_for_branch(loc, &row.branch).as_deref(),
            Some("MERGED")
        );
        let should = (merged && cfg.auto_clean_on_merge) || (!merged && cfg.clean_on_pr_closed);
        if !should {
            continue;
        }
        if let Ok(reclaimed) = superzej_core::worktree::clean_target(&path)
            && reclaimed > 0
        {
            let _ = db.delete_worktree_disk(&row.worktree);
            let verb = if merged { "merged" } else { "closed" };
            let msg = format!(
                "{} cleaned ({} reclaimed)",
                verb,
                superzej_core::disk::human(reclaimed)
            );
            let _ = db.put_notification("disk_cleaned", &row.branch, &msg, &row.worktree);
        }
    }
}

/// Background per-worktree disk scan. Enumerates every known worktree, `du`s
/// each (skipping any refreshed within `scan_interval_secs` — the coarse ticker
/// would otherwise re-scan everything every 30s), caches sizes in
/// `worktree_disk`, and pulses the waker so the sidebar/statusbar repaint with
/// fresh sizes. Runs on `spawn_blocking`; the (seconds-long) `du` never touches
/// the event loop. Sizes themselves ride the cheap model hydrate via
/// [`collect_sidebar_status`].
pub(crate) fn spawn_disk_scan(
    cfg: superzej_core::config::DiskConfig,
    waker: Option<TerminalWaker>,
) {
    if !cfg.show_sizes {
        return;
    }
    task::spawn_blocking(move || {
        let Ok(db) = superzej_core::db::Db::open() else {
            return;
        };
        let Ok(rows) = db.worktrees() else {
            return;
        };
        let now = superzej_core::util::now();
        let ttl = cfg.scan_interval_secs.max(1) as i64;
        let mut scanned = 0u32;
        for row in rows {
            let path = std::path::PathBuf::from(&row.worktree);
            if !path.is_dir() {
                // Vanished worktree — drop any stale size so the badge clears.
                let _ = db.delete_worktree_disk(&row.worktree);
                continue;
            }
            // Coalesce: skip entries scanned within the TTL window.
            if let Ok(Some((_, _, fetched_at))) = db.get_worktree_disk(&row.worktree)
                && now - fetched_at < ttl
            {
                continue;
            }
            let usage = superzej_core::disk::measure_worktree(&path);
            let _ = db.put_worktree_disk(
                &row.worktree,
                usage.total_bytes as i64,
                usage.target_bytes as i64,
            );
            scanned += 1;
        }
        if scanned > 0
            && let Some(w) = &waker
        {
            let _ = w.wake();
        }
    });
}

/// Refresh the issue-tracker cache for the active worktree's repo.  Runs
/// entirely off-thread (no event-loop contact); writes the fresh JSON into
/// `issue_cache` and pulses the waker so the loop rehydrates promptly.
pub(crate) fn spawn_issue_cache_refresh(
    session: crate::session::Session,
    cfg: superzej_core::config::IssuesConfig,
    waker: Option<TerminalWaker>,
) {
    task::spawn_blocking(move || {
        use superzej_core::issue::IssueFilter;
        use superzej_svc::issue::IssueRouter;

        let cwd = active_tab_path(&session);
        if !cwd.is_dir() {
            return;
        }
        let router = IssueRouter::from_config(&cfg);
        if !router.is_configured() {
            return;
        }
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let filter = IssueFilter {
            assignee_me: cfg.filter_assignee_me,
            limit: cfg.max_issues,
            ..Default::default()
        };
        // Fetch every configured provider; cache and diff each under its own
        // `(repo_root, provider)` key so trackers aggregate without clobbering.
        let per_provider = rt.block_on(router.list_per_provider(&filter));
        let Ok(db) = superzej_core::db::Db::open() else {
            return;
        };
        let repo_key = cwd.to_string_lossy();
        let linked: std::collections::HashSet<String> = db
            .linked_issues(&repo_key)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut changed = false;
        for (provider, result) in per_provider {
            let Ok(issues) = result else {
                continue; // a failing provider leaves its prior cache intact
            };
            let Ok(json) = serde_json::to_string(&issues) else {
                continue;
            };
            // Diff old vs new for this provider to emit notifications first.
            let old_issues: Vec<superzej_core::issue::Issue> = db
                .get_issue_cache(&repo_key, provider)
                .ok()
                .flatten()
                .and_then(|(j, _)| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            let old_map: std::collections::HashMap<&str, &superzej_core::issue::IssueStatus> =
                old_issues
                    .iter()
                    .map(|i| (i.id.as_str(), &i.status))
                    .collect();
            for issue in &issues {
                if let Some(&old_status) = old_map.get(issue.id.as_str())
                    && *old_status != issue.status
                    && linked.contains(&issue.id)
                {
                    let msg = format!(
                        "{} status changed to {}",
                        issue.number,
                        issue.status.label()
                    );
                    let _ = db.put_notification("status_changed", &issue.id, &msg, &repo_key);
                }
            }
            let _ = db.put_issue_cache(&repo_key, provider, &json);
            changed = true;
        }
        if changed && let Some(w) = &waker {
            let _ = w.wake();
        }
    });
}

/// Map an issue's triage priority to a `WorkRow` urgency weight (higher = more
/// urgent), so the unified feed sorts the same way the Issues section does.
fn issue_urgency(p: superzej_core::issue::IssuePriority) -> u8 {
    use superzej_core::issue::IssuePriority as P;
    match p {
        P::Urgent => 4,
        P::High => 3,
        P::Medium => 2,
        P::Low => 1,
        P::None => 0,
    }
}

fn pr_search_row(
    p: superzej_core::github::PrSearchRow,
    group: superzej_core::work::WorkGroup,
) -> superzej_core::work::WorkRow {
    superzej_core::work::WorkRow {
        group,
        kind: superzej_core::work::WorkKind::Pr,
        provider: "github".into(),
        number: format!("#{}", p.number),
        title: p.title,
        repo: p.repository.name_with_owner,
        url: p.url,
        urgency: 2,
        issue_id: None,
        branch_hint: None,
        worktree_path: None,
    }
}

/// The set of worktree paths belonging to a repo (`repo_root`), from the DB
/// registry. Used to scope the "My Work" feed's notifications to the current
/// repo — a notification for a sibling worktree of the same repo is relevant;
/// one for an unrelated repo (often on another host) is not.
fn repo_worktree_paths(
    db: &superzej_core::db::Db,
    repo_root: &std::path::Path,
) -> std::collections::HashSet<String> {
    let rr = repo_root.to_string_lossy();
    db.worktrees()
        .map(|wts| {
            wts.into_iter()
                .filter(|w| w.repo_root == rr)
                .map(|w| w.worktree)
                .collect()
        })
        .unwrap_or_default()
}

/// Refresh the unified "My Work" feed for a scope: assigned issues (all
/// configured providers), review-requested / authored PRs, and high-priority
/// unread notifications. By default (`all == false`) everything is scoped to the
/// **active worktree's repo** — GitHub via `--repo owner/repo`, Linear/Jira via
/// the repo-overlaid team/project, notifications to the repo's own worktrees —
/// and written to the `my_work_cache` row keyed by the repo root. With
/// `all == true` the fetch is cross-repo and written to the `ALL_SCOPE` row (the
/// panel's "all repos" toggle). Pulses the waker when done.
pub(crate) fn spawn_my_work_refresh(
    session: crate::session::Session,
    cfg: superzej_core::config::Config,
    all: bool,
    waker: Option<TerminalWaker>,
) {
    task::spawn_blocking(move || {
        use superzej_core::work::{ALL_SCOPE, WorkGroup, WorkKind, WorkRow};

        let cwd = active_tab_path(&session);
        if !cwd.is_dir() {
            return;
        }
        let loc = superzej_core::remote::GitLoc::for_worktree(&cwd);
        let repo_root = superzej_core::repo::main_worktree(&cwd).unwrap_or_else(|| cwd.clone());
        // Repo scope (unless `all`): `owner/repo` for GitHub, the repo `[issues]`
        // overlay for Linear/Jira, and the cache key.
        let nwo = if all {
            None
        } else {
            superzej_core::github::origin_nwo(&loc)
        };
        let issues_cfg = if all {
            cfg.issues.clone()
        } else {
            cfg.repo_issues(Some(&repo_root))
        };
        let scope_key = if all {
            ALL_SCOPE.to_string()
        } else {
            repo_root.to_string_lossy().into_owned()
        };

        let mut rows: Vec<WorkRow> = Vec::new();

        // 1) Issues assigned to me, aggregated across configured providers.
        let router = superzej_svc::issue::IssueRouter::from_config(&issues_cfg);
        if router.is_configured()
            && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        {
            let mut filter =
                superzej_core::issue::IssueFilter::my_open(issues_cfg.max_issues.max(1));
            filter.repo = nwo.clone(); // GitHub repo scope; other providers ignore it.
            if let Ok(issues) = rt.block_on(router.list_issues(&filter)) {
                for i in issues {
                    // Tag GitHub issues with the repo for display; Linear/Jira
                    // scope by team/project, so leave their repo blank.
                    let repo = if i.provider == "github" {
                        nwo.clone().unwrap_or_default()
                    } else {
                        String::new()
                    };
                    rows.push(WorkRow {
                        group: WorkGroup::Assigned,
                        kind: WorkKind::Issue,
                        provider: i.provider,
                        number: i.number,
                        title: i.title,
                        repo,
                        url: i.url,
                        urgency: issue_urgency(i.priority),
                        issue_id: Some(i.id),
                        branch_hint: i.branch_hint,
                        worktree_path: None,
                    });
                }
            }
        }

        // 2) PRs via `gh search` — scoped to `nwo` unless `all`.
        if let Ok(prs) =
            superzej_core::github::search_prs(&loc, "--review-requested=@me", nwo.as_deref(), 30)
        {
            rows.extend(
                prs.into_iter()
                    .map(|p| pr_search_row(p, WorkGroup::ReviewRequested)),
            );
        }
        if let Ok(prs) = superzej_core::github::search_prs(&loc, "--author=@me", nwo.as_deref(), 30)
        {
            rows.extend(
                prs.into_iter()
                    .map(|p| pr_search_row(p, WorkGroup::NeedsAttention)),
            );
        }

        // 3) High-priority unread notifications (mentions / blockers / pr-linked),
        //    scoped to this repo's own worktrees unless `all`.
        if let Ok(db) = superzej_core::db::Db::open()
            && let Ok(notes) = db.get_all_notifications(50)
        {
            use superzej_core::notification::NotificationKind as K;
            let repo_paths = (!all).then(|| repo_worktree_paths(&db, &repo_root));
            for n in notes.into_iter().filter(|n| !n.read) {
                if !matches!(n.kind, K::Mentioned | K::BlockerResolved | K::PrLinked) {
                    continue;
                }
                // Repo-scoped: drop notifications that don't belong to one of this
                // repo's worktrees (untagged/global ones only surface under `all`).
                if let Some(paths) = &repo_paths
                    && (n.worktree_path.is_empty() || !paths.contains(&n.worktree_path))
                {
                    continue;
                }
                rows.push(WorkRow {
                    group: WorkGroup::NeedsAttention,
                    kind: WorkKind::Notification,
                    title: n.message,
                    urgency: 1,
                    worktree_path: if n.worktree_path.is_empty() {
                        None
                    } else {
                        Some(n.worktree_path)
                    },
                    ..Default::default()
                });
            }
        }

        // Always write — an emptied feed must clear the scope's cache row, not
        // keep stale rows.
        if let Ok(db) = superzej_core::db::Db::open()
            && let Ok(json) = serde_json::to_string(&rows)
        {
            let _ = db.put_my_work_cache(&scope_key, &json);
        }
        if let Some(w) = &waker {
            let _ = w.wake();
        }
    });
}

/// Toggle the Mine feed between the active repo (default) and all repos, kick off
/// a scoped refresh, and return the status line. Extracted from the panel key
/// handler so the god-file `run.rs` stays under the file-size ratchet.
pub(crate) fn toggle_mine_scope(
    session: &crate::session::Session,
    cfg: &superzej_core::config::Config,
    waker: &TerminalWaker,
) -> String {
    let all = crate::panel::scope::toggle_mine_all();
    spawn_my_work_refresh(session.clone(), cfg.clone(), all, Some(waker.clone()));
    if all {
        "My Work: all repos".into()
    } else {
        "My Work: this repo".into()
    }
}

/// Toggle the System tab between this repo (default) and every worktree,
/// rehydrate the active model so the scoped notification list refreshes, and
/// return the status line. Extracted from the panel key handler for the ratchet.
pub(crate) fn toggle_system_scope(
    tx: &tokio_mpsc::UnboundedSender<(u64, FrameModel)>,
    generation: u64,
    session: &crate::session::Session,
    waker: &TerminalWaker,
    open: crate::panel::Section,
    expanded: bool,
) -> String {
    let all = crate::panel::scope::toggle_system_all();
    spawn_model_hydration(
        tx.clone(),
        generation,
        session.clone(),
        Some(waker.clone()),
        HydrateHints {
            open,
            expanded,
            ..Default::default()
        },
    );
    if all {
        "System: all worktrees".into()
    } else {
        "System: this repo".into()
    }
}

/// Refresh the CI run-history cache for the active worktree (AV group). Off the
/// event loop: resolves the provider from `[ci]` config + the git remote,
/// fetches recent runs for the current branch via the async `CiProvider`, writes
/// `ci_runs_cache`, and pulses the waker so the panel rehydrates. A missing
/// provider / unconfigured CI / fetch error simply leaves the cache untouched.
pub(crate) fn spawn_ci_cache_refresh(
    session: crate::session::Session,
    cfg: superzej_core::config::CiConfig,
    waker: Option<TerminalWaker>,
) {
    task::spawn_blocking(move || {
        let cwd = active_tab_path(&session);
        if !cwd.is_dir() {
            return;
        }
        let loc = superzej_core::remote::GitLoc::for_worktree(&cwd);
        let Some(client) = superzej_svc::ci::provider_for(&loc, &cfg) else {
            return;
        };
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let branch = loc
            .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .filter(|b| !b.is_empty());
        let runs = rt.block_on(client.runs(&loc, branch.as_deref(), cfg.max_runs));
        if let Ok(runs) = runs
            && let Ok(json) = serde_json::to_string(&runs)
            && let Ok(db) = superzej_core::db::Db::open()
        {
            let _ = db.put_ci_cache(&loc.path(), branch.as_deref().unwrap_or(""), &json);
            if let Some(w) = &waker {
                let _ = w.wake();
            }
        }
    });
}

/// Bind (or re-bind) the diff fs-watcher to the active worktree path. A no-op if
/// the active worktree is unchanged. On a debounced filesystem event under the
/// worktree, pushes `RefreshKind::Model` and pulses the waker so the loop
/// rehydrates the diff panel promptly. The previous watcher (if any) is dropped,
/// which unregisters its watch.
/// True when `p` lies inside a `.git` directory (any path component is
/// `.git`) — used to filter the recursive worktree watcher so git's own
/// metadata churn doesn't drive a refresh loop.
fn in_dot_git(p: &std::path::Path) -> bool {
    p.components().any(|c| c.as_os_str() == ".git")
}

/// True for the subset of `.git`-internal paths that signal a real *git-state*
/// change — a commit, checkout, reset, branch/tag move, or a merge / rebase /
/// cherry-pick / revert progressing. These are the events the panel must react
/// to even though they live under `.git`.
///
/// Deliberately an allowlist, not a blocklist: the high-churn internals —
/// `index` (hydration's own `git status`/`diff` rewrite its stat cache, the
/// ~2 Hz feedback loop that once read as a freeze), the object store, lock
/// files, `COMMIT_EDITMSG` — never match, so they can never drive a refresh
/// loop no matter what new files git starts writing.
fn is_git_state_path(p: &std::path::Path) -> bool {
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".lock") {
        // The transient `*.lock` git writes while preparing a ref/HEAD update;
        // react to the final write that replaces it, not the lock churn.
        return false;
    }
    // `logs/HEAD` (reflog) is appended on commit/checkout/reset/merge/rebase;
    // `refs/…` + `packed-refs` move on branch/tag updates; the rebase-* dirs
    // and *_HEAD pseudo-refs track an in-progress sequencer operation.
    p.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("refs") | Some("logs") | Some("rebase-merge") | Some("rebase-apply")
        )
    }) || matches!(
        name,
        "HEAD"
            | "packed-refs"
            | "MERGE_HEAD"
            | "ORIG_HEAD"
            | "CHERRY_PICK_HEAD"
            | "REVERT_HEAD"
            | "BISECT_LOG"
    )
}

/// Whether a single diff-watcher event path should drive a model re-hydration.
/// Three cases, in precedence order:
/// 1. `.git`-internal paths (inside a `.git` component, or under the resolved
///    gitdir/common-dir `roots`) refresh ONLY for real git-state changes —
///    commits, checkouts, branch/tag moves, in-progress merge/rebase — gated by
///    [`is_git_state_path`] so index/object-store churn can't drive a loop.
/// 2. Otherwise, gitignored worktree paths (build artifacts like `target/`)
///    never refresh: they can't appear in `git diff HEAD`, so a rebuild would be
///    pure waste — and a cargo/agent running in the tree churns them constantly.
/// 3. Everything else (real edits to tracked/untracked source files) refreshes.
///
/// Pure (given a prebuilt matcher), so the precedence is unit-tested.
fn watcher_path_triggers_refresh(
    p: &std::path::Path,
    roots: &[std::path::PathBuf],
    ignore: &ignore::gitignore::Gitignore,
) -> bool {
    if in_dot_git(p) || roots.iter().any(|r| p.starts_with(r)) {
        is_git_state_path(p)
    } else {
        // Case 2 vs 3: gitignored build churn is dropped; everything else (real
        // source edits) refreshes.
        !ignore
            .matched_path_or_any_parents(p, p.is_dir())
            .is_ignore()
    }
}

pub(crate) fn retarget_diff_watcher(
    session: &crate::session::Session,
    watched: &mut Option<std::path::PathBuf>,
    watcher: &mut Option<notify::RecommendedWatcher>,
    watcher_tx: &tokio_mpsc::UnboundedSender<(std::path::PathBuf, notify::RecommendedWatcher)>,
    refresh_tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) {
    let cwd = active_tab_path(session);
    if !cwd.is_dir() {
        return;
    }
    if watched.as_deref() == Some(cwd.as_path()) {
        return; // already watching this worktree
    }
    *watched = Some(cwd.clone());

    // Build + recursively register the watcher off-thread: on a large worktree
    // the recursive inotify registration walks every directory (~1s on this
    // repo) and must never block startup or a tab switch. The old watcher is
    // dropped off-thread too — removing thousands of watches isn't free. The
    // finished watcher comes back via `watcher_tx`; the loop adopts it if the
    // user hasn't switched away again. Until it lands, the 2s safety-net tick
    // covers diff refresh.
    let old = watcher.take();
    let tx = refresh_tx.clone();
    let wtx = watcher_tx.clone();
    let w = waker.clone();
    std::thread::spawn(move || {
        drop(old);

        // Resolve this worktree's gitdir + common dir. For a *linked* worktree
        // `<cwd>/.git` is a file pointer, so the HEAD / reflog / refs that
        // signal a commit live OUTSIDE the watched tree (in the main repo's
        // `.git/worktrees/<name>` + shared `.git`); we must watch those too or
        // pane-driven commits never reach the panel. For the main checkout both
        // resolve back under `cwd` and the recursive root watch already covers
        // them. `git rev-parse` runs here, off the event loop.
        let git_dir = superzej_core::util::git_out(
            &cwd,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )
        .map(std::path::PathBuf::from);
        let common_dir = superzej_core::util::git_out(
            &cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .map(std::path::PathBuf::from);
        // Roots used by the event filter to recognise git-internal paths even
        // for bare/relocated gitdirs whose path has no literal `.git` component.
        let git_roots: Vec<std::path::PathBuf> = [git_dir.clone(), common_dir.clone()]
            .into_iter()
            .flatten()
            .collect();

        let mut last_send = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let wake = w.clone();
        let roots = git_roots.clone();
        // Drop watcher events for gitignored paths (`target/`, `node_modules/`,
        // build outputs): a change to an ignored file can never alter
        // `git diff HEAD`, so firing a model rebuild for it is pure waste — yet a
        // cargo/sccache/agent running inside the worktree churns these constantly,
        // which was the dominant source of redundant ~Hz hydrations. Built once
        // per retarget from the worktree's root `.gitignore` (nested `.gitignore`s
        // are rare for the high-churn dirs we care about; revisit only if
        // profiling shows residual churn). A missing/unreadable `.gitignore`
        // yields an empty matcher → every path passes → unchanged behavior, so
        // remote/provider worktrees with no local `.gitignore` are unaffected.
        // NOTE: a force-added (`git add -f`) or negate-pattern (`!keep`) ignored
        // file *can* appear in the diff and would be dropped here; that's rare,
        // and the safety-net ticker still rebuilds the panel within a few seconds.
        let ignore = {
            let mut b = ignore::gitignore::GitignoreBuilder::new(&cwd);
            let _ = b.add(".gitignore");
            b.build()
                .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
        };
        let new_watcher = recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(ev) = res
                && matches!(
                    ev.kind,
                    notify::EventKind::Modify(_)
                        | notify::EventKind::Create(_)
                        | notify::EventKind::Remove(_)
                )
                // React to real worktree edits (the diffs this watcher exists to
                // track) AND to git-state changes — commits, checkouts, branch
                // moves, rebase/merge progress — wherever they land. The latter
                // are gated through `is_git_state_path` so the index stat-cache
                // that hydration's own `git` reads rewrite (and the object-store
                // churn on commit/gc) never match: that allowlist is what keeps
                // the old self-sustaining ~2 Hz refresh loop — which once read
                // as a freeze — from coming back.
                && (ev.paths.is_empty()
                    || ev
                        .paths
                        .iter()
                        .any(|p| watcher_path_triggers_refresh(p, &roots, &ignore)))
                && last_send.elapsed() > Duration::from_millis(500)
            {
                if tx.send(RefreshKind::Model).is_ok() {
                    let _ = wake.wake();
                }
                last_send = Instant::now();
            }
        });
        if let Ok(mut nw) = new_watcher
            && nw.watch(&cwd, RecursiveMode::Recursive).is_ok()
        {
            // Linked worktree: add targeted watches on the external gitdir's
            // state-bearing subtrees. Non-recursive on the gitdir roots (so we
            // never descend into `objects/`, which floods on every commit/gc);
            // `logs/` (reflog) and `refs/` are small and never written by
            // hydration's read-only git, so a recursive watch there is storm-
            // safe. Any root already under `cwd` is skipped — the recursive
            // root watch above covers the main checkout.
            for root in [git_dir.as_ref(), common_dir.as_ref()]
                .into_iter()
                .flatten()
            {
                if root.starts_with(&cwd) {
                    continue;
                }
                let _ = nw.watch(root, RecursiveMode::NonRecursive);
                let _ = nw.watch(&root.join("logs"), RecursiveMode::Recursive);
                let _ = nw.watch(&root.join("refs"), RecursiveMode::Recursive);
            }
            if wtx.send((cwd, nw)).is_ok() {
                let _ = w.wake();
            }
        }
    });
}

/// Build the cross-worktree attention stream from every worktree's cached CI:
/// each worktree's failing runs become excerpts, grouped + sorted by
/// [`superzej_core::aggregate`]. Pure DB reads (the CI cache), so it is cheap and
/// safe to run on the model-hydration `spawn_blocking`. As dirty-file / content
/// producers land they append their excerpts here too.
fn build_across(db: &superzej_core::db::Db) -> superzej_core::aggregate::Aggregation {
    use superzej_core::aggregate::{Aggregation, ci_failure_excerpts};
    let mut excerpts = Vec::new();
    for w in db.worktrees().unwrap_or_default() {
        let label = if w.branch.is_empty() {
            w.tab_name.clone()
        } else {
            w.branch.clone()
        };
        if let Ok(Some((json, _))) = db.get_ci_cache(&w.worktree)
            && let Ok(runs) = serde_json::from_str::<Vec<superzej_core::ci::CiRun>>(&json)
        {
            excerpts.extend(ci_failure_excerpts(&w.worktree, &label, &runs));
        }
    }
    Aggregation::from_excerpts(excerpts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    fn one_tab_session() -> Session {
        Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
            active: 0,
        }
    }

    #[test]
    fn glyph_rescan_tiering() {
        let ttl = Duration::from_secs(5);
        // The active worktree always rescans, regardless of cache freshness.
        assert!(should_rescan_glyphs(true, Some(Duration::ZERO), ttl));
        assert!(should_rescan_glyphs(true, None, ttl));
        // A background worktree with no cached row must scan once to populate.
        assert!(should_rescan_glyphs(false, None, ttl));
        // A background worktree with a fresh cached row is served from cache.
        assert!(!should_rescan_glyphs(
            false,
            Some(Duration::from_secs(2)),
            ttl
        ));
        // ...and rescans once the cached row ages past the TTL.
        assert!(should_rescan_glyphs(
            false,
            Some(Duration::from_secs(6)),
            ttl
        ));
        // TTL of 0 (the env opt-out) reverts to always-rescan for background too.
        assert!(should_rescan_glyphs(
            false,
            Some(Duration::from_millis(1)),
            Duration::ZERO
        ));
    }

    #[test]
    fn glyph_scan_clean_read_updates() {
        // A fully successful read produces the scanned values and is `clean` so
        // the caller updates the cache.
        let (row, clean) = merge_glyph_scan(
            None,
            Ok(true),
            Ok(Some((4, 1))),
            Ok(Some("feat".into())),
            "/repo".into(),
        );
        assert_eq!(row, (true, 4, 1, Some("feat".into()), "/repo".into()));
        assert!(clean);
    }

    #[test]
    fn glyph_scan_no_upstream_is_zero_not_error() {
        // `Ok(None)` from ahead_behind is the genuine "no upstream" state: zero
        // arrows, and still a clean read.
        let prior: GlyphRow = (true, 4, 1, Some("feat".into()), "/repo".into());
        let (row, clean) = merge_glyph_scan(
            Some(&prior),
            Ok(false),
            Ok(None),
            Ok(Some("feat".into())),
            "/repo".into(),
        );
        assert_eq!(row, (false, 0, 0, Some("feat".into()), "/repo".into()));
        assert!(clean);
    }

    #[test]
    fn glyph_scan_transient_error_reuses_prior() {
        // A transient gix error on every read must reuse the prior row, not
        // collapse to zero/clean, and the row is NOT clean (cache untouched).
        let prior: GlyphRow = (true, 4, 1, Some("feat".into()), "/repo".into());
        let (row, clean) =
            merge_glyph_scan(Some(&prior), Err(()), Err(()), Err(()), "/repo".into());
        assert_eq!(row, (true, 4, 1, Some("feat".into()), "/repo".into()));
        assert!(!clean);
    }

    #[test]
    fn glyph_scan_partial_error_keeps_only_failed_field() {
        // ahead_behind errors (reuse prior counts) while dirty succeeds (fresh).
        let prior: GlyphRow = (true, 4, 1, Some("feat".into()), "/repo".into());
        let (row, clean) = merge_glyph_scan(
            Some(&prior),
            Ok(false),
            Err(()),
            Ok(Some("feat".into())),
            "/repo".into(),
        );
        assert_eq!(row, (false, 4, 1, Some("feat".into()), "/repo".into()));
        assert!(!clean);
    }

    #[test]
    fn glyph_scan_error_without_prior_falls_back_to_defaults() {
        // First-ever scan that errors has no prior to reuse: best-effort zeros,
        // and not clean so it won't be cached.
        let (row, clean) = merge_glyph_scan(None, Err(()), Err(()), Err(()), "/repo".into());
        assert_eq!(row, (false, 0, 0, None, "/repo".into()));
        assert!(!clean);
    }

    /// End-to-end over the I/O seam: a real temp git repo with an edited
    /// entity-bearing file → `compute_entity_summary` parses the diff + source
    /// and attributes churn to the function.
    #[test]
    fn compute_entity_summary_over_a_real_repo() {
        use superzej_core::util::{git_cmd, git_out};
        let dir = std::env::temp_dir().join(format!("sz-sem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // test code: fixture setup, never on the event loop.
        #[expect(clippy::disallowed_methods)]
        let run = |args: &[&str]| {
            assert!(
                git_cmd(&dir).args(args).status().unwrap().success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t.t"]);
        run(&["config", "user.name", "t"]);
        let file = dir.join("lib.rs");
        std::fs::write(&file, "fn greet() -> u8 {\n    1\n}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        // Edit the function body → a real `git diff HEAD`.
        std::fs::write(&file, "fn greet() -> u8 {\n    42\n}\n").unwrap();

        let loc = superzej_core::remote::GitLoc::for_worktree(&dir);
        // A non-empty diff_entries list (only its length gates the call).
        let entries = vec![superzej_svc::git::DiffEntry {
            path: "lib.rs".into(),
            added: 1,
            deleted: 1,
        }];
        let summary = compute_entity_summary(&loc, &entries).expect("entity summary");
        assert_eq!(summary.per_file.len(), 1, "{summary:?}");
        let (path, changes) = &summary.per_file[0];
        assert_eq!(path, "lib.rs");
        assert_eq!(changes[0].name, "greet");
        assert!(changes[0].added > 0 && changes[0].deleted > 0);
        let impact = summary.impact.expect("impact");
        assert_eq!(impact.entities, 1);

        // A clean repo (no diff vs HEAD) yields None. `git_out` returns None on
        // empty output, so a clean tree shows no changed names.
        run(&["checkout", "--", "lib.rs"]);
        assert!(git_out(&dir, &["diff", "--name-only", "HEAD"]).is_none());
        assert!(compute_entity_summary(&loc, &entries).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn initial_model_is_cheap_and_marks_hydration_pending() {
        let session = one_tab_session();
        let model = build_initial_model(&session, None);
        assert_eq!(model.worktree, "app/home");
        assert_eq!(model.tabs, vec!["1".to_string()]);
        assert_eq!(model.active_tab, 0);
        // The cheap initial model carries no derived rows yet (the event loop
        // builds them once view state is loaded).
        assert!(model.sidebar_rows.is_empty());
        assert!(model.panel.branch == "app/home");
        assert!(model.status.contains("Starting szhost"));
    }

    /// Workspace tuple: (slug, display, kind, repo_path).
    fn ws(slug: &str, path: &str) -> (String, String, String, String) {
        (
            slug.to_string(),
            slug.to_uppercase(),
            "repo".to_string(),
            path.to_string(),
        )
    }

    #[test]
    fn merge_keeps_db_order_and_appends_unknown_live_at_end() {
        let merged = merge_workspace_lists(
            vec![ws("alpha", "/r/alpha"), ws("beta", "/r/beta")],
            vec![ws("beta", ""), ws("gamma", "")],
        );
        let slugs: Vec<_> = merged.iter().map(|(s, _, _, _)| s.as_str()).collect();
        assert_eq!(slugs, vec!["alpha", "beta", "gamma"]);
        assert_eq!(merged[1].3, "/r/beta", "DB entry wins over live fallback");
    }

    #[test]
    fn merge_drops_stale_live_fallback_entries() {
        // "old" is a live fallback (empty path) from a workspace we already
        // switched away from: it must not survive a refresh that no longer
        // lists it as live.
        let merged = merge_workspace_lists(
            vec![ws("alpha", "/r/alpha"), ws("old", "")],
            vec![ws("alpha", "")],
        );
        let slugs: Vec<_> = merged.iter().map(|(s, _, _, _)| s.as_str()).collect();
        assert_eq!(slugs, vec!["alpha"]);
    }

    #[test]
    fn merge_is_idempotent_and_never_duplicates_by_slug() {
        let db_backed = vec![ws("alpha", "/r/alpha")];
        let live = vec![ws("alpha", ""), ws("new", "")];
        let once = merge_workspace_lists(db_backed, live.clone());
        let twice = merge_workspace_lists(once.clone(), live);
        assert_eq!(once, twice);
        assert_eq!(twice.len(), 2);
    }

    #[test]
    fn workspace_list_with_db_lists_current_workspace_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sj-hydrate-test-{}-{}/db.sqlite",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let db = superzej_core::db::Db::open_at(&p).unwrap();

        // A mixed-case repo registered in the DB, with its live home group
        // named by the canonical slug (as the host now creates it).
        db.put_workspace("/tmp/WASHU", "WASHU", "repo").unwrap();
        let slug = superzej_core::repo::repo_slug_with(&db, std::path::Path::new("/tmp/WASHU"));
        let session = Session {
            id: "/tmp/WASHU".into(),
            worktrees: vec![WorktreeGroup::new(
                superzej_core::repo::home_tab(&slug),
                GroupKind::Home,
                "/tmp/WASHU",
            )],
            active: 0,
        };

        let list = workspace_list(&session, Some(&db));
        assert_eq!(list.len(), 1, "live + DB entries collapse to one: {list:?}");
        assert_eq!(list[0].0, "washu");
        assert_eq!(
            list[0].3, "/tmp/WASHU",
            "the DB-backed entry (with path) wins"
        );
    }

    #[test]
    fn git_state_paths_signal_commits_and_branch_moves() {
        let yes = |p: &str| is_git_state_path(std::path::Path::new(p));
        // Main checkout: state files live under `<wt>/.git`.
        assert!(yes("/repo/.git/HEAD"));
        assert!(yes("/repo/.git/logs/HEAD")); // reflog — commit/checkout/reset
        assert!(yes("/repo/.git/refs/heads/main")); // branch move
        assert!(yes("/repo/.git/packed-refs"));
        assert!(yes("/repo/.git/MERGE_HEAD"));
        assert!(yes("/repo/.git/ORIG_HEAD"));
        assert!(yes("/repo/.git/rebase-merge/done")); // rebase in progress
        // Linked worktree: state lives in the main repo's external gitdir.
        assert!(yes("/repo/.git/worktrees/feat/HEAD"));
        assert!(yes("/repo/.git/worktrees/feat/logs/HEAD"));
    }

    #[test]
    fn git_state_path_ignores_churn_that_caused_the_refresh_storm() {
        let no = |p: &str| !is_git_state_path(std::path::Path::new(p));
        // The index stat-cache — hydration's own `git status`/`diff` rewrite it,
        // the ~2 Hz self-sustaining loop the allowlist exists to prevent.
        assert!(no("/repo/.git/index"));
        // Object store floods on every commit / gc.
        assert!(no("/repo/.git/objects/ab/cdef0123"));
        assert!(no("/repo/.git/objects/pack/pack-deadbeef.pack"));
        // Transient lock files (react to the final write, not the lock).
        assert!(no("/repo/.git/index.lock"));
        assert!(no("/repo/.git/refs/heads/main.lock"));
        assert!(no("/repo/.git/HEAD.lock"));
        // Editor scratch + config — not a state change.
        assert!(no("/repo/.git/COMMIT_EDITMSG"));
        assert!(no("/repo/.git/config"));
    }

    #[test]
    fn watcher_drops_gitignored_churn_but_keeps_source_and_git_state() {
        use std::path::{Path, PathBuf};
        // Matcher built like the live watcher, but from inline patterns so the
        // test needs no temp `.gitignore` on disk.
        let mut b = ignore::gitignore::GitignoreBuilder::new("/repo");
        // `/target` is the ROOT-ANCHORED form this repo's own `.gitignore` uses —
        // the fix hinges on the anchored pattern matching via parent lookup.
        b.add_line(None, "/target").unwrap();
        b.add_line(None, "*.log").unwrap();
        let ig = b.build().unwrap();
        let roots: Vec<PathBuf> = vec![PathBuf::from("/repo/.git")];
        let fires = |p: &str| watcher_path_triggers_refresh(Path::new(p), &roots, &ig);

        // Gitignored build churn — the storm this filter exists to kill.
        assert!(!fires("/repo/target/debug/szhost"));
        assert!(!fires("/repo/target/debug/.fingerprint/x"));
        assert!(!fires("/repo/run.log"));
        // Real source edits still refresh the panel.
        assert!(fires("/repo/src/main.rs"));
        assert!(fires("/repo/crates/foo/Cargo.toml"));
        // Git-state changes still refresh (the `.git` branch wins; the gitignore
        // matcher never even sees these).
        assert!(fires("/repo/.git/HEAD"));
        assert!(fires("/repo/.git/refs/heads/main"));
        // Git-internal churn stays dropped (index/objects).
        assert!(!fires("/repo/.git/index"));
        assert!(!fires("/repo/.git/objects/ab/cdef"));
    }

    #[test]
    fn empty_gitignore_matcher_passes_every_worktree_edit() {
        // Remote/provider worktrees (no local `.gitignore`) build an empty
        // matcher; it must not drop any edit — unchanged pre-filter behavior.
        use std::path::{Path, PathBuf};
        let ig = ignore::gitignore::Gitignore::empty();
        let roots: Vec<PathBuf> = vec![];
        assert!(watcher_path_triggers_refresh(
            Path::new("/wt/target/x"),
            &roots,
            &ig
        ));
        assert!(watcher_path_triggers_refresh(
            Path::new("/wt/src/main.rs"),
            &roots,
            &ig
        ));
    }

    #[test]
    fn ticker_pr_cadence_is_a_multiple_of_the_model_cadence() {
        // The ticker emits `RefreshKind::Pr` only from inside the `model_every`
        // block, so PR auto-refresh silently stops unless model_every divides
        // pr_every. Lock that for the shipped defaults.
        let base_ms = 500u64;
        assert_eq!(
            DEFAULT_MODEL_REFRESH_MS % base_ms,
            0,
            "must align to base tick"
        );
        let model_every = (DEFAULT_MODEL_REFRESH_MS / base_ms).max(1);
        let pr_every = PR_REFRESH_INTERVAL.as_millis() as u64 / base_ms;
        assert_eq!(
            pr_every % model_every,
            0,
            "pr_every={pr_every} not a multiple of model_every={model_every}"
        );
    }

    #[test]
    fn load_or_seed_session_registers_bootstrap_workspace() {
        // The bootstrap workspace must land in the `workspaces` table: without
        // a row it exists only as a live fallback in `workspace_list` and
        // vanishes from the sidebar after the first switch away.
        let state_home =
            std::env::temp_dir().join(format!("sj-hydrate-bootstrap-{}-state", std::process::id()));
        let ws_dir =
            std::env::temp_dir().join(format!("sj-hydrate-bootstrap-{}-ws", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_home);
        let _ = std::fs::remove_dir_all(&ws_dir);
        std::fs::create_dir_all(state_home.join("superzej")).unwrap();
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws_str = ws_dir.to_string_lossy().into_owned();

        // Pin SUPERZEJ_SESSION so resolution is deterministic even when the
        // test itself runs inside a live superzej.
        let _env = crate::testenv::EnvVarGuard::set(&[
            ("XDG_STATE_HOME", state_home.to_str().unwrap()),
            ("SUPERZEJ_SESSION", &ws_str),
        ]);
        let (session, seeded) = load_or_seed_session(&ws_dir);

        assert!(seeded);
        assert_eq!(session.id, ws_str);
        let db = superzej_core::db::Db::open_at(&state_home.join("superzej/superzej.db")).unwrap();
        let rows = db.workspaces().unwrap();
        let row = rows
            .iter()
            .find(|w| w.repo_path == ws_str)
            .expect("bootstrap workspace registered in the workspaces table");
        assert_eq!(row.kind, "dir", "a plain dir bootstraps as a dir workspace");

        drop(_env);
        let _ = std::fs::remove_dir_all(&state_home);
        let _ = std::fs::remove_dir_all(&ws_dir);
    }

    #[test]
    fn bootstrap_workspace_survives_switch_in_workspace_list() {
        // End-to-end regression for the disappearing-original-workspace bug:
        // bootstrap, switch to a second workspace, and the original must still
        // be listed (DB-backed, non-empty path) — not dropped as a stale live
        // fallback by merge_workspace_lists.
        let state_home =
            std::env::temp_dir().join(format!("sj-hydrate-survive-{}-state", std::process::id()));
        let ws_a =
            std::env::temp_dir().join(format!("sj-hydrate-survive-{}-a", std::process::id()));
        let ws_b =
            std::env::temp_dir().join(format!("sj-hydrate-survive-{}-b", std::process::id()));
        for d in [&state_home, &ws_a, &ws_b] {
            let _ = std::fs::remove_dir_all(d);
        }
        std::fs::create_dir_all(state_home.join("superzej")).unwrap();
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();
        let a_str = ws_a.to_string_lossy().into_owned();
        let b_str = ws_b.to_string_lossy().into_owned();

        let _env = crate::testenv::EnvVarGuard::set(&[
            ("XDG_STATE_HOME", state_home.to_str().unwrap()),
            ("SUPERZEJ_SESSION", &a_str),
        ]);
        let (mut session, _) = load_or_seed_session(&ws_a);
        let db = superzej_core::db::Db::open_at(&state_home.join("superzej/superzej.db")).unwrap();
        session.switch_to_workspace(&b_str, &db).unwrap();

        let list = workspace_list(&session, Some(&db));
        let a_slug = superzej_core::repo::repo_slug_with(&db, &ws_a);
        let entry = list
            .iter()
            .find(|(slug, _, _, _)| *slug == a_slug)
            .expect("original workspace still listed after switching away");
        assert_eq!(
            entry.3, a_str,
            "original workspace is DB-backed (non-empty path), not a live fallback"
        );

        drop(_env);
        for d in [&state_home, &ws_a, &ws_b] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
