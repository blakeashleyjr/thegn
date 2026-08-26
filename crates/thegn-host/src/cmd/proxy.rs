//! `thegn proxy <action>` — model-proxy control (THE-58).
//!
//! Operator-surface verbs projecting the `model_proxy.*` capability rows:
//! `status`/`stats` are read-scoped introspection, `start`/`stop` are
//! admin-scoped lifecycle. The hidden `serve` runs `tgproxy` in the foreground
//! (what the supervisor launches). Stats read the shared rollup straight from the
//! audit tables, so the CLI, `/stats`, and the usage panel all agree.

use anyhow::{Result, bail};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::proxy::stats;
use thegn_core::store::ModelProxyStore;
use thegn_core::{msg, outln};

use crate::cmd::emit_json;
use crate::model_proxy_daemon as daemon;

/// `thegn proxy` subcommands.
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Report the proxy: enabled, listen, reachability, providers, routes.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show the spend/token/latency stats rollup (from the audit tables).
    Stats {
        /// Rollup window in hours (default 24).
        #[arg(long, default_value_t = 24)]
        hours: i64,
        #[arg(long)]
        json: bool,
    },
    /// Launch the proxy daemon (no-op if already running).
    Start,
    /// Stop the proxy daemon gracefully.
    Stop,
    /// Run the proxy in the foreground (internal; used by the supervisor).
    #[command(hide = true)]
    Serve,
}

/// Dispatches a `thegn proxy` action.
pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Status { json } => status(cfg, json),
        Action::Stats { hours, json } => stats_cmd(cfg, hours, json),
        Action::Start => start(cfg),
        Action::Stop => stop(cfg),
        Action::Serve => serve(cfg),
    }
}

fn status(cfg: &Config, json: bool) -> Result<()> {
    let mp = &cfg.model_proxy;
    let up = mp.enabled && daemon::probe_up(mp);
    let providers: Vec<serde_json::Value> = mp
        .providers
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "kind": p.kind.as_str(),
                "reserved": thegn_core::seam::Kind::is_reserved(p.kind),
                "cost_bearing": p.is_cost_bearing(),
            })
        })
        .collect();
    if json {
        return emit_json(&serde_json::json!({
            "enabled": mp.enabled,
            "listen": mp.listen,
            "loopback": mp.listen_is_loopback(),
            "up": up,
            "routing": mp.routing.as_str(),
            "usage_aware": mp.usage_aware,
            "providers": providers,
            "routes": mp.routes.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
        }));
    }
    outln!("model proxy");
    outln!("  enabled   {}", mp.enabled);
    outln!(
        "  listen    {}{}",
        mp.listen,
        if mp.listen_is_loopback() {
            ""
        } else {
            "  (non-loopback!)"
        }
    );
    outln!(
        "  status    {}",
        if !mp.enabled {
            "disabled"
        } else if up {
            "up"
        } else {
            "down"
        }
    );
    outln!("  routing   {}", mp.routing.as_str());
    outln!("  providers {}", mp.providers.len());
    for p in &mp.providers {
        let reserved = if thegn_core::seam::Kind::is_reserved(p.kind) {
            "  (reserved)"
        } else {
            ""
        };
        outln!("    - {} [{}]{}", p.name, p.kind.as_str(), reserved);
    }
    outln!(
        "  routes    {}",
        mp.routes
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

fn stats_cmd(_cfg: &Config, hours: i64, json: bool) -> Result<()> {
    let db = Db::open()?;
    let since_ms = chrono::Utc::now().timestamp_millis() - hours.max(0) * 3_600_000;
    let rows = db.model_proxy_requests_since(since_ms, 100_000)?;
    let rollup = stats::rollup(&rows);
    if json {
        return emit_json(&rollup);
    }
    let t = &rollup.totals;
    outln!("model proxy — last {hours}h");
    outln!(
        "  requests   {} ({} ok, {} failed)",
        t.requests,
        t.ok,
        t.failed
    );
    outln!(
        "  tokens     {} in / {} out",
        t.input_tokens,
        t.output_tokens
    );
    if t.cache_read_tokens + t.cache_creation_tokens > 0 {
        outln!(
            "  cache      {} read / {} created",
            t.cache_read_tokens,
            t.cache_creation_tokens
        );
    }
    outln!("  spend      ${:.4}", t.cost_usd);
    outln!(
        "  latency    p50 {}ms  p95 {}ms",
        t.duration_p50_ms,
        t.duration_p95_ms
    );
    if t.tokens_per_sec > 0.0 {
        outln!("  throughput {:.1} tok/s", t.tokens_per_sec);
    }
    if !rollup.by_route.is_empty() {
        outln!("  by route:");
        for r in &rollup.by_route {
            outln!(
                "    {:<16} {} req  ${:.4}",
                r.name,
                r.agg.requests,
                r.agg.cost_usd
            );
        }
    }
    Ok(())
}

fn start(cfg: &Config) -> Result<()> {
    if !cfg.model_proxy.enabled {
        bail!("[model_proxy] is disabled; set enabled = true to start the proxy");
    }
    if daemon::ensure_running(&cfg.model_proxy)? {
        msg::info(&format!("model proxy up on {}", cfg.model_proxy.listen));
        Ok(())
    } else {
        bail!(
            "model proxy failed to come up on {}",
            cfg.model_proxy.listen
        )
    }
}

fn stop(cfg: &Config) -> Result<()> {
    if daemon::stop(&cfg.model_proxy)? {
        msg::info("model proxy stop signal sent");
    } else {
        msg::info("model proxy was not running");
    }
    Ok(())
}

fn serve(cfg: &Config) -> Result<()> {
    if !cfg.model_proxy.enabled {
        bail!("[model_proxy] is disabled");
    }
    let code = daemon::serve_foreground(&cfg.model_proxy)?;
    std::process::exit(code);
}
