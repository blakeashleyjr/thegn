//! Tracker (issue) hydration: the off-loop issue-cache refresh and the
//! panel-side cache loads. Extracted from the ratchet-pinned `hydrate.rs`
//! (mirroring the `hydrate_feed.rs` split).
//!
//! Threading contract: `spawn_issue_cache_refresh` runs its network + DB work
//! on `sched::spawn_bg` and pulses the waker once at the end;
//! `populate_tracker` runs on the hydration thread and reads only the DB
//! cache (no network).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use termwiz::terminal::TerminalWaker;
use thegn_core::issue::{IssueCacheEntry, IssueCachePartial};
use thegn_core::store::{CacheStore, WorktreeAuxStore};

const BUSY_RETRY_DELAY: Duration = Duration::from_secs(2);

fn partial_cache_entry(
    last_good: Option<IssueCacheEntry>,
    partial_rows: Vec<thegn_core::issue::Issue>,
    account: &str,
    reason: &str,
    now_ms: i64,
) -> IssueCacheEntry {
    let (issues, showing_last_good) = match last_good.filter(|entry| !entry.issues.is_empty()) {
        Some(entry) => {
            let showing_last_good = match entry.partial.as_ref() {
                Some(partial) => partial.showing_last_good,
                None => Some(true),
            };
            (entry.issues, showing_last_good)
        }
        None => (partial_rows, Some(false)),
    };
    IssueCacheEntry {
        issues,
        partial: Some(IssueCachePartial {
            account: account.to_owned(),
            reason: reason.to_owned(),
            checked_at_ms: now_ms,
            showing_last_good,
        }),
    }
}

struct BusyRetry {
    pending: Mutex<Option<(PathBuf, Instant)>>,
}

impl BusyRetry {
    fn new() -> Self {
        Self {
            pending: Mutex::new(None),
        }
    }

    fn record(&self, path: PathBuf, now: Instant) {
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path, now));
    }

    fn take(&self, now: Instant) -> Option<PathBuf> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.as_ref().is_some_and(|(_, recorded)| {
            now.saturating_duration_since(*recorded) >= BUSY_RETRY_DELAY
        }) {
            pending.take().map(|(path, _)| path)
        } else {
            None
        }
    }
}

fn busy_retry() -> &'static BusyRetry {
    static RETRY: OnceLock<BusyRetry> = OnceLock::new();
    RETRY.get_or_init(BusyRetry::new)
}

pub(crate) fn take_busy_retry(now: Instant) -> Option<PathBuf> {
    busy_retry().take(now)
}

fn tracker_failures() -> &'static Mutex<HashMap<String, String>> {
    static FAILURES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_tracker_failure(cwd: &std::path::Path, message: &str, waker: Option<&TerminalWaker>) {
    tracker_failures()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            cwd.to_string_lossy().into_owned(),
            safe_report_text(message),
        );
    if let Some(w) = waker
        && let Err(error) = w.wake()
    {
        tracing::debug!(target: "thegn::tracker", %error, "tracker failure waker pulse failed");
    }
}

fn tracker_load(rows: Result<usize, ()>, failure: Option<&str>) -> crate::panel::gitui::DocLoad {
    use crate::panel::gitui::DocLoad;

    match rows {
        Err(()) => DocLoad::Failed("issue cache unreadable".to_string()),
        Ok(count) if count > 0 => DocLoad::Ready,
        Ok(_) => failure.map_or(DocLoad::Pending, |message| {
            DocLoad::Failed(message.to_string())
        }),
    }
}

/// A refresh request waiting behind the currently admitted pass for one repo.
struct RefreshRequest {
    cwd: std::path::PathBuf,
    app_cfg: thegn_core::config::Config,
    waker: Option<TerminalWaker>,
    report: Option<RefreshReportTarget>,
}

#[derive(Clone)]
pub(crate) struct RefreshReportTarget {
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    pub(crate) waker: TerminalWaker,
}

/// One active refresh per key, with only the newest overlapping request kept.
struct RefreshCoalescer<R> {
    pending: Mutex<HashMap<String, Option<R>>>,
}

impl<R> RefreshCoalescer<R> {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    fn begin(&self, key: &str, req: R) -> Option<R> {
        self.begin_with(key, req, |_, new| new)
    }

    fn begin_with(&self, key: &str, req: R, merge: impl FnOnce(R, R) -> R) -> Option<R> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = pending.get_mut(key) {
            if let Some(previous) = slot.take() {
                *slot = Some(merge(previous, req));
            } else {
                *slot = Some(req);
            }
            None
        } else {
            pending.insert(key.to_owned(), None);
            Some(req)
        }
    }

    fn next(&self, key: &str) -> Option<R> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let request = pending.get_mut(key)?.take();
        if request.is_none() {
            pending.remove(key);
        }
        request
    }

    fn abandon(&self, key: &str) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
    }
}

fn refresh_coalescer() -> &'static RefreshCoalescer<RefreshRequest> {
    static COALESCER: OnceLock<RefreshCoalescer<RefreshRequest>> = OnceLock::new();
    COALESCER.get_or_init(RefreshCoalescer::new)
}

struct RefreshGuard<'a, R> {
    coalescer: &'a RefreshCoalescer<R>,
    key: String,
}

impl<R> Drop for RefreshGuard<'_, R> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.coalescer.abandon(&self.key);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RefreshStart {
    Started,
    Coalesced,
    Busy,
}

/// Refresh the per-repo issue cache off-thread: fetch every configured
/// provider, diff old vs new per `(repo_root, provider)` key for
/// status-change notifications on linked issues, and rewrite the cache. A
/// failing provider leaves its prior cache intact.
///
/// Takes the full app `Config` so the fetch honors the repo's own `[issues]`
/// overlay (`Config::repo_issues`), and scopes GitHub to the active repo via
/// `--repo owner/repo` — without that, `gh issue list` resolves against the
/// **process cwd's** repo and every workspace's Issues section shows that one
/// repo's issues.
pub(crate) fn spawn_issue_cache_refresh(
    cwd: std::path::PathBuf,
    app_cfg: thegn_core::config::Config,
    waker: Option<TerminalWaker>,
) {
    let retry_path = cwd.clone();
    match request_issue_refresh(cwd, app_cfg, waker, None) {
        RefreshStart::Started | RefreshStart::Coalesced => {}
        RefreshStart::Busy => {
            busy_retry().record(retry_path, Instant::now());
            tracing::debug!(target: "thegn::tracker", "issue cache refresh admission refused");
        }
    }
}

/// Request a user-visible or background refresh. Overlapping requests for the
/// same worktree keep only the newest config/waker and share one admission slot.
/// If admission is refused, cleanup also drops any request coalesced between
/// `begin` and the refusal; those callers already received `Coalesced` and
/// have no separate notification that their queued pass was discarded.
pub(crate) fn request_issue_refresh(
    cwd: std::path::PathBuf,
    app_cfg: thegn_core::config::Config,
    waker: Option<TerminalWaker>,
    report: Option<RefreshReportTarget>,
) -> RefreshStart {
    let key = cwd.to_string_lossy().into_owned();
    let request = RefreshRequest {
        cwd,
        app_cfg,
        waker,
        report,
    };
    let Some(first) = refresh_coalescer().begin_with(&key, request, |previous, mut latest| {
        if latest.report.is_none() {
            latest.report = previous.report;
        }
        latest
    }) else {
        return RefreshStart::Coalesced;
    };
    let worker_key = key.clone();
    let admission = crate::sched::spawn_bg(move || {
        let _guard = RefreshGuard {
            coalescer: refresh_coalescer(),
            key: worker_key.clone(),
        };
        let mut current = Some(first);
        while let Some(request) = current {
            run_once(request);
            current = refresh_coalescer().next(&worker_key);
        }
    });
    match admission {
        crate::sched::Admission::Admitted => RefreshStart::Started,
        crate::sched::Admission::Saturated | crate::sched::Admission::Unavailable => {
            refresh_coalescer().abandon(&key);
            RefreshStart::Busy
        }
    }
}

