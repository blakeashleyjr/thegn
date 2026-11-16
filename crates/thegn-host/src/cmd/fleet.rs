//! `thegn fleet list|cost` — every provider sandbox across the configured
//! `[env.<name>]` entries, joined to the worktree that owns it, with an
//! estimated cost. Read-only: nothing is stopped or destroyed.
//!
//! The read itself lives in [`crate::fleet_read`] (shared with the masthead
//! `fleet` widget). Each env with an API provider is inventoried once (20 s
//! budget per env, all envs concurrently). An env whose provider cannot be built (its token
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
//! - `cost.budget[]`: `provider`, `level` (`"warn"` | `"max"`), `hourly`,
//!   `limit` — providers whose burn reached a `[placement.fleet_budget]`
//!   limit (empty under `fleet list --env`, which reads only one env).
//!
//! All amounts are USD estimates from the bundled price table and
//! `[placement.price]`.

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_core::placement::fleet_cost::{self, FleetCost, FleetView};

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
    let view = crate::fleet_read::gather(cfg, only_env.as_deref())?;
    let now_ms = thegn_core::util::now_ms();
    // Budgets compare a provider's WHOLE burn: a one-env read cannot judge
    // them, so `--env` reports none rather than an undercount.
    let cost = if only_env.is_none() {
        crate::fleet_read::priced(cfg, &view, now_ms)
    } else {
        fleet_cost::fleet_cost(
            &view,
            fleet_cost::BUNDLED_PRICES,
            &cfg.placement.price,
            now_ms,
        )
    };
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

fn usd(value: f64, decimals: usize) -> String {
    format!("${value:.decimals$}")
}

/// Compact age: `3d4h`, `5h12m`, `42m`, `?` when unknown. Shared with the
/// fleet popup (`detail/fleet.rs`).
pub(crate) fn age(created_ms: Option<i64>, now_ms: i64) -> String {
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
    for breach in &cost.budget {
        out.push(format!(
            "budget: {} {}/h ≥ {} {}/h",
            breach.provider,
            usd(breach.hourly, 2),
            breach.level.label(),
            usd(breach.limit, 2),
        ));
    }
    partial_section(view, &mut out);
    out.push(String::new());
    out.push(footer(cost));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::placement::fleet_cost::{FleetRow, PartialEnv, PriceRow, RecordedSandbox};

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
        assert!(
            !text.contains("budget:"),
            "no budget, no breach line: {text}"
        );
    }

    #[test]
    fn cost_summary_prints_one_line_per_breach() {
        use thegn_core::placement::fleet_cost::{BudgetBreach, BudgetLevel};
        let view = view();
        let mut cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let plain = render_cost(&view, &cost);
        cost.budget = vec![
            BudgetBreach {
                provider: "fly".into(),
                level: BudgetLevel::Warn,
                hourly: 3.2,
                limit: 3.0,
            },
            BudgetBreach {
                provider: "sprites".into(),
                level: BudgetLevel::Max,
                hourly: 5.0,
                limit: 5.0,
            },
        ];
        let text = render_cost(&view, &cost);
        let breaches: Vec<&str> = text.lines().filter(|l| l.starts_with("budget:")).collect();
        assert_eq!(
            breaches,
            [
                "budget: fly $3.20/h ≥ warn $3.00/h",
                "budget: sprites $5.00/h ≥ max $5.00/h"
            ],
            "{text}"
        );
        // Removing the breach lines leaves the existing output untouched.
        let without: Vec<&str> = text.lines().filter(|l| !l.starts_with("budget:")).collect();
        assert_eq!(without.join("\n"), plain);
        let json = serde_json::to_value(&cost).unwrap();
        assert_eq!(json["budget"][0]["level"], "warn");
        assert_eq!(json["budget"][1]["provider"], "sprites");
    }

    #[test]
    fn ages() {
        assert_eq!(age(None, 0), "?");
        assert_eq!(age(Some(0), 42 * 60_000), "42m");
        assert_eq!(age(Some(0), NOW), "2h5m");
        assert_eq!(age(Some(0), 3 * 86_400_000 + 4 * 3_600_000), "3d4h");
        assert_eq!(age(Some(10), 0), "0m");
    }
}
