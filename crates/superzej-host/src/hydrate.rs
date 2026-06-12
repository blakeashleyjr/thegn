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

use crate::chrome::FrameModel;
use crate::run::now_secs;

const MODEL_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PR_REFRESH_INTERVAL: Duration = Duration::from_secs(20);

/// A refresh request delivered to the event loop. `Model` rehydrates the
/// sidebar/panel/diff (cheap, gix-backed, off-thread); `Pr` additionally kicks
/// the GitHub PR-cache refresh. Both arrive event-driven (worktree fs-watch,
/// tab switch) and on a low-frequency safety-net interval.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RefreshKind {
    Model,
    Pr,
}

/// Background ticker: emits a `Model` refresh every `MODEL_REFRESH_INTERVAL` and
/// a `Pr` refresh every `PR_REFRESH_INTERVAL`, pulsing the waker so an idle loop
/// wakes to service it. This is the staleness backstop; fs-watch + on-switch
/// refresh handle the common, latency-sensitive cases.
///
/// Runs on a dedicated OS thread (not `tokio::spawn`) so it can never be starved
/// by the main loop blocking a runtime worker in `poll_input(None)` — true even
/// on a single-core runtime. The thread sleeps in 500ms half-ticks: fine enough
/// for the Telemetry section's live graphs (`stats_live` set while it's open)
/// while the model/PR cadences keep their 2s/20s rates as whole multiples of
/// the half-tick.
pub(crate) fn spawn_refresh_ticker(
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    stats_tx: tokio_mpsc::UnboundedSender<crate::stats::StatsSnapshot>,
    stats_interval_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stats_live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    waker: TerminalWaker,
) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let tick = Duration::from_millis(500);
        let model_every = MODEL_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let pr_every = PR_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let mut ticks: u64 = 0;
        // System stats for the top bar ride the same thread/cadence — the
        // /proc reads never touch the event loop.
        let mut sampler = crate::stats::StatsSampler::new();
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
            // Live mode (telemetry layer open) samples every half-tick;
            // otherwise the user-cycled rate (1/2/5/10s) is honored.
            let interval =
                Duration::from_millis(stats_interval_ms.load(Ordering::Relaxed).max(500));
            if stats_live.load(Ordering::Relaxed) || last_stats.elapsed() >= interval {
                last_stats = Instant::now();
                if stats_tx.send(sampler.sample()).is_err() {
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
    // creates) a workspace. An inherited SUPERZEJ_SESSION wins (so child shells
    // stay in the same session); otherwise we reopen the most-recently-active
    // workspace recorded in the DB (`workspaces()` is `last_active DESC`). Only
    // a genuine first run — no env, no DB history — falls back to the cwd.
    let session_name = env_session
        .clone()
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
        });
    }
    out
}

/// Gather per-worktree git/agent/activity status for every tab in the session.
/// Runs on the hydration thread (git can be slow); the event loop merges this
/// into the tree at render time. Also advances the activity FSM in-process.
fn collect_sidebar_status(
    session: &crate::session::Session,
    db: &superzej_core::db::Db,
) -> crate::sidebar::SidebarStatus {
    use superzej_core::remote::GitLoc;
    use superzej_svc::git::{GitBackend, GixGit};
    let git = GixGit::new();
    let mut status = crate::sidebar::SidebarStatus::default();
    let t0 = std::time::Instant::now();

    // Advance the activity state machine over the session's managed worktrees,
    // then read the fresh states (keyed by tab name).
    let managed: Vec<superzej_core::activity::ManagedWorktree> = session
        .worktrees
        .iter()
        .filter(|g| !g.path.is_empty())
        .map(|g| superzej_core::activity::ManagedWorktree {
            worktree: g.path.clone(),
            tab: g.name.clone(),
        })
        .collect();
    superzej_core::activity::poll_and_save(&managed);
    status.activity = superzej_core::activity::read_states()
        .into_iter()
        .map(|(tab, st)| (tab, crate::sidebar::ActivityState::from_str(&st)))
        .collect();

    // git glyphs + agent per distinct worktree path.
    let mut seen = std::collections::HashSet::new();
    for g in &session.worktrees {
        if g.path.is_empty() || !seen.insert(g.path.clone()) {
            continue;
        }
        let path = std::path::Path::new(&g.path);
        if !path.is_dir() {
            continue;
        }
        let loc = GitLoc::for_worktree(path);
        let dirty = git.is_dirty(&loc).unwrap_or(false);
        let (ahead, behind) = git.ahead_behind(&loc).ok().flatten().unwrap_or((0, 0));
        status.git.insert(
            g.path.clone(),
            crate::sidebar::GitGlyphs {
                dirty,
                ahead,
                behind,
            },
        );
        if let Ok(Some(agent)) = db.worktree_agent(&g.path) {
            status.agent.insert(g.path.clone(), agent);
        }
    }
    tracing::debug!(
        target: "szhost::hydrate",
        status_ms = t0.elapsed().as_millis() as u64,
        worktrees = seen.len(),
        "sidebar status collected"
    );
    status
}