fn run_once(request: RefreshRequest) {
    let RefreshRequest {
        cwd,
        app_cfg,
        waker,
        report,
    } = request;
    // The UI pass reports through `report` and the waker; the summary is
    // for the daemon tick.
    refresh_repo_inner(
        &cwd,
        &app_cfg,
        waker.as_ref(),
        RefreshSite::Ui,
        report.as_ref(),
        &ListMemo::default(),
    );
}

/// Who runs a tracker refresh. `Ui` is the interactive process (reports,
/// toasts, the local autopilot driver, and yielding to the daemon's lease);
/// `Daemon` is the headless producer tick (THE-2209): no reports or toasts,
/// and autopilot pickup goes through the daemon's queues producer.
#[allow(
    dead_code,
    reason = "the daemon tracker tick drives it once the b161 lane 5 owner arms it"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RefreshSite {
    Ui,
    Daemon,
}

/// What one [`refresh_repo`] pass did.
#[allow(
    dead_code,
    reason = "the daemon tracker tick drives it once the b161 lane 5 owner arms it"
)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RefreshSummary {
    /// Accounts whose list call returned (fully).
    pub(crate) accounts_ok: usize,
    /// Accounts that failed or returned a partial list.
    pub(crate) accounts_failed: usize,
    /// Notification events written by this pass (diff + overdue emits).
    pub(crate) events: usize,
    /// The UI pass yielded to the daemon's live tracker lease: no fetch ran.
    pub(crate) yielded: bool,
}

impl RefreshSummary {
    /// The UI yielded to the daemon: nothing was fetched.
    pub(crate) const YIELDED: Self = Self {
        accounts_ok: 0,
        accounts_failed: 0,
        events: 0,
        yielded: true,
    };
}

/// One account's list outcome in a cloneable form, so a tick memo can hand
/// the same answer to every repo that shares the account.
#[derive(Clone, Debug)]
pub(crate) enum ListOutcome {
    Ok(Vec<thegn_core::issue::Issue>),
    /// A bounded listing stopped early; never cached as complete.
    Partial {
        issues: Vec<thegn_core::issue::Issue>,
        reason: &'static str,
    },
    /// The list call failed. `transient` is a dropped link (offline evidence).
    Failed {
        message: String,
        transient: bool,
    },
}

impl ListOutcome {
    fn from_result(
        result: Result<Vec<thegn_core::issue::Issue>, thegn_svc::issue::IssueError>,
    ) -> Self {
        match result {
            Ok(issues) => Self::Ok(issues),
            Err(thegn_svc::issue::IssueError::PartialList { issues, reason }) => {
                Self::Partial { issues, reason }
            }
            Err(error) => Self::Failed {
                message: error.to_string(),
                transient: error.is_transient(),
            },
        }
    }

    fn issues(&self) -> &[thegn_core::issue::Issue] {
        match self {
            Self::Ok(issues) | Self::Partial { issues, .. } => issues,
            Self::Failed { .. } => &[],
        }
    }
}

/// One account's list result plus the provider id that produced it.
#[derive(Clone, Debug)]
pub(crate) struct ProviderListResult {
    pub(crate) provider: String,
    pub(crate) outcome: ListOutcome,
}

/// Per-tick memo of list results, keyed by account name + provider + a digest
/// of the account's config entry + the filter (see [`list_memo_key`]). The
/// daemon tick shares one across every repo it refreshes, so N repos sharing a
/// tracker account make one list call per tick instead of N. Errors are
/// memoized too: one failing account is not retried once per repo.
#[derive(Default)]
pub(crate) struct ListMemo(Mutex<HashMap<String, ProviderListResult>>);

impl ListMemo {
    fn get(&self, key: &str) -> Option<ProviderListResult> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    fn put(&self, key: String, value: ProviderListResult) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, value);
    }
}

/// The memo key for one account under one filter. The account's config entry
/// (token included) is folded into an in-memory digest, never written out.
/// A GitHub account with no explicit repo scope resolves against the worktree
/// (`gh` runs in `cwd`), so its key includes `cwd`.
fn list_memo_key(
    account: &thegn_core::config::IssueAccount,
    filter: &thegn_core::issue::IssueFilter,
    cwd: &std::path::Path,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(account)
        .unwrap_or_else(|_| format!("{account:?}"))
        .hash(&mut hasher);
    let mut keyed_filter = filter.clone();
    if account.provider != thegn_core::config::IssueProviderKind::Github {
        keyed_filter.repo = None;
    }
    let filter_json =
        serde_json::to_string(&keyed_filter).unwrap_or_else(|_| format!("{keyed_filter:?}"));
    let anchor = if account.provider == thegn_core::config::IssueProviderKind::Github
        && filter.repo.is_none()
    {
        cwd.to_string_lossy().into_owned()
    } else {
        String::new()
    };
    format!(
        "{}\u{0}{}\u{0}{:016x}\u{0}{filter_json}\u{0}{anchor}",
        account.name,
        account.provider.as_str(),
        hasher.finish()
    )
}

/// Pure memo driver: answer every account from `memo` where it can, and call
/// `fetch` once with the misses (or with none when `always_fetch`, e.g. live
/// plugin providers that are never memoized). `fetch` returns
/// `(account, result)` rows; rows for memo-miss accounts are stored. Returns
/// rows in account order, then any extra rows `fetch` produced (plugins), or
/// `None` when `fetch` could not run at all (nothing memoized then).
fn fetch_memoized(
    accounts: &[(thegn_core::config::IssueAccount, String)],
    memo: &ListMemo,
    always_fetch: bool,
    fetch: &mut dyn FnMut(
        Vec<thegn_core::config::IssueAccount>,
    ) -> Option<Vec<(String, ProviderListResult)>>,
) -> Option<Vec<(String, ProviderListResult)>> {
    let mut hits: Vec<Option<ProviderListResult>> = Vec::with_capacity(accounts.len());
    let mut misses = Vec::new();
    for (account, key) in accounts {
        let hit = memo.get(key);
        if hit.is_none() {
            misses.push(account.clone());
        }
        hits.push(hit);
    }
    let mut fetched = if misses.is_empty() && !always_fetch {
        Vec::new()
    } else {
        fetch(misses)?
    };
    let mut out = Vec::with_capacity(accounts.len() + fetched.len());
    for ((account, key), hit) in accounts.iter().zip(hits) {
        if let Some(hit) = hit {
            out.push((account.name.clone(), hit));
        } else if let Some(at) = fetched.iter().position(|(name, _)| *name == account.name) {
            let row = fetched.remove(at);
            memo.put(key.clone(), row.1.clone());
            out.push(row);
        }
    }
    out.extend(fetched);
    Some(out)
}

/// Pure: does this pass reach the network? Only a UI pass whose cache the
/// daemon is keeping warm (it holds the tracker lease AND refreshes this very
/// cache key) skips the fetch. The daemon never yields to itself.
pub(crate) fn should_fetch(site: RefreshSite, daemon_covers: bool) -> bool {
    match site {
        RefreshSite::Daemon => true,
        RefreshSite::Ui => !daemon_covers,
    }
}

/// One autopilot pickup handed over by a refresh pass.
pub(crate) struct PickupArgs<'a> {
    pub(crate) cfg: &'a thegn_core::config::Config,
    pub(crate) repo_root: &'a std::path::Path,
    pub(crate) cwd: &'a std::path::Path,
    pub(crate) account: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) issues: &'a [thegn_core::issue::Issue],
    pub(crate) from_assignee_me: bool,
}

fn ui_pickup(args: &PickupArgs<'_>) {
    crate::autopilot_driver::pickup(
        args.cfg,
        args.repo_root,
        args.cwd,
        args.account,
        args.provider,
        args.issues,
        args.from_assignee_me,
    );
}

fn daemon_pickup(args: &PickupArgs<'_>) {
    crate::daemon::producers::queues::daemon_pickup(
        args.cfg,
        &crate::daemon::producers::queues::PickupRequest {
            repo_root: args.repo_root,
            cwd: args.cwd,
            account: args.account,
            provider: args.provider,
            issues: args.issues,
            from_assignee_me: args.from_assignee_me,
        },
    );
}

