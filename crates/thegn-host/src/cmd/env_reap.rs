//! `thegn env reap --provider vercel|sprites [--apply] [--yes] [--json]`.
//!
//! Vercel: find sandboxes this host's controller created (tag proof) that the
//! local ledger no longer records. Sprites: see [`sprites`] (ledger proof;
//! unrecorded sprites are reported, never deleted). Dry-run unless `--apply`;
//! never runs automatically.
//!
//! Vercel safety, in order:
//! - Only sandboxes the listing shows tagged for this controller are
//!   candidates (the listing itself filters on the tag).
//! - The ledger is read **after** the listing (a create writes its intent
//!   before the POST, so a sandbox being created is already recorded), and
//!   re-read right before each delete. Any record, of any provider, and any
//!   unreadable record name keeps a sandbox off the orphan list. An unreadable
//!   ledger refuses the whole command.
//! - A `Partial`/`Unknown` listing, or an env whose provider cannot be built,
//!   is reported and refuses `--apply`.
//! - `--apply` asks for a typed `yes` on a tty unless `--yes`; with no tty and
//!   no `--yes` it refuses.
//! - Every delete goes through `delete_with_snapshots`, which re-reads the
//!   sandbox and checks the controller tag itself before deleting.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Result, bail};
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_svc::provider::{InventoryReport, SandboxInfo};
use thegn_svc::vercel::usage::UsageRow;
use thegn_svc::vps::registry;

const KIND: &str = "vercel";

/// One account's listing, as the reap logic sees it.
enum Listing {
    Complete(Vec<SandboxInfo>),
    Partial {
        rows: Vec<SandboxInfo>,
        reason: String,
    },
    Unknown(String),
}

/// The provider calls reap makes: the account fingerprint (for grouping), the
/// controller-tagged listing, a best-effort cost read, and the proven delete.
/// Production wraps [`thegn_svc::vercel::VercelProvider`]; tests use a fake.
trait ReapApi {
    fn account(&self) -> Result<String>;
    fn listing(&self) -> Listing;
    /// Cost rows by name (best-effort; empty on failure).
    fn usage(&self) -> HashMap<String, UsageRow>;
    /// `delete_with_snapshots`: its own GET + tag proof, then delete.
    fn delete(&self, name: &str) -> Result<()>;
}

struct LiveReap(thegn_svc::vercel::VercelProvider);

impl ReapApi for LiveReap {
    fn account(&self) -> Result<String> {
        self.0.account()
    }

    fn listing(&self) -> Listing {
        match crate::agent::block_on_provider(|| async { self.0.sandbox_inventory().await }) {
            Ok(InventoryReport::Complete(rows)) => Listing::Complete(rows),
            Ok(InventoryReport::Partial { rows, reason }) => Listing::Partial { rows, reason },
            Err(error) => Listing::Unknown(format!("{error:#}")),
        }
    }

    fn usage(&self) -> HashMap<String, UsageRow> {
        crate::agent::block_on_provider(|| async { self.0.usage_report().await })
            .map(|(rows, _)| rows.into_iter().map(|r| (r.name.clone(), r)).collect())
            .unwrap_or_default()
    }

    fn delete(&self, name: &str) -> Result<()> {
        crate::agent::block_on_provider(|| async { self.0.delete_with_snapshots(name).await })
    }
}

/// One orphan candidate.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct Orphan {
    name: String,
    status: String,
    created_ms: Option<i64>,
    age: String,
    /// Current session accrual (CPU + memory), when it could be priced.
    session_usd: Option<f64>,
    snapshots_usd_month: Option<f64>,
}

/// One account (envs sharing an account fingerprint are listed once).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct AccountReport {
    envs: Vec<String>,
    /// `complete`, `partial`, `unknown` or `unavailable`.
    listing: String,
    reason: Option<String>,
    orphans: Vec<Orphan>,
}

/// The outcome of one delete under `--apply`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct DeleteResult {
    name: String,
    deleted: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct ReapReport {
    provider: &'static str,
    apply: bool,
    /// Every account listed completely (the precondition for `--apply`).
    complete: bool,
    accounts: Vec<AccountReport>,
    /// Empty on a dry run (or a refused/declined apply).
    results: Vec<DeleteResult>,
    /// Why `--apply` did not delete anything, when it did not.
    refused: Option<String>,
}

impl ReapReport {
    fn orphan_count(&self) -> usize {
        self.accounts.iter().map(|a| a.orphans.len()).sum()
    }

    fn failed(&self) -> usize {
        self.results.iter().filter(|r| !r.deleted).count()
    }
}

/// How `--apply` gets its confirmation: `None` = no tty; else the typed line.
type Ask<'a> = &'a mut dyn FnMut(&str) -> Option<String>;

#[derive(Debug, Clone, Copy)]
struct Opts {
    apply: bool,
    yes: bool,
}

pub fn run(
    cfg: &Config,
    provider: &str,
    apply: bool,
    yes: bool,
    json: bool,
    unrecorded: bool,
) -> Result<()> {
    let kind = provider.trim();
    if kind.eq_ignore_ascii_case(sprites::KIND) {
        return sprites::run(cfg, apply, yes, json, unrecorded);
    }
    if !kind.eq_ignore_ascii_case(KIND) {
        bail!("env reap supports --provider vercel, sprites (got {kind:?})");
    }
    if unrecorded {
        bail!(
            "--unrecorded is sprites only (a Vercel listing shows only this controller's sandboxes)"
        );
    }
    let mut envs: Vec<(String, Option<Box<dyn ReapApi>>)> = Vec::new();
    for (name, envc) in &cfg.env {
        if envc.provider.provider.trim() != KIND {
            continue;
        }
        let api = crate::provider_factory::vercel_provider_for(&envc.provider, "")
            .map(|p| Box::new(LiveReap(p)) as Box<dyn ReapApi>);
        envs.push((name.clone(), api));
    }
    if envs.is_empty() {
        bail!("no [env.<name>] with provider = \"vercel\" is configured (see `thegn env list`)");
    }
    // Never race the background reaper over the same sandboxes.
    let _lease = if apply {
        match crate::reaper_policy::try_reaper_lease(KIND) {
            Ok(Some(lease)) => Some(lease),
            Ok(None) => bail!("another thegn process is reconciling vercel; retry shortly"),
            Err(reason) => bail!("env reap: {reason}"),
        }
    } else {
        None
    };
    let mut ask = |prompt: &str| tty_prompt(prompt);
    let report = reap(
        &envs,
        Opts { apply, yes },
        &mut ask,
        thegn_core::util::now_ms(),
    )?;
    if json {
        super::emit_json(&report)?;
    } else {
        outln!("{}", render(&report));
    }
    if let Some(refused) = &report.refused
        && apply
    {
        bail!("{refused}");
    }
    let failed = report.failed();
    if failed > 0 {
        bail!(
            "{failed} of {} sandbox deletes failed",
            report.results.len()
        );
    }
    Ok(())
}

/// The typed confirmation; `None` when stdin is not a terminal (piped or absent).
#[allow(clippy::disallowed_macros)] // a raw interactive prompt, not a log line
fn tty_prompt(prompt: &str) -> Option<String> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return None;
    }
    eprint!("{prompt}: ");
    std::io::stderr().flush().ok()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line)
}