/// tokei line count for `path`, cached in `loc_cache` (hydration thread —
/// tokei walks the whole tree). Stale cache (>5 min) refreshes in place;
/// missing tokei yields `None` and the widget hides.
fn worktree_loc(db: &superzej_core::db::Db, path: &std::path::Path) -> Option<u64> {
    const TTL_SECS: i64 = 300;
    let key = path.to_string_lossy().into_owned();
    if let Ok(Some((loc, fetched_at))) = db.get_loc_cache_entry(&key)
        && now_secs() - fetched_at < TTL_SECS
    {
        return Some(loc as u64);
    }
    let out = std::process::Command::new("tokei")
        .args(["--output", "json"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let code = v.get("Total")?.get("code")?.as_u64()?;
    let _ = db.put_loc_cache(&key, code as usize);
    Some(code)
}

/// A cheap first-frame model: no git, no diff, no DB recents. It gives the
/// user immediate chrome/status while the expensive model hydrates in the
/// background.
pub(crate) fn build_initial_model(session: &crate::session::Session) -> FrameModel {
    let active_name = session
        .active_group()
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "workspace/home".into());
    let (worktree, tabs, active_tab) = tab_strip(session);
    FrameModel {
        worktree,
        tabs,
        active_tab,
        panel: crate::panel::PanelData {
            branch: active_name,
            ..Default::default()
        },
        panel_focused: false,
        status: format!(
            "Starting szhost (build: {})… panes usable while git status hydrates",
            env!("SZHOST_BUILD_TIME")
        ),
        accent: superzej_core::theme::TEAL.to_string(),
        ..Default::default()
    }
}

/// What the open panel needs from this hydration pass — lets `build_model`
/// skip work for closed sections (the git log, the file count).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HydrateHints {
    pub open: crate::panel::Section,
    pub expanded: bool,
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
    use superzej_svc::git::{GitBackend, GixGit};

    let t0 = std::time::Instant::now();
    let cwd = active_tab_path(session);
    let loc = GitLoc::for_worktree(&cwd);
    let git = GixGit::new();
    let branch = git.current_branch(&loc).unwrap_or_else(|_| "—".into());

    let sidebar_workspaces = workspace_list(session, Some(db));
    let sidebar_db_worktrees = db_worktree_list(db);
    let sidebar_status = collect_sidebar_status(session, db);
    let loc_count = worktree_loc(db, &cwd);

    let mut panel = crate::panel::PanelData {
        branch: branch.clone(),
        ..Default::default()
    };

    // The typed PR cache: summary + checks + review threads + issues.
    if let Ok(Some((json, _))) = db.get_pr_cache(&loc.path())
        && let Ok(cached) = serde_json::from_str::<superzej_core::github::PrPanel>(&json)
    {
        apply_pr_cache(&mut panel, cached);
    }

    let diff_entries = git.diff_files(&loc, "HEAD").unwrap_or_default();
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
    let status = git.status(&loc).unwrap_or_default();
    panel.changes = crate::panel::build_change_rows(&status, &diff_entries);

    // Header zone: upstream divergence + merge-in-progress banner.
    panel.ahead_behind = git.ahead_behind(&loc).ok().flatten();
    let unresolved = superzej_svc::git::conflict_count(&status);
    let merge_info = git.merge_state(&loc).ok().flatten();
    let total = merge_total(db, &loc.path(), merge_info.is_some(), unresolved);
    panel.merge = merge_info.map(|m| crate::panel::MergeBanner {
        label: m.kind.label().to_string(),
        onto: m.onto,
        unresolved,
        total,
    });
    panel.stash_count = git.stash_count(&loc).unwrap_or(0);

    // The git section's LOG block — only fetched while that section is open.
    if hints.open == crate::panel::Section::Git {
        let n = if hints.expanded { 12 } else { 6 };
        panel.log = git.log_graph(&loc, n).unwrap_or_default();
    }

    // Tests section snapshot from the cache (summary + failures + history).
    if let Ok(Some((json, _))) = db.get_test_cache(&loc.path())
        && let Ok(cache) = serde_json::from_str::<crate::testkit::model::TestCache>(&json)
    {
        panel.tests = Some(crate::panel::tests_lite(&cache));
    }

    // Tracked-file count for the files summary — only while files is open
    // (`git ls-files` is cheap but not free on big repos every 2s).
    if hints.open == crate::panel::Section::Files
        && let Ok(out) = loc.git_command(&["ls-files"]).output()
        && out.status.success()
    {
        panel.file_count = Some(out.stdout.iter().filter(|&&b| b == b'\n').count() as u64);
    }

    tracing::debug!(
        target: "szhost::hydrate",
        build_model_ms = t0.elapsed().as_millis() as u64,
        diff_files = panel.files.len(),
        changes = panel.changes.len(),
        merging = panel.merge.is_some(),
        "model hydrated"
    );
    let (worktree, tabs, active_tab) = tab_strip(session);
    FrameModel {
        worktree,
        tabs,
        active_tab,
        sidebar_workspaces,
        sidebar_db_worktrees,
        sidebar_status,
        loc: loc_count,
        containers: superzej_core::sandbox::running_containers(),
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
        if let Ok(db) = superzej_core::db::Db::open()
            && tx
                .send((generation, build_model(&session, &db, hints)))
                .is_ok()
            && let Some(w) = &waker
        {
            let _ = w.wake();
        }
    });
}