/// Route one pickup by site. The UI keeps its own driver (which checks the
/// repo's autopilot policy itself); the daemon hands over to the queues
/// producer, and only for a repo whose `[autopilot]` is enabled, so a daemon
/// tick can never start work for a repo with autopilot off.
fn route_pickup(
    site: RefreshSite,
    args: &PickupArgs<'_>,
    ui: &dyn Fn(&PickupArgs<'_>),
    daemon: &dyn Fn(&PickupArgs<'_>),
    autopilot_enabled: &dyn Fn(&PickupArgs<'_>) -> bool,
) {
    match site {
        RefreshSite::Ui => ui(args),
        RefreshSite::Daemon => {
            if autopilot_enabled(args) {
                daemon(args);
            }
        }
    }
}

fn repo_autopilot_enabled(args: &PickupArgs<'_>) -> bool {
    args.cfg.repo_autopilot(args.repo_root).enabled
}

/// Refresh one worktree's issue cache: fetch every enabled account of the
/// repo's tracker config (`IssuesConfig::active_accounts`), diff and emit per
/// `(cache key, provider, account)`, rewrite the cache, and run autopilot
/// pickup for the site. Blocking (network + DB); callers are off-loop.
///
/// A `Ui` pass yields while the daemon holds the tracker lease and refreshes
/// this cache key (the repo's main checkout): it fetches nothing, wakes the
/// loop so the hydration pass re-reads the cache (`populate_tracker`), and
/// returns [`RefreshSummary::YIELDED`].
#[allow(
    dead_code,
    reason = "the daemon tracker tick drives it once the b161 lane 5 owner arms it"
)]
pub(crate) fn refresh_repo(
    cwd: &std::path::Path,
    app_cfg: &thegn_core::config::Config,
    waker: Option<&TerminalWaker>,
    site: RefreshSite,
) -> RefreshSummary {
    refresh_repo_inner(cwd, app_cfg, waker, site, None, &ListMemo::default())
}

/// [`refresh_repo`] sharing a caller-owned [`ListMemo`]: the daemon tick
/// passes one memo for every repo it refreshes, so an account shared by N
/// repos is listed once per tick. The cache diff and emit stay per repo.
pub(crate) fn refresh_repo_memo(
    cwd: &std::path::Path,
    app_cfg: &thegn_core::config::Config,
    site: RefreshSite,
    memo: &ListMemo,
) -> RefreshSummary {
    refresh_repo_inner(cwd, app_cfg, None, site, None, memo)
}

fn refresh_repo_inner(
    cwd: &std::path::Path,
    app_cfg: &thegn_core::config::Config,
    waker: Option<&TerminalWaker>,
    site: RefreshSite,
    report: Option<&RefreshReportTarget>,
    memo: &ListMemo,
) -> RefreshSummary {
    use thegn_core::issue::IssueFilter;
    use thegn_svc::issue::IssueRouter;

    // Reports and toasts belong to the interactive process only.
    let report = match site {
        RefreshSite::Ui => report,
        RefreshSite::Daemon => None,
    };
    let mut summary = RefreshSummary::default();
    let cwd = cwd.to_path_buf();
    if !cwd.is_dir() {
        record_tracker_failure(&cwd, "worktree unavailable", waker);
        if let Some(report) = report {
            send_tracker_toast(
                &report.tx,
                &report.waker,
                "Issue refresh failed — worktree is unavailable".to_owned(),
                thegn_core::notification::Priority::Alert,
            );
        }
        return summary;
    }
    let repo_root = thegn_core::repo::main_worktree(&cwd).unwrap_or_else(|| cwd.clone());
    // Only the background producer yields; an explicit refresh (report set) runs as before.
    let yield_to_daemon = site == RefreshSite::Ui
        && report.is_none()
        && crate::producer_yield::daemon_owns(thegn_core::producer_lease::ProducerKind::Tracker);
    let plugin_accounts: std::collections::HashSet<String> =
        crate::plugin_providers::issue_backends()
            .into_iter()
            .map(|(account, _)| account)
            .collect();
    if site == RefreshSite::Ui {
        // The daemon refreshes each repo's main checkout, so it only keeps
        // THIS cache key warm when the UI is on that checkout; a linked
        // worktree's own key still needs the UI's pass. Plugin issue
        // providers live only in the UI process (the daemon cannot reach
        // them), so a UI with any keeps its own pass too.
        let daemon_covers = yield_to_daemon && repo_root == cwd && plugin_accounts.is_empty();
        if !should_fetch(site, daemon_covers) {
            if let Some(w) = waker {
                let _ = w.wake(); // best-effort: waker pulse: the hydration pass re-reads the daemon-written cache
            }
            return RefreshSummary::YIELDED;
        }
    }
    let cfg = app_cfg.repo_issues(Some(&repo_root));
    let accounts = cfg.active_accounts();
    if accounts.is_empty() && plugin_accounts.is_empty() {
        if let Some(report) = report {
            send_tracker_toast(
                &report.tx,
                &report.waker,
                "No issue tracker configured".to_owned(),
                thegn_core::notification::Priority::Info,
            );
        }
        return summary;
    }
    if report.is_none()
        && plugin_accounts.is_empty()
        && issue_caches_are_fresh(&cwd, &accounts, cfg.ttl_secs, thegn_core::util::now())
    {
        return summary;
    }
    // Repo scope for GitHub (other providers scope by team/project and
    // ignore `repo`). Mirrors `spawn_my_work_refresh`.
    let loc = thegn_core::remote::GitLoc::for_worktree(&cwd);
    let filter = IssueFilter {
        assignee_me: cfg.filter_assignee_me,
        limit: cfg.max_issues,
        repo: crate::forge_handle::get()
            .for_loc(&loc)
            .repo_ref(&loc)
            .map(|r| r.nwo()),
        ..Default::default()
    };
    // Fetch every configured account; cache and diff each under its own
    // `(repo_root, provider, account)` key so trackers (and multiple
    // accounts of one provider) aggregate without clobbering. Accounts the
    // memo already answered this tick (another repo sharing the account and
    // filter) are not listed again; live plugin providers are never memoized.
    let keyed: Vec<_> = accounts
        .into_iter()
        .map(|account| {
            let key = list_memo_key(&account, &filter, &cwd);
            (account, key)
        })
        .collect();
    let mut fetch_misses = |misses: Vec<thegn_core::config::IssueAccount>| {
        let mut scoped = cfg.clone();
        scoped.issue_accounts = misses;
        // An empty miss list must mean no accounts, not the legacy synthesis.
        scoped.accounts_restricted = true;
        let mut router = IssueRouter::from_config_at(&scoped, Some(&cwd));
        // Provider-as-plugin: append live plugin issue providers.
        crate::plugin_providers::extend_issue_router(&mut router);
        if !router.is_configured() {
            return Some(Vec::new());
        }
        let rows = thegn_svc::issue::block_on_tracker(router.list_per_provider(&filter)).ok()?;
        Some(
            rows.into_iter()
                .map(|(account, provider, result)| {
                    (
                        account,
                        ProviderListResult {
                            provider: provider.to_owned(),
                            outcome: ListOutcome::from_result(result),
                        },
                    )
                })
                .collect::<Vec<_>>(),
        )
    };
    let Some(per_provider) =
        fetch_memoized(&keyed, memo, !plugin_accounts.is_empty(), &mut fetch_misses)
    else {
        record_tracker_failure(&cwd, "could not start tracker runtime", waker);
        if let Some(report) = report {
            send_tracker_toast(
                &report.tx,
                &report.waker,
                "Issue refresh failed — could not start the tracker runtime".to_owned(),
                thegn_core::notification::Priority::Alert,
            );
        }
        return summary;
    };
    let mut failures = Vec::new();
    let mut any_success = false;
    let Ok(db) = thegn_core::db::Db::open() else {
        record_tracker_failure(&cwd, "could not open issue cache", waker);
        if let Some(report) = report {
            send_refresh_report(report, &[], Some("could not open the issue cache"), false);
        }
        return summary;
    };
    let repo_key = cwd.to_string_lossy();
    let linked: std::collections::HashSet<String> = db
        .linked_issues(&repo_key)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let mut changed = false;
    let shared = shared_issue_ids(&per_provider);
    for (account, ProviderListResult { provider, outcome }) in per_provider {
        let provider: &str = &provider;
        // The daemon produces non-plugin accounts while it holds the lease.
        let daemon_produces = yield_to_daemon && !plugin_accounts.contains(&account);
        if daemon_produces && repo_root == cwd {
            continue; // the daemon writes and diffs this cache row
        }
        let issues = match outcome {
            ListOutcome::Ok(issues) => {
                any_success = true;
                summary.accounts_ok += 1;
                // Reached the tracker — online evidence for the app-wide holder.
                thegn_core::connectivity::report_success();
                issues
            }
            ListOutcome::Partial { issues, reason } => {
                tracing::warn!(account = %account, provider, reason, "tracker issue list was partial");
                summary.accounts_failed += 1;
                failures.push((format!("{provider}/{account}"), reason.to_owned()));
                let last_good = db
                    .get_issue_cache(&repo_key, provider, &account)
                    .ok()
                    .flatten()
                    .and_then(|(json, _)| IssueCacheEntry::parse(&json))
                    .map(|mut entry| {
                        entry.issues.retain(|issue| {
                            thegn_svc::issue::validate_issue_identity(issue).is_ok()
                        });
                        entry
                    });
                let partial_rows = issues
                    .into_iter()
                    .filter(|issue| thegn_svc::issue::validate_issue_identity(issue).is_ok())
                    .collect();
                let entry = partial_cache_entry(
                    last_good,
                    partial_rows,
                    &account,
                    reason,
                    thegn_core::util::now_ms(),
                );
                if let Ok(json) = entry.to_json()
                    && let Err(error) = db.put_issue_cache(&repo_key, provider, &account, &json)
                {
                    tracing::debug!(account = %account, provider, %error, "could not store partial issue cache entry");
                }
                changed = true;
                continue;
            }
            ListOutcome::Failed { message, transient } => {
                // The arm was silent, so a permanently-failing account (bad
                // token, 400) showed as an empty tracker forever with no
                // trace anywhere (THE-72). One warn; the ring picks it up
                // for crash reports/bundles even with no sink installed.
                tracing::warn!(account = %account, provider, error = %message, "tracker refresh failed");
                summary.accounts_failed += 1;
                failures.push((format!("{provider}/{account}"), message));
                // Only a dropped link is offline evidence (not a bad token).
                if transient {
                    thegn_core::connectivity::report_failure();
                }
                continue; // a failing account leaves its prior cache intact
            }
        };
        let Ok(json) = serde_json::to_string(&issues) else {
            continue;
        };
        // Diff old vs new for this account to emit notifications first.
        let cached = db
            .get_issue_cache(&repo_key, provider, &account)
            .ok()
            .flatten();
        let parsed = cached.and_then(|(j, _)| IssueCacheEntry::parse(&j));
        let had_cache = parsed.is_some();
        let old_issues: Vec<thegn_core::issue::Issue> = parsed
            .map(|entry| entry.issues)
            .unwrap_or_default()
            .into_iter()
            .filter(|issue| thegn_svc::issue::validate_issue_identity(issue).is_ok())
            .collect();
        for (kind, source_ref, msg) in tracker_diff_notifications(&old_issues, &issues, &linked) {
            let msg = account_qualified(msg, &source_ref, &shared, &account);
            // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            if crate::automation_events::emit(&db, kind, &source_ref, &msg, &repo_key).is_ok() {
                summary.events += 1;
            }
        }
        if !daemon_produces {
            crate::automation_issue::emit_matched(
                &db,
                &crate::automation_issue::IssueRefresh {
                    cfg: app_cfg,
                    repo_key: &repo_key,
                    provider,
                    account: &account,
                    assignee_me: cfg.filter_assignee_me,
                    had_cache,
                },
                &old_issues,
                &issues,
            );
        }
        // Overdue is re-derived (not diffed) each refresh; the store-side
        // emit-once (`put_notification_once`) keeps it to one row per
        // (issue, due date).
        for (source_ref, msg) in overdue_notifications(&issues, &linked, thegn_core::util::now()) {
            let msg = account_qualified(msg, &source_ref, &shared, &account);
            // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            if matches!(
                crate::automation_events::emit_once(&db, "overdue", &source_ref, &msg, &repo_key),
                Ok(true)
            ) {
                summary.events += 1;
            }
        }
        let _ = db.put_issue_cache(&repo_key, provider, &account, &json); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        changed = true;
        // The UI refresh already runs on `sched::spawn_bg` (the daemon tick
        // on `spawn_blocking`); autopilot claims here but schedules each slow
        // agent on its own bounded background worker. It receives filter
        // provenance rather than guessing consent from issue display fields.
        if !daemon_produces {
            route_pickup(
                site,
                &PickupArgs {
                    cfg: app_cfg,
                    repo_root: &repo_root,
                    cwd: &cwd,
                    account: &account,
                    provider,
                    issues: &issues,
                    from_assignee_me: cfg.filter_assignee_me,
                },
                &ui_pickup,
                &daemon_pickup,
                &repo_autopilot_enabled,
            );
        }
    }
    let failure_only = !any_success && !failures.is_empty();
    if failure_only {
        if let Some((_, message)) = failures.first() {
            tracker_failures()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    cwd.to_string_lossy().into_owned(),
                    safe_report_text(message),
                );
        }
    } else if any_success {
        tracker_failures()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&cwd.to_string_lossy().into_owned());
    }
    if (changed || failure_only)
        && let Some(w) = waker
    {
        let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    }
    if let Some(report) = report {
        send_refresh_report(report, &failures, None, any_success);
    }
    summary
}

