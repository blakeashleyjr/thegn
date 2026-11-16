//! `thegn fleet list|cost` — every provider sandbox across the configured
//! `[env.<name>]` entries, joined to the worktree that owns it, with an
//! estimated cost. Read-only: nothing is stopped or destroyed.
//!
//! Each env with an API provider is inventoried once (20 s budget per env,
//! all envs concurrently). An env whose provider cannot be built (its token
//! does not resolve) or whose read fails is reported as unavailable, not as a
//! command error. Two envs sharing one account may list the same sandbox; it
//! is shown once, under the first env.
//!
//! `--json` emits one document (`FleetReport`) with these stable fields:
//! - `generated_ms`: epoch ms of the estimate.
//! - `view.rows[]`: `provider`, `env`, `id`, `name`, `state`, `size`, `region`,
//!   `created_ms`, `worktree` (`null` = orphan: no worktree records it).
//! - `view.orphans`: count of rows with no worktree.
//! - `view.missing[]`: `worktree`, `provider`, `env`, `id` — recorded by a
//!   worktree, not listed by its (completely read) provider.
//! - `view.partial[]`: `env`, `reason` — envs read partially or not at all.
//! - `cost.per_row[]`: `provider`, `id`, `rate` (`null` = unpriced, else
//!   `usd_per_hour` and `source`: `"config"` or `{"bundled":{"date":…}}`),
//!   `burn_usd_per_hour`, `accrued_usd`, `today_usd`, `mtd_usd`; in
//!   `view.rows` order.
//! - `cost.per_provider_hourly`, `cost.total_hourly`, `cost.today`,
//!   `cost.mtd`, `cost.projected_24h`, `cost.projected_30d`, `cost.unpriced`,
//!   `cost.price_table_date`.
//!
//! All amounts are USD estimates from the bundled price table and
//! `[placement.price]`.

use anyhow::Result;
use std::path::Path;
use std::time::Duration;
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_core::placement::fleet_cost::{
    self, Completeness, EnvInventory, FleetCost, FleetView, RecordedSandbox, SandboxRow,
};
use thegn_svc::provider::{InventoryReport, Provider, SandboxInfo};

/// Per-env inventory budget.
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Every provider sandbox per env with its worktree (or ORPHAN), plus
    /// recorded sandboxes the provider no longer lists.
    List {
        /// Only this `[env.<name>]`.
        #[arg(long)]
        env: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Estimated spend: hourly burn, today, month to date, 24 h / 30 d
    /// projections.
    Cost {
        #[arg(long)]
        json: bool,
    },
}

#[derive(serde::Serialize)]
struct FleetReport {
    generated_ms: i64,
    view: FleetView,
    cost: FleetCost,
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    let (only_env, json, list) = match action {
        Action::List { env, json } => (env, json, true),
        Action::Cost { json } => (None, json, false),
    };
    if let Some(name) = &only_env {
        anyhow::ensure!(
            cfg.env.contains_key(name),
            "no [env.{name}] is configured (see `thegn env list`)"
        );
    }
    let view = gather(cfg, only_env.as_deref())?;
    let now_ms = thegn_core::util::now_ms();
    let cost = fleet_cost::fleet_cost(
        &view,
        fleet_cost::BUNDLED_PRICES,
        &cfg.placement.price,
        now_ms,
    );
    if json {
        return super::emit_json(&FleetReport {
            generated_ms: now_ms,
            view,
            cost,
        });
    }
    let text = if list {
        render_list(&view, &cost, now_ms)
    } else {
        render_cost(&view, &cost)
    };
    outln!("{text}");
    Ok(())
}

/// One env to inventory, in config order.
struct Slot {
    env: String,
    provider_name: String,
    provider: Option<Provider>,
}