pub(crate) fn spawn_pr_cache_refresh(
    session: crate::session::Session,
    waker: Option<TerminalWaker>,
) {
    task::spawn_blocking(move || {
        let cwd = active_tab_path(&session);
        if !cwd.is_dir() {
            return;
        }
        let loc = superzej_core::remote::GitLoc::for_worktree(&cwd);
        // The full feed: PR + checks + review threads + issues (extras are
        // best-effort and never fail the panel).
        let panel = superzej_core::github::pr_status_full(&loc);
        let Ok(json) = serde_json::to_string(&panel) else {
            return;
        };
        if let Ok(db) = superzej_core::db::Db::open() {
            let _ = db.put_pr_cache(&loc.path(), &panel.branch, &json);
        }
        // PR cache landing should surface via a model rehydrate; pulse the waker
        // so an idle loop repaints promptly.
        if let Some(w) = &waker {
            let _ = w.wake();
        }
    });
}

/// Bind (or re-bind) the diff fs-watcher to the active worktree path. A no-op if
/// the active worktree is unchanged. On a debounced filesystem event under the
/// worktree, pushes `RefreshKind::Model` and pulses the waker so the loop
/// rehydrates the diff panel promptly. The previous watcher (if any) is dropped,
/// which unregisters its watch.
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
        let mut last_send = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let wake = w.clone();
        let new_watcher = recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(ev) = res
                && matches!(
                    ev.kind,
                    notify::EventKind::Modify(_)
                        | notify::EventKind::Create(_)
                        | notify::EventKind::Remove(_)
                )
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
            && wtx.send((cwd, nw)).is_ok()
        {
            let _ = w.wake();
        }
    });
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
    fn initial_model_is_cheap_and_marks_hydration_pending() {
        let session = one_tab_session();
        let model = build_initial_model(&session);
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
}
