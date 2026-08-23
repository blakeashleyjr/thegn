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
use crate::hydrate_tuning::{bg_glyph_ttl, model_refresh_interval};
use crate::run::now_secs;
use thegn_core::store::{
    CacheStore, IntentStore, NotificationStore, WorkspaceStore, WorktreeAuxStore,
};

const PR_REFRESH_INTERVAL: Duration = Duration::from_secs(20);
const ISSUE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Cached git-glyph row for one worktree: `(dirty, ahead, behind, branch,
/// repo_root, uncommitted_add, uncommitted_del, branch_diff)`. Computing it runs
/// a full `git status` (50-150ms), so only the *active* worktree pays that every
/// Model tick; background worktrees reuse the last value until it goes stale (see
/// [`should_rescan_glyphs`]). `branch_diff` is the `(added, deleted)` total vs
/// the repo's LOCAL default branch (`thegn_svc::git::glyph_base` — not
/// `origin/HEAD`, so an unpushed trunk doesn't leak its backlog into every row),
/// `None` when no base is resolvable.
pub(crate) type GlyphRow = (
    bool,
    usize,
    usize,
    Option<String>,
    String,
    u32,
    u32,
    Option<(u32, u32)>,
);

/// Process-global staleness cache for background-worktree git glyphs. Mirrors
/// the global-state pattern of the sibling `activity` subsystem, so it needs no
/// threading through `spawn_model_hydration`'s ~dozen call sites. The `Mutex`
/// covers the (rare) case of overlapping hydrations; it's just a cache, so a
/// racing miss only costs a redundant scan.
pub(crate) fn glyph_cache()
-> &'static std::sync::Mutex<std::collections::HashMap<String, (GlyphRow, Instant)>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (GlyphRow, Instant)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(crate::warmcache::load_glyphs()))
}

/// Drop the cached glyph rows for `paths`, so the next hydration rescans them
/// instead of serving a row that is merely younger than [`bg_glyph_ttl`].
///
/// Background worktrees are TTL-cached (a rescan is a `git status` fan-out per
/// row), which is right for ambient staleness but wrong right after an event
/// that we KNOW changed their counts — a background `git fetch` moving
/// `refs/remotes/…` under every worktree of the repo. Evicting turns the
/// badge update from "within one TTL window" into "on the next hydration".
/// Cheap (a map remove per path) and safe from any thread.
pub(crate) fn invalidate_glyphs(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let mut cache = glyph_cache().lock().unwrap_or_else(|e| e.into_inner());
    for p in paths {
        cache.remove(p);
    }
}

/// Combine the registered-worktree paths and the known repo roots into the
/// set of glyph-cache keys that must stay resident across hydrations (repo
/// roots back dormant workspaces' *home* rows, which have no registry row).
/// The returned bool says whether the set is trustworthy for EVICTION —
/// `false` when either DB read failed, because evicting on a transient error
/// would blank every dormant workspace's glyphs until restart (the DB copy of
/// the cache is only re-read at launch). Seeding from a partial set is always
/// safe (seed only ever adds). Pure, so it's unit-tested.
pub(crate) fn glyph_keep_set(
    registry_paths: Option<Vec<String>>,
    repo_roots: Option<Vec<String>>,
) -> (Vec<String>, bool) {
    let keep_ok = registry_paths.is_some() && repo_roots.is_some();
    let set = registry_paths
        .into_iter()
        .flatten()
        .chain(repo_roots.into_iter().flatten())
        .filter(|p| !p.is_empty())
        .collect();
    (set, keep_ok)
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
    uncommitted: std::result::Result<(u32, u32), ()>,
    branch_diff: std::result::Result<Option<(u32, u32)>, ()>,
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
    let (add, del) = match uncommitted {
        Ok(ad) => ad,
        Err(()) => {
            clean = false;
            prior.map(|p| (p.5, p.6)).unwrap_or((0, 0))
        }
    };
    let branch_diff = match branch_diff {
        Ok(bd) => bd,
        Err(()) => {
            clean = false;
            prior.and_then(|p| p.7)
        }
    };
    (
        (
            dirty,
            ahead,
            behind,
            branch,
            repo_root,
            add,
            del,
            branch_diff,
        ),
        clean,
    )
}

/// A refresh request delivered to the event loop. `Model` rehydrates the
/// sidebar/panel/diff (cheap, gix-backed, off-thread); `Pr` additionally kicks
/// the GitHub PR-cache refresh; `Issues` kicks the issue-tracker cache refresh.
/// All arrive event-driven (worktree fs-watch, tab switch) and on low-frequency
/// safety-net intervals.
// Not `Copy`: the `CiDetail` variant boxes a `CiDetailPayload`. Every send is a
// literal and the loop drains by value, so `Copy` was never relied upon.
#[derive(Clone, Debug)]
pub(crate) enum RefreshKind {
    Model,
    Pr,
    /// The wall clock crossed a display boundary, so the `date`/`clock` bar
    /// widgets now render different text.
    ///
    /// Emitted by the ticker only when `now / period` actually changes — once a
    /// minute by default, or once a second if the configured `[bars]` formats
    /// render seconds (see [`thegn_core::config::strftime_needs_seconds`]).
    /// Before this existed, clock liveness was an accident of `StatsSnapshot`'s
    /// `uptime_secs` advancing on every stats sample, which made minute
    /// rollover land up to `[stats] refresh_secs` late and would have frozen
    /// the clock outright had that field ever stopped changing.
    ///
    /// One extra idle wake per minute — an order of magnitude fewer than the
    /// stats wakes already happening — and it only ever sets `bars_dirty`, so
    /// the frame is a two-1-row-rect recompose, never a chrome repaint.
    ClockTick,
    /// A PR-queue refresh pass: re-classify every queued pull request and act.
    /// Runs on `[pr_queue] poll_interval_secs`, and is also kicked immediately
    /// by a remote-ref move (a push) so a just-pushed fix is seen at once
    /// instead of a minute later. Inert while `[pr_queue] enabled = false`.
    PrQueue,
    Issues,
    /// CI run-history cache refresh (AV group), on its own `[ci]
    /// poll_interval_secs` cadence. `force` bypasses the `[ci] ttl_secs`
    /// skip-if-fresh guard — set by user-initiated refreshes (the `g` key,
    /// post-mutation) but not by the ticker/on-switch backstops.
    Ci {
        force: bool,
    },
    /// Per-worktree disk-size scan (off-loop `du`, cached in the DB). Slow, so
    /// it runs on a long cadence and the scan itself coalesces by `fetched_at`.
    Disk,
    /// A CI-run drill's async detail (jobs/steps + failing-log tail) fetched
    /// off-loop, delivered into the live modal overlay by
    /// [`crate::detail::apply_ci_detail`].
    CiDetail(Box<crate::detail::CiDetailPayload>),
    /// Periodic calendar sync: refresh every enabled `[[calendar.accounts]]`
    /// over the configured horizon. Runs on the shortest account interval
    /// (floored at 60s); no slot is emitted at all when nothing is configured.
    Calendar,
    /// Coarse reminder due-check over the cached events. No network, no DB
    /// write — a pure `calendar::reminders::due` call, which is why it can ride
    /// the ticker instead of needing a timer thread of its own.
    CalendarReminders,
    /// One month's calendar events, fetched off-loop when the popup lands on a
    /// month it has not cached, delivered by
    /// [`crate::detail::apply_calendar`]. Boxed so a page of events doesn't
    /// bloat every `RefreshKind`.
    CalendarMonth(Box<crate::detail::CalendarPayload>),
    /// The usage overlay's off-loop gather (per-account rate-limit windows),
    /// delivered into the live overlay by `crate::detail::apply_usage`.
    Usage(Box<crate::detail::UsagePayload>),
    /// The pane daemon's live session list, fetched over the control socket when
    /// the status modal opens (`crate::handlers::status::probe_sessions`) and
    /// delivered into it by `detail::status_modal::refresh_open`.
    DaemonSessions(Box<crate::detail::DaemonSessions>),
    /// An onboarding-wizard probe answer (gh auth / sandbox backends / ssh
    /// host), delivered into the live wizard by
    /// [`crate::handlers::onboarding::apply_probe`].
    Onboarding(Box<crate::onboarding::ProbeResult>),
    /// The repo's branch ref (e.g. `refs/heads/main`) moved out from under a
    /// checkout — an external `git update-ref` or a fold-actor CAS land in
    /// another process. Drives an off-loop, guarded fast-forward of the canonical
    /// main checkout's working tree ([`crate::git_watch::spawn_main_checkout_heal`])
    /// so a running
    /// instance whose live checkout is on that branch syncs itself instead of
    /// showing the advance as pending "changes".
    MainRefMoved,
    /// Background host-heal tick ([`crate::handlers::host_heal`]): the handler
    /// no-ops from hydrated model state unless a Failed(retryable) host exists.
    HostHeal,
    /// Poll the remote for new upstream commits (`[git] auto_fetch`) so the
    /// sidebar's `↓behind` markers and the "updates available" notification are
    /// honest without a manual fetch — see [`crate::remote_poll`]. `sweep` is set
    /// by the periodic ticker (which also rotates through the background
    /// worktrees) and clear for the event-driven triggers (startup, worktree
    /// switch), which poll only the active repo.
    AutoFetch {
        sweep: bool,
    },
    /// Offline recovery re-probe: emitted ONLY while offline (throttled by
    /// `connectivity::should_probe`), so an online machine pays nothing. The
    /// handler spawns one bounded PR fetch whose success flips the holder online.
    ConnRecover,
    /// A splash-scoped animation tick ([`crate::loading::ticker::SplashTicker`]):
    /// repaint the visible loading splash (spinner frame / elapsed / hints).
    /// The ticker thread exists ONLY while a splash is visible — the 0%-idle
    /// contract — and a straggler tick after the splash cleared leaves damage
    /// empty, so `render_plan` still Skips.
    SplashTick,
    /// The transient in-app projection of a routed notification — sent from an
    /// off-loop dispatch site ([`crate::notify::record`]) when the routing
    /// decision authorizes an in-app toast (`RouteDecision.toast`). The loop
    /// pushes it onto the [`crate::toast::Toasts`] stack (colored by priority)
    /// and schedules the one-shot expiry wake, so the transient toast and the
    /// persistent inbox entry are two views of the one routed event.
    Toast {
        message: String,
        priority: thegn_core::notification::Priority,
    },
}

const CONTAINER_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Disk-scan tick cadence. The scan is `du`-heavy, so this is a coarse backstop
/// (the per-worktree scan further skips entries refreshed within the configured
/// `[disk].scan_interval_secs`). A whole multiple of the 500ms half-tick.
const DISK_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Daemon registry-row refresh cadence (feeds the far-right chip + modal).
const DAEMON_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Ticker slot (500ms each) of the one-shot startup remote poll — 3s in, well
/// clear of the launch→first-frame path so `[git] auto_fetch` can never show up
/// in the startup waterfall.
const STARTUP_FETCH_SLOT: u64 = 6;

/// Background ticker: emits a `Model` refresh every [`model_refresh_interval`]
/// and a `Pr` refresh every `PR_REFRESH_INTERVAL`, pulsing the waker so an idle loop
/// wakes to service it. This is the staleness backstop; fs-watch + on-switch
/// refresh handle the common, latency-sensitive cases.
///
/// Also refreshes the container list on a 5s cadence (sent on `container_tx`),
/// keeping the sandbox panel live without blocking the hydration path.
///
/// One stats sample plus the wall-clock instant it was taken.
///
/// The timestamp rides *here* and deliberately **not** on `StatsSnapshot`: the
/// loop compares `model.stats != snap` to decide whether the bars need
/// repainting, and a monotonically-increasing field would make that comparison
/// always unequal — turning a fully idle machine into a 0.5–2 Hz repaint source
/// and breaking the ~0%-idle invariant. Keeping it in the envelope preserves the
/// snapshot's value semantics.
#[derive(Debug, Clone)]
pub(crate) struct StatsTick {
    pub snap: thegn_metrics::StatsSnapshot,
    /// Unix milliseconds.
    pub at_ms: u64,
}

impl StatsTick {
    /// Stamp a sample with the current wall clock.
    fn now(snap: thegn_metrics::StatsSnapshot) -> Self {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        StatsTick { snap, at_ms }
    }
}

/// Shared `(pid, pane_id)` map for process attribution.
///
/// The inner `Arc` is swapped wholesale rather than mutated, so the sampler
/// thread's read is one pointer clone under a lock held for nanoseconds.
pub(crate) type PanePids = std::sync::Arc<std::sync::Mutex<std::sync::Arc<[(u32, u32)]>>>;

/// Per-process sampling, on its **own** OS thread.
///
/// Deliberately not folded into the refresh ticker: that thread is also the
/// model/PR/CI/auto-fetch scheduler, and a full process enumeration on a
/// thousand-process box (tens of milliseconds) would delay every one of those
/// cadences. Here it delays only itself.
///
/// The whole thread is gated on `live`: when the Processes tab is closed it
/// parks on a half-tick sleep, holds no process table, and does no work at all.
pub(crate) fn spawn_proc_sampler(
    tx: tokio_mpsc::UnboundedSender<thegn_metrics::ProcSnapshot>,
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pane_pids: PanePids,
    daemon_pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
    rows: usize,
    waker: TerminalWaker,
) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let mut sampler = thegn_metrics::ProcSampler::new(rows);
        // True while the gate was open on the previous pass, so closing it can
        // release the process table exactly once.
        let mut was_live = false;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if !live.load(Ordering::Relaxed) {
                if was_live {
                    // Drop the table and the CPU baseline: a closed tab must
                    // cost nothing, and a delta across a long gap is meaningless.
                    sampler.reset();
                    was_live = false;
                }
                continue;
            }
            was_live = true;
            let now = Instant::now();
            if !sampler.due(now) {
                continue;
            }
            let pids = pane_pids.lock().map(|g| g.to_vec()).unwrap_or_default();
            sampler.set_pane_pids(pids);
            sampler.set_daemon_pid(match daemon_pid.load(Ordering::Relaxed) {
                0 => None,
                p => Some(p),
            });
            let snap = {
                let _g = crate::perf::measure(crate::perf::Subsys::Stats);
                sampler.sample()
            };
            if tx.send(snap).is_err() {
                break;
            }
            // best-effort: a gone terminal means we're shutting down anyway.
            let _ = waker.wake();
        }
    });
}

