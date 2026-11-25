//! `thegn env status [--watch] [--json]` — the live health of the current
//! worktree's remote sandbox, on any exec-capable provider (OpenSpec
//! `remote-sandbox-visibility`, slice 2).
//!
//! One report joins the provider's inventory row (state, created, size,
//! region), the lifetime left before `max_lifetime_secs`, the fleet price of
//! that one row, and the one-shot health probe from
//! `thegn_core::remote_health`. A sandbox the provider reports asleep is not
//! probed: a probe would wake and bill it. The same report is served read-only
//! over the control API (`env.status`, `GET /v1/env/status`) and MCP.
//!
//! Blocking: callers on an async runtime run [`control_report`] on a blocking
//! worker.

use anyhow::{Result, anyhow, bail};
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_core::placement::fleet_cost::{self, FleetRow, FleetView};
use thegn_core::remote_health::{self, HealthSample};
use thegn_svc::provider::{InventoryReport, SandboxInfo};

use super::fleet_inspect::{HealthReport, health_lines};

/// Default `--watch` refresh period.
const DEFAULT_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone, clap::Args)]
pub struct StatusArgs {
    /// Re-probe every `--interval` seconds and print restart / OOM events as
    /// they are classified (Ctrl-C to stop).
    #[arg(long)]
    pub watch: bool,
    /// Seconds between `--watch` refreshes.
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_INTERVAL_SECS, requires = "watch")]
    pub interval: u64,
    /// One JSON object per report (one line per refresh with `--watch`).
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub target: super::target::WorktreeTarget,
}

/// One sandbox's cost at "now" from the bundled price table and `[placement.price]`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CostView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_usd_per_hour: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accrued_usd: Option<f64>,
}

/// The `env status` report (also the `env.status` control payload).
#[derive(Debug, serde::Serialize)]
pub struct EnvStatusReport {
    pub generated_ms: i64,
    pub worktree: String,
    pub env: String,
    pub provider: String,
    pub sandbox_id: String,
    /// Provider state (`running`, `warm`, `cold`, `stopped`, …); `None` when
    /// the inventory could not be read or has no row for the sandbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostView>,
    /// Why the inventory row is missing (read failed, partial listing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_note: Option<String>,
    pub health: HealthReport,
}

/// Provider states that mean "not running": probing would wake (and bill) it.
pub(crate) fn is_asleep(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "cold" | "stopped" | "suspended" | "paused" | "exited" | "hibernating" | "sleeping"
    )
}

/// The fleet price of one inventory row at `now_ms`.
fn row_cost(cfg: &Config, row: FleetRow, now_ms: i64) -> Option<CostView> {
    let id = row.id.clone();
    let view = FleetView {
        rows: vec![row],
        ..FleetView::default()
    };
    let cost = fleet_cost::fleet_cost(
        &view,
        fleet_cost::BUNDLED_PRICES,
        &cfg.placement.price,
        now_ms,
    );
    cost.per_row
        .into_iter()
        .find(|r| r.id == id)
        .map(|r| CostView {
            burn_usd_per_hour: r.burn_usd_per_hour,
            accrued_usd: r.accrued_usd,
        })
}