/// The reap pass (testable core: no stdout, no tty, no config).
fn reap(
    envs: &[(String, Option<Box<dyn ReapApi>>)],
    opts: Opts,
    ask: Ask<'_>,
    now_ms: i64,
) -> Result<ReapReport> {
    // 1. Group envs by account fingerprint (an unbuildable env, or one whose
    //    fingerprint fails, is its own group).
    let mut groups: BTreeMap<String, (Vec<String>, Option<&dyn ReapApi>)> = BTreeMap::new();
    for (name, api) in envs {
        let key = match api.as_deref().map(|api| api.account()) {
            Some(Ok(account)) => format!("account:{account}"),
            _ => format!("env:{name}"),
        };
        let entry = groups
            .entry(key)
            .or_insert_with(|| (Vec::new(), api.as_deref()));
        entry.0.push(name.clone());
    }

    // 2. List every account first (tag-filtered).
    let mut listed: Vec<(Vec<String>, Option<&dyn ReapApi>, Listing)> = Vec::new();
    for (_, (names, api)) in groups {
        let listing = match api {
            Some(api) => api.listing(),
            None => Listing::Unknown(
                "the vercel provider could not be built (token or scope unavailable)".into(),
            ),
        };
        listed.push((names, api, listing));
    }

    // 3. Then the ledger: any record or unreadable record name is not an orphan.
    let ledger = registry::list_checked().map_err(|error| {
        anyhow::anyhow!("env reap: the ledger is unreadable ({error:#}); refusing to judge orphans")
    })?;
    let recorded: BTreeSet<String> = ledger
        .records
        .iter()
        .map(|r| r.name.clone())
        .chain(ledger.unreadable.iter().cloned())
        .collect();

    let mut complete = true;
    let mut accounts = Vec::new();
    // (account index, name) of every orphan, in report order.
    let mut targets: Vec<(usize, String)> = Vec::new();
    for (names, api, listing) in listed {
        let (state, reason, rows) = match listing {
            Listing::Complete(rows) => ("complete", None, rows),
            Listing::Partial { rows, reason } => {
                complete = false;
                ("partial", Some(reason), rows)
            }
            Listing::Unknown(reason) => {
                complete = false;
                let state = if api.is_some() {
                    "unknown"
                } else {
                    "unavailable"
                };
                (state, Some(reason), Vec::new())
            }
        };
        let mut rows: Vec<SandboxInfo> = rows
            .into_iter()
            .filter(|row| !recorded.contains(&row.id))
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows.dedup_by(|a, b| a.id == b.id);
        let usage = match api {
            Some(api) if !rows.is_empty() => api.usage(),
            _ => HashMap::new(),
        };
        let index = accounts.len();
        let orphans = rows
            .into_iter()
            .map(|row| {
                let cost = usage.get(&row.id);
                targets.push((index, row.id.clone()));
                Orphan {
                    status: row.state.clone().unwrap_or_else(|| "?".into()),
                    created_ms: row.created_at_ms,
                    age: super::fleet::age(row.created_at_ms, now_ms),
                    session_usd: cost
                        .and_then(|c| c.session)
                        .map(|a| a.cpu_usd + a.memory_usd),
                    snapshots_usd_month: cost.map(|c| c.snapshots_usd_month),
                    name: row.id,
                }
            })
            .collect();
        accounts.push((
            AccountReport {
                envs: names,
                listing: state.into(),
                reason,
                orphans,
            },
            api,
        ));
    }

    let mut report = ReapReport {
        provider: KIND,
        apply: opts.apply,
        complete,
        accounts: Vec::new(),
        results: Vec::new(),
        refused: None,
    };
    let apis: Vec<Option<&dyn ReapApi>> = accounts.iter().map(|(_, api)| *api).collect();
    report.accounts = accounts.into_iter().map(|(a, _)| a).collect();

    if !opts.apply {
        return Ok(report);
    }
    if !complete {
        report.refused = Some(
            "env reap: a vercel listing is incomplete; refusing --apply (an absent sandbox cannot be judged)"
                .into(),
        );
        return Ok(report);
    }
    if targets.is_empty() {
        return Ok(report);
    }
    if !opts.yes {
        let prompt = format!("type yes to delete {} sandboxes", targets.len());
        match ask(&prompt) {
            None => {
                report.refused =
                    Some("env reap --apply: refusing without a TTY; pass --yes to confirm".into());
                return Ok(report);
            }
            Some(answer) if answer.trim() == "yes" => {}
            Some(_) => {
                report.refused = Some("env reap --apply: aborted (not confirmed)".into());
                return Ok(report);
            }
        }
    }
    for (index, name) in targets {
        let result = match registry::read_checked(&name) {
            Ok(None) => match apis[index] {
                Some(api) => api.delete(&name).map_err(|e| format!("{e:#}")),
                None => Err("provider unavailable".into()),
            },
            Ok(Some(_)) => Err("a ledger record appeared; left alone".into()),
            Err(error) => Err(format!("ledger unreadable ({error:#}); left alone")),
        };
        report.results.push(match result {
            Ok(()) => DeleteResult {
                name,
                deleted: true,
                error: None,
            },
            Err(error) => DeleteResult {
                name,
                deleted: false,
                error: Some(error),
            },
        });
    }
    Ok(report)
}

fn usd(value: Option<f64>, decimals: usize) -> String {
    value.map_or_else(|| "?".into(), |v| format!("${v:.decimals$}"))
}

/// The text report (pure).
fn render(report: &ReapReport) -> String {
    let mut out = Vec::new();
    for account in &report.accounts {
        let envs = account.envs.join(", ");
        out.push(match &account.reason {
            Some(reason) => format!("vercel ({envs}): listing {}: {reason}", account.listing),
            None => format!("vercel ({envs})"),
        });
        if account.orphans.is_empty() {
            out.push("  no orphaned sandboxes".into());
            continue;
        }
        let mut rows = vec![
            [
                "NAME",
                "STATUS",
                "AGE",
                "SESSION $ (ESTIMATE)",
                "SNAPSHOTS $/MONTH",
                "TAG",
            ]
            .map(String::from)
            .to_vec(),
        ];
        for o in &account.orphans {
            rows.push(vec![
                o.name.clone(),
                o.status.clone(),
                o.age.clone(),
                usd(o.session_usd, 4),
                usd(o.snapshots_usd_month, 2),
                "this controller".into(),
            ]);
        }
        for line in super::fleet::table(&rows).lines() {
            out.push(format!("  {line}"));
        }
    }
    let orphans = report.orphan_count();
    if !report.complete {
        out.push("listing incomplete: --apply is refused".into());
    }
    if !report.apply {
        out.push(format!("{orphans} orphaned sandbox(es)"));
        out.push("dry run: nothing deleted; re-run with --apply".into());
    } else if let Some(refused) = &report.refused {
        out.push(format!("{refused}; nothing deleted"));
    } else {
        for r in &report.results {
            match &r.error {
                None => out.push(format!("deleted {}", r.name)),
                Some(error) => out.push(format!("failed {}: {error}", r.name)),
            }
        }
        out.push(format!(
            "deleted {}/{} sandbox(es)",
            report.results.len() - report.failed(),
            report.results.len()
        ));
    }
    out.join("\n")
}

/// `env reap --provider sprites`: reconcile the sprites ownership ledger
/// (`vps::registry`, `provider = "sprites"`) against each account's listing.
///
/// Classes (one row per sprite or record):
/// - `orphan-record`: a ready record under this host's scope whose sprite the
///   (complete) listing no longer shows → `--apply` removes the record only.
/// - `stale-creating`: a `creating` record older than an hour with no sprite →
///   `--apply` removes the record.
/// - `over-lifetime`: a sprite with a ledger record under this host's scope
///   older than the env's `max_lifetime_secs` (age from the later of the API
///   `created_at` and the record's, so lifetime counts from adoption) → `--apply` destroys it through
///   `SpritesProvider::destroy`, which proves ownership again itself.
/// - `over-lifetime-bound`: an over-lifetime sprite whose name is bound to a
///   registered worktree → kept, never deleted by `--apply`.
/// - `unrecorded`: in the account with no record of this host (including one
///   a worktree location row names, and every pre-ledger sprite) → reported,
///   **never** deleted.
///
/// A `Partial`/`Unknown` listing, an env with no ownership scope, or one whose
/// provider cannot be built refuses `--apply`; an unreadable ledger refuses
/// the command. Records of other providers in the shared registry are never
/// read as sprites.
mod sprites {
    use std::collections::{BTreeMap, BTreeSet};

    use anyhow::{Result, bail};
    use thegn_core::config::Config;
    use thegn_core::outln;
    use thegn_svc::provider::{InventoryReport, Provider, RemoteProvider, SandboxInfo};
    use thegn_svc::sprites::ledger::{self, LEDGER_PROVIDER, OwnerScope};
    use thegn_svc::vps::registry::{self, VpsRecord};

    use super::{Ask, Listing, Opts};

    pub(super) const KIND: &str = "sprites";
    /// A `creating` record with no sprite older than this is stale.
    const STALE_CREATING_SECS: i64 = 3600;

    /// The provider calls the sprites reap makes. Production wraps a scoped
    /// [`thegn_svc::provider::SpritesProvider`]; tests use a fake.
    pub(super) trait SpritesReapApi {
        /// The ownership scope (`None` = no controller id or account).
        fn scope(&self) -> Option<OwnerScope>;
        /// The account's sprites, every page.
        fn listing(&self) -> Listing;
        /// Usage for a sprite during the last 30 days.
        fn usage_30d(&self, name: &str) -> Result<Option<thegn_svc::sprites::usage::SpriteUsage>>;
        /// `SpritesProvider::destroy`: its own ownership proof, then DELETE.
        fn destroy(&self, name: &str) -> Result<()>;
    }

    struct LiveSprites(thegn_svc::provider::SpritesProvider);

    impl SpritesReapApi for LiveSprites {
        fn scope(&self) -> Option<OwnerScope> {
            self.0.scope().cloned()
        }

