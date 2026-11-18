//! Sprites lifetime reaper (opt-in, `[env.<n>.provider] lifetime_reap = true`),
//! on the hydration cadence beside `vercel_reaper`: destroys sprites this host
//! can prove it owns once older than `max_lifetime_secs`, never a `running`
//! one. Inert unless an env opts in (an early-return config scan: no I/O and
//! no thread at idle).
//!
//! One pass, per account (deduplicated by the ownership scope's account
//! fingerprint):
//! - one paged listing; a `Partial` or `Unknown` listing means no action at
//!   all for that account (an absent sprite cannot be judged);
//! - only ledger records under this host's scope are considered (a sprite with
//!   no record of ours is never touched);
//! - a `ready` record older than the ceiling by **its own** `created_at` (adopt
//!   writes `now`, so lifetime counts from adoption) whose sprite is listed
//!   with a known status other than `running` is destroyed through
//!   `SpritesProvider::destroy` (which proves ownership again itself);
//! - a `running` one is skipped and warned about once per sprite per process;
//! - a `creating` record over an hour old whose sprite is absent, and a
//!   `ready` record over 60 s old whose sprite is absent, are removed (the
//!   same rules as `thegn env reap --provider sprites`).
//!
//! Never creates, resumes or execs; no subprocess (soak exec ceiling 0). The
//! `env reap` sprites lease is shared, so this never races an `--apply`.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Result;
use thegn_core::config::{Config, EnvConfig, PlacementMode};
use thegn_svc::provider::{InventoryReport, Provider, RemoteProvider, SandboxInfo};
use thegn_svc::sprites::ledger::{self, LEDGER_PROVIDER, OwnerScope};
use thegn_svc::vps::registry::{self, VpsRecord};

const KIND: &str = "sprites";
const TICK_INTERVAL: Duration = Duration::from_secs(300);
/// A `creating` record with no sprite older than this is stale (as `env reap`).
const STALE_CREATING_SECS: i64 = 3600;
/// A `ready` record younger than this is never judged orphaned (a create that
/// committed after the listing was taken).
const ORPHAN_GRACE_SECS: i64 = 60;

/// Threads started by [`tick`] (the test seam for "no thread at idle").
static PASSES_STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn is_sprites_provider_env(env: &EnvConfig) -> bool {
    env.placement == PlacementMode::Provider && env.provider.provider.trim() == KIND
}

fn opts_in(env: &EnvConfig) -> bool {
    is_sprites_provider_env(env)
        && env.provider.lifetime_reap
        && env.provider.max_lifetime_secs > 0
        && env.provider.max_lifetime_secs <= thegn_core::time_policy::MAX_DURATION_SECS
}

/// One throttled reaper pass. Returns at once when no env opts in.
pub fn tick(cfg: &Config) {
    if !reaper_enabled(cfg) {
        return;
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    {
        let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|t| t.elapsed() < TICK_INTERVAL) {
            IN_FLIGHT.store(false, Ordering::Release);
            return;
        }
        *last = Some(Instant::now());
    }
    let envs: Vec<(String, EnvConfig)> = cfg
        .env
        .iter()
        .filter(|(_, env)| is_sprites_provider_env(env))
        .map(|(name, env)| (name.clone(), env.clone()))
        .collect();
    let spawned = thegn_core::thread_name::try_spawn_named("thegn-sprite-reap", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        struct FlightGuard;
        impl Drop for FlightGuard {
            fn drop(&mut self) {
                IN_FLIGHT.store(false, Ordering::Release);
            }
        }
        let _guard = FlightGuard;
        run_pass(&envs);
    });
    match spawned {
        Ok(_) => {
            PASSES_STARTED.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            IN_FLIGHT.store(false, Ordering::Release);
            tracing::warn!(target: "thegn::provider", %error, "sprites reaper thread could not start");
        }
    }
}

/// Whether any env opts into the background lifetime reaper (pure config scan).
pub(crate) fn reaper_enabled(cfg: &Config) -> bool {
    cfg.env.values().any(opts_in)
}

/// The provider calls one pass makes. Production wraps a scoped
/// [`thegn_svc::provider::SpritesProvider`] on the pass's own runtime; tests
/// use a fake.
pub(crate) trait ReapApi {
    /// The ownership scope (`None` = no controller id or account).
    fn scope(&self) -> Option<OwnerScope>;
    /// The account's sprites, every page.
    fn listing(&self) -> Listing;
    /// `SpritesProvider::destroy`: its own ownership proof, then DELETE.
    fn destroy(&self, name: &str) -> Result<()>;
}

