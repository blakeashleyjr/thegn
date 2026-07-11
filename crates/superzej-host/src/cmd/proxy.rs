//! `superzej proxy <action>` — inspect and manage the LLM proxy (`szproxy`):
//! live status, the stats rollup (requests / tokens / cost / latency /
//! tokens-per-second), virtual keys (scoped accounts), budgets, and a
//! foreground `serve` for running the proxy standalone.
//!
//! Stats and budgets read the shared DB directly (the daemon writes audit rows
//! there), so they work whether or not a daemon is running; `status` probes the
//! daemon's HTTP surface at `[llm_proxy].listen`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::outln;
use superzej_core::proxy::stats::{NamedAgg, rollup};
use superzej_core::store::{ProxyStore, budget_period_len_ms};
use superzej_core::util;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Probe the running daemon: /health (backend cooldowns) + /resolved.
    Status,
    /// The stats rollup: requests, tokens, cost, latency percentiles, and
    /// tokens/sec — total and by backend / route / caller scope.
    Stats {
        /// Rollup window in seconds (default 24h).
        #[arg(long, default_value_t = 86_400)]
        since_secs: i64,
        /// Emit the raw JSON rollup instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Manage virtual keys (per-worktree/workspace/agent scoped accounts).
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Show or set per-scope budgets and the kill-switch.
    Budget {
        #[command(subcommand)]
        action: BudgetAction,
    },
    /// Run the proxy in the foreground (standalone service; ignores `enabled`).
    Serve,
}

#[derive(clap::Subcommand, Clone)]
pub enum KeysAction {
    /// List registered virtual keys (metadata only — never token hashes).
    List,
    /// Mint a virtual key for a scope (`worktree:<path>`, `workspace:<repo>`,
    /// `agent:<name>`, or `global`), printing the bearer token once.
    Mint {
        /// The budget/attribution scope the key resolves to.
        scope: String,
        /// Pin the key's traffic to this upstream provider's lanes.
        #[arg(long)]
        upstream: Option<String>,
        /// Human label (defaults to the scope).
        #[arg(long)]
        label: Option<String>,
    },
    /// Revoke a virtual key by id.
    Revoke { key_id: String },
}

#[derive(clap::Subcommand, Clone)]
pub enum BudgetAction {
    /// List all budget scopes with spend, caps, and window state.
    Show,
    /// Set a scope's caps + rolling window (anchor starts one period from now).
    Set {
        scope: String,
        /// Window: daily | weekly | monthly.
        #[arg(long, default_value = "monthly")]
        period: String,
        /// Token cap for the window (omit for no token cap).
        #[arg(long)]
        tokens: Option<i64>,
        /// USD cap for the window (omit for no cost cap).
        #[arg(long)]
        cost: Option<f64>,
    },
    /// Trip a scope's kill-switch (refuses all its requests).
    Kill {
        scope: String,
        /// Clear the kill-switch instead of setting it.
        #[arg(long)]
        clear: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Status => status(cfg),
        Action::Stats { since_secs, json } => stats(since_secs, json),
        Action::Keys { action } => keys(action),
        Action::Budget { action } => budget(action),
        Action::Serve => serve(cfg),
    }
}

fn base_url(cfg: &Config) -> String {
    format!("http://{}", cfg.llm_proxy.listen)
}

fn get_json(url: &str) -> Result<serde_json::Value> {
    let resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    Ok(resp.json()?)
}

fn status(cfg: &Config) -> Result<()> {
    let base = base_url(cfg);
    let health = match get_json(&format!("{base}/health")) {
        Ok(v) => v,
        Err(_) => {
            outln!("proxy: not running at {} ([llm_proxy].listen)", base);
            outln!("  start it with `superzej proxy serve` or `[llm_proxy] enabled = true`");
            return Ok(());
        }
    };
    outln!(
        "proxy: {} at {base}",
        health["status"].as_str().unwrap_or("?")
    );
    match health["backends"].as_object() {
        Some(b) if !b.is_empty() => {
            outln!("cooling backends:");
            for (ident, v) in b {
                outln!(
                    "  {ident}: {} ({})",
                    v["status"].as_str().unwrap_or("?"),
                    v["reason"].as_str().unwrap_or("")
                );
            }
        }
        _ => outln!("backends: all healthy"),
    }
    if let Ok(resolved) = get_json(&format!("{base}/resolved"))
        && let Some(map) = resolved.as_object()
        && !map.is_empty()
    {
        outln!("resolved routes:");
        for (route, backend) in map {
            outln!("  {route} → {}", backend.as_str().unwrap_or("?"));
        }
    }
    Ok(())
}