fn issue_caches_are_fresh(
    cwd: &std::path::Path,
    accounts: &[thegn_core::config::IssueAccount],
    ttl_secs: u64,
    now: i64,
) -> bool {
    let Ok(db) = thegn_core::db::Db::open() else {
        return false;
    };
    let repo_key = cwd.to_string_lossy();
    accounts.iter().all(|account| {
        db.get_issue_cache(&repo_key, account.provider.as_str(), &account.name)
            .ok()
            .flatten()
            .is_some_and(|(_, fetched_at)| {
                now.saturating_sub(fetched_at)
                    < thegn_core::time_policy::saturating_i64(u128::from(ttl_secs))
            })
    })
}

const MAX_REPORT_ENTRIES: usize = 3;
const MAX_REPORT_ENTRY_CHARS: usize = 180;
const MAX_REPORT_MESSAGE_CHARS: usize = MAX_REPORT_ENTRIES * (2 * MAX_REPORT_ENTRY_CHARS + 4) + 64;

fn safe_report_text(value: &str) -> String {
    truncate_report_text(&sanitize_and_redact(value), MAX_REPORT_ENTRY_CHARS)
}

fn sanitize_and_redact(value: &str) -> String {
    let sanitized = thegn_svc::lsp::sanitize_for_terminal(value);
    thegn_core::ci_log::redact(&sanitized)
}

fn truncate_report_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let short: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

fn format_refresh_report(
    failures: &[(String, String)],
    db_error: Option<&str>,
    any_success: bool,
) -> Option<(String, thegn_core::notification::Priority)> {
    let mut entries: Vec<String> = failures
        .iter()
        .take(MAX_REPORT_ENTRIES)
        .map(|(account, error)| {
            format!("{}: {}", safe_report_text(account), safe_report_text(error))
        })
        .collect();
    let omitted = failures.len().saturating_sub(MAX_REPORT_ENTRIES);
    if omitted > 0 {
        entries.push(format!("and {omitted} more"));
    }
    if let Some(error) = db_error {
        entries.clear();
        entries.push(format!("issue cache: {}", safe_report_text(error)));
    }
    if !entries.is_empty() {
        Some((
            format!("Issue refresh failed — {}", entries.join("; ")),
            thegn_core::notification::Priority::Alert,
        ))
    } else if any_success {
        Some((
            "Issues refreshed".to_owned(),
            thegn_core::notification::Priority::Info,
        ))
    } else {
        None
    }
}