/// Build the report for `worktree` (or the resolved current one). Reads the
/// provider inventory and runs one probe unless the sandbox is asleep.
pub(crate) fn build_report(cfg: &Config, worktree: Option<String>) -> Result<EnvStatusReport> {
    let env = super::env::resolve_for(cfg, worktree.clone())?;
    let wt = super::resolve_worktree(worktree)
        .to_string_lossy()
        .into_owned();
    let id = match &env.placement {
        thegn_core::placement::Placement::Provider(p) => p.id.trim().to_string(),
        _ => String::new(),
    };
    if id.is_empty() {
        bail!(
            "env {} has no remote sandbox for this worktree (placement {})",
            env.name,
            env.placement.label()
        );
    }
    let pc = &cfg
        .env
        .get(&env.name)
        .ok_or_else(|| anyhow!("env {} has no provider config", env.name))?
        .provider;
    let provider = crate::provider_factory::provider_for_inventory(pc)
        .map_err(|reason| anyhow!("env {}: {reason}", env.name))?;
    let provider_name = pc.provider.trim().to_string();
    let now_ms = thegn_core::util::now_ms();

    let (info, inventory_note) =
        match crate::agent::block_on_provider(|| async { provider.inventory().await }) {
            Ok(InventoryReport::Complete(rows)) => match find_row(rows, &id) {
                Some(row) => (Some(row), None),
                None => (
                    None,
                    Some("the provider does not list this sandbox".to_string()),
                ),
            },
            Ok(InventoryReport::Partial { rows, reason }) => match find_row(rows, &id) {
                Some(row) => (Some(row), None),
                None => (
                    None,
                    Some(format!("not in the (partial) listing: {reason}")),
                ),
            },
            Err(error) => (None, Some(format!("inventory read failed: {error:#}"))),
        };
    let lifetime_left = super::fleet_inspect::lifetime_left_from_ms(
        info.as_ref().and_then(|i| i.created_at_ms),
        pc.max_lifetime_secs,
        now_ms,
    );
    let state = info.as_ref().and_then(|i| i.state.clone());
    let health = match state.as_deref() {
        Some(s) if is_asleep(s) => super::fleet_inspect::unprobed_report(
            format!("not probed: the sandbox is {s} (a probe would wake and bill it)"),
            lifetime_left,
        ),
        _ => super::fleet_inspect::probe_provider(&provider, &id, lifetime_left, "sandbox"),
    };
    let cost = info.as_ref().and_then(|i| {
        row_cost(
            cfg,
            FleetRow {
                provider: provider_name.clone(),
                env: env.name.clone(),
                id: id.clone(),
                name: i.name.clone(),
                state: i.state.clone(),
                size: i.size.clone(),
                region: i.region.clone(),
                created_ms: i.created_at_ms,
                worktree: Some(wt.clone()),
                unrecorded: false,
            },
            now_ms,
        )
    });
    Ok(EnvStatusReport {
        generated_ms: now_ms,
        worktree: wt,
        env: env.name.clone(),
        provider: provider_name,
        sandbox_id: id,
        state,
        created_at_ms: info.as_ref().and_then(|i| i.created_at_ms),
        size: info.as_ref().and_then(|i| i.size.clone()),
        region: info.as_ref().and_then(|i| i.region.clone()),
        cost,
        inventory_note,
        health,
    })
}

fn find_row(rows: Vec<SandboxInfo>, id: &str) -> Option<SandboxInfo> {
    rows.into_iter().find(|row| row.id == id)
}

/// The control-API / MCP payload: the report as sanitized JSON.
pub(crate) fn control_report(cfg: &Config, worktree: &str) -> Result<serde_json::Value> {
    let report = build_report(cfg, Some(worktree.to_string()))?;
    let mut value = serde_json::to_value(report)?;
    super::fleet::sanitize_json(&mut value);
    Ok(value)
}

/// The human lines of one report.
pub(crate) fn report_lines(report: &EnvStatusReport) -> Vec<String> {
    let clean = |s: &str| super::fleet::sanitize_text(s, 128);
    let mut head = format!(
        "sandbox {}  env {}  provider {}  state {}",
        clean(&report.sandbox_id),
        clean(&report.env),
        clean(&report.provider),
        report
            .state
            .as_deref()
            .map(clean)
            .unwrap_or_else(|| "-".into()),
    );
    if let Some(size) = &report.size {
        head.push_str(&format!("  size {}", clean(size)));
    }
    if let Some(region) = &report.region {
        head.push_str(&format!("  region {}", clean(region)));
    }
    let mut lines = vec![head];
    if let Some(cost) = &report.cost {
        let burn = cost
            .burn_usd_per_hour
            .map(|v| format!("${v:.4}/h"))
            .unwrap_or_else(|| "unpriced".into());
        let accrued = cost
            .accrued_usd
            .map(|v| format!("  accrued ${v:.2}"))
            .unwrap_or_default();
        lines.push(format!("cost {burn}{accrued}"));
    }
    if let Some(note) = &report.inventory_note {
        lines.push(format!(
            "! inventory: {}",
            super::fleet::sanitize_text(note, 256)
        ));
    }
    lines.extend(health_lines(&report.health));
    lines
}

