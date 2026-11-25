//! The fleet read shared by `thegn fleet list|cost` and the masthead `fleet`
//! widget: every provider sandbox across the configured `[env.<name>]`
//! entries, joined to the worktree that owns it, then priced.
//!
//! Each env with an API provider is inventoried once (20 s budget per env,
//! all envs concurrently, through [`crate::agent::block_on_provider`]). An env
//! whose provider cannot be built (its token does not resolve) or whose read
//! fails is reported as unavailable, not as an error. Two envs sharing one
//! account may list the same sandbox; it is shown once, under the first env.
//!
//! Blocking: callers on the event loop must run [`read`] off it (the widget
//! poll rides `tokio::task::spawn_blocking`, see `hydrate_fleet`).
//!
//! [`admit_new_sandbox`] is the `[placement.fleet_budget] max_hourly` gate
//! provisioning calls (off-loop) before it creates a sandbox. It reads the
//! network only when that env's provider has a cap set, and fails closed:
//! a fleet it cannot read is a new sandbox it refuses.

use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;
use thegn_core::config::Config;
use thegn_core::placement::fleet_cost::{
    self, Completeness, EnvInventory, FleetCost, FleetView, RecordedSandbox, SandboxRow,
};
use thegn_svc::provider::{InventoryReport, Provider, SandboxInfo};

/// Per-env inventory budget.
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(20);

/// One priced fleet read, as the widget and its popup hold it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FleetSnapshot {
    /// Epoch ms of the estimate.
    pub(crate) generated_ms: i64,
    pub(crate) view: FleetView,
    pub(crate) cost: FleetCost,
    /// Metered AI spend from the model proxy's audit rows. Default (all zero)
    /// when the proxy has recorded nothing or its database cannot be read.
    pub(crate) ai: thegn_core::proxy::stats::AiSpend,
}

/// Inventory every provider env and price the result at "now".
pub(crate) fn read(cfg: &Config) -> Result<FleetSnapshot> {
    let view = gather(cfg, None)?;
    let generated_ms = thegn_core::util::now_ms();
    let mut cost = priced(cfg, &view, generated_ms);
    apply_sprites(cfg, &view, &mut cost, generated_ms);
    let ai = ai_spend_now(generated_ms);
    Ok(FleetSnapshot {
        generated_ms,
        view,
        cost,
        ai,
    })
}

/// Window for the Sprites burn estimate: usage since the start of the UTC
/// hour two hours back (so 1 to 2 hours of buckets).
const SPRITES_RECENT_BACK_SECS: i64 = 2 * 3600;

/// Read each configured Sprites account's usage (today, month to date, and a
/// recent window) and fold it into `cost`. Account-wide, so sprites another
/// host created on the same account are billed here too. A read that fails
/// leaves `cost` unchanged (logged at debug). Blocking; off-loop only.
pub(crate) fn apply_sprites(cfg: &Config, view: &FleetView, cost: &mut FleetCost, now_ms: i64) {
    let usage = sprites_account_usage(cfg, now_ms / 1000);
    fleet_cost::apply_sprites_usage(cost, view, &usage);
}