fn send_refresh_report(
    report: &RefreshReportTarget,
    failures: &[(String, String)],
    db_error: Option<&str>,
    any_success: bool,
) {
    if let Some((message, priority)) = format_refresh_report(failures, db_error, any_success) {
        send_tracker_toast(&report.tx, &report.waker, message, priority);
    }
}

pub(crate) fn send_tracker_toast(
    tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: &TerminalWaker,
    message: String,
    priority: thegn_core::notification::Priority,
) {
    let message = prepare_tracker_toast_message(&message);
    if let Err(error) = tx.send(crate::hydrate::RefreshKind::Toast { message, priority }) {
        tracing::debug!(%error, "failed to send tracker toast");
    }
    if let Err(error) = waker.wake() {
        tracing::debug!(%error, "failed to wake after tracker toast");
    }
}

fn prepare_tracker_toast_message(message: &str) -> String {
    truncate_report_text(&sanitize_and_redact(message), MAX_REPORT_MESSAGE_CHARS)
}

/// The pure old-vs-new cache diff behind the tracker refresh's notification
/// emits: `(kind, source_ref, message)` rows for
/// - **status_changed** — a LINKED issue's status changed;
/// - **blocker_resolved** — a blocker of a LINKED issue flipped to Done this
///   refresh (the blocker itself need not be linked). Requires the blocker to
///   have been present before at a non-Done status, so a first fetch (empty
///   old cache) can never spray stale "resolved" notifications.
///
/// Pure so the emit-once semantics are unit-testable without a DB or network.
pub(crate) fn tracker_diff_notifications(
    old: &[thegn_core::issue::Issue],
    new: &[thegn_core::issue::Issue],
    linked: &std::collections::HashSet<String>,
) -> Vec<(&'static str, String, String)> {
    use thegn_core::issue::IssueStatus;
    let old_status: std::collections::HashMap<&str, IssueStatus> =
        old.iter().map(|i| (i.id.as_str(), i.status)).collect();
    let new_by_id: std::collections::HashMap<&str, &thegn_core::issue::Issue> =
        new.iter().map(|i| (i.id.as_str(), i)).collect();
    let mut out = Vec::new();
    for issue in new {
        if !linked.contains(&issue.id) {
            continue;
        }
        if let Some(&os) = old_status.get(issue.id.as_str())
            && os != issue.status
        {
            out.push((
                "status_changed",
                issue.id.clone(),
                format!(
                    "{} status changed to {}",
                    issue.number,
                    issue.status.label()
                ),
            ));
        }
        // A blocker done ⇒ this linked issue is unblocked. Attribute the
        // notification to the LINKED issue (that's whose worktree the user
        // cares about), naming the blocker in the message.
        for b_id in &issue.blocked_by {
            let Some(b_new) = new_by_id.get(b_id.as_str()) else {
                continue;
            };
            if b_new.status == IssueStatus::Done
                && old_status
                    .get(b_id.as_str())
                    .is_some_and(|&os| os != IssueStatus::Done)
            {
                out.push((
                    "blocker_resolved",
                    issue.id.clone(),
                    format!(
                        "{} unblocked — blocker {} is done",
                        issue.number, b_new.number
                    ),
                ));
            }
        }
    }
    out
}

/// Pure: `(source_ref, message)` rows for LINKED issues that are past their
/// due date and not Done/Cancelled. The message embeds the due date, so the
/// store's `(kind, ref, message)` emit-once dedupe fires once per (issue,
/// due date) — moving the date re-arms, a rerun with the same date doesn't.
pub(crate) fn overdue_notifications(
    issues: &[thegn_core::issue::Issue],
    linked: &std::collections::HashSet<String>,
    now_ms: i64,
) -> Vec<(String, String)> {
    use thegn_core::issue::IssueStatus;
    issues
        .iter()
        .filter(|i| linked.contains(&i.id))
        .filter(|i| !matches!(i.status, IssueStatus::Done | IssueStatus::Cancelled))
        .filter_map(|i| {
            let due = i.due_at_ms?;
            if due >= now_ms {
                return None;
            }
            let date = chrono::DateTime::from_timestamp_millis(due)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| due.to_string());
            Some((
                i.id.clone(),
                format!("{} overdue (was due {date})", i.number),
            ))
        })
        .collect()
}