/// New restart / OOM events between the previous sample and this report's,
/// as warning lines; updates `prev` to this report's sample.
pub(crate) fn watch_events(
    prev: &mut Option<HealthSample>,
    report: &EnvStatusReport,
) -> Vec<String> {
    let Some(cur) = &report.health.sample else {
        return Vec::new();
    };
    let lines = remote_health::classify(prev.as_ref(), cur)
        .iter()
        .map(|event| remote_health::warning("sandbox", event))
        .collect();
    *prev = Some(cur.clone());
    lines
}

pub fn run(cfg: &Config, args: StatusArgs) -> Result<()> {
    let worktree = args.target.get();
    if !args.watch {
        let report = build_report(cfg, worktree)?;
        return print_report(&report, args.json);
    }
    let interval = std::time::Duration::from_secs(args.interval.max(1));
    let mut prev: Option<HealthSample> = None;
    loop {
        let report = build_report(cfg, worktree.clone())?;
        print_report(&report, args.json)?;
        if !args.json {
            for line in watch_events(&mut prev, &report) {
                outln!("! event: {}", super::fleet::sanitize_text(&line, 256));
            }
            outln!("");
        } else {
            let _ = watch_events(&mut prev, &report);
        }
        std::thread::sleep(interval);
    }
}

fn print_report(report: &EnvStatusReport, json: bool) -> Result<()> {
    if json {
        return super::emit_json(report);
    }
    outln!("{}", report_lines(report).join("\n"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(probe: &str) -> EnvStatusReport {
        let health = super::super::fleet_inspect::health_report_for_tests(probe);
        EnvStatusReport {
            generated_ms: 1,
            worktree: "/w".into(),
            env: "vx".into(),
            provider: "vercel".into(),
            sandbox_id: "sb1".into(),
            state: Some("running".into()),
            created_at_ms: Some(0),
            size: None,
            region: None,
            cost: Some(CostView {
                burn_usd_per_hour: Some(0.128),
                accrued_usd: Some(1.5),
            }),
            inventory_note: None,
            health,
        }
    }

    #[test]
    fn asleep_states_are_never_probed() {
        for s in ["cold", "Stopped", " suspended "] {
            assert!(is_asleep(s), "{s}");
        }
        for s in ["running", "warm", "started", ""] {
            assert!(!is_asleep(s), "{s}");
        }
    }

    #[test]
    fn report_lines_show_cost_state_and_health() {
        let r = report("thegn-health v1\nboot_id b1\nuptime 120\n");
        let lines = report_lines(&r);
        assert_eq!(
            lines[0],
            "sandbox sb1  env vx  provider vercel  state running"
        );
        assert_eq!(lines[1], "cost $0.1280/h  accrued $1.50");
        assert_eq!(lines[2], "health ok  uptime 2m  boot b1");
    }

    #[test]
    fn watch_reports_a_restart_once() {
        let mut prev = None;
        let first = report("thegn-health v1\nboot_id b1\nuptime 500\n");
        assert!(watch_events(&mut prev, &first).is_empty());
        assert!(watch_events(&mut prev, &first).is_empty(), "same boot");
        let second = report("thegn-health v1\nboot_id b2\nuptime 5\n");
        let events = watch_events(&mut prev, &second);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].contains("restarted"), "{events:?}");
        assert!(watch_events(&mut prev, &second).is_empty(), "reported once");
    }

    #[test]
    fn unprobed_reports_yield_no_watch_events() {
        let mut prev = Some(HealthSample::default());
        let mut r = report("thegn-health v1\n");
        r.health = super::super::fleet_inspect::unprobed_report("asleep".into(), None);
        assert!(watch_events(&mut prev, &r).is_empty());
        assert!(prev.is_some(), "the last real sample is kept");
    }

    #[test]
    fn clap_parses_status_flags() {
        use clap::Parser;
        let parsed = crate::Cli::try_parse_from([
            "thegn",
            "env",
            "status",
            "--watch",
            "--interval",
            "2",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(crate::Command::Env {
                action: crate::cmd::env::Action::Status(StatusArgs {
                    watch: true,
                    interval: 2,
                    json: true,
                    ..
                })
            })
        ));
        assert!(crate::Cli::try_parse_from(["thegn", "env", "status", "--interval", "2"]).is_err());
    }
}
