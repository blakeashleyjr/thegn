//! `thegn env reap --provider vercel [--apply] [--yes] [--json]`: find Vercel
//! sandboxes this host's controller created (tag proof) that the local ledger
//! no longer records. Dry-run unless `--apply`; never runs automatically.
//!
//! Safety, in order:
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

pub fn run(cfg: &Config, provider: &str, apply: bool, yes: bool, json: bool) -> Result<()> {
    if !provider.trim().eq_ignore_ascii_case(KIND) {
        bail!(
            "env reap supports --provider vercel only (got {:?})",
            provider.trim()
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
    let mut ask = |prompt: &str| -> Option<String> {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return None;
        }
        eprint!("{prompt}: ");
        use std::io::{BufRead, Write};
        let _ = std::io::stderr().flush(); // best-effort: flush: display-only
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).ok()?;
        Some(line)
    };
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