        fn listing(&self) -> Listing {
            match crate::agent::block_on_provider(|| async { self.0.sandbox_inventory().await }) {
                Ok(InventoryReport::Complete(rows)) => Listing::Complete(rows),
                Ok(InventoryReport::Partial { rows, reason }) => Listing::Partial { rows, reason },
                Err(error) => Listing::Unknown(format!("{error:#}")),
            }
        }

        fn usage_30d(&self, name: &str) -> Result<Option<thegn_svc::sprites::usage::SpriteUsage>> {
            let start = unix_to_rfc3339(
                thegn_core::util::now().saturating_sub(30_i64.saturating_mul(24 * 60 * 60)),
            );
            crate::agent::block_on_provider(|| async { self.0.usage_since(name, &start).await })
        }

        fn destroy(&self, name: &str) -> Result<()> {
            crate::agent::block_on_provider(|| async { self.0.destroy(name).await })
        }
    }

    fn unix_to_rfc3339(secs: i64) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
            .unwrap_or_default()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// One configured sprites env.
    pub(super) struct SpritesEnv {
        pub name: String,
        pub api: Option<Box<dyn SpritesReapApi>>,
        pub max_lifetime_secs: u64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub(super) enum Class {
        OrphanRecord,
        StaleCreating,
        OverLifetime,
        OverLifetimeBound,
        Unrecorded,
    }

    impl Class {
        fn label(self) -> &'static str {
            match self {
                Class::OrphanRecord => "orphan-record",
                Class::StaleCreating => "stale-creating",
                Class::OverLifetime => "over-lifetime",
                Class::OverLifetimeBound => "over-lifetime-bound",
                Class::Unrecorded => "unrecorded",
            }
        }

        /// What `--apply` does to this class (`None` = nothing, ever).
        fn action(self) -> Option<&'static str> {
            match self {
                Class::OrphanRecord | Class::StaleCreating => Some("remove record"),
                Class::OverLifetime => Some("destroy sprite"),
                Class::OverLifetimeBound | Class::Unrecorded => None,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, serde::Serialize)]
    pub(super) struct Entry {
        pub name: String,
        pub class: Class,
        /// The sprite's API status, else the ledger record's state.
        pub status: String,
        pub created_ms: Option<i64>,
        pub age: String,
        /// `ledger` or `none`.
        pub proof: &'static str,
        pub note: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub cpu_hours_30d: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub gb_hours_30d: Option<f64>,
        #[serde(skip)]
        usage_error: bool,
    }

    #[derive(Debug, Clone, PartialEq, serde::Serialize)]
    pub(super) struct AccountReport {
        pub envs: Vec<String>,
        /// `complete`, `partial`, `unknown` or `unavailable`.
        pub listing: String,
        pub reason: Option<String>,
        /// The ceiling applied (`0` = none).
        pub max_lifetime_secs: u64,
        /// Ledger records under this host's scope.
        pub recorded: usize,
        pub entries: Vec<Entry>,
    }