fn sprites_account_usage(cfg: &Config, now_secs: i64) -> Vec<fleet_cost::SpritesAccountUsage> {
    use thegn_svc::sprites::usage::{month_start_utc, usage_window_start};
    let channel = crate::channel_state::current();
    let rfc = |secs: i64| {
        chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
            .unwrap_or_default()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    let day_start = now_secs - now_secs.rem_euclid(86_400);
    let recent_start = usage_window_start(now_secs - SPRITES_RECENT_BACK_SECS);
    let window_hours = (now_secs - recent_start) as f64 / 3600.0;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (name, envc) in &cfg.env {
        if envc.provider.provider.trim() != "sprites"
            || !fleet_provider_allowed(channel, &envc.provider.provider)
        {
            continue;
        }
        let Ok(Provider::Sprites(provider)) =
            crate::provider_factory::provider_for_inventory(&envc.provider)
        else {
            continue;
        };
        let account = provider
            .scope()
            .map(|scope| scope.account.clone())
            .unwrap_or_else(|| format!("env:{name}"));
        if !seen.insert(account) {
            continue;
        }
        let read = |since: i64| {
            let start = rfc(since);
            crate::agent::block_on_provider(|| async {
                tokio::time::timeout(INVENTORY_TIMEOUT, provider.account_usage_since(&start))
                    .await
                    .map_err(|_| anyhow::anyhow!("timed out"))?
            })
        };
        let reads = (
            read(day_start),
            read(month_start_utc(now_secs)),
            read(recent_start),
        );
        match reads {
            (Ok(day), Ok(month), Ok(recent)) => out.push(usage_from_readings(
                &day.usage,
                &month.usage,
                &recent.usage,
                window_hours,
            )),
            (day, month, recent) => {
                let error = [day.err(), month.err(), recent.err()]
                    .into_iter()
                    .flatten()
                    .next();
                tracing::debug!(target: "thegn::fleet", env = %name, error = ?error, "sprites account usage read failed");
            }
        }
    }
    out
}

/// Shape three usage readings (today, month to date, recent window) into the
/// cost input. Pure.
pub(crate) fn usage_from_readings(
    day: &thegn_svc::sprites::usage::SpriteUsage,
    month: &thegn_svc::sprites::usage::SpriteUsage,
    recent: &thegn_svc::sprites::usage::SpriteUsage,
    window_hours: f64,
) -> fleet_cost::SpritesAccountUsage {
    let hours = |seconds: f64| {
        if seconds.is_finite() && seconds > 0.0 {
            seconds / 3600.0
        } else {
            0.0
        }
    };
    fleet_cost::SpritesAccountUsage {
        today_cpu_hours: hours(day.cpu_seconds),
        today_gb_hours: day.reserved_gb_hours,
        mtd_cpu_hours: hours(month.cpu_seconds),
        mtd_gb_hours: month.reserved_gb_hours,
        recent_cpu_hours: hours(recent.cpu_seconds),
        recent_gb_hours: recent.reserved_gb_hours,
        recent_window_hours: window_hours,
    }
}

/// Metered AI spend (model-proxy audit rows, month to date) at `now_ms`. A
/// database that cannot be read is no spend: the widget then shows VM burn only.
/// Blocking; runs inside the off-loop fleet read.
fn ai_spend_now(now_ms: i64) -> thegn_core::proxy::stats::AiSpend {
    use thegn_core::store::ModelProxyStore;
    let month = fleet_cost::utc_month_start_ms(now_ms);
    let rows = thegn_core::db::Db::open()
        .and_then(|db| db.model_proxy_requests_since(month, 200_000))
        .unwrap_or_default();
    thegn_core::proxy::stats::ai_spend(&rows, now_ms, fleet_cost::utc_day_start_ms(now_ms), month)
}

/// Price a view at `now_ms` and record its `[placement.fleet_budget]`
/// breaches.
pub(crate) fn priced(cfg: &Config, view: &FleetView, now_ms: i64) -> FleetCost {
    let mut cost = fleet_cost::fleet_cost(
        view,
        fleet_cost::BUNDLED_PRICES,
        &cfg.placement.price,
        now_ms,
    );
    fleet_cost::apply_budgets(&mut cost, &cfg.placement.fleet_budget);
    cost
}

/// The `max_hourly` gate for a NEW provider sandbox `sandbox_id` in env
/// `env_name`. `Ok` without any network read when the env's provider has no
/// `max_hourly`; `Ok` when the provider already lists `sandbox_id` (resume
/// and reuse are never refused); otherwise the provider's burn across every
/// env using it must be under the cap. An inventory that cannot be read, or
/// is incomplete, refuses ("cannot verify fleet budget"). Blocking.
pub(crate) fn admit_new_sandbox(
    cfg: &Config,
    env_name: &str,
    sandbox_id: &str,
) -> std::result::Result<(), String> {
    admit_with(cfg, env_name, sandbox_id, |provider| {
        gather_where(cfg, |_, envc| envc.provider.provider == provider)
    })
}

/// [`admit_new_sandbox`] over an injected inventory read (the test seam).
fn admit_with(
    cfg: &Config,
    env_name: &str,
    sandbox_id: &str,
    inventory: impl FnOnce(&str) -> Result<FleetView>,
) -> std::result::Result<(), String> {
    let Some(envc) = cfg.env.get(env_name) else {
        return Ok(());
    };
    let provider = envc.provider.provider.as_str();
    let budgets = &cfg.placement.fleet_budget;
    let capped = budgets
        .get(provider)
        .is_some_and(|b| b.max_hourly.is_finite() && b.max_hourly > 0.0);
    if !capped {
        return Ok(());
    }
    let refuse = |why: &str| {
        format!(
            "cannot verify fleet budget for {provider}: {why}; a new sandbox is refused while \
             [placement.fleet_budget.{provider}] max_hourly is set"
        )
    };
    let view = match inventory(provider) {
        Ok(view) => view,
        Err(error) => {
            tracing::debug!(target: "thegn::fleet", provider, error = %format!("{error:#}"), "budget inventory read failed");
            return Err(refuse("the fleet inventory read failed"));
        }
    };
    if view.rows.iter().any(|row| {
        row.provider == provider
            && (row.id == sandbox_id || row.name.as_deref() == Some(sandbox_id))
    }) {
        return Ok(());
    }
    if let Some(partial) = view.partial.first() {
        return Err(refuse(&format!(
            "env {} could not be fully listed",
            partial.env
        )));
    }
    let cost = priced(cfg, &view, thegn_core::util::now_ms());
    fleet_cost::admit_new(&cost, provider, budgets)
}

/// One env to inventory, in config order.
struct Slot {
    env: String,
    provider_name: String,
    provider: Result<Provider, String>,
}

pub(crate) fn gather(cfg: &Config, only_env: Option<&str>) -> Result<FleetView> {
    gather_where(cfg, |name, _| only_env.is_none_or(|only| only == name))
}

/// Whether `thegn fleet` (and the masthead widget) may read an env whose
/// `provider` is named `provider` in `channel`: every provider on dev, only the
/// stable ones (Sprites and Vercel) on stable. The config clamp already drops
/// the other envs from a stable config; this is the read path's own guard.
pub(crate) fn fleet_provider_allowed(
    channel: thegn_core::channel::Channel,
    provider: &str,
) -> bool {
    channel == thegn_core::channel::Channel::Dev
        || thegn_core::config::EnvProviderKind::of(provider).stable_channel()
}

/// [`gather`] over the API-provider envs `keep` selects.
fn gather_where(
    cfg: &Config,
    keep: impl Fn(&str, &thegn_core::config::EnvConfig) -> bool,
) -> Result<FleetView> {
    use thegn_core::config::EnvProviderKind;
    let channel = crate::channel_state::current();
    let slots: Vec<Slot> = cfg
        .env
        .iter()
        .filter(|(name, envc)| keep(name.as_str(), envc))
        .filter(|(_, envc)| fleet_provider_allowed(channel, &envc.provider.provider))
        .filter(|(_, envc)| {
            !matches!(
                EnvProviderKind::of(&envc.provider.provider),
                EnvProviderKind::Custom
            )
        })
        .map(|(name, envc)| Slot {
            env: name.clone(),
            provider_name: envc.provider.provider.clone(),
            provider: crate::provider_factory::provider_for_inventory(&envc.provider),
        })
        .collect();
    let reads = crate::agent::block_on_provider(|| async {
        let reads = slots.iter().map(|slot| async move {
            match &slot.provider {
                Ok(provider) => tokio::time::timeout(INVENTORY_TIMEOUT, provider.inventory())
                    .await
                    .map_err(|_| format!("timed out after {}s", INVENTORY_TIMEOUT.as_secs()))
                    .and_then(|read| read.map_err(|error| format!("{error:#}"))),
                Err(reason) => Err(reason.clone()),
            }
        });
        Ok(futures::future::join_all(reads).await)
    })?;
    let inventories = slots
        .iter()
        .zip(reads)
        .map(|(slot, read)| env_inventory(&slot.env, &slot.provider_name, read))
        .collect();
    let mut view = fleet_cost::join(inventories, recorded_sandboxes(cfg));
    fleet_cost::split_down(&mut view, |record| {
        crate::provider_workdir::is_provisioned_locally(&record.id)
    });

    let mut sprites_names = BTreeMap::<String, Option<BTreeSet<String>>>::new();
    for slot in &slots {
        let Ok(Provider::Sprites(provider)) = &slot.provider else {
            continue;
        };
        let names = provider.scope().map(|scope| {
            thegn_svc::sprites::ledger::owned(scope)
                .map(|owned| {
                    owned
                        .recorded
                        .into_iter()
                        .map(|record| record.name)
                        .chain(owned.unreadable)
                        .collect()
                })
                .map_err(|error| {
                    tracing::debug!(
                        target: "thegn::fleet",
                        env = %slot.env,
                        error = %format!("{error:#}"),
                        "sprites ownership ledger read failed"
                    );
                })
                .ok()
        });
        sprites_names.insert(slot.env.clone(), names.flatten());
    }
    fleet_cost::mark_unrecorded(&mut view, |row| {
        sprites_ours(
            sprites_names.get(&row.env).and_then(|names| names.as_ref()),
            row,
        )
    });
    Ok(view)
}

fn sprites_ours(names: Option<&BTreeSet<String>>, row: &fleet_cost::FleetRow) -> Option<bool> {
    if row.provider != "sprites" {
        return None;
    }
    names.map(|names| names.contains(&row.id))
}

/// Shape one env's read. `Err` carries the reason (build or read failure).
pub(crate) fn env_inventory(
    env: &str,
    provider: &str,
    read: std::result::Result<InventoryReport, String>,
) -> EnvInventory {
    let to_rows = |rows: Vec<SandboxInfo>| -> Vec<SandboxRow> {
        rows.into_iter()
            .map(|info| SandboxRow {
                provider: provider.to_string(),
                env: env.to_string(),
                id: info.id,
                name: info.name,
                state: info.state,
                size: info.size,
                region: info.region,
                created_ms: info.created_at_ms,
            })
            .collect()
    };
    let (completeness, rows) = match read {
        Err(reason) => (
            Completeness::Unavailable(format!("unavailable: {reason}")),
            Vec::new(),
        ),
        Ok(InventoryReport::Complete(rows)) => (Completeness::Complete, to_rows(rows)),
        Ok(InventoryReport::Partial { rows, reason }) => (
            Completeness::Partial(format!("partial: {reason}")),
            to_rows(rows),
        ),
    };
    EnvInventory {
        env: env.to_string(),
        provider: provider.to_string(),
        completeness,
        rows,
    }
}

/// Every recorded worktree whose resolved env is a provider placement, with
/// its bound sandbox id. Unreadable rows are skipped (logged at debug).
fn recorded_sandboxes(cfg: &Config) -> Vec<RecordedSandbox> {
    use thegn_core::store::WorkspaceStore;
    let rows = match thegn_core::db::Db::open().and_then(|db| db.worktrees()) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(target: "thegn::fleet", error = %format!("{error:#}"), "worktree rows unreadable; no sandbox is joined");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter(|row| Path::new(&row.worktree).is_dir())
        .filter_map(|row| recorded_for(cfg, &row.worktree))
        .collect()
}

fn recorded_for(cfg: &Config, worktree: &str) -> Option<RecordedSandbox> {
    let env = match crate::cmd::env::resolve_for(cfg, Some(worktree.to_string())) {
        Ok(env) => env,
        Err(error) => {
            tracing::debug!(target: "thegn::fleet", worktree, error = %format!("{error:#}"), "env unresolved");
            return None;
        }
    };
    let thegn_core::placement::Placement::Provider(placement) = &env.placement else {
        return None;
    };
    let id = match crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, &env.name)
    {
        Ok(Some(id)) => id,
        Ok(None) => placement.id.clone(),
        Err(error) => {
            tracing::debug!(target: "thegn::fleet", worktree, error = %format!("{error:#}"), "sandbox binding unreadable");
            return None;
        }
    };
    Some(RecordedSandbox {
        worktree: worktree.to_string(),
        provider: placement.provider.clone(),
        env: env.name.clone(),
        id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn sprites_usage_readings_price_from_fake_responses() {
        use thegn_svc::sprites::usage::parse_usage;
        let body = |cpu: f64, mem: f64| serde_json::json!({"usage": [{"cpu_time_sec": cpu, "reserved_memory_byte_sec": mem}]});
        let gib_hour = (1u64 << 30) as f64 * 3600.0;
        let day = parse_usage(&body(7200.0, 4.0 * gib_hour)).unwrap();
        let month = parse_usage(&body(36_000.0, 20.0 * gib_hour)).unwrap();
        let recent = parse_usage(&body(3600.0, 2.0 * gib_hour)).unwrap();
        let usage = usage_from_readings(&day, &month, &recent, 2.0);
        assert!((usage.today_cpu_hours - 2.0).abs() < 1e-9);
        assert!((usage.mtd_gb_hours - 20.0).abs() < 1e-6);
        let view = FleetView::default();
        let mut cost =
            fleet_cost::fleet_cost(&view, fleet_cost::BUNDLED_PRICES, &BTreeMap::new(), 0);
        fleet_cost::apply_sprites_usage(&mut cost, &view, &[usage]);
        let burn = (0.07 + 2.0 * 0.04375) / 2.0;
        assert!((cost.total_hourly - burn).abs() < 1e-6);
        assert!(cost.today > 0.0 && cost.mtd > cost.today);
    }

    #[test]
    fn stable_fleet_reads_only_sprites_and_vercel() {
        use thegn_core::channel::Channel;
        for stable in ["sprites", "vercel"] {
            assert!(fleet_provider_allowed(Channel::Stable, stable), "{stable}");
        }
        for dev_only in [
            "fly",
            "digitalocean",
            "hetzner",
            "daytona",
            "machine0",
            "custom",
        ] {
            assert!(
                !fleet_provider_allowed(Channel::Stable, dev_only),
                "{dev_only}"
            );
            assert!(fleet_provider_allowed(Channel::Dev, dev_only), "{dev_only}");
        }
    }

    #[test]
    fn env_inventory_shapes_each_read() {
        let info = SandboxInfo {
            id: "a".into(),
            size: Some("s".into()),
            ..SandboxInfo::default()
        };
        let complete = env_inventory(
            "e",
            "fly",
            Ok(InventoryReport::Complete(vec![info.clone()])),
        );
        assert_eq!(complete.completeness, Completeness::Complete);
        assert_eq!(complete.rows[0].provider, "fly");
        assert_eq!(complete.rows[0].env, "e");
        assert_eq!(complete.rows[0].size.as_deref(), Some("s"));
        let partial = env_inventory(
            "e",
            "fly",
            Ok(InventoryReport::Partial {
                rows: vec![info],
                reason: "x".into(),
            }),
        );
        assert_eq!(
            partial.completeness,
            Completeness::Partial("partial: x".into())
        );
        assert_eq!(partial.rows.len(), 1);
        let failed = env_inventory("e", "fly", Err("boom".into()));
        assert_eq!(
            failed.completeness,
            Completeness::Unavailable("unavailable: boom".into())
        );
        let unbuilt = env_inventory(
            "e",
            "fly",
            Err("API token FLY_X could not be resolved".into()),
        );
        assert_eq!(
            unbuilt.completeness,
            Completeness::Unavailable("unavailable: API token FLY_X could not be resolved".into())
        );
    }

    #[test]
    fn sprites_ours_table() {
        let names = BTreeSet::from(["owned".to_string()]);
        let sprite = |id: &str| fleet_cost::FleetRow {
            provider: "sprites".into(),
            id: id.into(),
            ..fleet_cost::FleetRow::default()
        };
        assert_eq!(sprites_ours(None, &sprite("owned")), None);
        assert_eq!(sprites_ours(Some(&names), &sprite("owned")), Some(true));
        assert_eq!(sprites_ours(Some(&names), &sprite("foreign")), Some(false));
        assert_eq!(
            sprites_ours(
                Some(&names),
                &fleet_cost::FleetRow {
                    provider: "vercel".into(),
                    id: "owned".into(),
                    ..fleet_cost::FleetRow::default()
                }
            ),
            None
        );
    }

    fn budget_cfg(max_hourly: f64) -> Config {
        toml::from_str(&format!(
            r#"
            [env.e]
            placement = "provider"
            [env.e.provider]
            provider = "fly"
            [env.other]
            placement = "provider"
            [env.other.provider]
            provider = "digitalocean"
            [placement.price]
            fly = 1.0
            [placement.fleet_budget.fly]
            warn_hourly = 1.0
            max_hourly = {max_hourly:.1}
            "#
        ))
        .expect("budget config parses")
    }

    fn running(ids: &[&str]) -> FleetView {
        FleetView {
            rows: ids
                .iter()
                .map(|id| fleet_cost::FleetRow {
                    provider: "fly".into(),
                    env: "e".into(),
                    id: (*id).to_string(),
                    state: Some("started".into()),
                    ..fleet_cost::FleetRow::default()
                })
                .collect(),
            ..FleetView::default()
        }
    }

    #[test]
    fn admit_skips_the_read_without_a_cap() {
        let no_read = |_: &str| -> Result<FleetView> { panic!("no network without a cap") };
        assert_eq!(admit_with(&budget_cfg(0.0), "e", "new", no_read), Ok(()));
        assert_eq!(
            admit_with(&budget_cfg(2.0), "other", "new", no_read),
            Ok(()),
            "a provider without a budget is never read"
        );
        assert_eq!(admit_with(&budget_cfg(2.0), "nope", "new", no_read), Ok(()));
    }

    #[test]
    fn admit_reads_only_the_capped_provider() {
        let mut asked = None;
        let got = admit_with(&budget_cfg(2.0), "e", "new", |provider| {
            asked = Some(provider.to_string());
            Ok(running(&["a"]))
        });
        assert_eq!(got, Ok(()), "1.0/h is under a 2.0/h cap");
        assert_eq!(asked.as_deref(), Some("fly"));
    }

    #[test]
    fn admit_refuses_a_new_id_at_max_but_admits_an_existing_one() {
        let cfg = budget_cfg(2.0);
        let err = admit_with(&cfg, "e", "new", |_| Ok(running(&["a", "b"]))).unwrap_err();
        assert!(err.contains("max_hourly $2.00/h"), "{err}");
        assert_eq!(
            admit_with(&cfg, "e", "b", |_| Ok(running(&["a", "b"]))),
            Ok(()),
            "resume of a listed sandbox is never refused"
        );
        let mut by_name = running(&["a", "machine-id"]);
        by_name.rows[1].name = Some("named".into());
        assert_eq!(admit_with(&cfg, "e", "named", |_| Ok(by_name)), Ok(()));
        let mut stopped = running(&["a", "b"]);
        stopped.rows[1].state = Some("stopped".into());
        assert_eq!(
            admit_with(&cfg, "e", "new", |_| Ok(stopped)),
            Ok(()),
            "a stopped sandbox does not burn"
        );
    }

    #[test]
    fn admit_counts_unrecorded_rows_toward_the_budget() {
        let cfg = budget_cfg(2.0);
        let mut view = running(&["a", "unrecorded"]);
        view.rows[1].unrecorded = true;
        view.unrecorded = 1;
        let error = admit_with(&cfg, "e", "new", |_| Ok(view)).unwrap_err();
        assert!(error.contains("max_hourly $2.00/h"), "{error}");
    }

    #[test]
    fn admit_fails_closed_on_an_unreadable_fleet() {
        let cfg = budget_cfg(2.0);
        let err = admit_with(&cfg, "e", "new", |_| {
            Err(anyhow::anyhow!("401 body: token=hunter2"))
        })
        .unwrap_err();
        assert!(err.contains("cannot verify fleet budget"), "{err}");
        assert!(
            !err.contains("hunter2"),
            "no response body in the error: {err}"
        );
        let mut partial = running(&[]);
        partial.partial.push(fleet_cost::PartialEnv {
            env: "e".into(),
            reason: "unavailable: secret body".into(),
        });
        let err = admit_with(&cfg, "e", "new", |_| Ok(partial)).unwrap_err();
        assert!(err.contains("cannot verify fleet budget"), "{err}");
        assert!(!err.contains("secret body"), "{err}");
    }

    #[test]
    fn priced_applies_budgets() {
        let cost = priced(&budget_cfg(5.0), &running(&["a", "b"]), 0);
        assert_eq!(cost.budget.len(), 1);
        assert_eq!(cost.budget[0].level, fleet_cost::BudgetLevel::Warn);
        assert!((cost.budget[0].hourly - 2.0).abs() < 1e-9);
    }
}