/// An account listing with its completeness explicit.
pub(crate) enum Listing {
    Complete(Vec<SandboxInfo>),
    Partial(String),
    Unknown(String),
}

struct LiveSprites<'rt> {
    provider: thegn_svc::provider::SpritesProvider,
    rt: &'rt tokio::runtime::Runtime,
}

impl ReapApi for LiveSprites<'_> {
    fn scope(&self) -> Option<OwnerScope> {
        self.provider.scope().cloned()
    }

    fn listing(&self) -> Listing {
        match self.rt.block_on(self.provider.sandbox_inventory()) {
            Ok(InventoryReport::Complete(rows)) => Listing::Complete(rows),
            Ok(InventoryReport::Partial { reason, .. }) => Listing::Partial(reason),
            Err(error) => Listing::Unknown(format!("{error:#}")),
        }
    }

    fn destroy(&self, name: &str) -> Result<()> {
        self.rt.block_on(self.provider.destroy(name))
    }
}

/// One configured sprites env, as the pass sees it.
pub(crate) struct ReapEnv<'a> {
    pub name: String,
    pub api: Option<Box<dyn ReapApi + 'a>>,
    pub lifetime_reap: bool,
    pub max_lifetime_secs: u64,
}

fn run_pass(envs: &[(String, EnvConfig)]) {
    let _lease = match crate::reaper_policy::try_reaper_lease(KIND) {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            tracing::debug!(target: "thegn::provider", "sprites reaper: another thegn process is reconciling sprites");
            return;
        }
        Err(reason) => {
            tracing::debug!(target: "thegn::provider", %reason, "sprites reaper: no lease; skipped");
            return;
        }
    };
    let rt = match crate::agent::provider_runtime() {
        Ok(rt) => rt,
        Err(error) => {
            tracing::warn!(target: "thegn::provider", error = %format!("{error:#}"), "sprites reaper: no runtime");
            return;
        }
    };
    let reap_envs: Vec<ReapEnv<'_>> = envs
        .iter()
        .map(|(name, env)| ReapEnv {
            name: name.clone(),
            api: match crate::provider_factory::provider_for_inventory(&env.provider) {
                Ok(Provider::Sprites(provider)) => {
                    Some(Box::new(LiveSprites { provider, rt: &rt }) as Box<dyn ReapApi + '_>)
                }
                _ => None,
            },
            lifetime_reap: env.provider.lifetime_reap,
            max_lifetime_secs: env.provider.max_lifetime_secs,
        })
        .collect();
    let summary = pass(&reap_envs, thegn_core::util::now_ms());
    for name in &summary.running {
        if first_running_warning(name) {
            thegn_core::msg::warn(&format!(
                "sprite {name} is past its lifetime but running; not reaped"
            ));
        }
    }
    if let Some(line) = summary.info_line() {
        thegn_core::msg::info(&line);
    }
}

/// Whether this process has not yet warned about `name` running past its
/// lifetime (and records that it now has).
fn first_running_warning(name: &str) -> bool {
    static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut warned = WARNED.lock().unwrap_or_else(PoisonError::into_inner);
    warned
        .get_or_insert_with(HashSet::new)
        .insert(name.to_string())
}

/// What one record warrants (pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Keep,
    /// Over its lifetime and idle: destroy through the provider.
    Destroy,
    /// Over its lifetime but `running`: never reaped, reported once.
    SkipRunning,
    /// A `creating` record over an hour old whose sprite is absent.
    ForgetStaleCreating,
    /// A `ready` record past the grace whose sprite is absent.
    ForgetOrphan,
}

