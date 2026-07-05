//! `superzej placement <action>` — inspect the placement engine: the per-host
//! resource view (declared spec / reserved floors / measured sample — for
//! EVERY host, managed and independent), a pure dry-run of the broker's
//! decision for a worktree, and the recorded decision traces. Headless
//! counterpart of the (upcoming) Hosts-panel capacity rows; also the smoke
//! test's assertion surface.

use anyhow::Result;
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::outln;
use superzej_core::store::{HostStore, PlacementStore};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Per-host resource view: ownership, declared spec, reserved floors +
    /// tenants, measured load, engine lane.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Pure dry-run: what would the broker decide for this worktree right
    /// now, and why was each candidate (in)eligible? No reservation is made.
    Plan {
        /// Worktree path (defaults to the current directory).
        worktree: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Why the last spawn landed where it did (newest decision trace).
    Explain {
        /// Worktree path (defaults to the most recent decision overall).
        worktree: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Recent placement decisions.
    Events {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(cfg, json),
        Action::Plan { worktree, json } => plan(cfg, worktree, json),
        Action::Explain { worktree, json } => explain(worktree, json),
        Action::Events { limit } => events(limit),
    }
}

fn cwd_worktree(explicit: Option<String>) -> String {
    explicit.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}

/// One row of the unified resource view, JSON-shaped for scripting.
#[derive(serde::Serialize)]
struct HostView {
    host: String,
    ownership: String,
    state: String,
    /// Declared spec (`null` = unknown ⇒ dedicated-only, never packed).
    spec: Option<serde_json::Value>,
    reserved: serde_json::Value,
    tenants: Vec<String>,
    /// Latest observational sample (display + ranking hints).
    measured: Option<serde_json::Value>,
    /// Engine lane (`provider/template`) for engine-created hosts, else "".
    lane: String,
    /// Effective co-tenancy trust class (one notch down for unattested
    /// user-owned hosts; "unprobed" until a runtime probe ran).
    trust: String,
}

fn list(cfg: &Config, json: bool) -> Result<()> {
    let db = Db::open()?;
    let mut views = Vec::new();
    for (name, hc) in &cfg.host {
        if hc.reach == superzej_core::host_config::HostReach::Cloud {
            continue; // provider templates are spillover, not machines
        }
        let Some(binding) = cfg.host_binding(name) else {
            continue;
        };
        let id = binding.id.clone();
        let row = db.host_get(&id).ok().flatten();
        let cap = db.capacity_get(&id).ok().flatten();
        let reserved = db.reserved_totals(&id).unwrap_or_default();
        let tenants = db.tenants_of(&id).unwrap_or_default();
        let spec = cap.as_ref().and_then(|c| c.spec).or(binding.declared_spec);
        let ownership = cap
            .as_ref()
            .map(|c| c.ownership)
            .unwrap_or(superzej_core::capacity::HostOwnership::Independent);
        let trust = match row.as_ref().and_then(|r| r.caps.as_ref()) {
            Some(caps) => superzej_core::trust_class::effective_class(
                caps,
                ownership,
                hc.trust_egress_enforced,
            )
            .to_string(),
            None => "unprobed".to_string(),
        };
        views.push(HostView {
            host: id.to_string(),
            ownership: cap
                .as_ref()
                .map(|c| c.ownership.as_str())
                .unwrap_or("independent")
                .to_string(),
            state: row
                .as_ref()
                .and_then(|r| r.state.durable_tag())
                .unwrap_or("unknown")
                .to_string(),
            spec: spec.map(|s| serde_json::json!({ "cpu_milli": s.cpu_milli, "mem_mb": s.mem_mb })),
            reserved: serde_json::json!({
                "cpu_milli": reserved.cpu_milli,
                "mem_mb": reserved.mem_mb,
                "tenants": reserved.tenants,
            }),
            tenants: tenants
                .iter()
                .map(|t| format!("{} ({})", t.sandbox, t.mode.as_str()))
                .collect(),
            measured: cap.as_ref().and_then(|c| c.measured).map(|m| {
                serde_json::json!({
                    "cpu_milli": m.cpu_milli, "mem_mb": m.mem_mb, "at": m.at
                })
            }),
            trust,
            lane: cap
                .map(|c| {
                    if c.template.is_empty() {
                        String::new()
                    } else {
                        format!("{}/{}", c.provider, c.template)
                    }
                })
                .unwrap_or_default(),
        });
    }
    if json {
        outln!("{}", serde_json::to_string_pretty(&views)?);
        return Ok(());
    }
    if views.is_empty() {
        outln!("(no hosts — define [host.<name>] entries or enable autoscale)");
    }
    for v in views {
        let spec = v
            .spec
            .as_ref()
            .map(|s| {
                format!(
                    "{:.1} cpu / {} MiB",
                    s["cpu_milli"].as_u64().unwrap_or(0) as f64 / 1000.0,
                    s["mem_mb"].as_u64().unwrap_or(0)
                )
            })
            .unwrap_or_else(|| "unknown size".into());
        let lane = if v.lane.is_empty() {
            String::new()
        } else {
            format!("  [{}]", v.lane)
        };
        outln!(
            "{}  {}  {}  trust: {}  spec: {}  reserved: {:.1} cpu / {} MiB ({} tenant(s)){}",
            v.host,
            v.ownership,
            v.state,
            v.trust,
            spec,
            v.reserved["cpu_milli"].as_u64().unwrap_or(0) as f64 / 1000.0,
            v.reserved["mem_mb"].as_u64().unwrap_or(0),
            v.reserved["tenants"].as_u64().unwrap_or(0),
            lane,
        );
        for t in &v.tenants {
            outln!("    · {t}");
        }
        if let Some(m) = &v.measured {
            outln!(
                "    measured: {:.1} cpu / {} MiB (at {})",
                m["cpu_milli"].as_u64().unwrap_or(0) as f64 / 1000.0,
                m["mem_mb"].as_u64().unwrap_or(0),
                m["at"].as_i64().unwrap_or(0),
            );
        }
    }
    Ok(())
}

fn plan(cfg: &Config, worktree: Option<String>, json: bool) -> Result<()> {
    let wt = cwd_worktree(worktree);
    let out = crate::placement_flow::plan(cfg, &wt);
    if json {
        outln!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        match &out {
            Some(p) => {
                outln!("decision: {} → {}", p.decision, p.chosen);
                for c in &p.candidates {
                    outln!("  {}: {}", c.host, c.outcome);
                }
            }
            None => outln!("(placement engine off or not applicable for {wt})"),
        }
    }
    Ok(())
}

fn explain(worktree: Option<String>, json: bool) -> Result<()> {
    let db = Db::open()?;
    let events = db.placement_events(worktree.as_deref(), 1)?;
    let Some(e) = events.first() else {
        outln!("(no placement decisions recorded)");
        return Ok(());
    };
    if json {
        outln!(
            "{}",
            serde_json::json!({
                "ts": e.ts, "worktree": e.worktree, "decision": e.decision,
                "chosen": e.chosen,
                "candidates": serde_json::from_str::<serde_json::Value>(&e.trace_json)
                    .unwrap_or(serde_json::Value::Null),
            })
        );
        return Ok(());
    }
    outln!(
        "{}  {} → {}  ({})",
        e.worktree,
        e.decision,
        if e.chosen.is_empty() { "-" } else { &e.chosen },
        superzej_core::util::age(e.ts),
    );
    if let Ok(cands) = serde_json::from_str::<Vec<serde_json::Value>>(&e.trace_json) {
        for c in cands {
            outln!(
                "  {}: {}",
                c["host"].as_str().unwrap_or("?"),
                c["outcome"].as_str().unwrap_or("?")
            );
        }
    }
    Ok(())
}

fn events(limit: usize) -> Result<()> {
    let db = Db::open()?;
    let events = db.placement_events(None, limit)?;
    if events.is_empty() {
        outln!("(no placement decisions recorded)");
    }
    for e in events {
        outln!(
            "{}  {}  {} → {}",
            superzej_core::util::age(e.ts),
            e.worktree,
            e.decision,
            if e.chosen.is_empty() { "-" } else { &e.chosen },
        );
    }
    Ok(())
}