/// Runs on a dedicated OS thread (not `tokio::spawn`) so it can never be starved
/// by the main loop blocking a runtime worker in `poll_input(None)` — true even
/// on a single-core runtime. The thread sleeps in 500ms half-ticks: fine enough
/// for the Telemetry section's live graphs (`stats_live` set while it's open)
/// while the model/PR cadences (default 1s/20s, model tunable via
/// `THEGN_MODEL_REFRESH_MS`) stay whole multiples of the half-tick.
#[allow(clippy::too_many_arguments)] // one-call-site startup wiring, not an API
pub(crate) fn spawn_refresh_ticker(
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    stats_tx: tokio_mpsc::UnboundedSender<StatsTick>,
    container_tx: tokio_mpsc::UnboundedSender<Vec<thegn_core::sandbox::ContainerInfo>>,
    daemon_tx: tokio_mpsc::UnboundedSender<crate::chrome::DaemonStatus>,
    stats_interval_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stats_live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    disk_path: std::path::PathBuf,
    ci_poll_secs: u64,
    // `[pr_queue] poll_interval_secs`, or `None` when the PR queue is off — in
    // which case no PR-queue slot is ever emitted, so the feature costs nothing.
    prq_poll_secs: Option<u64>,
    auto_fetch_secs: Option<u64>,
    // Seconds per `date`/`clock` display step — 60 normally, 1 when the
    // configured `[bars]` formats render seconds. An atomic (like
    // `stats_interval_ms`) so a config reload retunes it live.
    clock_period_secs: std::sync::Arc<std::sync::atomic::AtomicU64>,
    // Seconds between calendar syncs, or `None` when no `[[calendar.accounts]]`
    // is enabled — in which case no calendar slot is ever emitted and the
    // feature costs a user without one exactly nothing.
    calendar_poll_secs: Option<u64>,
    // Whether `[calendar] reminders_enabled` is on. Gated at the ticker rather
    // than inside the handler so a user who turned reminders off pays no idle
    // wake at all, instead of waking twice a minute to learn there is nothing
    // to do.
    calendar_reminders: bool,
    waker: TerminalWaker,
) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let tick = Duration::from_millis(500);
        let model_every = (model_refresh_interval().as_millis() as u64 / 500).max(1);
        let pr_every = PR_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let ci_every = crate::ci_refresh::ci_every_slots(ci_poll_secs);
        let fetch_every = auto_fetch_secs.and_then(crate::remote_poll::fetch_every_slots);
        let issue_every = ISSUE_REFRESH_INTERVAL.as_millis() as u64 / 500;
        // Floored the same way `[pr_queue] poll_secs` is, so a misconfigured 0
        // can't spin the ticker against the forge's rate limit.
        let prq_every = prq_poll_secs.map(|s| (s.max(15) * 1000) / 500);
        // Floored the same way, so a misconfigured 0 can't spin against a
        // provider's rate limit. (`CalendarAccount::refresh_secs` already
        // clamps; this is belt-and-braces at the one place that loops.)
        let calendar_every = calendar_poll_secs
            .map(|s| (s.max(thegn_core::config_calendar::MIN_REFRESH_SECS) * 1000) / 500);
        // Reminders are checked on a coarse fixed cadence: worst-case 30s
        // lateness is irrelevant for a "10 minutes before" alert, and the check
        // is pure, so this is far cheaper than a per-reminder timer.
        let reminder_every = 60u64;
        let container_every = CONTAINER_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let disk_every = DISK_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let daemon_every = DAEMON_REFRESH_INTERVAL.as_millis() as u64 / 500;
        let heal_every = 30; // 15s host-heal consideration (backoff: core::heal)
        let mut ticks: u64 = 0;
        // System stats for the top bar ride the same thread/cadence — the
        // /proc reads never touch the event loop.
        let mut sampler = thegn_metrics::StatsSampler::new(disk_path);
        let _ = stats_tx.send(StatsTick::now(sampler.sample())); // prime rate deltas
        let mut last_stats = Instant::now();
        // Seeded from `now` so the first boundary crossing (not startup itself)
        // is what emits the first ClockTick — the initial frame already renders
        // the current time.
        let mut last_clock_unit = {
            let period = clock_period_secs.load(Ordering::Relaxed).max(1) as i64;
            chrono::Local::now().timestamp().div_euclid(period)
        };
        // Daemon/status: a read-only DB handle + this state dir's scope, read on
        // the disk cadence to fill the far-right chip's modal and to point the
        // per-process sampler at the daemon PID. Best-effort — a DB-open failure
        // just leaves the status absent (the chip falls back to NonPersist).
        let daemon_scope = crate::daemon::scope_key();
        // Opened per refresh (cheap on WAL): a failed open at startup used to
        // leave the chip a dim `○` for the whole session, and a transient
        // read error downgraded a serving daemon to "none". Both now keep the
        // last known row and log.
        let refresh_daemon =
            |sampler: &mut thegn_metrics::StatsSampler| -> Option<crate::chrome::DaemonStatus> {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let status = thegn_core::db::Db::open()
                    .and_then(|db| crate::handlers::status::snapshot(&db, &daemon_scope, now_ms));
                match status {
                    Ok(status) => {
                        sampler.set_daemon_pid(status.pid);
                        Some(status)
                    }
                    Err(e) => {
                        tracing::warn!(target: "thegn::hydrate", error = %e, "daemon registry read failed; keeping last status");
                        None
                    }
                }
            };
        // Prime once so the sampler watches the daemon PID from the first sample.
        if let Some(status) = refresh_daemon(&mut sampler) {
            let _ = daemon_tx.send(status);
        }
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
            // CI run-history on its own `[ci] poll_interval_secs` cadence (AV
            // group); the refresh itself further coalesces via `[ci] ttl_secs`.
            if ticks.is_multiple_of(ci_every) {
                if tx.send(RefreshKind::Ci { force: false }).is_err() {
                    break;
                }
                wake = true;
            }
            if ticks.is_multiple_of(issue_every) {
                if tx.send(RefreshKind::Issues).is_err() {
                    break;
                }
                wake = true;
            }
            if let Some(n) = prq_every
                && ticks.is_multiple_of(n)
            {
                if tx.send(RefreshKind::PrQueue).is_err() {
                    break;
                }
                wake = true;
            }
            // Remote poll (`[git] auto_fetch`). The one-shot STARTUP_FETCH_SLOT
            // kick is what makes a freshly-opened session show the night's
            // commits; it deliberately trails the first frame by a few seconds so
            // a network round trip can never sit on the launch path. After that
            // the configured cadence takes over (and sweeps the background
            // worktrees). Both are coalesced per-repo by `remote_poll`.
            if auto_fetch_secs.is_some()
                && (ticks == STARTUP_FETCH_SLOT
                    || fetch_every.is_some_and(|n| ticks.is_multiple_of(n)))
            {
                let sweep = ticks != STARTUP_FETCH_SLOT;
                if tx.send(RefreshKind::AutoFetch { sweep }).is_err() {
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
            // Daemon/status refresh: re-resolves the daemon PID for the
            // per-process sampler and updates the chip + modal. A cheap
            // registry read, so it runs every 10 s — with the 30 s disk slot
            // a dead daemon read "healthy" for up to 90 s.
            if ticks.is_multiple_of(daemon_every)
                && let Some(status) = refresh_daemon(&mut sampler)
            {
                if daemon_tx.send(status).is_err() {
                    break;
                }
                wake = true;
            }
            // Host-heal consideration: O(1) send; the handler no-ops unless a
            // Failed(retryable) host exists (0%-idle invariant preserved).
            if ticks.is_multiple_of(heal_every) {
                if tx.send(RefreshKind::HostHeal).is_err() {
                    break;
                }
                wake = true;
            }
            // Offline recovery: only while offline, throttled. `is_offline()` is
            // a lock-free atomic — an online machine never sends.
            if thegn_core::connectivity::is_offline() && thegn_core::connectivity::should_probe() {
                if tx.send(RefreshKind::ConnRecover).is_err() {
                    break;
                }
                wake = true;
            }
            // Coarse backstop for the main-checkout self-heal: the diff watcher
            // catches a `refs/heads/*` move sub-second, but a missed event (a
            // `packed-refs` rewrite, the watcher-retarget window, a network mount)
            // is caught here within the PR cadence. The heal itself is a cheap
            // guarded no-op when the checkout is already coherent (the common case).
            if ticks.is_multiple_of(pr_every) && tx.send(RefreshKind::MainRefMoved).is_err() {
                break;
            }
            if let Some(n) = calendar_every
                && ticks.is_multiple_of(n)
            {
                if tx.send(RefreshKind::Calendar).is_err() {
                    break;
                }
                wake = true;
            }
            if calendar_every.is_some()
                && calendar_reminders
                && ticks.is_multiple_of(reminder_every)
            {
                if tx.send(RefreshKind::CalendarReminders).is_err() {
                    break;
                }
                wake = true;
            }
            // Clock: fire only when the rendered text would actually change,
            // i.e. when the current wall time crosses a display boundary. Cheap
            // (one `now()` per half-tick, no allocation) and self-correcting
            // across suspend/resume or a wall-clock jump, because it compares
            // absolute units rather than counting elapsed ticks.
            {
                let period = clock_period_secs.load(Ordering::Relaxed).max(1) as i64;
                let unit = chrono::Local::now().timestamp().div_euclid(period);
                if unit != last_clock_unit {
                    last_clock_unit = unit;
                    if tx.send(RefreshKind::ClockTick).is_err() {
                        break;
                    }
                    wake = true;
                }
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
                if stats_tx.send(StatsTick::now(snap)).is_err() {
                    break;
                }
                wake = true;
            }
            // Container list refresh: runs OCI `ps` subprocesses, so keep it on
            // its own cadence (5s) rather than tying it to the fast stats tick.
            if ticks.is_multiple_of(container_every) {
                let containers = {
                    let _g = crate::perf::measure(crate::perf::Subsys::Container);
                    thegn_core::sandbox::running_containers()
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

/// Drop session groups whose local worktree dir has vanished (deleted/moved
/// outside thegn — including a merge-queue `on_landed = remove/detach` land),
/// forgetting their registry rows so nothing re-adopts them. Remote worktrees (a
/// `location` in the registry) are exempt — their path isn't local. Active focus
/// is re-pinned by name and the session re-persisted. Returns how many were
/// pruned. Cheap (one `is_dir` stat per group); call on a real event, never idle.
pub(crate) fn prune_stale_worktree_groups(
    session: &mut crate::session::Session,
    db: &thegn_core::db::Db,
    session_name: &str,
    cfg: &thegn_core::config::Config,
) -> usize {
    let mut ambient_cache = std::collections::HashMap::new();
    let remote: std::collections::HashSet<String> = db
        .worktrees()
        .map(|rows| {
            rows.into_iter()
                .filter(|w| {
                    row_is_remote_effective(
                        db,
                        cfg,
                        &w.location,
                        w.env_name.as_deref(),
                        &w.repo_root,
                        &mut ambient_cache,
                    )
                })
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
            // `del_worktree` cascades caches + merge-queue; the activity-FSM
            // entry is file-based and pruned separately.
            let _ = db.del_worktree(&g.path);
            thegn_core::activity::forget(&g.path);
        }
        session.active = active_name
            .and_then(|n| session.worktrees.iter().position(|g| g.name == n))
            .unwrap_or(0);
        let _ = session.persist(db, session_name, now_secs());
        tracing::info!(
            target: "thegn::startup",
            pruned = dead.len(),
            "stale worktrees pruned (dirs gone from disk)"
        );
    }
    dead.len()
}

/// Resurrect the persisted tab list, seeding a single Home tab for the current
/// worktree if the session is empty (and persisting it so the next launch
/// restores it). The native host owns this — it's the resurrect path that
/// replaced zellij's session serialization.
///
/// The `bool` is true when the session was freshly SEEDED (first launch / new
/// workspace) rather than resurrected — the launch splash shows only then.
pub(crate) fn load_or_seed_session(
    cwd: &std::path::Path,
    cfg: &thegn_core::config::Config,
) -> (crate::session::Session, bool) {
    let _span = tracing::info_span!("load_or_seed_session").entered();
    use crate::session::{GroupKind, Session, WorktreeGroup};

    let sess = thegn_core::db::session();
    let base = cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());

    let mut env_session = std::env::var("THEGN_SESSION").ok();
    if let Some(ref s) = env_session
        && s == "thegn"
    {
        // Ignore the old legacy default
        env_session = None;
    }

    let cwd_str = cwd.to_string_lossy().into_owned();
    // One DB handle for both the workspace lookup and the resurrect below —
    // every `open` re-runs pragmas + migration checks, so don't repeat it.
    // `XDG_STATE_HOME` selects the explicit DB in test/bench scenarios.
    let db = if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        let path = std::path::Path::new(&state_home).join("thegn/thegn.db");
        thegn_core::db::Db::open_at(&path)
    } else {
        thegn_core::db::Db::open()
    };

    // tg is directory-agnostic: the launch directory never selects (or
    // creates) a workspace. Resolution order:
    //   1. An inherited THEGN_SESSION (so child shells stay in the session).
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
            let s = thegn_core::util::slugify(&base);
            if s.is_empty() { "repo".to_string() } else { s }
        };
        return (
            Session {
                id: sess.to_string(),
                worktrees: vec![WorktreeGroup::new(
                    thegn_core::repo::home_tab(&slug),
                    GroupKind::Home,
                    cwd.to_string_lossy().into_owned(),
                )],
                active: 0,
            },
            true,
        );
    };

    // A resurrect ERROR (not an empty Ok) must not become an empty session: the
    // startup/quit persist does clear-then-insert, so an empty session from a
    // transient read failure (SQLITE_BUSY under a concurrent instance) would
    // DELETE the user's real persisted layout. Retry with a short backoff (the
    // busy-timeout usually clears it); only fall to default after logging loudly.
    let mut session = {
        let mut attempt = Session::resurrect_with_cfg(&db, &session_name, cfg);
        for _ in 0..3 {
            if attempt.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
            attempt = Session::resurrect_with_cfg(&db, &session_name, cfg);
        }
        attempt.unwrap_or_else(|e| {
            tracing::error!(target: "thegn::session", session = %session_name, "resurrect failed after retries ({e}); starting empty — the layout persist may be stale");
            Session::default()
        })
    };

    // git is the source of truth for worktrees on disk: drop resurrected
    // groups whose local dir vanished (deleted/moved outside thegn). Remote
    // worktrees (non-local placement) are exempt — their tree isn't on the host.
    let _ = prune_stale_worktree_groups(&mut session, &db, &session_name, cfg);

    let mut seeded = false;
    if session.worktrees.is_empty() {
        // Key the home group by the canonical DB slug (`{slug}/home`), never
        // the raw basename — the sidebar dedupes workspaces by this prefix.
        let slug = thegn_core::repo::repo_slug_with(&db, std::path::Path::new(&session_name));
        // Directory-agnostic: anchor the home group at the session's own path
        // (the resolved workspace), not the launch cwd.
        let home_path = if Path::new(&session_name).is_dir() {
            session_name.clone()
        } else {
            cwd.to_string_lossy().into_owned()
        };
        session.worktrees.push(WorktreeGroup::new(
            thegn_core::repo::home_tab(&slug),
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
    // A workspace the user explicitly removed is tombstoned (see
    // `WorkspaceStore::tombstone_workspace`): its home checkout stays on disk
    // (git is truth), so this cold start can still resolve to its directory via
    // the cwd fallback. Honour the removal — run it only as a transient live
    // fallback, never re-registering it in `workspaces` or re-pinning it active
    // — so "remove workspace" sticks instead of resurrecting on the next launch.
    let tombstoned = db.workspace_tombstoned(&session.id).unwrap_or(false);
    if !tombstoned && Path::new(&session.id).is_dir() {
        let name = Path::new(&session.id)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".into());
        // A path that resolves to a git main-worktree is a "repo" workspace;
        // anything else is a plain "dir" workspace (mirrors switch_to_workspace).
        let kind = if thegn_core::repo::main_worktree(Path::new(&session.id)).is_some() {
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
    // Skipped for a tombstoned workspace so it doesn't re-pin itself active.
    if !tombstoned {
        let _ = db.set_active_workspace(&session.id);
    }
    (session, seeded)
}

/// The identity the loop's switch detection and the switch cache key on.
/// Worktree groups key on their dir; a terminal group has no dir and every
/// terminal used to fall back to the process cwd — so terminal→terminal was
/// never detected as a switch (the tab bar kept the previous terminal's
/// `[ssh]`/backend chips) and two terminals shared one cached slice. Terminals
/// key on a synthetic, never-on-disk path instead. Use [`active_tab_path`]
/// when a real directory is needed (git reads, prefetch).
pub(crate) fn active_slice_key(session: &crate::session::Session) -> std::path::PathBuf {
    match session.active_group() {
        Some(g) if g.kind == crate::session::GroupKind::Terminal => {
            std::path::PathBuf::from(format!("\u{0}terminal:{}", g.name))
        }
        _ => active_tab_path(session),
    }
}

/// True when the active group is a terminal (no worktree dir of its own).
pub(crate) fn active_is_terminal(session: &crate::session::Session) -> bool {
    session
        .active_group()
        .is_some_and(|g| g.kind == crate::session::GroupKind::Terminal)
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

/// The worktree dirs immediately above and below the active one in the
/// sidebar's DISPLAY order (`order`: group indices as the sidebar shows them,
/// see `run::sidebar_worktree_order`) — the prefetch targets so moving to a
/// neighbor is already warm. Alt+↑/↓ steps in display order, so session-index
/// neighbors would warm the WRONG worktrees whenever pins/sort/filter reorder
/// the tree. Wraps at the ends (the cycle wraps too); falls back to session
/// ±1 when the active group isn't in `order` (e.g. filtered away). Skips the
/// active worktree and empties; the existence check lives off-loop in
/// `spawn_panel_prefetch` (no fs stat on the loop).
pub(crate) fn neighbor_worktree_paths(
    session: &crate::session::Session,
    order: &[usize],
) -> Vec<std::path::PathBuf> {
    let active = session.active;
    let neighbors: Vec<usize> = match order.iter().position(|&g| g == active) {
        Some(p) if order.len() > 1 => {
            let n = order.len();
            vec![order[(p + n - 1) % n], order[(p + 1) % n]]
        }
        Some(_) => Vec::new(),
        None => vec![active.wrapping_sub(1), active + 1],
    };
    neighbors
        .into_iter()
        .filter(|&i| i != active)
        .filter_map(|i| session.worktrees.get(i))
        .filter(|g| !g.path.is_empty())
        .map(|g| std::path::PathBuf::from(&g.path))
        .collect()
}

/// Every worktree dir in the ACTIVE worktree's workspace, in proximity order
/// from the active one (next, prev, next+1, prev-1, … in the sidebar's
/// display order, wrapping) — the widened prefetch target set, so ANY
/// in-workspace switch lands on a warm cache, not just the two immediate
/// neighbors. Skips the active worktree itself and empty paths; existence
/// checks stay off-loop in `spawn_panel_prefetch`.
pub(crate) fn workspace_worktree_paths(
    session: &crate::session::Session,
    order: &[usize],
) -> Vec<std::path::PathBuf> {
    let active_slug = session
        .worktrees
        .get(session.active)
        .and_then(|g| crate::sidebar::split_tab(&g.name).map(|(s, _)| s));
    if active_slug.is_none() {
        return neighbor_worktree_paths(session, order);
    }
    let ring: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&g| {
            session
                .worktrees
                .get(g)
                .and_then(|w| crate::sidebar::split_tab(&w.name).map(|(s, _)| s))
                == active_slug
        })
        .collect();
    let Some(p) = ring.iter().position(|&g| g == session.active) else {
        return neighbor_worktree_paths(session, order);
    };
    let n = ring.len();
    let mut out = Vec::new();
    // Proximity interleave: +1, -1, +2, -2, … (display-order distance).
    for k in 1..n {
        for idx in [(p + k) % n, (p + n - k) % n] {
            if idx == p {
                continue;
            }
            let Some(g) = session.worktrees.get(ring[idx]) else {
                continue;
            };
            if g.path.is_empty() {
                continue;
            }
            let path = std::path::PathBuf::from(&g.path);
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
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
    db: Option<&thegn_core::db::Db>,
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
            let base = thegn_core::util::slugify(&display);
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

/// Whether a registry row is a REMOTE worktree — one whose tree legitimately
/// lives off the host, so a missing local dir is NOT proof it was deleted. True
/// when the row carries a `location` (a provisioned provider), OR its env's
/// configured placement is non-local (provider/ssh/k8s). Keys off the STABLE
/// config placement, not the transient `location` string: an ssh/k8s worktree
/// never persists a `location`, and a provider worktree whose bring-up failed
/// before writing one has an empty `location` too — both would otherwise be
/// silently reaped by the local-dir reconcile the instant their host dir is
/// absent (torn down on a failed remote bring-up).
pub(crate) fn row_is_remote(
    location: &str,
    env_name: Option<&str>,
    cfg: &thegn_core::config::Config,
) -> bool {
    if !location.is_empty() {
        return true;
    }
    env_name
        .and_then(|e| cfg.env.get(e))
        .is_some_and(|e| !matches!(e.placement, thegn_core::config::PlacementMode::Local))
}

/// [`row_is_remote`], but a row with no persisted `env_name` is treated as
/// inheriting the repo's ambient env (the same precedence
/// [`crate::wizard::ambient_env_name`] / `effective_env` walk at launch). A
/// clean-inherit worktree persists a NULL `env_name` (the wizard only pins a
/// choice that DIFFERS from the ambient default), so when the ambient default
/// is itself a remote (ssh/k8s/provider) env the raw column is empty and the
/// bare [`row_is_remote`] would wrongly classify it local — and the local-dir
/// reconcile would silently reap the worktree the instant its (off-host) tree
/// isn't on disk. Resolving the ambient env here closes that gap and heals
/// already-registered NULL rows without a migration.
pub(crate) fn row_is_remote_effective(
    db: &thegn_core::db::Db,
    cfg: &thegn_core::config::Config,
    location: &str,
    env_name: Option<&str>,
    repo_root: &str,
    ambient_cache: &mut std::collections::HashMap<String, String>,
) -> bool {
    if !location.is_empty() {
        return true;
    }
    let effective = match env_name.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => e.to_string(),
        None => ambient_cache
            .entry(repo_root.to_string())
            .or_insert_with(|| {
                crate::wizard::ambient_env_name(Some(db), cfg, std::path::Path::new(repo_root))
            })
            .clone(),
    };
    row_is_remote(location, Some(&effective), cfg)
}

/// Worktrees registered in the DB, ready for the sidebar's cross-workspace
/// rows: one entry per registry row whose dir still exists (or is remote).
pub(crate) fn db_worktree_list(
    db: &thegn_core::db::Db,
    cfg: &thegn_core::config::Config,
) -> Vec<crate::sidebar::DbWorktree> {
    let mut out = Vec::new();
    let mut ambient_cache = std::collections::HashMap::new();
    for w in db.worktrees().unwrap_or_default() {
        // git is the source of truth: a LOCAL registry row whose dir vanished
        // (deleted outside thegn) is dead — delete it here (we're on the
        // hydration thread) instead of merely hiding it, so deceased
        // worktrees stop resurfacing in the tree. Remote rows (a set `location`
        // OR a non-local env placement, including one INHERITED from the repo's
        // ambient default) are exempt: their tree lives off the host, so a
        // missing local dir is not proof of deletion.
        if !row_is_remote_effective(
            db,
            cfg,
            &w.location,
            w.env_name.as_deref(),
            &w.repo_root,
            &mut ambient_cache,
        ) && !std::path::Path::new(&w.worktree).is_dir()
        {
            tracing::warn!(
                target: "thegn::hydrate",
                worktree = %w.worktree,
                tab = %w.tab_name,
                "reaping registry row: local worktree dir is gone and env resolves local"
            );
            // `del_worktree` cascades caches + merge-queue; the activity-FSM
            // entry is file-based and pruned separately.
            let _ = db.del_worktree(&w.worktree);
            thegn_core::activity::forget(&w.worktree);
            continue;
        }
        let Some((slug, branch)) = crate::sidebar::split_tab(&w.tab_name) else {
            continue;
        };
        // Degraded pin: the worktree is pinned to a managed-PROVIDER env but its
        // content resolved local (empty `location` — never provisioned, or healed
        // back after a failover). The `«env»` badge then reads `«env ✗»` so the
        // sidebar doesn't imply the pane is on the provider. Gate on provider
        // (not any non-local env): ssh/k8s with `data=sync` legitimately keep
        // content local — same discriminator as the tab-bar chip.
        //
        // A **phantom pin** — a non-default env name that isn't defined under
        // `[env.<name>]` — is also degraded: the selection was silently dropped
        // and the worktree opened local, which is exactly the failure we mark.
        let env_degraded = w.location.is_empty()
            && w.env_name.as_deref().is_some_and(|e| {
                !e.is_empty()
                    && e != "default"
                    && match cfg.env.get(e) {
                        Some(ec) => {
                            matches!(ec.placement, thegn_core::config::PlacementMode::Provider)
                        }
                        None => true,
                    }
            });
        out.push(crate::sidebar::DbWorktree {
            slug,
            branch,
            repo_path: w.repo_root.clone(),
            tab_name: w.tab_name.clone(),
            path: w.worktree.clone(),
            folder_id: w.folder_id,
            sandbox_backend: w.sandbox_backend.clone(),
            env_name: w.env_name.clone(),
            env_degraded,
        });
    }
    out
}

/// Gather per-worktree git/agent/activity status for every tab in the session.
/// Runs on the hydration thread (git can be slow); the event loop merges this
/// into the tree at render time. Also advances the activity FSM in-process.
fn collect_sidebar_status(
    session: &crate::session::Session,
    db: &thegn_core::db::Db,
    // Distinguishes real agents from tool drawers (yazi/lazygit/editor/diff) via
    // `tool_command`, so a tool auto-prewarmed on every worktree never surfaces as
    // that worktree's agent glyph — even for rows whose DB `agent` was clobbered
    // by an older build (self-healing, no migration needed).
    app_cfg: &thegn_core::config::Config,
    alert_kinds: &[&str],
    counted_kinds: &[&str],
    // The budget-governed warm/lifecycle policy: reconciles the warm set (drops
    // idle bridges so sandboxes suspend) and gates remote git-glyph scans so a
    // suspended sandbox is never woken just to refresh the sidebar.
    lifecycle: &thegn_core::config::LifecycleConfig,
) -> crate::sidebar::SidebarStatus {
    use thegn_core::remote::GitLoc;
    let mut status = crate::sidebar::SidebarStatus::default();
    let t0 = std::time::Instant::now();
    // Worktrees mid-hibernation: drives the sidebar ⏾ badge + render cache.
    status.hibernated = crate::hibernator::refresh_hibernated(db);

    // Advance the activity state machine over ALL registered worktrees,
    // then read the fresh states (keyed by tab name). This keeps background
    // agents in other workspaces ticking.
    let mut managed_map = std::collections::BTreeMap::new();
    if let Ok(db_wts) = db.worktrees() {
        for wt in db_wts {
            if !wt.worktree.is_empty() {
                managed_map.insert(
                    wt.worktree.clone(),
                    thegn_core::activity::ManagedWorktree {
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
                thegn_core::activity::ManagedWorktree {
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
        if let Some(bridge) = thegn_svc::bridge::for_loc(&loc) {
            let workdir = loc.path();
            if let Ok(m) = bridge.proc_list(std::slice::from_ref(&workdir)) {
                activity_extra.insert(w.worktree.clone(), m.get(&workdir).copied().unwrap_or(0));
            }
        }
    }
    // Second busy signal: unsolicited agent-pane output stamps published by the
    // run loop (see `agent_output`) — keeps an agent's dot `active` while it is
    // blocked on network I/O (near-zero CPU) but still redrawing its spinner.
    let output_hints = crate::agent_output::snapshot();
    // Determinism freeze: leave the activity FSM alone so dots never decay
    // and the derived needs-you chip never appears mid-spec.
    if !crate::e2e_freeze::active() {
        thegn_core::activity::poll_and_save_with(&managed, &activity_extra, &output_hints);
    }
    status.activity = thegn_core::activity::read_states()
        .into_iter()
        .map(|(tab, st)| (tab, crate::sidebar::ActivityState::from_str(&st)))
        .collect();

    // Reconcile the warm set now (after fresh activity): drop resident bridges for
    // idle, over-budget remote sandboxes so they suspend — BEFORE the glyph scan
    // below, so the just-suspended ones serve cache instead of being woken.
    crate::lifecycle::reconcile(session, app_cfg, lifecycle);
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
            // Skip tool drawers (yazi/…): they're auto-prewarmed on every switch
            // and aren't the worktree's agent. Guards stale rows too.
            if !wt.agent.is_empty() && app_cfg.tool_command(&wt.agent).is_none() {
                status.agent.insert(wt.worktree.clone(), wt.agent.clone());
            }
            if !wt.branch.is_empty()
                && !wt.repo_root.is_empty()
                && let Ok(counts) = db.get_open_pr_counts_by_branch(&wt.repo_root)
                && let Some(&n) = counts.get(&wt.branch)
                && n > 0
            {
                status.pr_counts.insert(wt.worktree.clone(), n);
                // The compact `⬡N` chip: the branch's single open PR number
                // (ambiguous multi-PR branches stay count-only).
                if let Ok(nums) = db.get_open_pr_numbers_by_branch(&wt.repo_root)
                    && let Some(&num) = nums.get(&wt.branch)
                {
                    status.pr_numbers.insert(wt.worktree.clone(), num);
                }
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
    // All registered worktree paths (every workspace) PLUS every known repo
    // root: the retain + seed passes below use them to keep/serve other
    // workspaces' glyphs across a switch. Repo roots matter because a dormant
    // workspace's *home* row renders by its repo path (`gather_groups`), which
    // has no `worktrees` registry row — without them the home row's glyphs
    // were evicted (and never re-seeded) on the first hydration after a
    // switch.
    let (all_wt_paths, keep_ok) = glyph_keep_set(
        db.worktrees()
            .ok()
            .map(|rows| rows.into_iter().map(|w| w.worktree).collect()),
        db.known_repos().ok(),
    );

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
                let warm = is_remote && thegn_svc::bridge::for_loc(&loc).is_some();
                if !thegn_core::lifecycle::should_live_scan(is_remote, warm, is_active) {
                    // No cached row: leave the glyph ABSENT (renders blank,
                    // same as never-scanned) instead of fabricating an
                    // all-zero "clean" state for a worktree that was never
                    // actually scanned — a dirty suspended sandbox must not
                    // read as clean.
                    if let Some((row, _)) = cached {
                        reused.push((p.clone(), row.clone()));
                    }
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
                    // One batched round-trip for a bridged loc (status + ahead/
                    // behind + branch), gix/CLI reads for a local one.
                    let reads = thegn_svc::git::glyph_reads(&loc);
                    let dirty = reads.dirty.map_err(|_| ());
                    let ahead_behind = reads.ahead_behind.map_err(|_| ());
                    let branch = reads.branch.map(Some).map_err(|_| ());
                    let uncommitted = reads.uncommitted.map_err(|_| ());
                    let branch_diff = reads.branch_diff.map_err(|_| ());
                    let repo_root = thegn_core::repo::main_worktree(wt)
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.clone());
                    let (row, clean) = merge_glyph_scan(
                        prior_for_scan.get(p),
                        dirty,
                        ahead_behind,
                        branch,
                        repo_root,
                        uncommitted,
                        branch_diff,
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
    //
    // Only the in-memory insert/retain runs inside the mutex; the per-worktree
    // SQLite upserts are collected here and flushed AFTER the lock is dropped.
    // The mutex is taken loop-side (`glyph_refresh::seed_from_global_cache`, on
    // every tab/worktree/pane switch), and `put_glyph_cache` is a real
    // INSERT..ON CONFLICT that can stall up to the 5s `busy_timeout` under WAL
    // write contention — holding those writes under the lock would freeze the
    // event loop for the duration. Serialize outside the critical section too.
    let to_persist: Vec<(String, String)> = {
        let mut cache = glyph_cache().lock().unwrap();
        let mut persist = Vec::new();
        for (p, row, clean) in &scanned {
            if *clean {
                cache.insert(p.clone(), (row.clone(), now));
                persist.push(glyph_persist_entry(p, row));
            }
        }
        // Keep every registered worktree's (and repo root's) glyph resident,
        // not just the active session's — the DB copy is only re-read at
        // start, so a session-scoped retain would evict (and blank on switch)
        // other workspaces. Dead rows still get pruned — but only when the
        // registry reads succeeded, so a transient DB error can't masquerade
        // as an empty registry and flush the cache.
        if keep_ok {
            cache
                .retain(|k, _| paths.iter().any(|p| p == k) || all_wt_paths.iter().any(|p| p == k));
        }
        persist
    };
    // best-effort: the glyph cache is a warm-start convenience; git is the source
    // of truth. These writes are now outside the mutex so a stalled WAL write
    // can never block a loop-side `seed_from_global_cache`.
    for (p, json) in &to_persist {
        let _ = db.put_glyph_cache(p, json);
    }

    let scanned_n = scanned.len();
    let git_rows = scanned
        .into_iter()
        // A FIRST-ever scan that errored has nothing to merge prior values
        // from — `merge_glyph_scan` fell back to all-zeros. Leave the row
        // absent (blank) rather than rendering a dirty-but-unreadable
        // worktree as confidently clean; the next tick retries. A degraded
        // rescan (prior existed) keeps its merged last-known row as before.
        .filter(|(p, _, clean)| *clean || prior_for_scan.contains_key(p))
        .map(|(p, row, _clean)| (p, row))
        .chain(reused)
        .map(
            |(p, (dirty, ahead, behind, branch, repo_root, add, del, branch_diff))| {
                (
                    p,
                    dirty,
                    ahead,
                    behind,
                    branch,
                    repo_root,
                    add,
                    del,
                    branch_diff,
                )
            },
        );
    for (path, dirty, ahead, behind, branch, repo_root, add, del, branch_diff) in git_rows {
        status.git.insert(
            path.clone(),
            crate::sidebar::GitGlyphs {
                dirty,
                ahead,
                behind,
                add,
                del,
                branch_diff,
            },
        );
        if let Ok(Some(agent)) = db.worktree_agent(&path)
            && app_cfg.tool_command(&agent).is_none()
        {
            status.agent.insert(path.clone(), agent);
        }
        // The live HEAD branch drives the row's displayed branch — the tab name
        // is a creation-time identity and never tracks a `git checkout`.
        if let Some(branch) = &branch {
            status.branches.insert(path.clone(), branch.clone());
        }
        // PR badge: open PRs for this worktree's current branch, joined from the
        // repo-wide `pr_branch_cache` (keyed by repo root, so every worktree of
        // the repo — not just the active one — resolves its branch's count).
        // The live HEAD is authoritative for scanned rows: clear the entry the
        // registry pass above seeded from the CREATION-time branch first, or a
        // `git checkout` inside the worktree keeps showing the old branch's
        // count forever.
        status.pr_counts.remove(&path);
        status.pr_numbers.remove(&path);
        if let Some(branch) = branch
            && let Ok(counts) = db.get_open_pr_counts_by_branch(&repo_root)
            && let Some(&n) = counts.get(&branch)
            && n > 0
        {
            status.pr_counts.insert(path.clone(), n);
            // The compact `⬡N` chip: the branch's single open PR number
            // (ambiguous multi-PR branches stay count-only).
            if let Ok(nums) = db.get_open_pr_numbers_by_branch(&repo_root)
                && let Some(&num) = nums.get(&branch)
            {
                status.pr_numbers.insert(path.clone(), num);
            }
        }
    }
    // Serve other workspaces' last-known glyphs from cache (never scanning, never
    // wakes a sandbox) so a switch shows them instantly instead of blank.
    crate::glyph_refresh::seed_from_global_cache(
        &mut status.git,
        &mut status.branches,
        all_wt_paths.iter().cloned(),
    );

    // Attention scores + hysteresis-stable ranks (pure DB/snapshot reads; the
    // branching lives in core). After the git pass so `dirty` is fresh.
    crate::attention_status::collect_attention(session, db, &mut status);

    tracing::debug!(
        target: "thegn::hydrate",
        status_ms = t0.elapsed().as_millis() as u64,
        worktrees = paths.len(),
        scanned = scanned_n,
        cached = paths.len().saturating_sub(scanned_n),
        "sidebar status collected"
    );
    status
}

/// Serialize one clean glyph row into the `(worktree, json)` pair persisted to
/// the `glyph_cache` table. Split out of `collect_sidebar_status` so the DB
/// upserts can be batched and flushed *after* the process-global glyph mutex is
/// released — the mutex is also taken loop-side, so a stalled WAL write inside
/// it would freeze the event loop. Pure, so it's unit-tested.
fn glyph_persist_entry(path: &str, row: &GlyphRow) -> (String, String) {
    (
        path.to_string(),
        serde_json::to_string(row).unwrap_or_default(),
    )
}

/// tokei line count for `path`, cached in `loc_cache` (hydration thread —
/// tokei walks the whole tree). Stale cache (>5 min) refreshes in place;
/// missing tokei yields `None` and the widget hides.
fn worktree_loc(
    db: &thegn_core::db::Db,
    path: &std::path::Path,
    files_open: bool,
) -> Option<thegn_core::loc::LocReport> {
    use thegn_core::loc::LocReport;
    const TTL_SECS: i64 = 300;
    let key = path.to_string_lossy().into_owned();
    let cached = db.get_loc_cache_entry(&key).ok().flatten();
    if let Some((json, fetched_at)) = &cached
        && now_secs() - fetched_at < TTL_SECS
        && let Ok(report) = serde_json::from_str::<LocReport>(json)
    {
        return Some(report);
    }
    // The tokei walk is a synchronous full-tree scan that stalls the whole
    // hydration pass on big repos — pay it only while the Files section (its
    // consumer) is actually open; otherwise serve whatever the cache holds,
    // however old.
    if !files_open {
        return cached.and_then(|(json, _)| serde_json::from_str::<LocReport>(&json).ok());
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
    db: Option<&thegn_core::db::Db>,
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
    // Seed the sidebar's dynamic (OSC) worktree titles from the DB so persisted
    // titles show on the very first frame — before any pane re-emits one. The
    // loop's live-title merge then refreshes them in place.
    let sidebar_window_titles = db
        .and_then(|d| d.all_worktree_titles().ok())
        .unwrap_or_default();
    FrameModel {
        worktree,
        tabs,
        active_tab,
        sidebar_workspaces,
        sidebar_window_titles,
        active_container_name: thegn_core::sandbox::container_name_with_profile(
            &cwd.to_string_lossy(),
            Some(&thegn_core::profile::name()),
        ),
        panel: crate::panel::PanelData {
            branch: active_name,
            ..Default::default()
        },
        panel_focused: false,
        status: format!(
            "Starting thegn (build: {})… panes usable while git status hydrates",
            env!("THEGN_BUILD_TIME")
        ),
        load_steps: vec![
            LoadStep::pending("sandbox"),
            LoadStep::pending("container"),
            LoadStep::pending("shell"),
        ],
        accent: thegn_core::theme::TEAL.to_string(),
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
    /// Pre-warm the active worktree's `git log` commit cache on a switch even
    /// with the Commits section closed, so opening it is instant. Off the ticker.
    pub warm_commits: bool,
    /// Populate the closed git-family summaries (branch count + PR badges,
    /// first commit) from cache whenever the panel is visible, so they don't
    /// read `—` until the section is opened. Reads caches always; triggers the
    /// underlying `branches_full` subprocess only on a *cold* miss (not on TTL
    /// staleness), so the periodic ticker cost stays ~0. (Stash needs no list —
    /// its summary reads the always-fetched `stash_count`.)
    pub warm_git_summaries: bool,
}

impl HydrateHints {
    /// The Commits list is genuinely on screen (open section or the full git
    /// frame) — drives the TTL refresh + loading spinner. A `warm_git_summaries`
    /// pass reads the cache for the closed-row summary but does NOT count here,
    /// so warming never forces a per-tick `git log` (see `build_panel`).
    fn wants_commits(&self) -> bool {
        self.open == crate::panel::Section::Commits || (self.expanded && self.open.is_git_family())
    }

    /// The hints every switch-time hydration/prefetch builds identically: open
    /// section, expanded width, active profile (`warm_commits` stays per-call).
    pub(crate) fn for_switch(ui: &crate::panel::PanelUi, cfg: &thegn_core::config::Config) -> Self {
        HydrateHints {
            open: ui.open,
            expanded: ui.width.is_expanded(),
            profile: cfg.profile.clone(),
            ..Default::default()
        }
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
        || thegn_core::util::now().saturating_sub(*fetched_at) >= COMMIT_CACHE_TTL_SECS
}

// Closed-summary staleness bound: the collapsed `commits` row shows the latest
// sha + subject from this cache; refreshing it only on a COLD miss left that
// row unboundedly stale (a commit made in a terminal pane never appeared until
// the section was opened). One `git log -80` a minute is cheap.
const COMMIT_SUMMARY_TTL_SECS: i64 = 60;

/// Whether to (re)build the commit list this pass. An on-screen Commits section
/// refreshes on the TTL; a `warm_git_summaries` pass (closed summary) reloads
/// on a cold miss or once its own (longer) TTL lapses. Pure — unit-tested.
fn commit_load_needed(commits_open: bool, cache: Option<&(String, i64)>) -> bool {
    if commits_open {
        commit_cache_needs_refresh(cache)
    } else {
        match cache {
            None => true,
            Some((_, fetched_at)) => {
                thegn_core::util::now().saturating_sub(*fetched_at) >= COMMIT_SUMMARY_TTL_SECS
            }
        }
    }
}

/// Whether to run the repo-global `branches_full` subprocess this pass. Same
/// shape as `commit_load_needed`: TTL refresh when the Branches section is on
/// screen, cold-miss-only when merely warming a closed summary. Pure.
fn branch_fetch_needed(
    want: bool,
    branches_open: bool,
    cached_age: Option<std::time::Duration>,
    ttl: std::time::Duration,
) -> bool {
    if !want {
        return false;
    }
    if branches_open {
        crate::branch_cache::should_refetch(cached_age, ttl)
    } else {
        cached_age.is_none()
    }
}

fn refresh_commit_cache(db: &thegn_core::db::Db, session: &crate::session::Session) -> bool {
    use thegn_core::remote::GitLoc;
    use thegn_svc::git::{CliGit, GitBackend};

    let cwd = active_tab_path(session);
    if !cwd.is_dir() {
        return false;
    }
    let loc = GitLoc::for_worktree(&cwd);
    let rows = match CliGit.log_commits(&loc, 80) {
        Ok(rows) => rows,
        Err(_) => {
            // Distinguish "no commits yet" (unborn HEAD — `git log` exits 128
            // on a fresh `git init`) from a transient failure: write an EMPTY
            // cache row for the former so the section renders "no commits"
            // instead of spinning "loading commits…" forever while respawning
            // a doomed `git log` every tick. A transient failure leaves the
            // cache alone (stale beats blank).
            let unborn = loc
                .git_out(&["rev-parse", "--verify", "HEAD"])
                .is_none_or(|s| s.trim().is_empty());
            if unborn {
                return db
                    .put_commit_cache(&GitLoc::worktree_cache_key(&cwd), "[]")
                    .is_ok();
            }
            return false;
        }
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
        .and_then(|json| {
            db.put_commit_cache(&GitLoc::worktree_cache_key(&cwd), &json)
                .ok()
        })
        .is_some()
}

/// Whether a freshly-fetched PR panel state is DEFINITIVE — i.e. an authoritative
/// answer worth persisting to the `pr_cache`, as opposed to a transient failure
/// (`Error`/`Offline`/`RateLimited`) that must never overwrite a good cached row.
/// `Pr`/`NoPr`/`NotAuthenticated`/`NoGh` are all real answers about the PR/auth
/// state; the transient trio are network/quota blips. Pure, so it's unit-tested.
fn pr_state_is_definitive(state: &thegn_core::github::PanelState) -> bool {
    use thegn_core::github::PanelState;
    !matches!(
        state,
        PanelState::Error { .. } | PanelState::Offline | PanelState::RateLimited
    )
}

/// Map the typed PR cache into the panel's pr/checks/threads/issues fields.
fn apply_pr_cache(panel: &mut crate::panel::PanelData, cached: thegn_core::github::PrPanel) {
    use thegn_core::github::{Bucket, PanelState, check_bucket};
    let now = thegn_core::util::now();
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
            panel.pr_head_oid = pr.head_ref_oid.clone();
            panel.pr_mergeable = pr.mergeable.clone();
            panel.pr_merge_state = pr.merge_state_status.clone();
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
    db: &thegn_core::db::Db,
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

/// The first-frame status line: a few orienting chords plus the build stamp.
///
/// Chords resolve through the keymap registry (each action's `hint` supplies
/// the label), so a rebind is reflected here instead of leaving a wrong chord
/// on the very first thing a user reads. An action with no binding is dropped
/// rather than rendered stale.
pub(crate) fn startup_status_line(cfg: &thegn_core::config::Config) -> String {
    let mut parts: Vec<String> = Vec::new();
    for id in ["palette", "new-worktree", "switch-workspace", "quit"] {
        let (Some(chord), Some(spec)) = (
            crate::keymap::chord_hint_for(cfg, id),
            crate::keymap::action_spec(id),
        ) else {
            continue;
        };
        parts.push(format!("{chord} {}", spec.hint));
    }
    format!(
        "{}  [build {}]",
        parts.join("   "),
        env!("THEGN_BUILD_TIME")
    )
}

/// Build the chrome model from the resurrected session + the current worktree's
/// git state (best-effort — the host stays up even with no repo / no DB). This
/// is the in-process data flow the chrome relies on: read core + svc directly,
/// no IPC. This can be slow on large repos, so launch calls it on a background
/// worker after the first frame is already possible.
pub(crate) fn build_model(
    session: &crate::session::Session,
    db: &thegn_core::db::Db,
    hints: HydrateHints,
) -> FrameModel {
    use thegn_core::remote::GitLoc;

    let t0 = std::time::Instant::now();
    let cwd = active_tab_path(session);
    let loc = GitLoc::for_worktree(&cwd);
    // Record the active worktree's log tag so the Logs section can filter the
    // shared thegn.log tail to this worktree's + host-global lines by default.
    crate::panel::scope::set_active_wt_tag(&thegn_core::log_trace::wt_slug(&cwd));

    // Single layered-config load reused for notification priority + tasks below
    // (CLI overrides + DB-defined hosts included — see `load_hydration_config`).
    let app_cfg = load_hydration_config();
    // Mirror the active repo's merged-worktree grace period for the merge
    // section's countdown. Resolved per-repo (so a `[workspace.<slug>]` override
    // counts) and zeroed under any `on_landed` without a grace period, which the
    // renderer reads as "no countdown".
    crate::panel::scope::set_merged_ttl_secs(
        crate::integrate::main_checkout(&cwd)
            .map(|root| app_cfg.repo_merge_queue(&root))
            .filter(|mq| mq.on_landed == thegn_core::config::OnLanded::Expire)
            .map_or(0, |mq| mq.merged_ttl_secs),
    );
    let alert_kinds = app_cfg.notifications.alert_kind_names();
    let counted_kinds = app_cfg.notifications.counted_unread_kind_names();

    let sidebar_workspaces = workspace_list(session, Some(db));
    // Folders for every workspace shown in the sidebar (not just the active
    // tab's): the sidebar filters this list per-workspace by `repo_path`, so a
    // worktree filed into a folder stays visible whichever tab is active.
    let sidebar_db_folders: Vec<thegn_core::models::FolderRow> = sidebar_workspaces
        .iter()
        .filter(|(_, _, _, repo)| !repo.is_empty())
        .flat_map(|(_, _, _, repo)| db.folders_for_workspace(repo).unwrap_or_default())
        .collect();
    let sidebar_db_worktrees = db_worktree_list(db, &app_cfg);
    let sidebar_db_terminals = crate::hydrate_terminal::sidebar_terminals(db);
    // One-shot at process start: collapse any stale running/active activity dot
    // (a session killed mid-run) to a settled state before the sidebar first
    // paints, so a phantom forever-running dot never survives resurrection. The
    // live FSM re-derives the true state from fresh CPU deltas on the next poll.
    {
        use std::sync::Once;
        static RESTORE_COERCE: Once = Once::new();
        RESTORE_COERCE.call_once(|| {
            let grace_ms = app_cfg.session.restore_grace_secs.saturating_mul(1000);
            thegn_core::activity::coerce_stale_states(grace_ms);
        });
    }
    let sidebar_status = collect_sidebar_status(
        session,
        db,
        &app_cfg,
        &alert_kinds,
        &counted_kinds,
        &app_cfg.lifecycle,
    );
    // Self-throttled housekeeping (network/DB on own threads): VPS leak reaper
    // + placement engine + hibernator (snapshot-then-destroy for idle VMs).
    crate::vps_reaper::tick(&app_cfg);
    crate::fly_reaper::tick(&app_cfg);
    crate::placement_flow::maintain_tick(&app_cfg);
    crate::hibernator::tick(session, &app_cfg);
    let loc_count = worktree_loc(db, &cwd, hints.open == crate::panel::Section::Files);

    // Terse placement kind (ssh/mosh/k8s/<provider>) for the active worktree's
    // tab bar; pure config resolve, canonical repo_root from the sidebar list.
    // Key the DB lookups by the HOST worktree path (`cwd`), NOT `loc.path()`:
    // for a provider worktree `loc.path()` is the in-sandbox path, so the
    // host-path-keyed `worktrees` row (env pin, repo root) never matched and
    // the chip fell through to the local default env — rendering a bogus
    // `(bwrap)` backend chip on a machine0/sprite worktree.
    let active_path = cwd.to_string_lossy().into_owned();
    // Resolve the repo root the SAME way the loading splash + launch path do
    // (`db.repo_root_for` → `main_worktree`), not just the sidebar list: a
    // worktree that isn't in `sidebar_db_worktrees` yet (freshly created, still
    // provisioning) fell back to the worktree PATH as the repo root, so the
    // `effective_env`/workspace lookup below missed → env resolved to the LOCAL
    // default → a bogus `(bwrap)` backend chip on a provider (sprites/machine0)
    // worktree, diverging from the splash. Match the splash's resolution.
    let active_repo = sidebar_db_worktrees
        .iter()
        .find(|w| w.path == active_path)
        .map(|w| w.repo_path.clone())
        .or_else(|| db.repo_root_for(&active_path).ok().flatten())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            thegn_core::repo::main_worktree(std::path::Path::new(&active_path))
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| active_path.clone());
    // Use the EFFECTIVE env selection (per-worktree override, else the
    // workspace default) — the same source the loading splash's
    // `agent::loading_context` uses. Reading raw `worktree_env` here missed the
    // workspace-default case: a worktree created under a workspace whose default
    // env is a provider (e.g. `sprites`) has no per-worktree row, so the tab bar
    // resolved it as Local — no `[sprites]` chip and a bogus `(bwrap)` backend
    // chip — while the splash correctly showed the provider placement.
    let active_env = app_cfg.resolve_env(
        std::path::Path::new(&active_repo),
        &loc,
        std::path::Path::new(&active_path),
        db.effective_env(&active_path, &active_repo).as_deref(),
    );
    // A provider env whose CONTENT resolved local has degraded to the host
    // (provider unavailable + `failover`, or a stale remote location healed back
    // to local — see the `should_heal_degraded_location` heal on open). A healthy
    // provider always persists a `GitLoc::Provider`, so `loc == Local` for a
    // provider env is the durable "runs on the host" signal. Suppress the
    // placement chip entirely: don't claim the pane is on the provider when it is
    // really on the host. (ssh/k8s with `data=sync`/sshfs also keep content local
    // yet run remote, so gate on `is_provider()` — not "any non-local placement".)
    let loc_is_local = matches!(loc, GitLoc::Local(_));
    let degraded_to_host = active_env.placement.is_provider() && loc_is_local;
    let show_placement = !active_env.placement.is_local() && !degraded_to_host;
    let active_placement_kind = show_placement.then(|| active_env.placement.kind());
    let active_placement_label = show_placement.then(|| active_env.placement.label());

    let panel = build_panel(&cwd, db, &hints, &app_cfg);

    // Decorate the tab-bar placement chip with the backing host's readiness
    // (hosts-as-resources): `[ssh]` stays clean when the host is ready,
    // `[ssh ~<step>]` mid-provision, `[ssh !]` when it failed.
    let active_placement_kind = active_placement_kind.map(|kind| {
        let status = crate::host_ui::env_host_status(&app_cfg, &active_env.name, &panel.hosts);
        crate::host_ui::decorate_placement_kind(&kind, status.as_deref())
    });

    // Sandbox backend for the tab-bar `(backend)` chip; see `hydrate_terminal`.
    // For a REMOTE/provider placement the host `SandboxConfig` backend (e.g.
    // `bwrap`) is irrelevant — the sprite/provider IS the environment — and the
    // fallback that reads it produced a misleading `(bwrap)` chip next to the
    // sprite. The `[kind]` placement chip carries the environment instead.
    // Gate on the loc too: a worktree whose CONTENT lives remote (persisted
    // provider/ssh location) must never show a local backend chip, even when
    // env resolution degrades to Local (missing `[env.*]`, config drift).
    // (`loc_is_local` was bound above for the placement-chip degrade check.)
    if active_env.placement.is_local() && !loc_is_local {
        tracing::warn!(
            worktree = %active_path,
            env = %active_env.name,
            "worktree has a remote/provider location but its env resolved to a \
             local placement — check the worktree's env pin / [env.*] config"
        );
    }
    // A provider env that DEGRADED to the host runs in the host sandbox, so its
    // backend chip is the truthful one to show (the placement chip is already
    // suppressed above); blanking both left the degraded worktree
    // indistinguishable from a plain unsandboxed local one.
    let runs_on_host = (active_env.placement.is_local() || degraded_to_host) && loc_is_local;
    let active_sandbox_backend = if runs_on_host {
        crate::hydrate_terminal::active_backend(db, &loc.path(), active_env.sandbox.backend)
    } else {
        String::new()
    };

    // A TERMINAL tab has no worktree path, so the `resolve_env` above resolved the
    // launch cwd's workspace/global `[sandbox] default_env` — which wrongly labeled
    // a plain local shell with the workspace's default provider env (e.g.
    // `machine0`). A terminal's env is its own connection + sandbox, so override
    // the tab-bar chip triple from its DB row.
    let (active_placement_kind, active_placement_label, active_sandbox_backend) = if session
        .active_group()
        .is_some_and(|g| g.kind == crate::session::GroupKind::Terminal)
    {
        let name = session.active_group().map(|g| g.name.as_str());
        let row = name.and_then(|n| sidebar_db_terminals.iter().find(|t| t.name == n));
        crate::hydrate_terminal::terminal_env(row)
    } else {
        (
            active_placement_kind,
            active_placement_label,
            active_sandbox_backend,
        )
    };

    tracing::debug!(
        target: "thegn::hydrate",
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
        // Host-path identity, never `loc.path()`: a provider worktree's
        // in-sandbox path (`/workspace`) is shared by every provider worktree,
        // so both the container name and the audit-event key collided.
        active_container_name: thegn_core::sandbox::container_name_with_profile(
            &thegn_core::remote::GitLoc::worktree_cache_key(&cwd),
            if hints.profile.is_empty() {
                None
            } else {
                Some(&hints.profile)
            },
        ),
        active_sandbox_backend,
        active_placement_kind,
        active_placement_label,
        // containers is populated by the dedicated container refresh ticker
        // (run.rs) rather than inline here, to avoid blocking model hydration
        // on `podman ps` subprocess calls.
        containers: vec![],
        container_events: db
            .container_events(&thegn_core::remote::GitLoc::worktree_cache_key(&cwd), 10)
            .unwrap_or_default(),
        // Unified timeline: sandbox audit events, newest-first. A small
        // off-loop read on the hydration thread (never the event loop).
        timeline: thegn_core::models::merge_timeline(
            &db.container_events(&thegn_core::remote::GitLoc::worktree_cache_key(&cwd), 20)
                .unwrap_or_default(),
            20,
        ),
        panel,
        panel_focused: false,
        // `thegn open` mailbox: claim-and-delete on this hydration pass;
        // tolerates a DB missing the table (unmerged parallel-branch schema).
        intents: db.take_intents("focus_workspace").unwrap_or_default(),
        // `status` is loop-owned (`handlers::status_line`); never seeded here.
        accent: thegn_core::theme::TEAL.to_string(),
        connectivity: thegn_core::connectivity::current(),
        ..Default::default()
    }
}

/// Build just the right-side panel for a worktree directory. This is the
/// path-keyed core of model hydration: it touches only `cwd`/`db`/`hints`,
/// never the session, so a background task can warm a not-yet-focused
/// worktree's panel into the switch cache before the user lands on it.
pub(crate) fn build_panel(
    cwd: &std::path::Path,
    db: &thegn_core::db::Db,
    hints: &HydrateHints,
    app_cfg: &thegn_core::config::Config,
) -> crate::panel::PanelData {
    use thegn_core::remote::GitLoc;
    use thegn_svc::git::{GitBackend, GixGit};

    let loc = GitLoc::for_worktree(cwd);

    // Section-gate flags precomputed as plain `Copy` values so the fan-out
    // closures below never capture `&HydrateHints` (keeps them trivially `Send`).
    let want_log = hints.open == crate::panel::Section::Pr;
    let log_n = if hints.expanded { 12 } else { 6 };
    // The Full git frame shows every list, so any open git-family section at
    // Full hydrates branches + stashes too.
    let git_family_full = hints.expanded && hints.open.is_git_family();
    // The Branches summary (count + PR badges) needs the branch list, so warming
    // pulls it in too — but only from cache (see `need_branch_fetch` below, which
    // restricts the subprocess to a cold miss when merely warming).
    let want_branches = hints.open == crate::panel::Section::Branches
        || git_family_full
        || hints.warm_git_summaries;
    let want_stashes = hints.open == crate::panel::Section::Stash || git_family_full;
    let want_lsfiles = hints.open == crate::panel::Section::Files;
    // When the Branches section is actually open (or the full git frame is up),
    // keep the TTL refresh; when only warming a closed summary, fetch solely on a
    // cold cache miss so the ticker never re-runs `branches_full` every 5s.
    let branches_open = hints.open == crate::panel::Section::Branches || git_family_full;

    // Branches are repo-global (all worktrees share the same ref store), so the
    // heavy `branches_full` subprocess runs at most once per repo and is shared
    // across every tab via `branch_cache`. Only compute the repo root / consult
    // the cache when a section actually needs the list. The one per-worktree
    // field, `is_head`, is recomputed below from this worktree's `branch`.
    let repo_root = want_branches
        .then(|| thegn_core::repo::main_worktree(cwd).unwrap_or_else(|| cwd.to_path_buf()));
    let cached_branches = repo_root.as_deref().and_then(crate::branch_cache::get);
    let need_branch_fetch = branch_fetch_needed(
        want_branches,
        branches_open,
        cached_branches.as_ref().map(|(_, age)| *age),
        crate::branch_cache::BRANCH_CACHE_TTL,
    );

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
        fetched_branches,
        stashes_raw,
        ls_files,
        incoming,
    ) = std::thread::scope(|s| {
        // Raw `Result`s (branch/ahead/merge) merged post-scope: `panel_header_cache`.
        let h_branch = s.spawn(|| GixGit::new().current_branch(&loc).map_err(|_| ()));
        // diff + the semantic entity summary share the diff result and need only
        // `loc`, so they ride one thread (entity parsing is CPU, kept off the rest).
        let h_diff = s.spawn(|| {
            let entries = GixGit::new().diff_files(&loc, "HEAD").unwrap_or_default();
            let entities = crate::hydrate_semantic::compute_entity_summary(&loc, &entries);
            (entries, entities)
        });
        let h_status = s.spawn(|| GixGit::new().status(&loc).unwrap_or_default());
        let h_ahead = s.spawn(|| GixGit::new().ahead_behind(&loc).map_err(|_| ()));
        let h_merge = s.spawn(|| GixGit::new().merge_state(&loc).map_err(|_| ()));
        // While a merge/rebase is live, the working tree/index carries the whole
        // incoming diff staged, so the changes list is dominated by files the
        // *merge* brings in, not the user's own edits. Compute the incoming path
        // set (files that differ on the incoming side since the merge base:
        // `git diff HEAD...<HEAD-ref>`) so `build_change_rows` can tag and group
        // them apart. Empty (and near-free) outside a merge.
        let h_incoming = s.spawn(|| {
            GixGit::new()
                .merge_state(&loc)
                .ok()
                .flatten()
                .map(|mi| {
                    GixGit::new()
                        .diff_files(&loc, &format!("HEAD...{}", mi.kind.head_ref()))
                        .unwrap_or_default()
                        .into_iter()
                        .map(|d| d.path)
                        .collect::<std::collections::HashSet<String>>()
                })
                .unwrap_or_default()
        });
        let h_stash_count = s.spawn(|| GixGit::new().stash_count(&loc).unwrap_or(0));
        // Section-gated heavy reads: spawned only when their section is open, so
        // an idle panel pays nothing. The branch PR-badge join is DB-backed and
        // stays on the main thread below; only the raw `branches_full` runs here.
        let h_log =
            want_log.then(|| s.spawn(|| GixGit::new().log_graph(&loc, log_n).unwrap_or_default()));
        // Only re-run the subprocess on a repo-cache miss/stale; a warm entry
        // from another tab (or an earlier hydration) is reused verbatim below.
        let h_branches = need_branch_fetch
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
            h_incoming.join().unwrap(),
        )
    });
    tracing::debug!(
        target: "thegn::hydrate",
        panel_git_ms = t_git.elapsed().as_millis() as u64,
        "panel git fan-out done"
    );

    // Resolve the branch list: a fresh fetch refreshes the shared repo cache;
    // otherwise reuse the warm entry populated by this (or a sibling) worktree.
    let branches_raw = if need_branch_fetch {
        if let Some(root) = repo_root.as_deref() {
            crate::branch_cache::put(root, fetched_branches.clone());
        }
        fetched_branches
    } else {
        cached_branches.map(|(v, _)| v).unwrap_or_default()
    };

    // Per-worktree cache key: the HOST path, never `loc.path()` (which
    // collides across sandboxed worktrees — see `worktree_cache_key`).
    let cache_key = thegn_core::remote::GitLoc::worktree_cache_key(cwd);

    // Retain last-known-good header on transient git-read failure (never "—").
    // Keyed by the HOST worktree path, not `loc.path()` — the in-sandbox path
    // is `/workspace` for every provider worktree of an env, so keying by it
    // shares one last-known row across siblings (worktree B's failed read
    // would fall back to A's branch + ahead/behind).
    let (branch, ahead_behind, merge_info) =
        crate::panel_header_cache::merge_header(&cache_key, branch, ahead_behind, merge_info);
    let mut panel = crate::panel::PanelData {
        branch,
        ..Default::default()
    };

    // The typed PR cache: summary + checks + review threads + issues.
    if let Ok(Some((json, _))) = db.get_pr_cache(&cache_key)
        && let Ok(cached) = serde_json::from_str::<thegn_core::github::PrPanel>(&json)
    {
        // Defense-in-depth: the payload stamps the worktree it was fetched for
        // (`pr_status` stamps `loc.path()`); drop a row that belongs to a
        // different worktree (e.g. one written under the old colliding
        // sandbox-path key) instead of rendering the wrong repo's PR.
        if cached.worktree.is_empty()
            || cached.worktree == cache_key
            || cached.worktree == loc.path()
        {
            apply_pr_cache(&mut panel, cached);
        }
    }

    // The CI run-history cache feeds the `Ci` section rollup (AV group), with
    // its fetch age (the summary's "Ns ago" stamp) and any fetch-health note.
    if let Ok(Some((json, fetched_at))) = db.get_ci_cache(&cache_key)
        && let Ok(runs) = serde_json::from_str::<Vec<thegn_core::ci::CiRun>>(&json)
    {
        panel.ci_runs = runs;
        panel.ci_fetched_at = Some(fetched_at);
    }
    panel.ci_note = crate::ci_refresh::note_for(&cache_key);
    // A cache row fetched for a *different* branch (the fetcher queries the
    // branch that was checked out at fetch time) must not read as current
    // right after a branch switch — say so until the next refresh lands.
    if panel.ci_note.is_none()
        && !panel.branch.is_empty()
        && !panel.ci_runs.is_empty()
        && panel
            .ci_runs
            .iter()
            .all(|r| !r.branch.is_empty() && r.branch != panel.branch)
    {
        panel.ci_note = Some(format!(
            "showing runs for '{}' — refresh pending (g)",
            panel.ci_runs[0].branch
        ));
    }

    // The local merge queue (fold-actor) — a tiny table, read every model build
    // (no dedicated RefreshKind). Feeds the `MergeQueue` section + statusbar
    // badge. The table spans every workspace; scope the view to the active
    // repo's worktrees unless the section's all-workspaces toggle is on.
    let scope_repo_root = thegn_core::repo::main_worktree(cwd).unwrap_or_else(|| cwd.to_path_buf());
    panel.merge_queue = db.list_merge_queue().unwrap_or_default();
    if !crate::panel::scope::merge_all() {
        let repo_paths = repo_worktree_paths(db, &scope_repo_root);
        panel
            .merge_queue
            .retain(|r| repo_paths.contains(&r.worktree));
    }

    // The PR queue — same deal: a tiny table read every model build, feeding the
    // `PrQueue` section + statusbar badge. Scoped to THIS repo, because the queue
    // is per-repo (a shared state DB holds several repos' rows) and the section
    // must show what `pr queue drain` would act on.
    panel.pr_queue = match crate::integrate::main_checkout(std::path::Path::new(&loc.path())) {
        Some(root) => {
            let root_s = root.to_string_lossy().into_owned();
            db.list_pr_queue()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.repo_root == root_s)
                .collect()
        }
        None => Vec::new(),
    };

    // Cross-worktree attention stream (the `Across` section): every worktree's
    // failing CI, from the CI cache. Cheap DB reads only, off the event loop.
    // Scoped to the active repo's worktrees unless its toggle says otherwise.
    panel.across = build_across(
        db,
        (!crate::panel::scope::across_all()).then_some(scope_repo_root.as_path()),
    );

    // Hosts-as-resources: per-[host.*] display snapshots for the System ▸ Hosts
    // section and the wizard badges (hosts live in the panel, not the
    // sidebar). Small DB reads;
    // empty (and free) when no [host.*] is configured. The loop live-merges
    // HostRuntime progress on top after each drain.
    panel.hosts = crate::host_ui::host_snapshots(app_cfg, db);
    // Per-[env.*] display snapshots for the System ▸ Environments section (kind,
    // region/size, token presence). Cheap config walk; empty without any [env.*].
    panel.environments = crate::env_ui::env_snapshots(app_cfg);

    panel.files = diff_entries
        .iter()
        .map(|f| crate::panel::DiffFile {
            status: f.path.chars().next().unwrap_or('M'),
            path: f.path.clone(),
            added: f.added,
            deleted: f.deleted,
        })
        .collect();

    // Changes section: porcelain status joined with the diffstat, with
    // merge-incoming files tagged for the "incoming from <onto>" grouping.
    panel.changes = crate::panel::build_change_rows(&status, &diff_entries, &incoming);
    // Semantic git layer (items 311/313/317): entity-level view of the changes.
    // Blast-radius (313/316): enrich with the persisted caller→callee graph when
    // it has data (else `None` → the footer keeps its intra-diff summary).
    panel.entities = entities.map(|mut s| {
        s.blast = crate::blast_radius::read_blast(cwd, &s, db);
        s
    });

    // Header zone: upstream divergence + merge-in-progress banner.
    panel.ahead_behind = ahead_behind;
    let unresolved = thegn_svc::git::conflict_count(&status);
    // Host-path key (never `loc.path()` — see `worktree_cache_key`): two
    // provider worktrees sharing `/workspace` cross-contaminated each other's
    // "N/M resolved" merge banner through this row.
    let total = merge_total(db, &cache_key, merge_info.is_some(), unresolved);
    panel.merge = merge_info.map(|m| crate::panel::MergeBanner {
        label: m.kind.label().to_string(),
        onto: m.onto,
        unresolved,
        total,
    });
    panel.stash_count = stash_count;
    panel.log = log;

    let commits_open = hints.wants_commits();
    if commits_open || hints.warm_git_summaries {
        let cached = db.get_commit_cache(&cache_key).ok().flatten();
        if let Some((json, _)) = cached.as_ref()
            && let Ok(rows) = serde_json::from_str::<Vec<crate::panel::CommitRow>>(json)
        {
            // The FULL cached window (80), untruncated: the `/` filter and the
            // interactive-rebase planner both consume `panel.commits`, and the
            // old 20-row Normal-width truncation made a filter for commit #35
            // report "0 matches" while the cache held it. Display cost is
            // bounded by the frame's row budget, not the list length.
            panel.commits = rows;
        }
        // Open section: refresh on the TTL. Warm-only (closed summary):
        // refresh on a cold miss or the (longer) summary TTL.
        panel.commits_loading = commit_load_needed(commits_open, cached.as_ref());
    }
    // The per-repo open-PR cache: the `pr` section's OPEN PRS block, and the
    // branch-row badges (joined by head ref). The cache is keyed by the REPO
    // ROOT (that's what the writer uses — see `spawn_pr_cache_refresh`), not
    // the worktree path; reading with `loc.path()` always missed in linked
    // worktrees, blanking every PR badge there.
    let pr_cache_repo_root = thegn_core::repo::main_worktree(cwd)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_else(|| loc.path());
    if let Ok(Some((json, fetched_at))) = db.get_pr_branch_cache(&pr_cache_repo_root) {
        panel.open_prs = thegn_core::github::parse_pr_headers(&json);
        // Keep the age: an unaged row rendered a PR merged days ago (offline,
        // gh broken) as a live green badge, indistinguishable from fresh data.
        panel.open_prs_fetched_at = Some(fetched_at);
    }
    if want_branches {
        let badges = panel.open_prs.clone();
        let head_branch = panel.branch.clone();
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
                        // Recompute the HEAD marker for THIS worktree — the
                        // cached list is repo-global and its `is_head` reflects
                        // whichever worktree last fetched it.
                        is_head: b.name == head_branch,
                        name: b.name,
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
    if let Ok(Some((json, _))) = db.get_test_cache(&cache_key)
        && let Ok(cache) = serde_json::from_str::<crate::testkit::model::TestCache>(&json)
    {
        panel.tests = Some(crate::panel::tests_lite(&cache));
    }

    // Tracked-file list for the Files accordion — fetched in the fan-out above
    // (only while Files is open; `git ls-files` isn't free on big repos every 2s).
    if let Some(mut files) = ls_files {
        panel.file_count = Some(files.len() as u64);
        // Bounded: an unbounded monorepo listing made every tree rebuild a
        // visible stall. 50k rows is far past what the panel can usefully
        // browse; the count above still reports the true total.
        const MAX_FILES: usize = 50_000;
        files.truncate(MAX_FILES);
        // Pre-build the display tree HERE (off-loop) so the renderer and key
        // handlers never re-sort the listing on the event loop.
        panel.file_tree = crate::panel::build_file_tree(&files);
        panel.all_files = files;
    }

    // Issue tracker caches — DB-only reads; the background refresh keeps them
    // warm (see `hydrate_tracker.rs`). Keyed by the host worktree path, same
    // as the writer (`spawn_issue_cache_refresh` keys by its `cwd`).
    crate::hydrate_tracker::populate_tracker(db, &cache_key, cwd, app_cfg, &mut panel);
    // The active worktree's repo root — the default scoping unit for the panel's
    // otherwise-global sections (My Work, notifications).
    let repo_root = thegn_core::repo::main_worktree(cwd).unwrap_or_else(|| cwd.to_path_buf());
    // Unified "My Work" feed. Default: the active repo's scoped cache row (keyed
    // by repo root); under the Mine "all repos" toggle: the cross-repo
    // `ALL_SCOPE` row.
    let my_work_scope = if crate::panel::scope::mine_all() {
        thegn_core::work::ALL_SCOPE.to_string()
    } else {
        repo_root.to_string_lossy().into_owned()
    };
    if let Ok(Some((json, _))) = db.get_my_work_cache(&my_work_scope)
        && let Some(feed) = thegn_core::work::MyWorkFeed::from_cache_json(&json)
    {
        panel.my_work = feed.rows;
        panel.my_work_note = feed.note;
    }
    crate::hydrate_feed::populate_notifications(db, &repo_root, app_cfg, &mut panel);
    // Tasks section: populate task specs from config + auto-discovery (reusing the
    // single layered-config load above). Configured tasks win by name; discovered
    // tasks from manifests fill gaps.
    {
        let configured = app_cfg.tasks.clone();
        let discovered = crate::task::discover_all_tasks(cwd);
        panel.task_specs = crate::task::merge_tasks(configured, discovered);
    }

    // Logs section: tail the thegn log file.
    // Historically this `read`+parsed the WHOLE file every pass (up to the 5 MB
    // rotation cap) just to (a) count ERRORs and (b) build a bounded tail — on
    // every 5s tick + every fs-watch/prefetch pass, self-amplifying since
    // hydration itself logs. Now: the running ERROR total is tracked
    // incrementally (only the appended suffix is scanned each pass, resetting on
    // rotation), and the tails are built from a fixed-size END read, so cost is
    // bounded by the *appended* bytes, not the file size. See `plan_log_scan`.
    // Honor `[log] dir` — the hardcoded XDG path made the summary, the ERROR
    // notifications, and the drilldown read a file that doesn't exist when the
    // user relocated the log dir (the live tailer already used the config).
    let log_path = app_cfg.log.dir_path().join("thegn.log");
    if let Ok(meta) = std::fs::metadata(&log_path)
        && meta.is_file()
    {
        let cur_len = meta.len();
        // Incremental ERROR count: scan only the newly-appended suffix and fold
        // its error count into the process-global running total (reset on a
        // shrink = log rotation). Kept correct for `maybe_emit_log_error`, which
        // fires only when the *total* grows.
        let error_count = update_log_error_total(&log_path, cur_len);
        crate::hydrate_feed::maybe_emit_log_error(
            db,
            &panel.notifications,
            error_count,
            app_cfg.notifications.surface_self_log_errors,
        );

        // Bounded END read (~256 KB) for the tails — never the whole 5 MB file.
        // Snap to the first full line so a mid-line start doesn't yield a
        // corrupt row; parse just this window.
        let tail_lines = read_log_tail_lines(&log_path, cur_len, LOG_TAIL_BYTES);

        if hints.open == crate::panel::Section::Logs {
            let start = tail_lines.len().saturating_sub(500);
            panel.log_lines = tail_lines[start..].to_vec();
        }

        // Always keep a bounded tail (unlike section-gated `log_lines`) so the
        // notification → log drilldown modal has data without new blocking I/O.
        // The drilldown opens error-gated, and errors are sparse, so a plain last-N
        // slice usually held none of them ("no matching log lines"). Fold the recent
        // ERRORs back in — see `error_inclusive_tail`. Built from the same bounded
        // tail window (errors older than ~256 KB back have long scrolled out).
        panel.log_tail = thegn_core::log_view::error_inclusive_tail(&tail_lines, 400, 200);
    }
    panel
}

/// Size of the fixed END-of-file read used to build the Logs section tail +
/// drilldown payload. 256 KB is thousands of log lines — far more than the
/// 500-line `log_lines` / 400-line `log_tail` windows ever show — while bounding
/// the per-pass read regardless of the file's (up to 5 MB) size.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Process-global incremental log-scan state: `(last_scanned_len, error_total)`.
/// `error_total` is the running count of ERROR lines seen across the whole file;
/// each pass folds in only the ERRORs from the bytes appended since
/// `last_scanned_len`, so the O(file) full re-parse is gone. Reset when the file
/// shrinks (rotation). Mirrors the `glyph_cache` global-state pattern so it needs
/// no threading through `build_panel`'s many call sites.
fn log_scan_state() -> &'static std::sync::Mutex<(u64, usize)> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<(u64, usize)>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| std::sync::Mutex::new((0, 0)))
}

/// What to (re)scan given the previously-scanned length and the current file
/// length. Pure, so it's unit-tested — it encodes the rotation/append logic
/// without touching the filesystem.
#[derive(Debug, PartialEq, Eq)]
enum LogScanPlan {
    /// File unchanged since last pass — no new bytes, reuse the running total.
    Unchanged,
    /// File grew — scan only the bytes in `[offset, cur_len)`.
    Append { offset: u64 },
    /// File shrank/rotated (or first ever scan) — reset the total and scan all.
    FromStart,
}

fn plan_log_scan(prev_len: u64, cur_len: u64) -> LogScanPlan {
    if cur_len < prev_len {
        LogScanPlan::FromStart // rotation/truncation
    } else if cur_len == prev_len {
        LogScanPlan::Unchanged
    } else if prev_len == 0 {
        LogScanPlan::FromStart // first scan of this process
    } else {
        LogScanPlan::Append { offset: prev_len }
    }
}

/// Count ERROR lines in a byte slice of (possibly partial-line-bounded) log text.
/// The `offset` reads always start at a stored line boundary, so full lines
/// parse cleanly; `parse_log_line` already drops any blank/partial trailing line.
fn count_error_lines(bytes: &[u8]) -> usize {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(thegn_core::log_view::parse_log_line)
        .filter(|l| l.level == thegn_core::log_view::LogLevel::Error)
        .count()
}

/// Fold the ERROR lines appended since the last pass into the running total and
/// return it. Reads only `[offset, cur_len)` on a plain append (or the whole
/// file on first scan / after rotation), so cost tracks appended bytes, not file
/// size. Best-effort: a read error leaves the total unchanged.
fn update_log_error_total(log_path: &std::path::Path, cur_len: u64) -> usize {
    use std::io::{Read, Seek, SeekFrom};
    let mut st = log_scan_state().lock().unwrap();
    let (prev_len, total) = *st;
    match plan_log_scan(prev_len, cur_len) {
        LogScanPlan::Unchanged => total,
        plan => {
            let (offset, base) = match plan {
                LogScanPlan::Append { offset } => (offset, total),
                _ => (0, 0), // FromStart: reset the running total
            };
            let scanned = std::fs::File::open(log_path)
                .and_then(|mut f| {
                    f.seek(SeekFrom::Start(offset))?;
                    let mut buf = Vec::new();
                    f.take(cur_len.saturating_sub(offset))
                        .read_to_end(&mut buf)?;
                    Ok(buf)
                })
                .map(|buf| count_error_lines(&buf))
                .unwrap_or(0);
            let new_total = base + scanned;
            *st = (cur_len, new_total);
            new_total
        }
    }
}

/// Read the last `max_bytes` of the log and parse them into lines, snapping to
/// the first full line so a mid-line start never yields a corrupt row. Bounded
/// regardless of the file's size — the whole point of the change.
fn read_log_tail_lines(
    log_path: &std::path::Path,
    cur_len: u64,
    max_bytes: u64,
) -> Vec<thegn_core::log_view::LogLine> {
    use std::io::{Read, Seek, SeekFrom};
    let start = cur_len.saturating_sub(max_bytes);
    let buf = std::fs::File::open(log_path)
        .and_then(|mut f| {
            f.seek(SeekFrom::Start(start))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            Ok(buf)
        })
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&buf);
    // When we started mid-file, drop the (likely partial) first line.
    let body = if start > 0 {
        text.find('\n').map(|i| &text[i + 1..]).unwrap_or("")
    } else {
        text.as_ref()
    };
    body.lines()
        .filter_map(thegn_core::log_view::parse_log_line)
        .collect()
}

/// Whether `spawn_model_hydration` must emit a fallback model to release the
/// loop's `inflight_hydration_gen` gate. `Ok(Some(()))` = the body ran and sent
/// a model normally; `Ok(None)` = a handled early return (e.g. `Db::open`
/// failure) that sent nothing; `Err(_)` = a caught panic that sent nothing. Only
/// the first case has already signalled the loop. Pure, so it's unit-tested — it
/// locks the invariant that any non-normal exit still releases the gate.
fn needs_fallback_send<T>(outcome: &std::thread::Result<Option<T>>) -> bool {
    !matches!(outcome, Ok(Some(_)))
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
        // The loop gates every subsequent ticker/watcher hydration on THIS task
        // sending a model tagged with `generation` (run.rs: `inflight_hydration_gen`
        // is cleared only by an arriving model with that exact generation). If the
        // task ever exits WITHOUT sending — a transient `Db::open` failure, or a
        // panic inside `build_model`'s git/DB fan-outs — the gate strands `Some(gen)`
        // forever and periodic model/PR/CI refresh dies for the rest of the session
        // with nothing surfaced. So guarantee a completion signal on every exit path:
        // catch panics, and on any failure fall back to the cheap first-frame model
        // (still tagged `generation`) so the gate clears and the UI degrades to
        // last-known/cached data instead of freezing.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Ok(db) = thegn_core::db::Db::open() else {
                tracing::warn!(
                    target: "thegn::hydrate",
                    "Db::open failed during model hydration — falling back to cheap model"
                );
                return None;
            };
            let first = {
                let _g = crate::perf::measure(crate::perf::Subsys::Hydrate);
                build_model(&session, &db, hints.clone())
            };
            // `commits_loading` = the open Commits section needs a fresh list;
            // `warm_commits` (set on a switch) also pre-warms a *closed* section.
            let show_commits = first.panel.commits_loading;
            let warm = hints.warm_commits;
            if tx.send((generation, first)).is_ok()
                && let Some(w) = &waker
            {
                let _ = w.wake();
            }

            // `git log` can be expensive; run it only after the cache-backed model
            // landed. Resend a refreshed model only when the list is on screen; a
            // closed-section warm just leaves the DB cache fresh for the next open.
            // Generation tagging drops the resend if the user switched meanwhile.
            if show_commits || warm {
                // Refresh the DB cache for both the on-screen and warm cases;
                // only *resend* a model when the list is actually visible.
                let refreshed = refresh_commit_cache(&db, &session);
                if refreshed
                    && show_commits
                    && tx
                        .send((generation, build_model(&session, &db, hints)))
                        .is_ok()
                    && let Some(w) = &waker
                {
                    let _ = w.wake();
                }
            }
            Some(())
        }));

        // Every non-`Some(())` outcome (Db::open failure OR a caught panic) means
        // no model was sent for `generation`. Emit a fallback so the loop's gate
        // clears — the cheap model needs no DB and carries the session's sidebar,
        // so the UI keeps its last-known/cached data rather than freezing.
        if outcome.is_err() {
            tracing::warn!(
                target: "thegn::hydrate",
                "model hydration panicked — sending fallback model to release the loop gate"
            );
        }
        if needs_fallback_send(&outcome) {
            let fallback = build_initial_model(&session, None);
            if tx.send((generation, fallback)).is_ok()
                && let Some(w) = &waker
            {
                let _ = w.wake();
            }
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
    // Prefetch is background warming — ride the background lane so it never
    // starves the active worktree's (ungated) interactive hydration.
    crate::sched::spawn_bg(move || {
        if !cwd.is_dir() {
            return;
        }
        let Ok(db) = thegn_core::db::Db::open() else {
            return;
        };
        let app_cfg = load_hydration_config();
        let panel = build_panel(&cwd, &db, &hints, &app_cfg);
        if tx.send((cwd, panel)).is_ok()
            && let Some(w) = &waker
        {
            let _ = w.wake();
        }
    });
}

pub(crate) fn spawn_pr_cache_refresh(
    cwd: std::path::PathBuf,
    cfg: thegn_core::config::IssuesConfig,
    disk_cfg: thegn_core::config::DiskConfig,
    waker: Option<TerminalWaker>,
) {
    // Takes the worktree path, NOT the Session: the refreshers only ever read
    // the active tab's path, and a by-value Session is a String-heavy deep
    // clone on the loop thread at every call site (4× per worktree switch).
    let branch_cwd = cwd.clone();
    let branch_waker = waker.clone();
    crate::sched::spawn_bg(move || {
        if !cwd.is_dir() {
            return;
        }
        let loc = thegn_core::remote::GitLoc::for_worktree(&cwd);
        let Ok(db) = thegn_core::db::Db::open() else {
            return;
        };

        // Per-worktree cache key: the HOST path, never `loc.path()` (which
        // collides across sandboxed worktrees — see `worktree_cache_key`).
        let cache_key = thegn_core::remote::GitLoc::worktree_cache_key(&cwd);

        // Snapshot the old PR state BEFORE overwriting the cache.
        let old_pr_state: Option<String> = db
            .get_pr_cache(&cache_key)
            .ok()
            .flatten()
            .and_then(|(json, _)| serde_json::from_str::<thegn_core::github::PrPanel>(&json).ok())
            .and_then(|p| match p.state {
                thegn_core::github::PanelState::Pr(pr) => Some(pr.state),
                _ => None,
            });

        // The full feed: PR + checks + review threads + issues (extras are
        // best-effort and never fail the panel).
        let panel = thegn_core::github::pr_status_full(&loc);
        // Feed the app-wide connectivity holder (this CLI path is the 20s PR
        // backstop + the offline recovery probe).
        crate::connectivity_gate::report_pr_panel(&panel.state);
        let Ok(json) = serde_json::to_string(&panel) else {
            return;
        };
        // Only overwrite the cache for DEFINITIVE states. A transient
        // Error/Offline/RateLimited (a network blip) must NOT clobber a good
        // cached PrPanel: doing so drops the cached PR/checks/threads (the panel
        // then renders only the error note) AND resets `old_pr_state` to `None`,
        // so a later OPEN→MERGED transition through the blip would miss its
        // notification + `move_on_merge` automation. Keeping the last-known row
        // preserves both the displayed data and the transition diff. See
        // `pr_state_is_definitive` and `github.rs`'s Offline doc ("Stale cached
        // data may still be shown").
        if pr_state_is_definitive(&panel.state) {
            let _ = db.put_pr_cache(&cache_key, &panel.branch, &json);
        }

        // Emit a notification when the PR transitions between states
        // (e.g. OPEN → MERGED). Only fires when there was a prior known state
        // to diff against — avoids spurious notifications on first fetch.
        if let thegn_core::github::PanelState::Pr(ref pr) = panel.state
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
                let router = thegn_svc::issue::IssueRouter::from_config(&cfg);
                if router.is_configured()
                    && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                {
                    let patch = thegn_core::issue::IssuePatch {
                        status: Some(thegn_core::issue::IssueStatus::Done),
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
    crate::sched::spawn_bg(move || {
        let cwd = branch_cwd;
        if !cwd.is_dir() {
            return;
        }
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let loc = thegn_core::remote::GitLoc::for_worktree(&cwd);
        let prs = rt.block_on(async {
            use thegn_svc::gh::{GhBackend, GhNative};
            GhNative::new().pr_list(&loc).await
        });
        if let Ok(prs) = prs
            && let Ok(json) = serde_json::to_string(&prs)
            && let Ok(db) = thegn_core::db::Db::open()
        {
            // `pr_list` returns the repo's open PRs (branch-independent), so key
            // the cache by repo root — every worktree of the repo reads the same
            // entry to resolve its own branch's badge (item 28).
            let repo_root = thegn_core::repo::main_worktree(&cwd)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_else(|| loc.path());

            // On-merge auto-clean (background worktrees only): a branch that had
            // an open PR last round but is gone from the open set now has
            // transitioned (merged or closed). Resolve the precise state and, if
            // it matches the configured policy, reclaim that worktree's
            // `target/`. The active worktree is never touched (you may still be
            // working in it), nor one with a thegn-spawned build in flight.
            if disk_cfg.auto_clean_on_merge || disk_cfg.clean_on_pr_closed {
                maybe_clean_merged_worktrees(&db, &loc, &cwd, &repo_root, &prs, &disk_cfg);
            }

            // pr_linked producer: a PR newly entering the open set whose head
            // branch belongs to a linked worktree (or matches a linked
            // issue's branch_hint) notifies that worktree. Snapshot the OLD
            // open set before the cache overwrite below; a missing prior
            // cache emits nothing (first-fetch guard).
            let old_open: Option<std::collections::HashSet<String>> = db
                .get_pr_branch_cache(&repo_root)
                .ok()
                .flatten()
                .map(|(old_json, _)| {
                    thegn_core::github::parse_pr_headers(&old_json)
                        .into_iter()
                        .map(|p| p.head_ref)
                        .collect()
                });
            if let Some(old_open) = old_open {
                use thegn_core::store::WorkspaceStore;
                let wts: Vec<(String, String, Vec<String>)> = db
                    .worktrees()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|w| w.repo_root == repo_root)
                    .map(|w| {
                        let linked = db.linked_issues(&w.worktree).unwrap_or_default();
                        (w.worktree, w.branch, linked)
                    })
                    .collect();
                let mut hints: Vec<(String, String, String)> = Vec::new();
                for (path, _branch, linked) in &wts {
                    if linked.is_empty() {
                        continue;
                    }
                    if let Ok(cached) = db.get_all_issue_cache(path) {
                        for (_provider, cache_json) in cached {
                            let Ok(issues) =
                                serde_json::from_str::<Vec<thegn_core::issue::Issue>>(&cache_json)
                            else {
                                continue;
                            };
                            for i in issues {
                                if let Some(h) = i.branch_hint
                                    && linked.contains(&i.id)
                                {
                                    hints.push((i.number, h, path.clone()));
                                }
                            }
                        }
                    }
                }
                for (source_ref, msg, wt) in pr_linked_notifications(&old_open, &prs, &wts, &hints)
                {
                    let _ = db.put_notification("pr_linked", &source_ref, &msg, &wt);
                }
            }

            let _ = db.put_pr_branch_cache(&repo_root, &json);

            // Mentioned producer: poll GitHub's notifications API for
            // @mentions in this repo (`reason == "mention"`). Throttled to
            // one REST call per repo per 5 minutes via a ui_state stamp
            // (checked before the config load, so the steady-state cost is
            // one SELECT); the `ghn:<thread>:<updated_at>` ref + store-side
            // emit-once keeps it to one row per mention event. Chosen over
            // scanning review-thread bodies for `@login`: the API covers
            // issues AND PRs repo-wide with GitHub's own mention detection,
            // while thread snippets are truncated and per-PR only.
            const MENTION_POLL_MS: i64 = 5 * 60 * 1000;
            let now = thegn_core::util::now();
            let poll_due = db
                .get_ui_state("gh_mentions", &repo_root)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<i64>().ok())
                .is_none_or(|last| now.saturating_sub(last) >= MENTION_POLL_MS);
            if poll_due
                && load_hydration_config().notifications.github_mentions
                && let Some(nwo) = thegn_core::github::origin_nwo(&loc)
            {
                // Stamp before the fetch so a failing `gh` can't retry hot.
                let _ = db.set_ui_state("gh_mentions", &repo_root, &now.to_string());
                if let Ok(njson) = thegn_core::github::fetch_gh_notifications(&loc) {
                    for (source_ref, msg) in
                        thegn_core::github::parse_mention_notifications(&njson, &nwo)
                    {
                        let _ =
                            db.put_notification_once("mentioned", &source_ref, &msg, &repo_root);
                    }
                }
            }

            if let Some(w) = &branch_waker {
                let _ = w.wake();
            }
        }
    });
}

/// The pure diff behind the `pr_linked` producer: for each PR whose head
/// branch is NEW to the repo's open set, emit one `(source_ref, message,
/// worktree_path)` row when the branch belongs to a worktree with linked
/// issues, or matches a linked issue's provider `branch_hint`. Worktree-branch
/// matches win over hint matches; one emit per PR. Pure so the emit-once
/// semantics are unit-testable without a DB.
///
/// `worktrees`: `(path, branch, linked issue ids)` for this repo's worktrees.
/// `hints`: `(issue number, branch_hint, worktree path)` for linked issues.
pub(crate) fn pr_linked_notifications(
    old_open: &std::collections::HashSet<String>,
    prs: &[thegn_core::github::PrHeader],
    worktrees: &[(String, String, Vec<String>)],
    hints: &[(String, String, String)],
) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for pr in prs {
        if old_open.contains(&pr.head_ref) {
            continue;
        }
        let source_ref = format!("pr:{}", pr.number);
        if let Some((path, _, linked)) = worktrees
            .iter()
            .find(|(_, b, linked)| *b == pr.head_ref && !linked.is_empty())
        {
            out.push((
                source_ref,
                format!(
                    "PR #{} opened for {} (linked: {})",
                    pr.number,
                    pr.head_ref,
                    linked.join(", ")
                ),
                path.clone(),
            ));
        } else if let Some((number, _, path)) =
            hints.iter().find(|(_, hint, _)| *hint == pr.head_ref)
        {
            out.push((
                source_ref,
                format!(
                    "PR #{} opened for {} (issue {})",
                    pr.number, pr.head_ref, number
                ),
                path.clone(),
            ));
        }
    }
    out
}

/// Policy decision for auto-cleaning a worktree whose PR left the open set,
/// given the freshly-resolved PR `state`. Returns `(merged, should_clean)`.
/// ONLY a definitive `MERGED`/`CLOSED` acts: `None` (a `gh`/network error) and
/// `OPEN`/anything-else never clean, because deleting a worktree's build
/// artifacts on a transient failure or a still-open PR is unrecoverable.
fn pr_clean_decision(state: Option<&str>, cfg: &thegn_core::config::DiskConfig) -> (bool, bool) {
    match state {
        Some("MERGED") => (true, cfg.auto_clean_on_merge),
        Some("CLOSED") => (false, cfg.clean_on_pr_closed),
        _ => (false, false),
    }
}

/// Auto-clean `target/` for worktrees whose open PR has just transitioned away
/// (merged / closed-without-merge), gated by `[disk]` policy. Compares the
/// previously-cached open branches against the current open set; for each
/// branch that dropped out and maps to a known *background* worktree (not the
/// active one, no running build), resolves the precise PR state via a targeted
/// `gh pr view` and cleans on a policy match. Best-effort and silent on error.
fn maybe_clean_merged_worktrees(
    db: &thegn_core::db::Db,
    loc: &thegn_core::remote::GitLoc,
    active: &std::path::Path,
    repo_root: &str,
    open_now: &[thegn_core::github::PrHeader],
    cfg: &thegn_core::config::DiskConfig,
) {
    use std::collections::HashSet;

    // Branches with an open PR in the prior cache.
    let prev_open: HashSet<String> = db
        .get_pr_branch_cache(repo_root)
        .ok()
        .flatten()
        .and_then(|(json, _)| serde_json::from_str::<Vec<thegn_core::github::PrHeader>>(&json).ok())
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
        // Resolve the precise outcome against policy. Only a DEFINITIVE MERGED or
        // CLOSED state may trigger cleaning: `pr_state_for_branch` returns None on
        // any `gh`/network failure, and the PR may also have reopened since the
        // cache diff. Treating None/OPEN/unknown as "closed" (the old `!merged`
        // branch did) deletes the worktree's build artifacts on a transient error
        // or a still-open PR — unrecoverable. When unsure, do nothing.
        let state = thegn_core::github::pr_state_for_branch(loc, &row.branch);
        let (merged, should) = pr_clean_decision(state.as_deref(), cfg);
        if !should {
            continue;
        }
        if let Ok(reclaimed) = thegn_core::worktree::clean_target(&path)
            && reclaimed > 0
        {
            let _ = db.delete_worktree_disk(&row.worktree);
            let verb = if merged { "merged" } else { "closed" };
            let msg = format!(
                "{} cleaned ({} reclaimed)",
                verb,
                thegn_core::disk::human(reclaimed)
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
pub(crate) fn spawn_disk_scan(cfg: thegn_core::config::DiskConfig, waker: Option<TerminalWaker>) {
    if !cfg.show_sizes {
        return;
    }
    crate::sched::spawn_bg(move || {
        let Ok(db) = thegn_core::db::Db::open() else {
            return;
        };
        let Ok(rows) = db.worktrees() else {
            return;
        };
        let now = thegn_core::util::now();
        let ttl = cfg.scan_interval_secs.max(1) as i64;
        let mut scanned = 0u32;
        // Garbage-collect orphaned size-cache rows: any `worktree_disk` entry
        // whose worktree has left the registry (removed/pruned) is never
        // re-measured by the loop below and would otherwise inflate the
        // statusbar total forever. Self-heals pre-existing orphans on launch.
        let live: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.worktree.as_str()).collect();
        if let Ok(cached) = db.all_worktree_disk() {
            for path in cached.keys() {
                if !live.contains(path.as_str()) {
                    let _ = db.delete_worktree_disk(path);
                    scanned += 1;
                }
            }
        }
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
            let usage = thegn_core::disk::measure_worktree(&path);
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
fn pr_search_row(
    p: thegn_core::github::PrSearchRow,
    group: thegn_core::work::WorkGroup,
) -> thegn_core::work::WorkRow {
    thegn_core::work::WorkRow {
        group,
        kind: thegn_core::work::WorkKind::Pr,
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

/// The CLI config source (`--config` path + `--set` overrides), captured once
/// at startup so OFF-LOOP config loads (hydration, neighbor prefetch, cold-
/// switch fast-fill) build the same config the event loop runs with. Without
/// it those loads used the default path with no overrides — so `--config`-
/// declared hosts/envs and runtime-added (DB) hosts never appeared in the
/// System ▸ Hosts / Environments sections.
static CONFIG_SOURCE: std::sync::OnceLock<(Vec<String>, Option<std::path::PathBuf>)> =
    std::sync::OnceLock::new();

/// Record the CLI config source (called once from startup).
pub(crate) fn set_config_source(overrides: Vec<String>, config: Option<std::path::PathBuf>) {
    let _ = CONFIG_SOURCE.set((overrides, config));
}

/// The one config loader every off-loop hydration path uses: layered load with
/// the CLI source, plus the runtime-added `[host.*]` defs from the DB (which
/// `merge_db_hosts` synthesizes envs for) — matching what the loop holds.
pub(crate) fn load_hydration_config() -> thegn_core::config::Config {
    let (overrides, path) = CONFIG_SOURCE.get().cloned().unwrap_or_default();
    let mut cfg = thegn_core::config::Config::try_load_layered(
        &thegn_core::config::ProcessEnv,
        &overrides,
        path,
    )
    .unwrap_or_default();
    thegn_core::host_config::merge_db_hosts(&mut cfg);
    cfg
}

/// The set of worktree paths belonging to a repo (`repo_root`), from the DB
/// registry. Used to scope the "My Work" feed's notifications to the current
/// repo — a notification for a sibling worktree of the same repo is relevant;
/// one for an unrelated repo (often on another host) is not.
pub(crate) fn repo_worktree_paths(
    db: &thegn_core::db::Db,
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
    cwd: std::path::PathBuf,
    cfg: thegn_core::config::Config,
    all: bool,
    waker: Option<TerminalWaker>,
) {
    crate::sched::spawn_bg(move || {
        use thegn_core::work::{ALL_SCOPE, WorkGroup, WorkKind, WorkRow};

        if !cwd.is_dir() {
            return;
        }
        let loc = thegn_core::remote::GitLoc::for_worktree(&cwd);
        let repo_root = thegn_core::repo::main_worktree(&cwd).unwrap_or_else(|| cwd.clone());
        // Repo scope (unless `all`): `owner/repo` for GitHub, the repo `[issues]`
        // overlay for Linear/Jira, and the cache key.
        let nwo = if all {
            None
        } else {
            thegn_core::github::origin_nwo(&loc)
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
        let router = thegn_svc::issue::IssueRouter::from_config_at(&issues_cfg, Some(&cwd));
        if router.is_configured()
            && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        {
            let mut filter = thegn_core::issue::IssueFilter::my_open(issues_cfg.max_issues.max(1));
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
                        urgency: crate::hydrate_feed::issue_urgency(i.priority),
                        issue_id: Some(i.id),
                        branch_hint: i.branch_hint,
                        worktree_path: None,
                    });
                }
            }
        }

        // 2) PRs via `gh search` — scoped to `nwo` unless `all`. When the feed
        // is repo-scoped but the repo has no `origin` remote to derive
        // `owner/repo` from, SKIP the searches instead of running them
        // unscoped: an unscoped `gh search prs` spans every repo, and caching
        // that under the repo-scoped key made "mine · this repo" silently show
        // other workspaces' items. Surface why via the feed note instead.
        let mut note = String::new();
        if all || nwo.is_some() {
            if let Ok(prs) =
                thegn_core::github::search_prs(&loc, "--review-requested=@me", nwo.as_deref(), 30)
            {
                rows.extend(
                    prs.into_iter()
                        .map(|p| pr_search_row(p, WorkGroup::ReviewRequested)),
                );
            }
            if let Ok(prs) =
                thegn_core::github::search_prs(&loc, "--author=@me", nwo.as_deref(), 30)
            {
                rows.extend(
                    prs.into_iter()
                        .map(|p| pr_search_row(p, WorkGroup::NeedsAttention)),
                );
            }
        } else {
            note = "no `origin` remote — PR search needs it for repo scope (a = all repos)"
                .to_string();
        }

        // 3) High-priority unread notifications (mentions / blockers / pr-linked),
        //    scoped to this repo's own worktrees unless `all`.
        if let Ok(db) = thegn_core::db::Db::open()
            && let Ok(notes) = db.get_all_notifications(50)
        {
            use thegn_core::notification::NotificationKind as K;
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
        let feed = thegn_core::work::MyWorkFeed { rows, note };
        if let Ok(db) = thegn_core::db::Db::open()
            && let Ok(json) = serde_json::to_string(&feed)
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
    cfg: &thegn_core::config::Config,
    waker: &TerminalWaker,
) -> String {
    let all = crate::panel::scope::toggle_mine_all();
    spawn_my_work_refresh(
        active_tab_path(session),
        cfg.clone(),
        all,
        Some(waker.clone()),
    );
    if all {
        "My Work: all repos".into()
    } else {
        "My Work: this repo".into()
    }
}

/// Toggle between this repo (default) and every worktree, rehydrate the active
/// model so the scoped views refresh, and return the status line. Extracted from
/// the panel key handler for the ratchet.
///
/// This is the single "widen everything" escape hatch: it governs the System
/// tab's notification list *and* the needs-you nag surfaces (the `✋` badge, the
/// "Needs you" popup, the `Alt a` ring), which read it through
/// `handlers::attention::in_scope`.
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
        session.clone_for_hydrate(),
        Some(waker.clone()),
        HydrateHints {
            open,
            expanded,
            ..Default::default()
        },
    );
    // Lead with the section the user pressed `g` in — the toggle is shared
    // (it widens notifications + needs-you + containers + logs together), but
    // answering a Sandbox keypress with a status line about notifications
    // read as the wrong key having fired.
    let subject = match open {
        crate::panel::Section::Sandbox => "Containers, notifications & needs-you",
        crate::panel::Section::Logs => "Logs, notifications & needs-you",
        _ => "Notifications & needs-you",
    };
    if all {
        format!("{subject}: all worktrees")
    } else {
        format!("{subject}: this repo")
    }
}

/// Toggle the Across section between the active workspace (default) and every
/// workspace, rehydrate so the aggregation rebuilds under the new scope, and
/// return the status line. Same shape as [`toggle_system_scope`].
pub(crate) fn toggle_across_scope(
    tx: &tokio_mpsc::UnboundedSender<(u64, FrameModel)>,
    generation: u64,
    session: &crate::session::Session,
    waker: &TerminalWaker,
    open: crate::panel::Section,
    expanded: bool,
) -> String {
    let all = crate::panel::scope::toggle_across_all();
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
        "Across: all workspaces".into()
    } else {
        "Across: this workspace".into()
    }
}

/// Toggle the Merge-queue section between the active workspace (default) and
/// every workspace, rehydrate, and return the status line.
pub(crate) fn toggle_merge_scope(
    tx: &tokio_mpsc::UnboundedSender<(u64, FrameModel)>,
    generation: u64,
    session: &crate::session::Session,
    waker: &TerminalWaker,
    open: crate::panel::Section,
    expanded: bool,
) -> String {
    let all = crate::panel::scope::toggle_merge_all();
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
        "Merge queue: all workspaces".into()
    } else {
        "Merge queue: this workspace".into()
    }
}

/// Bind (or re-bind) the diff fs-watcher to the active worktree path. A no-op if
/// the active worktree is unchanged. On a debounced filesystem event under the
/// worktree, pushes `RefreshKind::Model` and pulses the waker so the loop
/// rehydrates the diff panel promptly. The previous watcher (if any) is dropped,
/// which unregisters its watch. Event classification + the ref-move self-heal it
/// drives live in [`crate::git_watch`].
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
    // the recursive watch registration walks every directory (~1s on this
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
        let git_dir =
            thegn_core::util::git_out(&cwd, &["rev-parse", "--path-format=absolute", "--git-dir"])
                .map(std::path::PathBuf::from);
        let common_dir = thegn_core::util::git_out(
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
                    || ev.paths.iter().any(|p| {
                        crate::git_watch::watcher_path_triggers_refresh(p, &roots, &ignore)
                    }))
                && last_send.elapsed() > Duration::from_millis(500)
            {
                if tx.send(RefreshKind::Model).is_ok() {
                    let _ = wake.wake();
                }
                // A branch-ref move also kicks the guarded main-checkout self-heal
                // so a checkout sitting on that branch fast-forwards its own tree
                // (external `update-ref` / a fold-actor CAS land elsewhere) without
                // waiting for a tab switch or restart.
                if ev
                    .paths
                    .iter()
                    .any(|p| crate::git_watch::is_ref_move_path(p))
                {
                    let _ = tx.send(RefreshKind::MainRefMoved);
                }
                // A remote-tracking ref moved — the local signature of a push
                // (or fetch): kick the PR + CI caches now so the just-pushed
                // branch's checks appear without waiting for the tickers.
                // Non-forced, so `[ci] ttl_secs` still bounds subprocess churn.
                if ev
                    .paths
                    .iter()
                    .any(|p| crate::git_watch::is_remote_ref_path(p))
                {
                    let _ = tx.send(RefreshKind::Pr);
                    let _ = tx.send(RefreshKind::Ci { force: false });
                    // The PR queue cares about exactly this event: a push is
                    // what unblocks a PR (or is the teammate the queue must not
                    // race), so re-evaluate now rather than up to a minute later.
                    // Inert when the queue is off — the pass finds no rows.
                    let _ = tx.send(RefreshKind::PrQueue);
                }
                last_send = Instant::now();
            }
        });
        let Ok(mut nw) = new_watcher else {
            tracing::warn!(
                target: "thegn::hydrate",
                worktree = %cwd.display(),
                "failed to construct diff fs-watcher — diff panel falls back to the 2s ticker"
            );
            return;
        };
        // Register the recursive root watch. On a Linux machine whose
        // `fs.inotify.max_user_watches` is exhausted (large monorepos, many
        // instances) this fails with ENOSPC — previously the thread just exited
        // silently, and `retarget`'s guard suppressed every retry, so the active
        // worktree lost sub-second diff/ref-move/push detection for the rest of
        // the session with no diagnostic. Fall back to a NON-recursive watch on
        // the worktree root (one watch, not thousands): coarser (top-level edits
        // + git-state paths under it still fire) but keeps the ref-move / push
        // kicks working, and — crucially — still sends a watcher back so the loop
        // adopts it (a later retarget away-and-back re-attempts the recursive one).
        let recursive_ok = nw.watch(&cwd, RecursiveMode::Recursive).is_ok();
        if !recursive_ok {
            let fallback_ok = nw.watch(&cwd, RecursiveMode::NonRecursive).is_ok();
            // The likely cause is OS-specific, and naming the wrong mechanism
            // sends the reader down a dead end: `notify` rides inotify on Linux
            // but FSEvents on macOS (which has no per-watch quota to exhaust —
            // there, a failure is a path/permission problem).
            let hint = if cfg!(target_os = "linux") {
                "inotify watches exhausted?"
            } else {
                "path unreadable or unwatchable?"
            };
            tracing::warn!(
                target: "thegn::hydrate",
                worktree = %cwd.display(),
                fallback_ok,
                "recursive diff fs-watch registration failed ({hint}) — \
                 fell back to a non-recursive root watch"
            );
            if !fallback_ok {
                // Nothing attached at all — don't ship a dead watcher; the 2s
                // safety-net ticker covers diff refresh until the next retarget.
                return;
            }
        }
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
    });
}

/// Build the cross-worktree attention stream from every worktree's cached CI:
/// each worktree's failing runs become excerpts, grouped + sorted by
/// [`thegn_core::aggregate`]. Pure DB reads (the CI cache), so it is cheap and
/// safe to run on the model-hydration `spawn_blocking`. As dirty-file / content
/// producers land they append their excerpts here too.
/// Build the Across aggregation from the CI caches of the registered
/// worktrees. `repo_root = Some(_)` keeps only that repo's worktrees (the
/// default, workspace-scoped view); `None` spans every workspace (the `a`
/// all-workspaces toggle).
fn build_across(
    db: &thegn_core::db::Db,
    repo_root: Option<&std::path::Path>,
) -> thegn_core::aggregate::Aggregation {
    use thegn_core::aggregate::{Aggregation, ci_failure_excerpts};
    let rr = repo_root.map(|r| r.to_string_lossy().into_owned());
    let mut excerpts = Vec::new();
    for w in db.worktrees().unwrap_or_default() {
        if let Some(rr) = &rr
            && w.repo_root != *rr
        {
            continue;
        }
        let label = if w.branch.is_empty() {
            w.tab_name.clone()
        } else {
            w.branch.clone()
        };
        if let Ok(Some((json, _))) = db.get_ci_cache(&w.worktree)
            && let Ok(runs) = serde_json::from_str::<Vec<thegn_core::ci::CiRun>>(&json)
        {
            excerpts.extend(ci_failure_excerpts(&w.worktree, &label, &runs));
        }
    }
    Aggregation::from_excerpts(excerpts)
}

#[cfg(test)]
#[path = "hydrate_tests.rs"]
mod tests;