fn gather(cfg: &Config, only_env: Option<&str>) -> Result<FleetView> {
    use thegn_core::config::EnvProviderKind;
    let slots: Vec<Slot> = cfg
        .env
        .iter()
        .filter(|(name, _)| only_env.is_none_or(|only| only == name.as_str()))
        .filter(|(_, envc)| {
            !matches!(
                EnvProviderKind::of(&envc.provider.provider),
                EnvProviderKind::Custom
            )
        })
        .map(|(name, envc)| Slot {
            env: name.clone(),
            provider_name: envc.provider.provider.clone(),
            provider: crate::provider_factory::provider_for_named(
                &envc.provider,
                &envc.provider.id,
            ),
        })
        .collect();
    let reads = crate::agent::block_on_provider(|| async {
        let reads = slots.iter().map(|slot| async move {
            match &slot.provider {
                Some(provider) => Some(
                    tokio::time::timeout(INVENTORY_TIMEOUT, provider.inventory())
                        .await
                        .map_err(|_| format!("timed out after {}s", INVENTORY_TIMEOUT.as_secs()))
                        .and_then(|read| read.map_err(|error| format!("{error:#}"))),
                ),
                None => None,
            }
        });
        Ok(futures::future::join_all(reads).await)
    })?;
    let inventories = slots
        .iter()
        .zip(reads)
        .map(|(slot, read)| env_inventory(&slot.env, &slot.provider_name, read))
        .collect();
    Ok(fleet_cost::join(inventories, recorded_sandboxes(cfg)))
}