    #[derive(Debug, Clone, PartialEq, serde::Serialize)]
    pub(super) struct ActionResult {
        pub name: String,
        pub class: Class,
        pub ok: bool,
        pub error: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, serde::Serialize)]
    pub(super) struct SpritesReport {
        pub provider: &'static str,
        pub apply: bool,
        pub complete: bool,
        pub accounts: Vec<AccountReport>,
        pub results: Vec<ActionResult>,
        pub refused: Option<String>,
        /// Non-fatal caveats (e.g. unreadable ledger records left alone).
        pub notes: Vec<String>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        pub unrecorded_only: bool,
    }

    impl SpritesReport {
        pub(super) fn count(&self, class: Class) -> usize {
            self.accounts
                .iter()
                .flat_map(|a| &a.entries)
                .filter(|e| e.class == class)
                .count()
        }

        pub(super) fn failed(&self) -> usize {
            self.results.iter().filter(|r| !r.ok).count()
        }
    }

    pub(super) fn run(
        cfg: &Config,
        apply: bool,
        yes: bool,
        json: bool,
        unrecorded: bool,
    ) -> Result<()> {
        if unrecorded && apply {
            bail!("env reap --unrecorded cannot be combined with --apply");
        }
        let mut envs = Vec::new();
        for (name, envc) in &cfg.env {
            if envc.provider.provider.trim() != KIND {
                continue;
            }
            let api = match crate::provider_factory::provider_for_inventory(&envc.provider) {
                Ok(Provider::Sprites(provider)) => {
                    Some(Box::new(LiveSprites(provider)) as Box<dyn SpritesReapApi>)
                }
                _ => None,
            };
            envs.push(SpritesEnv {
                name: name.clone(),
                api,
                max_lifetime_secs: envc.provider.max_lifetime_secs,
            });
        }
        if envs.is_empty() {
            bail!(
                "no [env.<name>] with provider = \"sprites\" is configured (see `thegn env list`)"
            );
        }
        let bound = match crate::sprites_reaper::bound_sandboxes(cfg) {
            Ok(bound) => bound,
            Err(error) if apply && !unrecorded => bail!(
                "env reap: cannot read the worktree registry ({error:#}); refusing --apply so no sprite holding unpulled work is destroyed"
            ),
            Err(_) => crate::sprites_reaper::BoundSet::unresolved("registry unreadable"),
        };
        let _lease = if apply && !unrecorded {
            match crate::reaper_policy::try_reaper_lease(KIND) {
                Ok(Some(lease)) => Some(lease),
                Ok(None) => bail!("another thegn process is reconciling sprites; retry shortly"),
                Err(reason) => bail!("env reap: {reason}"),
            }
        } else {
            None
        };
        let mut ask = |prompt: &str| super::tty_prompt(prompt);
        let mut report = reap(
            &envs,
            if unrecorded {
                Opts {
                    apply: false,
                    yes: false,
                }
            } else {
                Opts { apply, yes }
            },
            &mut ask,
            &bound,
            thegn_core::util::now_ms(),
        )?;
        if unrecorded {
            prepare_unrecorded(&mut report, &envs);
        }
        if json {
            crate::cmd::emit_json(&report)?;
        } else {
            outln!("{}", render(&report));
        }
        if let Some(refused) = &report.refused
            && apply
        {
            bail!("{refused}");
        }
        let failed = report.failed();
        if failed > 0 {
            bail!(
                "{failed} of {} sprites reap actions failed",
                report.results.len()
            );
        }
        Ok(())
    }

    /// The group's ceiling: none when any env sharing the account has none
    /// (a record carries no env, so the most permissive policy wins); an
    /// unsupported value is no ceiling.
    fn group_lifetime(lifetimes: &[u64]) -> u64 {
        if lifetimes
            .iter()
            .any(|&l| l == 0 || l > thegn_core::time_policy::MAX_DURATION_SECS)
        {
            return 0;
        }
        lifetimes.iter().copied().max().unwrap_or(0)
    }

    fn over(created_ms: Option<i64>, lifetime_secs: u64, now_ms: i64) -> bool {
        let Some(created) = created_ms else {
            return false;
        };
        let lifetime_ms = i64::try_from(lifetime_secs)
            .ok()
            .and_then(|s| s.checked_mul(1000));
        lifetime_secs > 0 && lifetime_ms.is_some_and(|limit| now_ms.saturating_sub(created) > limit)
    }

    fn record_ms(record: &VpsRecord) -> Option<i64> {
        if record.created_at > 0 {
            record.created_at.checked_mul(1000)
        } else {
            None
        }
    }

    /// When a recorded sprite's lifetime started: the LATER of the API
    /// `created_at` and the record's (written at create or at adoption), so
    /// an old sprite adopted recently is aged from its adoption (D1). Either
    /// alone when the other is unknown.
    fn lifetime_start_ms(row: &SandboxInfo, record: &VpsRecord) -> Option<i64> {
        match (row.created_at_ms, record_ms(record)) {
            (Some(api), Some(rec)) => Some(api.max(rec)),
            (api, rec) => api.or(rec),
        }
    }

    /// The reap pass (testable core: no stdout, no tty, no config). The ledger
    /// is the process registry dir (tests isolate it with `IsolatedState`).
    pub(super) fn reap(
        envs: &[SpritesEnv],
        opts: Opts,
        ask: Ask<'_>,
        bound: &crate::sprites_reaper::BoundSet,
        now_ms: i64,
    ) -> Result<SpritesReport> {
        // 1. Group envs by account (an env with no scope is its own group).
        struct Group<'a> {
            envs: Vec<String>,
            lifetimes: Vec<u64>,
            api: Option<&'a dyn SpritesReapApi>,
            scope: Option<OwnerScope>,
        }
        let mut groups: BTreeMap<String, Group<'_>> = BTreeMap::new();
        for env in envs {
            let scope = env.api.as_deref().and_then(|api| api.scope());
            let key = match &scope {
                Some(scope) => format!("account:{}", scope.account),
                None => format!("env:{}", env.name),
            };
            let group = groups.entry(key).or_insert_with(|| Group {
                envs: Vec::new(),
                lifetimes: Vec::new(),
                api: env.api.as_deref(),
                scope: scope.clone(),
            });
            group.envs.push(env.name.clone());
            group.lifetimes.push(env.max_lifetime_secs);
        }

        // 2. List every account first, so a create racing this pass has
        //    already written its intent when the ledger is read below.
        let mut listed = Vec::new();
        for (_, group) in groups {
            let listing = match (group.api, &group.scope) {
                (Some(api), Some(_)) => api.listing(),
                (Some(_), None) => Listing::Unknown(
                    "no ownership scope (controller id or account fingerprint unavailable)".into(),
                ),
                (None, _) => Listing::Unknown(
                    "the sprites provider could not be built (token unavailable)".into(),
                ),
            };
            listed.push((group, listing));
        }

        // 3. The ledger (all providers: a name another provider records is
        //    never treated as an unclaimed sprite record).
        let view = registry::list_checked().map_err(|error| {
            anyhow::anyhow!(
                "env reap: the ledger is unreadable ({error:#}); refusing to judge sprites"
            )
        })?;
        let unreadable: BTreeSet<String> = view.unreadable.iter().cloned().collect();
        let mut notes = Vec::new();
        if !unreadable.is_empty() {
            notes.push(format!(
                "{} unreadable ledger record(s) left alone: {}",
                unreadable.len(),
                unreadable.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }

        let mut complete = true;
        let mut accounts = Vec::new();
        let mut targets: Vec<(usize, String, Class)> = Vec::new();
        for (group, listing) in listed {
            let unavailable = group.api.is_none();
            let (state, reason, rows, whole) = match listing {
                Listing::Complete(rows) => ("complete", None, rows, true),
                Listing::Partial { rows, reason } => ("partial", Some(reason), rows, false),
                Listing::Unknown(reason) => {
                    let state = if unavailable {
                        "unavailable"
                    } else {
                        "unknown"
                    };
                    (state, Some(reason), Vec::new(), false)
                }
            };
            complete &= whole;
            let lifetime = group_lifetime(&group.lifetimes);
            let mine: Vec<&VpsRecord> = match &group.scope {
                Some(scope) => view
                    .records
                    .iter()
                    .filter(|r| {
                        r.provider == LEDGER_PROVIDER
                            && r.owner == scope.owner
                            && r.account == scope.account
                    })
                    .collect(),
                None => Vec::new(),
            };
            let by_name: BTreeMap<&str, &SandboxInfo> =
                rows.iter().map(|row| (row.id.as_str(), row)).collect();
            let recorded_names: BTreeSet<&str> = mine.iter().map(|r| r.name.as_str()).collect();
            let any_record: BTreeSet<&str> = view.records.iter().map(|r| r.name.as_str()).collect();
            let index = accounts.len();
            let mut entries = Vec::new();

            // Our records.
            for record in &mine {
                match by_name.get(record.name.as_str()) {
                    Some(row) => {
                        let created = lifetime_start_ms(row, record);
                        if over(created, lifetime, now_ms) {
                            let (class, note) = match bound.holder(&record.name) {
                                Some(holder) => (
                                    Class::OverLifetimeBound,
                                    Some(format!(
                                        "bound to worktree {holder}: kept; `thegn env down --worktree <wt>` pulls its commits first"
                                    )),
                                ),
                                None => (Class::OverLifetime, None),
                            };
                            entries.push(Entry {
                                name: record.name.clone(),
                                class,
                                status: row.state.clone().unwrap_or_else(|| "?".into()),
                                created_ms: created,
                                age: crate::cmd::fleet::age(created, now_ms),
                                proof: "ledger",
                                note,
                                cpu_hours_30d: None,
                                gb_hours_30d: None,
                                usage_error: false,
                            });
                        }
                    }
                    // Absence is evidence only from a complete listing.
                    None if whole => {
                        // A record written after the listing (a create that
                        // committed meanwhile) is not evidence of absence.
                        if record.created_at.saturating_mul(1000) >= now_ms.saturating_sub(60_000) {
                            continue;
                        }
                        let created = record_ms(record);
                        let class = if record.state == "creating" {
                            let age_secs = (now_ms / 1000).saturating_sub(record.created_at);
                            if age_secs <= STALE_CREATING_SECS {
                                continue; // a create in flight
                            }
                            Class::StaleCreating
                        } else {
                            Class::OrphanRecord
                        };
                        entries.push(Entry {
                            name: record.name.clone(),
                            class,
                            status: record.state.clone(),
                            created_ms: created,
                            age: crate::cmd::fleet::age(created, now_ms),
                            proof: "ledger",
                            note: Some("the sprite is gone".into()),
                            cpu_hours_30d: None,
                            gb_hours_30d: None,
                            usage_error: false,
                        });
                    }
                    None => {}
                }
            }

            // Sprites in the account with no record of ours.
            let mut seen = BTreeSet::new();
            for row in &rows {
                if recorded_names.contains(row.id.as_str()) || !seen.insert(row.id.clone()) {
                    continue;
                }
                let status = row.state.clone().unwrap_or_else(|| "?".into());
                let age = crate::cmd::fleet::age(row.created_at_ms, now_ms);
                let note = if unreadable.contains(&row.id) {
                    "its ledger record is unreadable"
                } else if any_record.contains(row.id.as_str()) {
                    "recorded by another controller, account or provider"
                } else {
                    "not recorded by this host"
                };
                entries.push(Entry {
                    name: row.id.clone(),
                    class: Class::Unrecorded,
                    status,
                    created_ms: row.created_at_ms,
                    age,
                    proof: "none",
                    note: Some(note.into()),
                    cpu_hours_30d: None,
                    gb_hours_30d: None,
                    usage_error: false,
                });
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            for entry in &entries {
                if entry.class.action().is_some() {
                    targets.push((index, entry.name.clone(), entry.class));
                }
            }
            accounts.push((
                AccountReport {
                    envs: group.envs,
                    listing: state.into(),
                    reason,
                    max_lifetime_secs: lifetime,
                    recorded: mine.len(),
                    entries,
                },
                group.api,
                group.scope,
            ));
        }

        let handles: Vec<(Option<&dyn SpritesReapApi>, Option<OwnerScope>)> = accounts
            .iter()
            .map(|(_, api, scope)| (*api, scope.clone()))
            .collect();
        let mut report = SpritesReport {
            provider: KIND,
            apply: opts.apply,
            complete,
            accounts: accounts.into_iter().map(|(a, _, _)| a).collect(),
            results: Vec::new(),
            refused: None,
            notes,
            unrecorded_only: false,
        };
        if !opts.apply {
            return Ok(report);
        }
        if !complete {
            report.refused = Some(
                "env reap: a sprites listing is incomplete; refusing --apply (an absent sprite cannot be judged)"
                    .into(),
            );
            return Ok(report);
        }
        if targets.is_empty() {
            return Ok(report);
        }
        if !opts.yes {
            let destroys = targets
                .iter()
                .filter(|(_, _, c)| *c == Class::OverLifetime)
                .count();
            let prompt = format!(
                "type yes to destroy {destroys} sprite(s) and remove {} stale record(s)",
                targets.len() - destroys
            );
            match ask(&prompt) {
                None => {
                    report.refused = Some(
                        "env reap --apply: refusing without a TTY; pass --yes to confirm".into(),
                    );
                    return Ok(report);
                }
                Some(answer) if answer.trim() == "yes" => {}
                Some(_) => {
                    report.refused = Some("env reap --apply: aborted (not confirmed)".into());
                    return Ok(report);
                }
            }
        }
        for (index, name, class) in targets {
            let (api, scope) = &handles[index];
            let result = match class {
                Class::OverLifetime => match api {
                    Some(api) => api.destroy(&name).map_err(|e| format!("{e:#}")),
                    None => Err("provider unavailable".into()),
                },
                Class::OrphanRecord | Class::StaleCreating => {
                    remove_record(&name, scope.as_ref(), class)
                }
                Class::OverLifetimeBound | Class::Unrecorded => Err("never deleted".into()),
            };
            report.results.push(ActionResult {
                name,
                class,
                ok: result.is_ok(),
                error: result.err(),
            });
        }
        Ok(report)
    }

    /// Turn the ordinary, read-only reap report into the unrecorded-only view.
    /// Usage is deliberately best effort: one failed usage request marks both
    /// cells with `?` and leaves the command successful.
    fn prepare_unrecorded(report: &mut SpritesReport, envs: &[SpritesEnv]) {
        report.unrecorded_only = true;
        for account in &mut report.accounts {
            account
                .entries
                .retain(|entry| entry.class == Class::Unrecorded);
            account.entries.sort_by(|left, right| {
                match (left.created_ms, right.created_ms) {
                    (Some(left), Some(right)) => left.cmp(&right),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
                .then_with(|| left.name.cmp(&right.name))
            });
        }

        let mut usage_notes = Vec::new();
        for account in &mut report.accounts {
            let api = account.envs.iter().find_map(|name| {
                envs.iter()
                    .find(|env| env.name == *name)
                    .and_then(|env| env.api.as_deref())
            });
            let Some(api) = api else {
                continue;
            };
            for entry in account.entries.iter_mut().take(50) {
                match api.usage_30d(&entry.name) {
                    Ok(Some(usage)) => {
                        entry.cpu_hours_30d = Some(usage.cpu_seconds / 3600.0);
                        entry.gb_hours_30d = Some(usage.reserved_gb_hours);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        entry.usage_error = true;
                        let error = format!("{error:#}");
                        usage_notes.push(format!(
                            "30-day usage unavailable for {}: {}",
                            cell(&entry.name),
                            cell(&error)
                        ));
                    }
                }
            }
        }
        report.notes.extend(usage_notes);
    }

    /// Remove a record the pass judged stale, after re-reading it: it must
    /// still be a sprites record of this scope in the same state.
    fn remove_record(
        name: &str,
        scope: Option<&OwnerScope>,
        class: Class,
    ) -> std::result::Result<(), String> {
        let Some(scope) = scope else {
            return Err("no ownership scope".into());
        };
        // Re-read, re-check and remove as one step under the ledger lock.
        ledger::forget_reaped(scope, name, class == Class::StaleCreating)
            .map_err(|error| format!("{error:#}"))
    }

    /// A terminal-safe table cell: control characters (API text is
    /// untrusted) become `?`, capped at 64 characters.
    fn cell(s: &str) -> String {
        s.chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .take(64)
            .collect()
    }

    /// The text report (pure).
    pub(super) fn render(report: &SpritesReport) -> String {
        if report.unrecorded_only {
            return render_unrecorded(report);
        }
        let mut out = Vec::new();
        for account in &report.accounts {
            let envs = account.envs.join(", ");
            out.push(match &account.reason {
                Some(reason) => format!(
                    "sprites ({envs}): listing {}: {}",
                    account.listing,
                    cell(reason)
                ),
                None => format!("sprites ({envs})"),
            });
            out.push(format!(
                "  {} recorded by this host; lifetime {}",
                account.recorded,
                if account.max_lifetime_secs == 0 {
                    "unlimited".to_string()
                } else {
                    format!("{}s", account.max_lifetime_secs)
                }
            ));
            if account.entries.is_empty() {
                out.push("  nothing to reap".into());
                continue;
            }
            let mut rows = vec![
                [
                    "NAME",
                    "CLASS",
                    "STATUS",
                    "AGE",
                    "PROOF",
                    "ON --apply",
                    "NOTE",
                ]
                .map(String::from)
                .to_vec(),
            ];
            for e in &account.entries {
                rows.push(vec![
                    cell(&e.name),
                    e.class.label().into(),
                    cell(&e.status),
                    e.age.clone(),
                    e.proof.into(),
                    e.class.action().unwrap_or("never deleted").into(),
                    e.note.as_deref().map(cell).unwrap_or_default(),
                ]);
            }
            for line in crate::cmd::fleet::table(&rows).lines() {
                out.push(format!("  {line}"));
            }
        }
        for note in &report.notes {
            out.push(format!("note: {note}"));
        }
        if !report.complete {
            out.push("listing incomplete: --apply is refused".into());
        }
        let unrecorded = report.count(Class::Unrecorded);
        if unrecorded > 0 {
            out.push(format!(
                "{unrecorded} sprite(s) not recorded by this host: never deleted by thegn; delete them in the Sprites dashboard if they are yours"
            ));
        }
        if !report.apply {
            let kept = report.count(Class::OverLifetimeBound);
            let kept = if kept > 0 {
                format!(", {kept} kept (bound to a worktree)")
            } else {
                String::new()
            };
            out.push(format!(
                "{} over-lifetime{kept}, {} orphan record(s), {} stale creating record(s)",
                report.count(Class::OverLifetime),
                report.count(Class::OrphanRecord),
                report.count(Class::StaleCreating)
            ));
            out.push("dry run: nothing deleted; re-run with --apply".into());
        } else if let Some(refused) = &report.refused {
            out.push(format!("{refused}; nothing deleted"));
        } else {
            for r in &report.results {
                let what = r.class.action().unwrap_or("skip");
                match &r.error {
                    None => out.push(format!("{what}: {}", cell(&r.name))),
                    Some(error) => out.push(format!("failed {what} {}: {error}", cell(&r.name))),
                }
            }
            out.push(format!(
                "{}/{} action(s) succeeded",
                report.results.len() - report.failed(),
                report.results.len()
            ));
        }
        out.join("\n")
    }

    fn usage_cell(value: Option<f64>, error: bool) -> String {
        if error {
            "?".into()
        } else {
            value.map_or_else(String::new, |value| format!("{value:.2}"))
        }
    }

    fn safe_adopt_name(name: &str) -> bool {
        (1..=63).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }

    fn render_unrecorded(report: &SpritesReport) -> String {
        let mut out = Vec::new();
        for account in &report.accounts {
            let envs = account.envs.join(", ");
            out.push(match &account.reason {
                Some(reason) => format!(
                    "sprites ({envs}): listing {}: {}",
                    account.listing,
                    cell(reason)
                ),
                None => format!("sprites ({envs})"),
            });
            let mut rows = vec![
                ["NAME", "STATUS", "AGE", "CPU-H 30D", "GB-H 30D", "NOTE"]
                    .map(String::from)
                    .to_vec(),
            ];
            for entry in &account.entries {
                rows.push(vec![
                    cell(&entry.name),
                    cell(&entry.status),
                    entry.age.clone(),
                    usage_cell(entry.cpu_hours_30d, entry.usage_error),
                    usage_cell(entry.gb_hours_30d, entry.usage_error),
                    entry.note.as_deref().map(cell).unwrap_or_default(),
                ]);
            }
            for line in crate::cmd::fleet::table(&rows).lines() {
                out.push(format!("  {line}"));
            }
            let count = account.entries.len();
            out.push(format!(
                "{count} sprite(s) on this account are not recorded by this host; thegn never deletes them."
            ));
            out.push("The Sprites API reports no size; 30-day usage is shown instead.".into());
            out.push("To let thegn manage one (env down / env reap can then remove it):".into());
            let first_env = account.envs.first().map(String::as_str).unwrap_or("?");
            for entry in &account.entries {
                if safe_adopt_name(&entry.name) {
                    out.push(format!(
                        "  thegn env adopt {} --env {}",
                        entry.name,
                        cell(first_env)
                    ));
                } else {
                    out.push(format!(
                        "  {}: unusual name, adopt by hand",
                        cell(&entry.name)
                    ));
                }
            }
            out.push("To delete one yourself: the Sprites dashboard.".into());
        }
        for note in &report.notes {
            out.push(format!("note: {}", cell(note)));
        }
        if !report.complete {
            out.push(
                "listing incomplete: the results may be incomplete; this report is read-only"
                    .into(),
            );
        }
        out.join("\n")
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::{Arc, Mutex};

        const HOUR_MS: i64 = 3_600_000;
        const NOW_MS: i64 = 1_800_000_000_000;

        struct Fake {
            scope: Option<OwnerScope>,
            listing: Mutex<Option<Listing>>,
            usage: Mutex<
                BTreeMap<
                    String,
                    std::result::Result<Option<thegn_svc::sprites::usage::SpriteUsage>, String>,
                >,
            >,
            usage_calls: Mutex<Vec<String>>,
            destroys: Mutex<Vec<String>>,
        }

        impl SpritesReapApi for Arc<Fake> {
            fn scope(&self) -> Option<OwnerScope> {
                self.scope.clone()
            }
            fn listing(&self) -> Listing {
                self.listing
                    .lock()
                    .unwrap()
                    .take()
                    .expect("listed once per account")
            }
            fn usage_30d(
                &self,
                name: &str,
            ) -> Result<Option<thegn_svc::sprites::usage::SpriteUsage>> {
                self.usage_calls.lock().unwrap().push(name.to_string());
                self.usage
                    .lock()
                    .unwrap()
                    .remove(name)
                    .unwrap_or(Ok(None))
                    .map_err(|error| anyhow::anyhow!("{error}"))
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
                usage: Mutex::new(BTreeMap::new()),
                usage_calls: Mutex::new(Vec::new()),
                destroys: Mutex::new(Vec::new()),
            })
        }

        fn envs(fake: &Arc<Fake>, lifetime: u64) -> Vec<SpritesEnv> {
            vec![SpritesEnv {
                name: "sx".into(),
                api: Some(Box::new(fake.clone()) as Box<dyn SpritesReapApi>),
                max_lifetime_secs: lifetime,
            }]
        }

        fn row(name: &str, created_ms: Option<i64>) -> SandboxInfo {
            SandboxInfo {
                id: name.into(),
                name: Some(name.into()),
                state: Some("warm".into()),
                size: None,
                region: None,
                created_at_ms: created_ms,
            }
        }

        fn write(name: &str, state: &str, created_secs: i64, owner: &str) {
            registry::write(&VpsRecord {
                name: name.into(),
                provider: LEDGER_PROVIDER.into(),
                state: state.into(),
                instance_id: String::new(),
                ip: String::new(),
                created_at: created_secs,
                owner: owner.into(),
                account: "acct1-a".into(),
            })
            .unwrap();
        }

        fn no_tty(_: &str) -> Option<String> {
            None
        }

        const DRY: Opts = Opts {
            apply: false,
            yes: false,
        };
        const APPLY_YES: Opts = Opts {
            apply: true,
            yes: true,
        };

        fn classes(report: &SpritesReport) -> Vec<(String, Class)> {
            report.accounts[0]
                .entries
                .iter()
                .map(|e| (e.name.clone(), e.class))
                .collect()
        }

        fn unrecorded_report(fake: &Arc<Fake>, yes: bool) -> SpritesReport {
            let mut report = reap(
                &envs(fake, 3600),
                Opts { apply: false, yes },
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            prepare_unrecorded(&mut report, &envs(fake, 3600));
            report
        }

        /// Every class in one account: an old recorded sprite, a gone ready
        /// record, a stale and a fresh `creating` record, a foreign sprite.
        fn seed() -> Arc<Fake> {
            let now_s = NOW_MS / 1000;
            write("sx-old", "ready", now_s - 2 * 3600, "ctl-a"); // both say old
            write("sx-gone", "ready", now_s - 100, "ctl-a");
            write("sx-stale", "creating", now_s - 7200, "ctl-a");
            write("sx-fresh", "creating", now_s - 60, "ctl-a");
            write("sx-young", "ready", now_s - 60, "ctl-a");
            fake(Listing::Complete(vec![
                row("sx-old", Some(NOW_MS - 5 * HOUR_MS)),
                row("sx-young", Some(NOW_MS - 60_000)),
                row("owner-personal", Some(NOW_MS - 50 * HOUR_MS)),
            ]))
        }

        #[test]
        fn sprites_reap_classifies_and_dry_run_deletes_nothing() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let fake = seed();
            let report = reap(
                &envs(&fake, 3600),
                DRY,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert!(fake.destroys.lock().unwrap().is_empty(), "dry run");
            assert_eq!(
                classes(&report),
                [
                    ("owner-personal".to_string(), Class::Unrecorded),
                    ("sx-gone".to_string(), Class::OrphanRecord),
                    ("sx-old".to_string(), Class::OverLifetime),
                    ("sx-stale".to_string(), Class::StaleCreating),
                ]
            );
            // Nothing was removed from the ledger on a dry run.
            assert!(registry::read_checked("sx-gone").unwrap().is_some());
            assert!(registry::read_checked("sx-stale").unwrap().is_some());
            let text = render(&report);
            assert!(text.contains("dry run: nothing deleted"), "{text}");
            assert!(text.contains("never deleted by thegn"), "{text}");
            assert_eq!(report.accounts[0].recorded, 5);
        }

        #[test]
        fn sprites_reap_over_lifetime_ages_from_the_later_created_at() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let now_s = NOW_MS / 1000;
            // An old sprite adopted recently: the API says 5 h old, the record
            // (written at adoption) says 10 s → lifetime counts from adoption,
            // not over a 1 h ceiling.
            write("sx-adopted", "ready", now_s - 10, "ctl-a");
            // The record says 5 h old; the API says young → not over.
            write("sx-new", "ready", now_s - 5 * 3600, "ctl-a");
            // No API time: the record's 5 h decides.
            write("sx-notime", "ready", now_s - 5 * 3600, "ctl-a");
            // Both old: aged from the later (the record's 2 h) → over.
            write("sx-old", "ready", now_s - 2 * 3600, "ctl-a");
            let fake = fake(Listing::Complete(vec![
                row("sx-adopted", Some(NOW_MS - 5 * HOUR_MS)),
                row("sx-new", Some(NOW_MS - 60_000)),
                row("sx-notime", None),
                row("sx-old", Some(NOW_MS - 5 * HOUR_MS)),
            ]));
            let report = reap(
                &envs(&fake, 3600),
                DRY,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert_eq!(
                classes(&report),
                [
                    ("sx-notime".to_string(), Class::OverLifetime),
                    ("sx-old".to_string(), Class::OverLifetime),
                ]
            );
            assert_eq!(report.accounts[0].entries[1].age, "2h0m");
            // Lifetime 0 = no ceiling. The listing names every recorded sprite
            // (the ledger still holds all four), so none is an orphan either.
            let fake0 = self::fake(Listing::Complete(vec![
                row("sx-adopted", Some(NOW_MS - 5 * HOUR_MS)),
                row("sx-new", Some(NOW_MS - 60_000)),
                row("sx-notime", None),
                row("sx-old", Some(NOW_MS - 5 * HOUR_MS)),
            ]));
            let report = reap(
                &envs(&fake0, 0),
                DRY,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert!(report.accounts[0].entries.is_empty());
        }

        #[test]
        fn sprites_reap_apply_never_deletes_unrecorded() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let fake = seed();
            let report = reap(
                &envs(&fake, 3600),
                APPLY_YES,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert_eq!(*fake.destroys.lock().unwrap(), ["sx-old"]);
            assert!(report.results.iter().all(|r| r.name != "owner-personal"));
            assert_eq!(report.failed(), 0, "{:?}", report.results);
            // The orphan and stale records are removed; the rest stay.
            assert!(registry::read_checked("sx-gone").unwrap().is_none());
            assert!(registry::read_checked("sx-stale").unwrap().is_none());
            assert!(registry::read_checked("sx-fresh").unwrap().is_some());
            assert!(registry::read_checked("sx-young").unwrap().is_some());
        }

        #[test]
        fn sprites_reap_apply_keeps_a_bound_over_lifetime_sprite() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let fake = seed();
            let bound = crate::sprites_reaper::BoundSet::with(&[("sx-old", "/wt/sx")], &[]);
            let report = reap(&envs(&fake, 3600), APPLY_YES, &mut no_tty, &bound, NOW_MS).unwrap();
            assert!(fake.destroys.lock().unwrap().is_empty());
            assert!(classes(&report).contains(&("sx-old".into(), Class::OverLifetimeBound)));
            let entry = report.accounts[0]
                .entries
                .iter()
                .find(|entry| entry.name == "sx-old")
                .unwrap();
            assert_eq!(
                entry.note.as_deref(),
                Some(
                    "bound to worktree /wt/sx: kept; `thegn env down --worktree <wt>` pulls its commits first"
                )
            );
            assert!(report.results.iter().all(|result| result.name != "sx-old"));
        }

        #[test]
        fn sprites_reap_spares_a_ready_record_committed_after_the_listing() {
            let _state = thegn_core::testenv::IsolatedState::new();
            write("sx-just-made", "ready", NOW_MS / 1000, "ctl-a");
            let fake = fake(Listing::Complete(Vec::new()));
            let report = reap(
                &envs(&fake, 3600),
                APPLY_YES,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert!(report.accounts[0].entries.is_empty(), "{report:?}");
            assert!(report.results.is_empty());
            assert!(registry::read_checked("sx-just-made").unwrap().is_some());
        }

        #[test]
        fn sprites_reap_partial_listing_refuses_apply() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let now_s = NOW_MS / 1000;
            write("sx-gone", "ready", now_s - 100, "ctl-a");
            write("sx-old", "ready", now_s - 5 * 3600, "ctl-a");
            let fake = fake(Listing::Partial {
                rows: vec![row("sx-old", Some(NOW_MS - 5 * HOUR_MS))],
                reason: "inventory exceeds the page cap".into(),
            });
            let report = reap(
                &envs(&fake, 3600),
                APPLY_YES,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert!(!report.complete);
            assert!(report.refused.as_deref().unwrap().contains("incomplete"));
            assert!(fake.destroys.lock().unwrap().is_empty());
            assert!(report.results.is_empty());
            // A partial listing never makes a missing sprite an orphan.
            assert!(
                report.accounts[0]
                    .entries
                    .iter()
                    .all(|e| e.class != Class::OrphanRecord)
            );
            assert!(registry::read_checked("sx-gone").unwrap().is_some());
        }

        #[test]
        fn sprites_reap_without_a_scope_refuses_apply() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let unscoped = Arc::new(Fake {
                scope: None,
                listing: Mutex::new(None),
                usage: Mutex::new(BTreeMap::new()),
                usage_calls: Mutex::new(Vec::new()),
                destroys: Mutex::new(Vec::new()),
            });
            let report = reap(
                &envs(&unscoped, 3600),
                APPLY_YES,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert_eq!(report.accounts[0].listing, "unknown");
            assert!(report.refused.is_some());
            assert!(unscoped.destroys.lock().unwrap().is_empty());
        }

        #[test]
        fn sprites_reap_pre_ledger_and_foreign_sprites_are_never_deleted() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let now_s = NOW_MS / 1000;
            // Another controller's record: never ours, never deleted.
            write("sx-theirs", "ready", now_s - 5 * 3600, "ctl-b");
            // A worktree location row naming a sprite proves nothing.
            {
                use thegn_core::store::WorkspaceStore;
                let db = thegn_core::db::Db::open().unwrap();
                let blob = serde_json::json!({
                    "control_prefix": ["/nix/store/x/bin/thegn", "sprite-exec", "sx-legacy", "--"],
                    "path": "/workspace",
                })
                .to_string();
                db.put_worktree("tab", "/repo", "/wt/l", "feat", Some(&blob), None)
                    .unwrap();
            }
            let fake = fake(Listing::Complete(vec![
                row("sx-legacy", Some(NOW_MS - 5 * HOUR_MS)),
                row("sx-theirs", Some(NOW_MS - 5 * HOUR_MS)),
            ]));
            let report = reap(
                &envs(&fake, 3600),
                APPLY_YES,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert_eq!(
                classes(&report),
                [
                    ("sx-legacy".to_string(), Class::Unrecorded),
                    ("sx-theirs".to_string(), Class::Unrecorded),
                ]
            );
            assert_eq!(
                report.accounts[0].entries[0].note.as_deref(),
                Some("not recorded by this host")
            );
            assert!(fake.destroys.lock().unwrap().is_empty());
            assert!(report.results.is_empty());
            assert!(registry::read_checked("sx-legacy").unwrap().is_none());
        }

        #[test]
        fn sprites_reap_ignores_other_providers_records() {
            let _state = thegn_core::testenv::IsolatedState::new();
            registry::write(&VpsRecord {
                name: "vx-a".into(),
                provider: "vercel".into(),
                state: "ready".into(),
                instance_id: "vx-a".into(),
                ip: String::new(),
                created_at: 1,
                owner: "ctl-a".into(),
                account: "acct1-a".into(),
            })
            .unwrap();
            let fake = fake(Listing::Complete(Vec::new()));
            let report = reap(
                &envs(&fake, 3600),
                APPLY_YES,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert!(report.accounts[0].entries.is_empty());
            assert!(registry::read_checked("vx-a").unwrap().is_some());
        }

        #[test]
        fn sprites_reap_apply_needs_confirmation_without_yes() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let fake = seed();
            let opts = Opts {
                apply: true,
                yes: false,
            };
            let report = reap(
                &envs(&fake, 3600),
                opts,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            assert!(report.refused.as_deref().unwrap().contains("--yes"));
            assert!(fake.destroys.lock().unwrap().is_empty());
            assert!(registry::read_checked("sx-gone").unwrap().is_some());
        }

        #[test]
        fn sprites_reap_json_shape() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let fake = seed();
            let report = reap(
                &envs(&fake, 3600),
                DRY,
                &mut no_tty,
                &crate::sprites_reaper::BoundSet::default(),
                NOW_MS,
            )
            .unwrap();
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(json["provider"], "sprites");
            assert_eq!(json["complete"], true);
            assert_eq!(json["accounts"][0]["entries"][0]["class"], "unrecorded");
            assert_eq!(json["accounts"][0]["max_lifetime_secs"], 3600);
            assert_eq!(json["results"], serde_json::json!([]));
        }

        #[test]
        fn unrecorded_mode_lists_only_unrecorded_oldest_first() {
            let _state = thegn_core::testenv::IsolatedState::new();
            write("sx-recorded", "ready", NOW_MS / 1000 - 100, "ctl-a");
            let fake = fake(Listing::Complete(vec![
                row("sx-young", Some(NOW_MS - HOUR_MS)),
                row("sx-old", Some(NOW_MS - 3 * HOUR_MS)),
                row("sx-unknown-age", None),
                row("sx-recorded", Some(NOW_MS - 5 * HOUR_MS)),
            ]));
            let report = unrecorded_report(&fake, true);
            assert!(report.unrecorded_only);
            assert_eq!(
                report.accounts[0]
                    .entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                ["sx-old", "sx-young", "sx-unknown-age"]
            );
            assert!(
                report.accounts[0]
                    .entries
                    .iter()
                    .all(|entry| entry.class == Class::Unrecorded)
            );
        }

        #[test]
        fn unrecorded_mode_never_destroys_or_removes_records() {
            let _state = thegn_core::testenv::IsolatedState::new();
            write("sx-recorded", "ready", NOW_MS / 1000 - 100, "ctl-a");
            let fake = fake(Listing::Complete(vec![
                row("sx-recorded", Some(NOW_MS - HOUR_MS)),
                row("sx-unrecorded", Some(NOW_MS - 2 * HOUR_MS)),
            ]));
            let report = unrecorded_report(&fake, true);
            assert_eq!(report.results, Vec::<ActionResult>::new());
            assert!(fake.destroys.lock().unwrap().is_empty());
            assert!(registry::read_checked("sx-recorded").unwrap().is_some());
        }

        #[test]
        fn unrecorded_mode_refuses_vercel() {
            let error = super::super::run(&Config::default(), "vercel", false, false, false, true)
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "--unrecorded is sprites only (a Vercel listing shows only this controller's sandboxes)"
            );
        }

        #[test]
        fn unrecorded_render_prints_adopt_lines_only_for_safe_names() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let fake = fake(Listing::Complete(vec![
                row("safe-name-1", Some(NOW_MS - HOUR_MS)),
                row("Unsafe/name", Some(NOW_MS - 2 * HOUR_MS)),
            ]));
            fake.usage.lock().unwrap().insert(
                "safe-name-1".into(),
                Ok(Some(thegn_svc::sprites::usage::SpriteUsage {
                    cpu_seconds: 7200.0,
                    reserved_gb_hours: 3.0,
                    partial: false,
                })),
            );
            let report = unrecorded_report(&fake, false);
            let text = render(&report);
            assert!(
                text.contains("  thegn env adopt safe-name-1 --env sx"),
                "{text}"
            );
            assert!(text.contains("2.00") && text.contains("3.00"), "{text}");
            assert!(
                text.contains("  Unsafe/name: unusual name, adopt by hand"),
                "{text}"
            );
            assert!(!text.contains("thegn env adopt Unsafe/name"), "{text}");
        }

        #[test]
        fn usage_error_is_a_note_not_a_failure() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let fake = fake(Listing::Complete(vec![row(
                "sx-unrecorded",
                Some(NOW_MS - HOUR_MS),
            )]));
            fake.usage
                .lock()
                .unwrap()
                .insert("sx-unrecorded".into(), Err("usage endpoint failed".into()));
            let report = unrecorded_report(&fake, false);
            let text = render(&report);
            assert_eq!(report.failed(), 0);
            assert!(
                report
                    .notes
                    .iter()
                    .any(|note| note.contains("usage endpoint failed"))
            );
            assert!(
                text.contains("CPU-H 30D") && text.contains("GB-H 30D"),
                "{text}"
            );
            assert!(text.contains("  ?"), "{text}");
        }

        #[test]
        fn usage_reads_capped_at_50() {
            let _state = thegn_core::testenv::IsolatedState::new();
            let rows = (0..51)
                .map(|index| {
                    row(
                        &format!("sx-{index:02}"),
                        Some(NOW_MS - (index as i64 + 1) * 60_000),
                    )
                })
                .collect();
            let fake = fake(Listing::Complete(rows));
            let _report = unrecorded_report(&fake, false);
            assert_eq!(fake.usage_calls.lock().unwrap().len(), 50);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeReap {
        account: String,
        listing: Mutex<Option<Listing>>,
        refuse: BTreeSet<String>,
        deletes: Mutex<Vec<String>>,
    }

    impl FakeReap {
        fn new(listing: Listing) -> Self {
            FakeReap {
                account: "acct".into(),
                listing: Mutex::new(Some(listing)),
                refuse: BTreeSet::new(),
                deletes: Mutex::new(Vec::new()),
            }
        }

        fn deletes(&self) -> Vec<String> {
            self.deletes.lock().unwrap().clone()
        }
    }

    impl ReapApi for std::sync::Arc<FakeReap> {
        fn account(&self) -> Result<String> {
            Ok(self.account.clone())
        }
        fn listing(&self) -> Listing {
            self.listing
                .lock()
                .unwrap()
                .take()
                .expect("listed once per account")
        }
        fn usage(&self) -> HashMap<String, UsageRow> {
            HashMap::new()
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.deletes.lock().unwrap().push(name.to_string());
            if self.refuse.contains(name) {
                bail!(
                    "vercel: refusing to delete {name}: it does not carry this thegn controller's tag"
                );
            }
            Ok(())
        }
    }

    fn row(name: &str) -> SandboxInfo {
        SandboxInfo {
            id: name.into(),
            name: Some(name.into()),
            state: Some("running".into()),
            size: None,
            region: None,
            created_at_ms: Some(0),
        }
    }

    fn envs(fake: &std::sync::Arc<FakeReap>) -> Vec<(String, Option<Box<dyn ReapApi>>)> {
        vec![(
            "vx".into(),
            Some(Box::new(fake.clone()) as Box<dyn ReapApi>),
        )]
    }

    fn record(name: &str) -> registry::VpsRecord {
        registry::VpsRecord {
            name: name.into(),
            provider: KIND.into(),
            state: "ready".into(),
            instance_id: name.into(),
            ip: String::new(),
            created_at: thegn_core::util::now(),
            owner: String::new(),
            account: String::new(),
        }
    }

    fn no_tty(_: &str) -> Option<String> {
        None
    }

    const DRY: Opts = Opts {
        apply: false,
        yes: false,
    };
    const APPLY_YES: Opts = Opts {
        apply: true,
        yes: true,
    };

    #[test]
    fn dry_run_makes_zero_delete_calls() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let fake = std::sync::Arc::new(FakeReap::new(Listing::Complete(vec![row("tg-orphan")])));
        let report = reap(&envs(&fake), DRY, &mut no_tty, 3_600_000).unwrap();
        assert!(fake.deletes().is_empty());
        assert_eq!(report.orphan_count(), 1);
        assert_eq!(report.accounts[0].orphans[0].age, "1h0m");
        let text = render(&report);
        assert!(
            text.contains("dry run: nothing deleted; re-run with --apply"),
            "{text}"
        );
        assert!(text.contains("tg-orphan"), "{text}");
    }

    #[test]
    fn non_tty_apply_without_yes_refuses() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let fake = std::sync::Arc::new(FakeReap::new(Listing::Complete(vec![row("tg-orphan")])));
        let opts = Opts {
            apply: true,
            yes: false,
        };
        let report = reap(&envs(&fake), opts, &mut no_tty, 0).unwrap();
        assert!(fake.deletes().is_empty());
        let refused = report.refused.as_deref().unwrap();
        assert!(refused.contains("without a TTY"), "{refused}");
        assert!(refused.contains("--yes"), "{refused}");
    }

    #[test]
    fn tty_apply_needs_a_typed_yes() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let opts = Opts {
            apply: true,
            yes: false,
        };
        let fake = std::sync::Arc::new(FakeReap::new(Listing::Complete(vec![row("tg-orphan")])));
        let mut prompts = Vec::new();
        let mut answer_y = |p: &str| {
            prompts.push(p.to_string());
            Some("y\n".to_string())
        };
        let report = reap(&envs(&fake), opts, &mut answer_y, 0).unwrap();
        assert_eq!(prompts, ["type yes to delete 1 sandboxes"]);
        assert!(fake.deletes().is_empty());
        assert!(report.refused.is_some());

        let fake = std::sync::Arc::new(FakeReap::new(Listing::Complete(vec![row("tg-orphan")])));
        let mut answer_yes = |_: &str| Some("yes\n".to_string());
        let report = reap(&envs(&fake), opts, &mut answer_yes, 0).unwrap();
        assert_eq!(fake.deletes(), ["tg-orphan"]);
        assert_eq!(report.failed(), 0);
    }

    #[test]
    fn partial_listing_refuses_apply() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let fake = std::sync::Arc::new(FakeReap::new(Listing::Partial {
            rows: vec![row("tg-orphan")],
            reason: "inventory exceeds the page cap".into(),
        }));
        let report = reap(&envs(&fake), APPLY_YES, &mut no_tty, 0).unwrap();
        assert!(fake.deletes().is_empty());
        assert!(!report.complete);
        assert_eq!(report.accounts[0].listing, "partial");
        assert!(report.refused.as_deref().unwrap().contains("incomplete"));
        let unknown = std::sync::Arc::new(FakeReap::new(Listing::Unknown("boom".into())));
        let report = reap(&envs(&unknown), APPLY_YES, &mut no_tty, 0).unwrap();
        assert!(!report.complete);
        assert_eq!(report.accounts[0].listing, "unknown");
        assert!(unknown.deletes().is_empty());
    }

    #[test]
    fn unavailable_env_refuses_apply_for_every_account() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let fake = std::sync::Arc::new(FakeReap::new(Listing::Complete(vec![row("tg-orphan")])));
        let mut list = envs(&fake);
        list.push(("broken".into(), None));
        let report = reap(&list, APPLY_YES, &mut no_tty, 0).unwrap();
        assert!(fake.deletes().is_empty());
        assert!(
            report
                .accounts
                .iter()
                .any(|a| a.listing == "unavailable" && a.envs == ["broken"])
        );
        assert!(report.refused.is_some());
    }

    #[test]
    fn untagged_sandbox_refusal_surfaces_and_fails_the_run() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let mut fake = FakeReap::new(Listing::Complete(vec![row("tg-a"), row("tg-untagged")]));
        fake.refuse.insert("tg-untagged".into());
        let fake = std::sync::Arc::new(fake);
        let report = reap(&envs(&fake), APPLY_YES, &mut no_tty, 0).unwrap();
        // Never stops on the first failure: both were attempted.
        assert_eq!(fake.deletes(), ["tg-a", "tg-untagged"]);
        assert_eq!(report.failed(), 1);
        let failed = report.results.iter().find(|r| !r.deleted).unwrap();
        assert_eq!(failed.name, "tg-untagged");
        assert!(
            failed
                .error
                .as_deref()
                .unwrap()
                .contains("refusing to delete"),
            "{failed:?}"
        );
        let text = render(&report);
        assert!(text.contains("deleted tg-a"), "{text}");
        assert!(text.contains("failed tg-untagged"), "{text}");
        assert!(text.contains("deleted 1/2 sandbox(es)"), "{text}");
    }

    #[test]
    fn ledger_names_are_never_orphans() {
        let _state = thegn_core::testenv::IsolatedState::new();
        registry::write(&record("tg-recorded")).unwrap();
        let fake = std::sync::Arc::new(FakeReap::new(Listing::Complete(vec![
            row("tg-recorded"),
            row("tg-orphan"),
        ])));
        let report = reap(&envs(&fake), APPLY_YES, &mut no_tty, 0).unwrap();
        let names: Vec<&str> = report.accounts[0]
            .orphans
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(names, ["tg-orphan"]);
        assert_eq!(fake.deletes(), ["tg-orphan"]);
    }

    #[test]
    fn envs_sharing_an_account_are_listed_once() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let fake = std::sync::Arc::new(FakeReap::new(Listing::Complete(vec![row("tg-orphan")])));
        let mut list = envs(&fake);
        list.push((
            "vx2".into(),
            Some(Box::new(fake.clone()) as Box<dyn ReapApi>),
        ));
        // `listing()` panics on a second call, so success proves one read.
        let report = reap(&list, DRY, &mut no_tty, 0).unwrap();
        assert_eq!(report.accounts.len(), 1);
        assert_eq!(report.accounts[0].envs, ["vx", "vx2"]);
        assert_eq!(report.orphan_count(), 1);
    }

    #[test]
    fn json_report_shape() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let fake = std::sync::Arc::new(FakeReap::new(Listing::Complete(vec![row("tg-orphan")])));
        let report = reap(&envs(&fake), DRY, &mut no_tty, 0).unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["provider"], "vercel");
        assert_eq!(json["apply"], false);
        assert_eq!(json["complete"], true);
        assert_eq!(json["accounts"][0]["orphans"][0]["name"], "tg-orphan");
        assert_eq!(json["results"], serde_json::json!([]));
    }
}