/// Classify one in-scope ledger record against a **complete** listing (a
/// partial or unknown one is never classified: the pass skips the account).
/// `row` is the record's sprite in the listing, if listed. Mirrors
/// `cmd::env_reap::sprites::reap`'s record branch (a private module there;
/// copied rather than widened), except that age is the record's
/// `created_at`, not the API's, so an adopted sprite's lifetime starts at
/// adoption.
pub(crate) fn classify(
    record: &VpsRecord,
    row: Option<&SandboxInfo>,
    lifetime_secs: u64,
    now_ms: i64,
) -> Decision {
    let now_s = now_ms.div_euclid(1000);
    let age_secs = if record.created_at > 0 {
        now_s.checked_sub(record.created_at)
    } else {
        None
    };
    match row {
        Some(row) => {
            // A `creating` record with a listed sprite is a create in flight
            // (or one that crashed after the POST): never judged here.
            if record.state != "ready" {
                return Decision::Keep;
            }
            let Ok(limit) = i64::try_from(lifetime_secs) else {
                return Decision::Keep;
            };
            if limit <= 0 || !age_secs.is_some_and(|age| age > limit) {
                return Decision::Keep;
            }
            match row.state.as_deref().map(str::trim) {
                Some(status) if status.eq_ignore_ascii_case("running") => Decision::SkipRunning,
                Some(status) if !status.is_empty() => Decision::Destroy,
                // Unknown status: not proven idle.
                _ => Decision::Keep,
            }
        }
        None => match age_secs {
            Some(age) if record.state == "creating" && age > STALE_CREATING_SECS => {
                Decision::ForgetStaleCreating
            }
            Some(age) if record.state == "ready" && age > ORPHAN_GRACE_SECS => {
                Decision::ForgetOrphan
            }
            _ => Decision::Keep,
        },
    }
}

/// What one pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    pub destroyed: Vec<String>,
    pub forgotten: Vec<String>,
    pub running: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl Summary {
    /// The one summary line for a pass that acted (`None` = nothing done).
    fn info_line(&self) -> Option<String> {
        if self.destroyed.is_empty() && self.forgotten.is_empty() && self.failed.is_empty() {
            return None;
        }
        let failed = if self.failed.is_empty() {
            String::new()
        } else {
            format!("; {} action(s) failed", self.failed.len())
        };
        Some(format!(
            "sprites reaper: destroyed {} over-lifetime sprite(s), removed {} stale record(s){failed}",
            self.destroyed.len(),
            self.forgotten.len()
        ))
    }
}

/// The lifetime an account is reaped at: none unless every sprites env sharing
/// the account opts in with a ceiling (a record carries no env, so the most
/// permissive policy wins), else the largest ceiling.
fn account_lifetime(envs: &[(bool, u64)]) -> u64 {
    if envs.iter().any(|&(reap, lifetime)| {
        !reap || lifetime == 0 || lifetime > thegn_core::time_policy::MAX_DURATION_SECS
    }) {
        return 0;
    }
    envs.iter().map(|&(_, l)| l).max().unwrap_or(0)
}

/// The reap pass (testable core: no config, no stdout). The ledger is the
/// process registry dir (tests isolate it with `IsolatedState`).
pub(crate) fn pass(envs: &[ReapEnv<'_>], now_ms: i64) -> Summary {
    struct Account<'e> {
        envs: Vec<String>,
        policy: Vec<(bool, u64)>,
        api: &'e dyn ReapApi,
        scope: OwnerScope,
    }
    let mut accounts: BTreeMap<String, Account<'_>> = BTreeMap::new();
    for env in envs {
        let Some(api) = env.api.as_deref() else {
            tracing::debug!(target: "thegn::provider", env = %env.name, "sprites reaper: provider unavailable; skipped");
            continue;
        };
        let Some(scope) = api.scope() else {
            tracing::debug!(target: "thegn::provider", env = %env.name, "sprites reaper: no ownership scope; skipped");
            continue;
        };
        let account = accounts
            .entry(scope.account.clone())
            .or_insert_with(|| Account {
                envs: Vec::new(),
                policy: Vec::new(),
                api,
                scope,
            });
        account.envs.push(env.name.clone());
        account
            .policy
            .push((env.lifetime_reap, env.max_lifetime_secs));
    }

    let mut summary = Summary::default();
    for account in accounts.values() {
        let lifetime = account_lifetime(&account.policy);
        if !account.policy.iter().any(|&(reap, _)| reap) {
            continue; // no env of this account opts in
        }
        // List first, so a create racing this pass has already written its
        // intent when the ledger is read below.
        let rows = match account.api.listing() {
            Listing::Complete(rows) => rows,
            Listing::Partial(reason) | Listing::Unknown(reason) => {
                tracing::debug!(target: "thegn::provider", envs = ?account.envs, %reason, "sprites reaper: listing incomplete; no deletes");
                continue;
            }
        };
        let owned = match ledger::owned(&account.scope) {
            Ok(owned) => owned,
            Err(error) => {
                tracing::debug!(target: "thegn::provider", error = %format!("{error:#}"), "sprites reaper: ledger unreadable; no deletes");
                continue;
            }
        };
        let by_name: BTreeMap<&str, &SandboxInfo> =
            rows.iter().map(|row| (row.id.as_str(), row)).collect();
        for record in &owned.recorded {
            let row = by_name.get(record.name.as_str()).copied();
            match classify(record, row, lifetime, now_ms) {
                Decision::Keep => {}
                Decision::SkipRunning => summary.running.push(record.name.clone()),
                Decision::Destroy => match account.api.destroy(&record.name) {
                    Ok(()) => summary.destroyed.push(record.name.clone()),
                    Err(error) => summary
                        .failed
                        .push((record.name.clone(), format!("{error:#}"))),
                },
                decision @ (Decision::ForgetStaleCreating | Decision::ForgetOrphan) => {
                    match remove_record(&record.name, &account.scope, decision) {
                        Ok(true) => summary.forgotten.push(record.name.clone()),
                        Ok(false) => {}
                        Err(error) => summary.failed.push((record.name.clone(), error)),
                    }
                }
            }
        }
    }
    summary
}