/// Shape one env's read. `None` = the provider could not be built.
fn env_inventory(
    env: &str,
    provider: &str,
    read: Option<std::result::Result<InventoryReport, String>>,
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
        None => (
            Completeness::Unavailable(
                "unavailable: no API provider or its token could not be resolved".into(),
            ),
            Vec::new(),
        ),
        Some(Err(reason)) => (
            Completeness::Unavailable(format!("unavailable: {reason}")),
            Vec::new(),
        ),
        Some(Ok(InventoryReport::Complete(rows))) => (Completeness::Complete, to_rows(rows)),
        Some(Ok(InventoryReport::Partial { rows, reason })) => (
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
    let env = match super::env::resolve_for(cfg, Some(worktree.to_string())) {
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

fn usd(value: f64, decimals: usize) -> String {
    format!("${value:.decimals$}")
}

/// Compact age: `3d4h`, `5h12m`, `42m`, `?` when unknown.
fn age(created_ms: Option<i64>, now_ms: i64) -> String {
    let Some(created) = created_ms else {
        return "?".into();
    };
    let mins = (now_ms - created).max(0) / 60_000;
    let (days, hours, minutes) = (mins / 1440, (mins % 1440) / 60, mins % 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// Left-aligned columns, two spaces apart, trailing spaces trimmed.
fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .filter_map(|row| row.get(c))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (c, cell) in row.iter().enumerate() {
                if c > 0 {
                    line.push_str("  ");
                }
                line.push_str(cell);
                let pad = widths[c].saturating_sub(cell.chars().count());
                line.extend(std::iter::repeat_n(' ', pad));
            }
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn opt(value: Option<&str>) -> String {
    value.filter(|v| !v.is_empty()).unwrap_or("-").to_string()
}

fn footer(cost: &FleetCost) -> String {
    format!(
        "burn {}/h · today {} · MTD {} · next 24h {} · next 30d {}\nestimates from the {} price table and [placement.price]; {} unpriced",
        usd(cost.total_hourly, 4),
        usd(cost.today, 2),
        usd(cost.mtd, 2),
        usd(cost.projected_24h, 2),
        usd(cost.projected_30d, 2),
        cost.price_table_date,
        cost.unpriced,
    )
}

fn partial_section(view: &FleetView, out: &mut Vec<String>) {
    if view.partial.is_empty() {
        return;
    }
    out.push(String::new());
    out.push("Incomplete envs:".into());
    for env in &view.partial {
        out.push(format!("  {}: {}", env.env, env.reason));
    }
}

/// The `fleet list` table (pure; `cost.per_row` is in `view.rows` order).
fn render_list(view: &FleetView, cost: &FleetCost, now_ms: i64) -> String {
    let mut rows = vec![
        [
            "PROVIDER/ENV",
            "ID",
            "NAME",
            "STATE",
            "SIZE",
            "REGION",
            "AGE",
            "$/H",
            "ACCRUED",
            "WORKTREE",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for (index, row) in view.rows.iter().enumerate() {
        let priced = cost.per_row.get(index).filter(|c| c.id == row.id);
        let rate = priced
            .and_then(|c| c.rate.as_ref())
            .map_or_else(|| "?".to_string(), |r| usd(r.usd_per_hour, 4));
        let accrued = priced
            .and_then(|c| c.accrued_usd)
            .map_or_else(|| "?".to_string(), |v| usd(v, 2));
        rows.push(vec![
            format!("{}/{}", row.provider, row.env),
            row.id.clone(),
            opt(row.name.as_deref()),
            opt(row.state.as_deref()),
            opt(row.size.as_deref()),
            opt(row.region.as_deref()),
            age(row.created_ms, now_ms),
            rate,
            accrued,
            row.worktree.clone().unwrap_or_else(|| "ORPHAN".into()),
        ]);
    }
    let mut out = vec![if view.rows.is_empty() {
        "No provider sandboxes listed.".to_string()
    } else {
        table(&rows)
    }];
    if !view.missing.is_empty() {
        out.push(String::new());
        out.push("Recorded, not listed by the provider:".into());
        let mut missing = vec![
            ["PROVIDER/ENV", "ID", "WORKTREE"]
                .map(String::from)
                .to_vec(),
        ];
        for rec in &view.missing {
            missing.push(vec![
                format!("{}/{}", rec.provider, rec.env),
                rec.id.clone(),
                rec.worktree.clone(),
            ]);
        }
        out.push(table(&missing));
    }
    partial_section(view, &mut out);
    out.push(String::new());
    out.push(format!(
        "{} sandbox(es), {} orphan(s), {} missing",
        view.rows.len(),
        view.orphans,
        view.missing.len()
    ));
    out.push(footer(cost));
    out.join("\n")
}

/// The `fleet cost` summary (pure).
fn render_cost(view: &FleetView, cost: &FleetCost) -> String {
    let mut rows = vec![["PROVIDER", "$/H"].map(String::from).to_vec()];
    for (provider, hourly) in &cost.per_provider_hourly {
        rows.push(vec![provider.clone(), usd(*hourly, 4)]);
    }
    let mut out = vec![table(&rows)];
    partial_section(view, &mut out);
    out.push(String::new());
    out.push(footer(cost));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::placement::fleet_cost::{FleetRow, PartialEnv, PriceRow};

    const TABLE: &[PriceRow] = &[PriceRow {
        provider: "fly",
        size: "shared-cpu-1x:256MB",
        region: None,
        usd_per_hour: 0.5,
        source: "test",
    }];

    fn view() -> FleetView {
        FleetView {
            rows: vec![
                FleetRow {
                    provider: "fly".into(),
                    env: "fly-dev".into(),
                    id: "tg-a".into(),
                    name: Some("m1".into()),
                    state: Some("started".into()),
                    size: Some("shared-cpu-1x:256MB".into()),
                    region: Some("iad".into()),
                    created_ms: Some(0),
                    worktree: Some("/wt/a".into()),
                },
                FleetRow {
                    provider: "sprites".into(),
                    env: "sp".into(),
                    id: "tg-orphan-long".into(),
                    ..FleetRow::default()
                },
            ],
            orphans: 1,
            missing: vec![RecordedSandbox {
                worktree: "/wt/gone".into(),
                provider: "fly".into(),
                env: "fly-dev".into(),
                id: "tg-gone".into(),
            }],
            partial: vec![PartialEnv {
                env: "do".into(),
                reason: "unavailable: no token".into(),
            }],
        }
    }

    const NOW: i64 = 2 * 3_600_000 + 5 * 60_000;

    #[test]
    fn list_table_aligns_marks_orphans_and_unpriced() {
        let view = view();
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let text = render_list(&view, &cost, NOW);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("PROVIDER/ENV  ID"), "{text}");
        let id_col = lines[0].find("  ID").unwrap() + 2;
        assert_eq!(lines[1].find("tg-a"), Some(id_col), "{text}");
        assert_eq!(lines[2].find("tg-orphan-long"), Some(id_col), "{text}");
        let wt_col = lines[0].find("WORKTREE").unwrap();
        assert_eq!(lines[1].find("/wt/a"), Some(wt_col), "{text}");
        assert_eq!(lines[2].find("ORPHAN"), Some(wt_col), "{text}");
        assert!(
            lines[1].contains("$0.5000") && lines[1].contains("$1.04"),
            "{text}"
        );
        assert!(lines[1].contains("2h5m"), "{text}");
        assert!(lines[2].contains(" ? "), "unpriced shows ?: {text}");
        assert!(
            text.contains("Recorded, not listed by the provider:"),
            "{text}"
        );
        assert!(
            text.contains("tg-gone") && text.contains("/wt/gone"),
            "{text}"
        );
        assert!(text.contains("do: unavailable: no token"), "{text}");
        assert!(
            text.contains("2 sandbox(es), 1 orphan(s), 1 missing"),
            "{text}"
        );
        assert!(text.contains("1 unpriced"), "{text}");
        assert!(text.contains(fleet_cost::PRICE_TABLE_DATE), "{text}");
        assert!(text.lines().all(|line| line == line.trim_end()));
    }

    #[test]
    fn empty_list_and_cost_summary() {
        let empty = FleetView::default();
        let cost = fleet_cost::fleet_cost(&empty, TABLE, &Default::default(), NOW);
        assert!(render_list(&empty, &cost, NOW).starts_with("No provider sandboxes listed."));
        let view = view();
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let text = render_cost(&view, &cost);
        assert!(
            text.starts_with("PROVIDER  $/H\nfly       $0.5000"),
            "{text}"
        );
        assert!(text.contains("burn $0.5000/h"), "{text}");
        assert!(text.contains("next 24h $12.00"), "{text}");
    }

    #[test]
    fn ages() {
        assert_eq!(age(None, 0), "?");
        assert_eq!(age(Some(0), 42 * 60_000), "42m");
        assert_eq!(age(Some(0), NOW), "2h5m");
        assert_eq!(age(Some(0), 3 * 86_400_000 + 4 * 3_600_000), "3d4h");
        assert_eq!(age(Some(10), 0), "0m");
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
            Some(Ok(InventoryReport::Complete(vec![info.clone()]))),
        );
        assert_eq!(complete.completeness, Completeness::Complete);
        assert_eq!(complete.rows[0].provider, "fly");
        assert_eq!(complete.rows[0].env, "e");
        assert_eq!(complete.rows[0].size.as_deref(), Some("s"));
        let partial = env_inventory(
            "e",
            "fly",
            Some(Ok(InventoryReport::Partial {
                rows: vec![info],
                reason: "x".into(),
            })),
        );
        assert_eq!(
            partial.completeness,
            Completeness::Partial("partial: x".into())
        );
        assert_eq!(partial.rows.len(), 1);
        let failed = env_inventory("e", "fly", Some(Err("boom".into())));
        assert_eq!(
            failed.completeness,
            Completeness::Unavailable("unavailable: boom".into())
        );
        let unbuilt = env_inventory("e", "fly", None);
        assert!(
            matches!(unbuilt.completeness, Completeness::Unavailable(r) if r.contains("token"))
        );
    }
}