fn stats(since_secs: i64, json: bool) -> Result<()> {
    let db = Db::open()?;
    let since_ms = util::now() * 1000 - since_secs.max(0) * 1000;
    let rows = db.proxy_requests_since(since_ms, 10_000)?;
    let r = rollup(&rows);
    if json {
        outln!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    let window = if since_secs % 3600 == 0 {
        format!("{}h", since_secs / 3600)
    } else {
        format!("{since_secs}s")
    };
    outln!(
        "last {window}: {} requests ({} ok, {} failed)  in {} / out {} tokens  ${:.4}",
        r.totals.requests,
        r.totals.ok,
        r.totals.failed,
        r.totals.input_tokens,
        r.totals.output_tokens,
        r.totals.cost_usd
    );
    outln!(
        "latency p50 {}ms  p95 {}ms  ttfb {}ms  throughput {:.1} tok/s{}",
        r.totals.duration_p50_ms,
        r.totals.duration_p95_ms,
        r.totals.avg_ttfb_ms,
        r.totals.tokens_per_sec,
        r.totals
            .last_tokens_per_sec
            .map(|t| format!(" (last {t:.1})"))
            .unwrap_or_default()
    );
    print_aggs("by backend", &r.by_backend);
    print_aggs("by route", &r.by_route);
    print_aggs("by scope", &r.by_scope);
    Ok(())
}

fn print_aggs(title: &str, aggs: &[NamedAgg]) {
    if aggs.is_empty() {
        return;
    }
    outln!("{title}:");
    for n in aggs {
        outln!(
            "  {:<32} {:>6} req  {:>10} tok  ${:<10.4} {:>7.1} tok/s  p95 {}ms",
            n.name,
            n.agg.requests,
            n.agg.input_tokens + n.agg.output_tokens,
            n.agg.cost_usd,
            n.agg.tokens_per_sec,
            n.agg.duration_p95_ms
        );
    }
}

fn keys(action: KeysAction) -> Result<()> {
    let db = Db::open()?;
    match action {
        KeysAction::List => {
            let keys = db.proxy_virtual_keys_all()?;
            if keys.is_empty() {
                outln!("(no virtual keys)");
            }
            for k in keys {
                let state = if k.revoked_at.is_some() {
                    "revoked"
                } else {
                    "active"
                };
                outln!(
                    "{:<40} {:<8} scope={} upstream={} label={:?}",
                    k.key_id,
                    state,
                    k.scope,
                    k.upstream.as_deref().unwrap_or("-"),
                    k.label
                );
            }
        }
        KeysAction::Mint {
            scope,
            upstream,
            label,
        } => {
            validate_scope(&scope)?;
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let key = format!("szk-{}-{nanos:x}", util::slugify(&scope));
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut hasher);
            db.put_proxy_virtual_key(
                &key,
                &format!("{:016x}", hasher.finish()),
                label.as_deref().unwrap_or(&scope),
                &scope,
                upstream.as_deref(),
                util::now(),
            )?;
            outln!("{key}");
        }
        KeysAction::Revoke { key_id } => {
            db.revoke_proxy_virtual_key(&key_id, util::now())?;
            outln!("revoked {key_id}");
        }
    }
    Ok(())
}

fn validate_scope(scope: &str) -> Result<()> {
    let ok = scope == "global"
        || ["worktree:", "workspace:", "agent:", "zone:"]
            .iter()
            .any(|p| scope.starts_with(p) && scope.len() > p.len());
    if !ok {
        bail!(
            "invalid scope '{scope}' — use global, worktree:<path>, workspace:<repo>, \
             agent:<name>, or zone:<name>"
        );
    }
    Ok(())
}

fn budget(action: BudgetAction) -> Result<()> {
    let db = Db::open()?;
    match action {
        BudgetAction::Show => {
            let budgets = db.proxy_budgets_all()?;
            if budgets.is_empty() {
                outln!("(no budget scopes yet — spend creates them)");
            }
            for b in budgets {
                let caps = match (b.limit_tokens, b.limit_cost) {
                    (None, None) => "no caps".to_string(),
                    (t, c) => format!(
                        "caps: {} tok / {}",
                        t.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                        c.map(|v| format!("${v:.2}")).unwrap_or_else(|| "-".into())
                    ),
                };
                outln!(
                    "{:<40} {:<8} spent {} tok ${:.4}  {}{}",
                    b.scope,
                    b.period,
                    b.spent_tokens,
                    b.spent_cost,
                    caps,
                    if b.killed { "  [KILLED]" } else { "" }
                );
            }
        }
        BudgetAction::Set {
            scope,
            period,
            tokens,
            cost,
        } => {
            validate_scope(&scope)?;
            if !matches!(period.as_str(), "daily" | "weekly" | "monthly") {
                bail!("period must be daily, weekly, or monthly");
            }
            let reset_ms = util::now() * 1000 + budget_period_len_ms(&period);
            db.set_proxy_budget_limits(&scope, &period, tokens, cost, reset_ms)?;
            outln!("budget set for {scope} ({period})");
        }
        BudgetAction::Kill { scope, clear } => {
            db.set_proxy_kill_switch(&scope, !clear)?;
            outln!(
                "kill-switch {} for {scope}",
                if clear { "cleared" } else { "SET" }
            );
        }
    }
    Ok(())
}

/// Runs `szproxy` in the foreground with the resolved `[llm_proxy]` env —
/// standalone mode, independent of `enabled` (which only gates the host's
/// auto-launch). The daemon binary ships as a sibling of `szhost`.
// off-loop: a synchronous CLI subcommand — no compositor event loop exists here.
#[expect(clippy::disallowed_methods)]
fn serve(cfg: &Config) -> Result<()> {
    let mut proxy_cfg = cfg.llm_proxy.clone();
    proxy_cfg.enabled = true; // launch_spec gates on enabled; serve is explicit
    let (program, args, env) = proxy_cfg
        .launch_spec()
        .context("build szproxy launch spec")?;
    let bin = crate::proxy_daemon::resolve_binary(&program);
    let status = std::process::Command::new(&bin)
        .args(&args)
        .envs(&env)
        .status()
        .with_context(|| format!("run {}", bin.display()))?;
    if !status.success() {
        bail!("szproxy exited with {status}");
    }
    Ok(())
}