/// Remove a record the pass judged stale, after re-reading it: it must still
/// be a sprites record of this scope in the same state (as `env reap`).
fn remove_record(
    name: &str,
    scope: &OwnerScope,
    decision: Decision,
) -> std::result::Result<bool, String> {
    let creating = decision == Decision::ForgetStaleCreating;
    match registry::read_checked(name) {
        Ok(Some(record))
            if record.provider == LEDGER_PROVIDER
                && record.owner == scope.owner
                && record.account == scope.account
                && (record.state == "creating") == creating =>
        {
            ledger::forget(name);
            Ok(true)
        }
        Ok(Some(_)) => Err("the record changed; left alone".into()),
        Ok(None) => Ok(false),
        Err(error) => Err(format!("ledger unreadable ({error:#}); left alone")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const NOW_MS: i64 = 1_800_000_000_000;
    const NOW_S: i64 = NOW_MS / 1000;

    fn rec(name: &str, state: &str, created_at: i64) -> VpsRecord {
        VpsRecord {
            name: name.into(),
            provider: LEDGER_PROVIDER.into(),
            state: state.into(),
            instance_id: String::new(),
            ip: String::new(),
            created_at,
            owner: "ctl-a".into(),
            account: "acct1-a".into(),
        }
    }

    fn row(name: &str, status: Option<&str>) -> SandboxInfo {
        SandboxInfo {
            id: name.into(),
            name: Some(name.into()),
            state: status.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn classify_table() {
        let old = NOW_S - 7200;
        let young = NOW_S - 30;
        let cases: Vec<(&str, VpsRecord, Option<SandboxInfo>, u64, Decision)> = vec![
            (
                "idle over lifetime",
                rec("a", "ready", old),
                Some(row("a", Some("warm"))),
                3600,
                Decision::Destroy,
            ),
            (
                "cold over lifetime",
                rec("a", "ready", old),
                Some(row("a", Some("cold"))),
                3600,
                Decision::Destroy,
            ),
            (
                "running skipped",
                rec("a", "ready", old),
                Some(row("a", Some(" Running "))),
                3600,
                Decision::SkipRunning,
            ),
            (
                "under lifetime kept",
                rec("a", "ready", NOW_S - 600),
                Some(row("a", Some("warm"))),
                3600,
                Decision::Keep,
            ),
            (
                "no ceiling kept",
                rec("a", "ready", old),
                Some(row("a", Some("warm"))),
                0,
                Decision::Keep,
            ),
            (
                "unknown status kept",
                rec("a", "ready", old),
                Some(row("a", None)),
                3600,
                Decision::Keep,
            ),
            (
                "unknown record age kept",
                rec("a", "ready", 0),
                Some(row("a", Some("warm"))),
                60,
                Decision::Keep,
            ),
            (
                "listed creating kept",
                rec("a", "creating", old),
                Some(row("a", Some("warm"))),
                60,
                Decision::Keep,
            ),
            (
                "stale creating forgotten",
                rec("a", "creating", old),
                None,
                3600,
                Decision::ForgetStaleCreating,
            ),
            (
                "fresh creating kept",
                rec("a", "creating", NOW_S - 600),
                None,
                3600,
                Decision::Keep,
            ),
            (
                "orphan ready forgotten",
                rec("a", "ready", NOW_S - 120),
                None,
                3600,
                Decision::ForgetOrphan,
            ),
            (
                "orphan inside grace kept",
                rec("a", "ready", young),
                None,
                3600,
                Decision::Keep,
            ),
        ];
        for (label, record, row, lifetime, want) in cases {
            assert_eq!(
                classify(&record, row.as_ref(), lifetime, NOW_MS),
                want,
                "{label}"
            );
        }
    }

    fn env_with(placement: PlacementMode, provider: &str, reap: bool, lifetime: u64) -> EnvConfig {
        let mut env = EnvConfig {
            placement,
            ..Default::default()
        };
        env.provider.provider = provider.into();
        env.provider.lifetime_reap = reap;
        env.provider.max_lifetime_secs = lifetime;
        env
    }

    fn cfg_with(env: EnvConfig) -> Config {
        let mut cfg = Config::default();
        cfg.env.insert("sx".into(), env);
        cfg
    }

    #[test]
    fn reaper_enabled_table() {
        use PlacementMode as P;
        let cases = [
            (P::Provider, "sprites", true, 60, true),
            (P::Provider, " sprites ", true, 3600, true),
            (P::Provider, "sprites", false, 60, false),
            (P::Provider, "sprites", true, 0, false),
            (P::Local, "sprites", true, 60, false),
            (P::Provider, "vercel", true, 60, false),
            (P::Provider, "fly", true, 60, false),
        ];
        for (placement, provider, reap, lifetime, want) in cases {
            let cfg = cfg_with(env_with(placement, provider, reap, lifetime));
            assert_eq!(
                reaper_enabled(&cfg),
                want,
                "{placement:?} {provider} reap={reap} lifetime={lifetime}"
            );
        }
        assert!(!reaper_enabled(&Config::default()));
    }

    #[test]
    fn tick_without_an_opted_in_sprites_env_starts_no_thread() {
        let before = PASSES_STARTED.load(Ordering::Relaxed);
        tick(&Config::default());
        tick(&cfg_with(env_with(
            PlacementMode::Provider,
            "sprites",
            false,
            60,
        )));
        tick(&cfg_with(env_with(
            PlacementMode::Provider,
            "sprites",
            true,
            0,
        )));
        assert_eq!(PASSES_STARTED.load(Ordering::Relaxed), before);
    }

    #[test]
    fn first_running_warning_is_once_per_sprite() {
        assert!(first_running_warning("sx-test-warn-once"));
        assert!(!first_running_warning("sx-test-warn-once"));
        assert!(first_running_warning("sx-test-warn-other"));
    }

    struct Fake {
        scope: Option<OwnerScope>,
        listing: Mutex<Option<Listing>>,
        destroys: Mutex<Vec<String>>,
        lists: std::sync::atomic::AtomicUsize,
    }

    impl ReapApi for Arc<Fake> {
        fn scope(&self) -> Option<OwnerScope> {
            self.scope.clone()
        }
        fn listing(&self) -> Listing {
            self.lists.fetch_add(1, Ordering::Relaxed);
            self.listing
                .lock()
                .unwrap()
                .take()
                .expect("listed once per account")
        }
        fn destroy(&self, name: &str) -> Result<()> {
            self.destroys.lock().unwrap().push(name.to_string());
            Ok(())
        }
    }

    fn scope() -> OwnerScope {
        OwnerScope {
            owner: "ctl-a".into(),
            account: "acct1-a".into(),
        }
    }

    fn fake(listing: Listing) -> Arc<Fake> {
        Arc::new(Fake {
            scope: Some(scope()),
            listing: Mutex::new(Some(listing)),
            destroys: Mutex::new(Vec::new()),
            lists: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn reap_env(name: &str, fake: &Arc<Fake>, reap: bool, lifetime: u64) -> ReapEnv<'static> {
        ReapEnv {
            name: name.into(),
            api: Some(Box::new(fake.clone()) as Box<dyn ReapApi>),
            lifetime_reap: reap,
            max_lifetime_secs: lifetime,
        }
    }

    fn write(name: &str, state: &str, created_at: i64, owner: &str) {
        let mut record = rec(name, state, created_at);
        record.owner = owner.into();
        registry::write(&record).unwrap();
    }

    /// An idle over-lifetime sprite, a running one, a young one, a foreign
    /// one (another controller's record), an unrecorded one, a gone record and
    /// a stale `creating` record.
    fn seed() -> Arc<Fake> {
        write("sx-old", "ready", NOW_S - 7200, "ctl-a");
        write("sx-busy", "ready", NOW_S - 7200, "ctl-a");
        write("sx-young", "ready", NOW_S - 60, "ctl-a");
        write("sx-foreign", "ready", NOW_S - 7200, "ctl-b");
        write("sx-gone", "ready", NOW_S - 600, "ctl-a");
        write("sx-stale", "creating", NOW_S - 7200, "ctl-a");
        fake(Listing::Complete(vec![
            row("sx-old", Some("warm")),
            row("sx-busy", Some("running")),
            row("sx-young", Some("warm")),
            row("sx-foreign", Some("cold")),
            row("owner-personal", Some("cold")),
        ]))
    }

    #[test]
    fn pass_destroys_exactly_the_over_lifetime_idle_sprite() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let fake = seed();
        let summary = pass(&[reap_env("sx", &fake, true, 3600)], NOW_MS);
        assert_eq!(*fake.destroys.lock().unwrap(), vec!["sx-old".to_string()]);
        assert_eq!(summary.destroyed, vec!["sx-old".to_string()]);
        assert_eq!(summary.running, vec!["sx-busy".to_string()]);
        let mut forgotten = summary.forgotten.clone();
        forgotten.sort();
        assert_eq!(
            forgotten,
            vec!["sx-gone".to_string(), "sx-stale".to_string()]
        );
        assert!(summary.failed.is_empty());
        assert!(registry::read_checked("sx-gone").unwrap().is_none());
        assert!(registry::read_checked("sx-stale").unwrap().is_none());
        assert!(registry::read_checked("sx-foreign").unwrap().is_some());
        assert!(registry::read_checked("sx-young").unwrap().is_some());
        assert!(summary.info_line().is_some());
    }

    #[test]
    fn pass_partial_or_unknown_listing_deletes_nothing() {
        for listing in [
            Listing::Partial("page cap".into()),
            Listing::Unknown("503".into()),
        ] {
            let _state = thegn_core::testenv::IsolatedState::new();
            write("sx-old", "ready", NOW_S - 7200, "ctl-a");
            write("sx-gone", "ready", NOW_S - 600, "ctl-a");
            let fake = fake(listing);
            let summary = pass(&[reap_env("sx", &fake, true, 60)], NOW_MS);
            assert!(fake.destroys.lock().unwrap().is_empty());
            assert_eq!(summary, Summary::default());
            assert!(summary.info_line().is_none());
            assert!(registry::read_checked("sx-gone").unwrap().is_some());
        }
    }

    #[test]
    fn pass_lists_once_per_account_and_respects_a_non_opted_in_sibling() {
        let _state = thegn_core::testenv::IsolatedState::new();
        write("sx-old", "ready", NOW_S - 7200, "ctl-a");
        let fake = fake(Listing::Complete(vec![row("sx-old", Some("warm"))]));
        // Two envs on one account; one does not opt in, so no lifetime applies.
        let summary = pass(
            &[
                reap_env("a", &fake, true, 60),
                reap_env("b", &fake, false, 0),
            ],
            NOW_MS,
        );
        assert_eq!(fake.lists.load(Ordering::Relaxed), 1);
        assert!(fake.destroys.lock().unwrap().is_empty());
        assert!(summary.destroyed.is_empty());
    }

    #[test]
    fn pass_with_no_scope_or_no_opt_in_does_no_io() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let unscoped = Arc::new(Fake {
            scope: None,
            listing: Mutex::new(None),
            destroys: Mutex::new(Vec::new()),
            lists: std::sync::atomic::AtomicUsize::new(0),
        });
        let off = fake(Listing::Complete(Vec::new()));
        let summary = pass(
            &[
                reap_env("a", &unscoped, true, 60),
                reap_env("b", &off, false, 60),
            ],
            NOW_MS,
        );
        assert_eq!(unscoped.lists.load(Ordering::Relaxed), 0);
        assert_eq!(off.lists.load(Ordering::Relaxed), 0);
        assert_eq!(summary, Summary::default());
    }

    #[test]
    fn account_lifetime_is_the_most_permissive() {
        assert_eq!(account_lifetime(&[(true, 60), (true, 3600)]), 3600);
        assert_eq!(account_lifetime(&[(true, 60), (false, 3600)]), 0);
        assert_eq!(account_lifetime(&[(true, 60), (true, 0)]), 0);
        assert_eq!(account_lifetime(&[]), 0);
    }
}