pub(crate) fn shared_issue_ids(
    per_account: &[(String, ProviderListResult)],
) -> std::collections::HashSet<String> {
    let rows = per_account.iter().flat_map(|(_, result)| {
        result
            .outcome
            .issues()
            .iter()
            .map(|issue| (issue.id.as_str(), issue.account.as_deref()))
    });
    thegn_core::issue::ids_shared_across_accounts(rows)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

pub(crate) fn account_qualified(
    message: String,
    source_ref: &str,
    shared: &std::collections::HashSet<String>,
    account: &str,
) -> String {
    if shared.contains(source_ref) {
        format!("{message} ({account})")
    } else {
        message
    }
}

pub(crate) fn move_on_merge_failure_message(id: &str, error: &str) -> String {
    format!(
        "could not move {id} to Done on merge: {}",
        safe_report_text(error)
    )
}

/// Load the tracker caches into the panel model (hydration-thread side, DB
/// only — the background refresh keeps the cache warm). Loads every cached
/// provider for this repo and concatenates, so multiple trackers (e.g.
/// Linear + Jira) aggregate into one list.
pub(crate) fn populate_tracker(
    db: &thegn_core::db::Db,
    repo_key: &str,
    cwd: &std::path::Path,
    app_cfg: &thegn_core::config::Config,
    panel: &mut crate::panel::PanelData,
) {
    let cached = db.get_all_issue_cache(repo_key);
    let load = tracker_load(
        cached.as_ref().map(|rows| rows.len()).map_err(|_| ()),
        tracker_failures()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(repo_key)
            .map(String::as_str),
    );
    panel.tracker_load = load;
    if let Ok(cached) = cached {
        for (_provider, json) in cached {
            if let Some(mut entry) = IssueCacheEntry::parse(&json) {
                entry
                    .issues
                    .retain(|issue| thegn_svc::issue::validate_issue_identity(issue).is_ok());
                panel.tracker_issues.append(&mut entry.issues);
                if let Some(partial) = entry.partial {
                    panel.tracker_partial.push(partial);
                }
            }
        }
    }
    if let Ok(links) = db.linked_issues(&cwd.to_string_lossy()) {
        panel.tracker_links = links;
    }
    // Pure config check (no secrets, no network): is any issue account active
    // (explicit `[[issue_accounts]]` or a synthesized legacy provider)? Lets the
    // panel say "off" (unconfigured) vs "clear" (empty) honestly.
    panel.issues_configured = !app_cfg.issues.active_accounts().is_empty();
}

#[cfg(test)]
mod tests {
    use super::{
        BusyRetry, MAX_REPORT_ENTRY_CHARS, RefreshCoalescer, RefreshGuard, account_qualified,
        format_refresh_report, move_on_merge_failure_message, overdue_notifications,
        partial_cache_entry, populate_tracker, prepare_tracker_toast_message,
        record_tracker_failure, safe_report_text, shared_issue_ids, tracker_failures, tracker_load,
    };
    use super::{PickupArgs, RefreshSite, RefreshSummary, route_pickup, should_fetch};

    #[test]
    fn shared_issue_ids_covers_ok_and_partial_accounts() {
        use super::{ListOutcome, ProviderListResult};
        use thegn_core::issue::Issue;

        let row = |account: &str, outcome| {
            (
                account.to_owned(),
                ProviderListResult {
                    provider: "linear".to_owned(),
                    outcome,
                },
            )
        };
        let per_provider = vec![
            row(
                "work",
                ListOutcome::Ok(vec![
                    Issue {
                        id: "linear:ENG-1".into(),
                        account: Some("work".into()),
                        ..Default::default()
                    },
                    Issue {
                        id: "linear:ENG-2".into(),
                        ..Default::default()
                    },
                ]),
            ),
            row(
                "home",
                ListOutcome::Partial {
                    issues: vec![Issue {
                        id: "linear:ENG-1".into(),
                        account: Some("home".into()),
                        ..Default::default()
                    }],
                    reason: "page cap",
                },
            ),
            row(
                "x",
                ListOutcome::Failed {
                    message: "not configured".into(),
                    transient: false,
                },
            ),
        ];

        assert_eq!(
            shared_issue_ids(&per_provider),
            std::collections::HashSet::from(["linear:ENG-1".to_owned()])
        );
    }

    mod list_memo {
        use super::super::{
            ListMemo, ListOutcome, ProviderListResult, fetch_memoized, list_memo_key,
        };
        use std::cell::Cell;
        use std::path::Path;
        use thegn_core::config::{IssueAccount, IssueProviderKind};
        use thegn_core::issue::{Issue, IssueFilter};

        type Rows = Vec<(String, ProviderListResult)>;

        fn account(name: &str, provider: IssueProviderKind) -> IssueAccount {
            IssueAccount {
                name: name.to_owned(),
                provider,
                token: "env:TRACKER_TOKEN".to_owned(),
                ..IssueAccount::default()
            }
        }

        #[test]
        fn repo_scope_only_splits_the_key_for_github() {
            let f = |r: &str| IssueFilter {
                repo: Some(r.into()),
                ..IssueFilter::default()
            };
            let lin = account("lin", IssueProviderKind::Linear);
            assert_eq!(
                list_memo_key(&lin, &f("o/a"), Path::new("/repo/a")),
                list_memo_key(&lin, &f("o/b"), Path::new("/repo/b"))
            );
            let gh = account("gh", IssueProviderKind::Github);
            assert_ne!(
                list_memo_key(&gh, &f("o/a"), Path::new("/repo/a")),
                list_memo_key(&gh, &f("o/b"), Path::new("/repo/b"))
            );
        }

        fn keyed(
            accounts: &[IssueAccount],
            filter: &IssueFilter,
            cwd: &str,
        ) -> Vec<(IssueAccount, String)> {
            accounts
                .iter()
                .map(|a| (a.clone(), list_memo_key(a, filter, Path::new(cwd))))
                .collect()
        }

        /// A fake router: counts list calls (one per fetch with misses) and
        /// answers every requested account with `outcome`.
        fn fake(
            calls: &Cell<usize>,
            listed: &Cell<usize>,
            outcome: ListOutcome,
        ) -> impl FnMut(Vec<IssueAccount>) -> Option<Rows> {
            move |misses| {
                calls.set(calls.get() + 1);
                listed.set(listed.get() + misses.len());
                Some(
                    misses
                        .into_iter()
                        .map(|a| {
                            (
                                a.name.clone(),
                                ProviderListResult {
                                    provider: a.provider.as_str().to_owned(),
                                    outcome: outcome.clone(),
                                },
                            )
                        })
                        .collect(),
                )
            }
        }

        fn ok_issue() -> ListOutcome {
            ListOutcome::Ok(vec![Issue {
                id: "linear:ENG-1".into(),
                ..Default::default()
            }])
        }

        #[test]
        fn two_repos_sharing_an_account_make_one_list_call_per_tick() {
            let accounts = [account("work", IssueProviderKind::Linear)];
            let filter = IssueFilter {
                assignee_me: true,
                limit: 100,
                ..IssueFilter::default()
            };
            let memo = ListMemo::default();
            let (calls, listed) = (Cell::new(0), Cell::new(0));
            let mut fetch = fake(&calls, &listed, ok_issue());
            let a = fetch_memoized(
                &keyed(&accounts, &filter, "/repo/a"),
                &memo,
                false,
                &mut fetch,
            )
            .unwrap();
            let b = fetch_memoized(
                &keyed(&accounts, &filter, "/repo/b"),
                &memo,
                false,
                &mut fetch,
            )
            .unwrap();
            assert_eq!(calls.get(), 1, "the second repo reused the tick memo");
            assert_eq!(listed.get(), 1);
            assert_eq!(a.len(), 1);
            assert_eq!(b.len(), 1);
            assert_eq!(b[0].0, "work");
            assert_eq!(b[0].1.provider, "linear");
            assert_eq!(b[0].1.outcome.issues().len(), 1);
        }

        #[test]
        fn different_filters_make_two_list_calls() {
            let accounts = [account("work", IssueProviderKind::Linear)];
            let mine = IssueFilter {
                assignee_me: true,
                ..IssueFilter::default()
            };
            let everyone = IssueFilter {
                assignee_me: false,
                ..IssueFilter::default()
            };
            let memo = ListMemo::default();
            let (calls, listed) = (Cell::new(0), Cell::new(0));
            let mut fetch = fake(&calls, &listed, ok_issue());
            fetch_memoized(
                &keyed(&accounts, &mine, "/repo/a"),
                &memo,
                false,
                &mut fetch,
            )
            .unwrap();
            fetch_memoized(
                &keyed(&accounts, &everyone, "/repo/b"),
                &memo,
                false,
                &mut fetch,
            )
            .unwrap();
            assert_eq!(calls.get(), 2);
        }

        #[test]
        fn a_changed_account_entry_is_not_a_memo_hit() {
            let filter = IssueFilter::default();
            let team_a = IssueAccount {
                team_id: "A".into(),
                ..account("work", IssueProviderKind::Linear)
            };
            let team_b = IssueAccount {
                team_id: "B".into(),
                ..account("work", IssueProviderKind::Linear)
            };
            assert_ne!(
                list_memo_key(&team_a, &filter, Path::new("/repo/a")),
                list_memo_key(&team_b, &filter, Path::new("/repo/a")),
            );
        }

        #[test]
        fn an_unscoped_github_account_is_keyed_per_worktree() {
            let gh = account("gh", IssueProviderKind::Github);
            let unscoped = IssueFilter::default();
            assert_ne!(
                list_memo_key(&gh, &unscoped, Path::new("/repo/a")),
                list_memo_key(&gh, &unscoped, Path::new("/repo/b")),
            );
            let scoped = IssueFilter {
                repo: Some("o/r".into()),
                ..IssueFilter::default()
            };
            assert_eq!(
                list_memo_key(&gh, &scoped, Path::new("/repo/a")),
                list_memo_key(&gh, &scoped, Path::new("/repo/b")),
            );
        }

        #[test]
        fn an_error_is_memoized() {
            let accounts = [account("work", IssueProviderKind::Linear)];
            let filter = IssueFilter::default();
            let memo = ListMemo::default();
            let (calls, listed) = (Cell::new(0), Cell::new(0));
            let failed = ListOutcome::Failed {
                message: "401 bad token".into(),
                transient: false,
            };
            let mut fetch = fake(&calls, &listed, failed);
            for repo in ["/repo/a", "/repo/b", "/repo/c"] {
                let rows =
                    fetch_memoized(&keyed(&accounts, &filter, repo), &memo, false, &mut fetch)
                        .unwrap();
                assert!(matches!(
                    &rows[0].1.outcome,
                    ListOutcome::Failed { message, .. } if message == "401 bad token"
                ));
            }
            assert_eq!(calls.get(), 1, "a failing account is not retried per repo");
        }

        #[test]
        fn a_runtime_failure_memoizes_nothing() {
            let accounts = [account("work", IssueProviderKind::Linear)];
            let filter = IssueFilter::default();
            let memo = ListMemo::default();
            let calls = Cell::new(0);
            let mut down = |_: Vec<IssueAccount>| -> Option<Rows> {
                calls.set(calls.get() + 1);
                None
            };
            let rows = keyed(&accounts, &filter, "/repo/a");
            assert!(fetch_memoized(&rows, &memo, false, &mut down).is_none());
            assert!(fetch_memoized(&rows, &memo, false, &mut down).is_none());
            assert_eq!(calls.get(), 2);
        }

        #[test]
        fn plugin_rows_are_always_fetched_and_never_memoized() {
            let accounts = [account("work", IssueProviderKind::Linear)];
            let filter = IssueFilter::default();
            let memo = ListMemo::default();
            let calls = Cell::new(0);
            let mut fetch = |misses: Vec<IssueAccount>| -> Option<Rows> {
                calls.set(calls.get() + 1);
                let mut rows: Rows = misses
                    .into_iter()
                    .map(|a| {
                        (
                            a.name,
                            ProviderListResult {
                                provider: "linear".into(),
                                outcome: ok_issue(),
                            },
                        )
                    })
                    .collect();
                rows.push((
                    "plug".into(),
                    ProviderListResult {
                        provider: "plugin:demo".into(),
                        outcome: ListOutcome::Ok(Vec::new()),
                    },
                ));
                Some(rows)
            };
            let first =
                fetch_memoized(&keyed(&accounts, &filter, "/a"), &memo, true, &mut fetch).unwrap();
            let second =
                fetch_memoized(&keyed(&accounts, &filter, "/b"), &memo, true, &mut fetch).unwrap();
            assert_eq!(calls.get(), 2);
            let names = |rows: &Rows| rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>();
            assert_eq!(names(&first), ["work", "plug"]);
            // The second pass took `work` from the memo; only the plugin was listed.
            assert_eq!(names(&second), ["work", "plug"]);
            assert_eq!(memo.0.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn account_qualified_tags_only_shared_ids() {
        let shared = std::collections::HashSet::from(["linear:ENG-1".to_owned()]);
        assert_eq!(
            account_qualified(
                "ENG-1 overdue (was due 2026-10-01)".into(),
                "linear:ENG-1",
                &shared,
                "work"
            ),
            "ENG-1 overdue (was due 2026-10-01) (work)"
        );
        assert_eq!(
            account_qualified(
                "ENG-2 overdue (was due 2026-10-01)".into(),
                "linear:ENG-2",
                &shared,
                "work"
            ),
            "ENG-2 overdue (was due 2026-10-01)"
        );
    }

    #[test]
    fn two_accounts_overdue_rows_stay_distinct() {
        use thegn_core::issue::Issue;

        let linked = std::collections::HashSet::from(["linear:ENG-1".to_owned()]);
        let shared = linked.clone();
        let messages = ["work", "home"].map(|account| {
            let issues = [Issue {
                id: "linear:ENG-1".into(),
                number: "ENG-1".into(),
                account: Some(account.into()),
                due_at_ms: Some(0),
                ..Default::default()
            }];
            let (_, message) = overdue_notifications(&issues, &linked, 1)
                .into_iter()
                .next()
                .unwrap();
            account_qualified(message, "linear:ENG-1", &shared, account)
        });

        assert_ne!(messages[0], messages[1]);
    }

    #[test]
    fn move_on_merge_failure_message_names_issue_and_redacts() {
        let message = move_on_merge_failure_message("linear:ENG-1", "auth failed\u{1b}evil");

        assert!(message.contains("linear:ENG-1"));
        assert!(message.contains("Done on merge"));
        assert!(!message.contains('\u{1b}'));
    }

    #[test]
    fn partial_refresh_keeps_last_good_rows_and_marks_partial() {
        let last_good = thegn_core::issue::IssueCacheEntry {
            issues: vec![thegn_core::issue::Issue::default(); 2],
            partial: None,
        };
        let entry = partial_cache_entry(
            Some(last_good.clone()),
            vec![thegn_core::issue::Issue::default()],
            "work",
            "page cap",
            7,
        );
        assert_eq!(entry.issues, last_good.issues);
        assert_eq!(
            entry.partial,
            Some(thegn_core::issue::IssueCachePartial {
                account: "work".into(),
                reason: "page cap".into(),
                checked_at_ms: 7,
                showing_last_good: Some(true),
            })
        );
    }

    #[test]
    fn partial_refresh_without_last_good_keeps_partial_rows() {
        let partial_rows = vec![thegn_core::issue::Issue::default()];
        for last_good in [None, Some(thegn_core::issue::IssueCacheEntry::default())] {
            let entry = partial_cache_entry(last_good, partial_rows.clone(), "work", "cap", 9);
            assert_eq!(entry.issues, partial_rows);
            assert!(entry.partial.is_some());
        }
    }

    #[test]
    fn repeated_partial_refresh_keeps_the_first_partial_snapshot() {
        let first_partial = vec![thegn_core::issue::Issue {
            id: "first".into(),
            ..Default::default()
        }];
        let cached = partial_cache_entry(None, first_partial.clone(), "work", "cap", 9);
        let refreshed = partial_cache_entry(
            Some(cached),
            vec![thegn_core::issue::Issue {
                id: "newer".into(),
                ..Default::default()
            }],
            "work",
            "cap",
            10,
        );
        assert_eq!(refreshed.issues, first_partial);
        assert!(
            refreshed
                .partial
                .as_ref()
                .is_some_and(|partial| partial.showing_last_good == Some(false))
        );
    }

    #[test]
    fn partial_cache_populates_panel_partial_state() {
        use std::path::Path;
        use thegn_core::store::CacheStore;

        let db = thegn_core::db::Db::open_memory().unwrap();
        let issue = thegn_core::issue::Issue {
            id: "linear:ABC-1".into(),
            number: "ABC-1".into(),
            provider: "linear".into(),
            url: "https://linear.example/ABC-1".into(),
            ..Default::default()
        };
        let entry = thegn_core::issue::IssueCacheEntry {
            issues: vec![issue.clone()],
            partial: Some(thegn_core::issue::IssueCachePartial {
                account: "work".into(),
                reason: "page cap".into(),
                checked_at_ms: 7,
                showing_last_good: Some(false),
            }),
        };
        db.put_issue_cache("fixture-repo", "linear", "work", &entry.to_json().unwrap())
            .unwrap();
        let mut panel = crate::panel::PanelData::default();
        populate_tracker(
            &db,
            "fixture-repo",
            Path::new("/fixture-worktree"),
            &thegn_core::config::Config::default(),
            &mut panel,
        );
        assert_eq!(panel.tracker_issues, vec![issue]);
        assert_eq!(
            panel.tracker_partial,
            entry.partial.into_iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn tracker_load_decides_pending_ready_failed() {
        use crate::panel::gitui::DocLoad;

        assert_eq!(tracker_load(Ok(0), None), DocLoad::Pending);
        assert_eq!(
            tracker_load(Ok(0), Some("offline")),
            DocLoad::Failed("offline".into())
        );
        assert_eq!(tracker_load(Ok(1), Some("offline")), DocLoad::Ready);
        assert_eq!(
            tracker_load(Err(()), None),
            DocLoad::Failed("issue cache unreadable".into())
        );
    }

    #[test]
    fn early_tracker_failure_is_visible_to_panel_load_state() {
        use crate::panel::gitui::DocLoad;
        use std::path::Path;

        let cwd = Path::new("/repo/early-tracker-failure-test");
        record_tracker_failure(cwd, "could not open issue cache", None);
        let message = tracker_failures()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&cwd.to_string_lossy().into_owned())
            .unwrap();

        assert_eq!(
            tracker_load(Ok(0), Some(&message)),
            DocLoad::Failed("could not open issue cache".into())
        );
    }

    #[test]
    fn busy_refresh_is_retried_once_after_backoff() {
        use std::path::PathBuf;
        use std::time::{Duration, Instant};

        let retry = BusyRetry::new();
        let now = Instant::now();
        retry.record(PathBuf::from("/repo"), now);
        assert_eq!(retry.take(now + Duration::from_secs(1)), None);
        assert_eq!(
            retry.take(now + Duration::from_secs(2)),
            Some(PathBuf::from("/repo"))
        );
        assert_eq!(retry.take(now + Duration::from_secs(3)), None);
    }

    #[test]
    fn coalesces_to_the_latest_request_and_releases_the_key() {
        let coalescer = RefreshCoalescer::new();
        assert_eq!(coalescer.begin("repo", 'A'), Some('A'));
        assert_eq!(coalescer.begin("repo", 'B'), None);
        assert_eq!(coalescer.begin("repo", 'C'), None);
        assert_eq!(coalescer.next("repo"), Some('C'));
        assert_eq!(coalescer.next("repo"), None);
        assert_eq!(coalescer.begin("repo", 'D'), Some('D'));
    }

    #[test]
    fn coalescer_merge_preserves_user_report_target_across_background_replacement() {
        let coalescer = RefreshCoalescer::new();
        assert_eq!(
            coalescer.begin("repo", ("active", None)),
            Some(("active", None))
        );
        assert_eq!(coalescer.begin("repo", ("user", Some("report"))), None);
        assert_eq!(
            coalescer.begin_with("repo", ("background", None), |previous, latest| {
                (latest.0, latest.1.or(previous.1))
            }),
            None
        );
        assert_eq!(coalescer.next("repo"), Some(("background", Some("report"))));
    }

    #[test]
    fn refresh_report_is_redacted_sanitized_and_bounded() {
        let failures = vec![
            (
                "linear/team".into(),
                "https://user:secret-token@host/path\r\u{1b}]0;evil\u{7} failed".into(),
            ),
            ("github/acct".into(), "failed".into()),
            ("jira/site".into(), "failed".into()),
            ("linear/extra".into(), "failed".into()),
        ];
        let (message, priority) = format_refresh_report(&failures, None, false).unwrap();
        assert_eq!(priority, thegn_core::notification::Priority::Alert);
        assert!(!message.contains("secret-token"));
        assert!(!message.contains('\u{1b}'));
        assert!(!message.contains('\r'));
        assert!(message.contains("and 1 more"));
        assert!(message.contains("linear/team:"));
        assert!(message.contains("github/acct:"));
        assert!(message.contains("jira/site:"));
        assert!(!message.contains("linear/extra:"));

        let (success, priority) = format_refresh_report(&[], None, true).unwrap();
        assert_eq!(success, "Issues refreshed");
        assert_eq!(priority, thegn_core::notification::Priority::Info);
        let (db_failure, priority) =
            format_refresh_report(&[], Some("cache lock held"), false).unwrap();
        assert_eq!(priority, thegn_core::notification::Priority::Alert);
        assert!(db_failure.contains("issue cache:"));
        assert!(
            safe_report_text(&"x".repeat(MAX_REPORT_ENTRY_CHARS + 10))
                .chars()
                .count()
                <= MAX_REPORT_ENTRY_CHARS + 1
        );
    }

    #[test]
    fn sent_toast_preserves_more_suffix_for_long_multi_account_failures() {
        let failures = (0..4)
            .map(|index| (format!("provider/account-{index}"), "x".repeat(180)))
            .collect::<Vec<_>>();
        let (message, _) = format_refresh_report(&failures, None, false).unwrap();

        let prepared = prepare_tracker_toast_message(&message);

        assert!(prepared.contains("and 1 more"));
        assert!(prepared.contains("provider/account-0:"));
        assert!(prepared.contains("provider/account-2:"));
        assert!(!prepared.contains("provider/account-3:"));
    }

    #[test]
    fn distinct_keys_do_not_coalesce() {
        let coalescer = RefreshCoalescer::new();
        assert_eq!(coalescer.begin("repo-a", 1), Some(1));
        assert_eq!(coalescer.begin("repo-b", 2), Some(2));
        assert_eq!(coalescer.next("repo-a"), None);
        assert_eq!(coalescer.next("repo-b"), None);
    }

    #[test]
    fn abandon_frees_the_key() {
        let coalescer = RefreshCoalescer::new();
        assert_eq!(coalescer.begin("repo", 'A'), Some('A'));
        assert_eq!(coalescer.begin("repo", 'B'), None);
        coalescer.abandon("repo");
        assert_eq!(coalescer.begin("repo", 'C'), Some('C'));
    }

    #[test]
    fn unwinding_worker_guard_frees_the_key() {
        let coalescer = RefreshCoalescer::new();
        assert_eq!(coalescer.begin("repo", 'A'), Some('A'));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = RefreshGuard {
                coalescer: &coalescer,
                key: "repo".to_owned(),
            };
            panic!("simulated refresh panic");
        }));
        assert!(result.is_err());
        assert_eq!(coalescer.begin("repo", 'B'), Some('B'));
    }

    #[test]
    fn finished_worker_guard_does_not_remove_a_new_workers_key() {
        let coalescer = RefreshCoalescer::new();
        assert_eq!(coalescer.begin("repo", 'A'), Some('A'));
        let guard = RefreshGuard {
            coalescer: &coalescer,
            key: "repo".to_owned(),
        };

        // The active worker's final next() releases its key before the
        // closure returns and drops its guard.
        assert_eq!(coalescer.next("repo"), None);
        assert_eq!(coalescer.begin("repo", 'B'), Some('B'));
        drop(guard);

        // A normal guard drop must not release worker B's admission slot.
        assert_eq!(coalescer.begin("repo", 'C'), None);
    }

    fn counting_pickup(site: RefreshSite, autopilot_on: bool) -> (usize /*ui*/, usize /*daemon*/) {
        use std::cell::Cell;
        let cfg = thegn_core::config::Config::default();
        let root = std::path::Path::new("/repo");
        let args = PickupArgs {
            cfg: &cfg,
            repo_root: root,
            cwd: root,
            account: "default",
            provider: "linear",
            issues: &[],
            from_assignee_me: true,
        };
        let ui = Cell::new(0);
        let daemon = Cell::new(0);
        route_pickup(
            site,
            &args,
            &|_| ui.set(ui.get() + 1),
            &|_| daemon.set(daemon.get() + 1),
            &|_| autopilot_on,
        );
        (ui.get(), daemon.get())
    }

    #[test]
    fn daemon_site_never_calls_the_ui_autopilot_driver() {
        assert_eq!(counting_pickup(RefreshSite::Daemon, true), (0, 1));
        assert_eq!(counting_pickup(RefreshSite::Ui, true), (1, 0));
    }

    #[test]
    fn daemon_site_starts_no_pickup_for_a_repo_with_autopilot_off() {
        assert_eq!(counting_pickup(RefreshSite::Daemon, false), (0, 0));
        // The UI driver checks the repo policy itself, as before.
        assert_eq!(counting_pickup(RefreshSite::Ui, false), (1, 0));
    }

    #[test]
    fn a_yielding_ui_pass_does_not_fetch_and_the_daemon_always_does() {
        assert!(!should_fetch(RefreshSite::Ui, true));
        assert!(should_fetch(RefreshSite::Ui, false));
        assert!(should_fetch(RefreshSite::Daemon, true));
        assert!(should_fetch(RefreshSite::Daemon, false));
        let yielded = RefreshSummary::YIELDED;
        assert!(yielded.yielded);
        assert_eq!(RefreshSummary::YIELDED.accounts_ok, 0);
        assert_eq!(RefreshSummary::YIELDED.events, 0);
    }

    #[test]
    fn the_yield_decision_precedes_every_provider_call() {
        // Source-order guard: in the refresh body the yield return comes
        // before the router is built, the fetch is issued, or the DB opens.
        let src = include_str!("hydrate_tracker.rs");
        let body_start = src.find(concat!("fn refresh_repo_", "inner(")).unwrap();
        let body = &src[body_start..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let yield_at = body.find(concat!("should_", "fetch(site")).unwrap();
        let ret_at = body
            .find(concat!("return RefreshSummary::", "YIELDED"))
            .unwrap();
        for call in [
            concat!("IssueRouter::", "from_config_at("),
            concat!("block_on_", "tracker("),
            concat!("list_per_", "provider("),
            concat!("Db::", "open()"),
            concat!("route_", "pickup("),
        ] {
            let at = body.find(call).unwrap_or_else(|| panic!("{call} missing"));
            assert!(yield_at < at && ret_at < at, "{call} runs before the yield");
        }
        let owns_at = body.find(concat!("daemon_", "owns(")).unwrap();
        let router_at = body
            .find(concat!("IssueRouter::", "from_config_at("))
            .unwrap();
        assert!(
            owns_at < router_at,
            "the lease check runs after the router is built"
        );
    }
}
