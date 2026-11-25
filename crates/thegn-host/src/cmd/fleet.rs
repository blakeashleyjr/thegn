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
//!   `created_ms`, `worktree` (`null` = orphan: no worktree records it),
//!   `unrecorded` (`true` = an unowned account-wide sprites row).
//! - `view.orphans`: count of rows with no worktree that are not unrecorded.
//! - `view.missing[]`: `worktree`, `provider`, `env`, `id` — recorded by a
//!   worktree, not listed by its (completely read) provider.
//! - `view.down[]`: `worktree`, `provider`, `env`, `id` — recorded sandboxes
//!   not listed and not locally provisioned (or already taken down).
//! - `view.unrecorded`: count of rows marked `rows[].unrecorded`.
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
//! - `vercel_usage[]` (`fleet cost` only; absent when no `provider = "vercel"`
//!   env is configured): `envs`, `complete`, `error`, `rows[]` with `name`,
//!   `status`, `session_usd` (`null` = unpriced), `snapshots_usd_month`,
//!   `snapshot_bytes`. Current session only; an estimate. Envs sharing one
//!   Vercel account are one entry. Never resumes a stopped sandbox.
//!
//! - `sprites_usage[]` (`fleet cost` only; absent when no `provider = "sprites"`
//!   env is configured): `envs`, `error`, `rows[]` with `name`, `status`,
//!   `cpu_hours`, `gb_hours`, `partial` (`true` when the API's window is
//!   incomplete, or a sprite younger than the usage lag reports a zero),
//!   `recent`, `usd` (`null` = unpriced or not yet recorded; priced only when
//!   `[placement.price]` has a `sprites` entry), `reported`,
//!   `last_recorded_at`, and per-row `error`. Only sprites this host proves it
//!   owns (the ledger); usage comes from the Sprites API.
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
    /// One sprite's status, URL auth and (with --policy) memory policy, read
    /// from the Sprites API without exec (`--policy` may wake a cold sprite;
    /// `--probe` runs one health exec inside it).
    Inspect(super::fleet_inspect::InspectArgs),
}

#[derive(serde::Serialize)]
struct FleetReport {
    generated_ms: i64,
    view: FleetView,
    cost: FleetCost,
    #[serde(skip_serializing_if = "Option::is_none")]
    vercel_usage: Option<Vec<VercelUsage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sprites_usage: Option<Vec<SpritesUsage>>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    agents: std::collections::BTreeMap<String, FleetAgent>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct FleetAgent {
    agent: String,
    status: String,
    issue: String,
    stage: Option<String>,
}

/// The list capability shares the CLI report shape but never makes the
/// additional provider usage calls reserved for `fleet cost`.
pub(crate) fn control_report(cfg: &Config) -> Result<serde_json::Value> {
    let snapshot = crate::fleet_read::read(cfg)?;
    fleet_list_json(&snapshot, remote_agents(&snapshot.view))
}

/// The pure composition behind [`control_report`]: one snapshot's list
/// report (no usage rows), with the worktree-joined agent rows, sanitized
/// for the control response. No I/O — the daemon handler owns that.
fn fleet_list_json(
    snapshot: &crate::fleet_read::FleetSnapshot,
    agents: std::collections::BTreeMap<String, FleetAgent>,
) -> Result<serde_json::Value> {
    let report = FleetReport {
        generated_ms: snapshot.generated_ms,
        view: snapshot.view.clone(),
        cost: snapshot.cost.clone(),
        vercel_usage: None,
        sprites_usage: None,
        agents,
    };
    let mut value = serde_json::to_value(report)?;
    sanitize_json(&mut value);
    Ok(value)
}

pub(crate) fn sanitize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = sanitize_text(text, 4096),
        serde_json::Value::Array(items) => items.iter_mut().for_each(sanitize_json),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(sanitize_json),
        _ => {}
    }
}

/// One sprites env's per-sprite usage (`fleet cost` only).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct SpritesUsage {
    env: String,
    /// Why the env could not be read at all.
    error: Option<String>,
    /// Month-to-date usage of the whole Sprites account (`None` when this env
    /// shares its account with an earlier env, or the API could not be read).
    account: Option<SpritesAccountUsage>,
    /// The earlier env whose account row covers this one.
    account_shared_with: Option<String>,
    rows: Vec<SpritesUsageRow>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct SpritesAccountUsage {
    since: String,
    cpu_hours: f64,
    gb_hours: f64,
    partial: bool,
    reported: bool,
    error: Option<String>,
}

const SPRITES_USAGE_LAG_SECS: i64 = 3600; // UNVERIFIED: the Sprites usage lag.

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct SpritesUsageRow {
    name: String,
    status: String,
    cpu_hours: f64,
    gb_hours: f64,
    /// The figures are incomplete: the API said so, or the sprite is younger
    /// than the usage lag and its usage reads zero.
    partial: bool,
    /// The sprite is younger than SPRITES_USAGE_LAG_SECS: the API may not have reported it yet.
    recent: bool,
    reported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_recorded_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// `None` = unpriced, or usage not recorded yet.
    usd: Option<f64>,
    /// The API had no usage for this sprite (404).
    unavailable: bool,
}

fn unix_to_rfc3339(secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn usage_is_recent(created_at: i64, now_secs: i64) -> bool {
    now_secs.saturating_sub(created_at) < SPRITES_USAGE_LAG_SECS
}

/// A sprite younger than the usage lag whose reading is a reported zero: the
/// API has a bucket for it but recorded no usage yet, so the zero means "not
/// yet available", not a measurement. Any nonzero figure, or a sprite older
/// than the lag, is read as reported.
fn usage_is_pending(recent: bool, reported: bool, cpu_hours: f64, gb_hours: f64) -> bool {
    recent && reported && cpu_hours == 0.0 && gb_hours == 0.0
}

pub(crate) fn sanitize_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

/// The earlier env whose account row already covers `account`; records `env`
/// as that account's first env when it is new (and returns `None`).
fn account_shared_with(
    first_by_account: &mut std::collections::BTreeMap<String, String>,
    env: &str,
    account: &str,
) -> Option<String> {
    if let Some(first) = first_by_account.get(account) {
        return Some(first.clone());
    }
    first_by_account.insert(account.to_string(), env.to_string());
    None
}

/// The Sprites section's data. `None` when no sprites env is configured, so
/// the rest of `fleet cost` is byte-identical. Only the explicit verb calls
/// the API; only ledger-proven sprites are read.
fn sprites_usage(cfg: &Config) -> Option<Vec<SpritesUsage>> {
    let price = cfg
        .placement
        .price
        .get("sprites")
        .copied()
        .filter(|p| p.is_finite() && *p >= 0.0);
    let mut out = Vec::new();
    let now = thegn_core::util::now_ms() / 1000;
    let since = unix_to_rfc3339(thegn_svc::sprites::usage::month_start_utc(now));
    let mut account_first_env = std::collections::BTreeMap::<String, String>::new();
    for (name, envc) in &cfg.env {
        if envc.provider.provider.trim() != "sprites" {
            continue;
        }
        let mut usage = SpritesUsage {
            env: name.clone(),
            error: None,
            account: None,
            account_shared_with: None,
            rows: Vec::new(),
        };
        match crate::provider_factory::provider_for_named(&envc.provider, "") {
            Some(thegn_svc::provider::Provider::Sprites(provider)) => {
                match provider.scope().cloned() {
                    None => {
                        usage.error = Some("no ownership scope (controller id unavailable)".into())
                    }
                    Some(scope) => {
                        if let Some(first) =
                            account_shared_with(&mut account_first_env, name, &scope.account)
                        {
                            usage.account_shared_with = Some(first);
                        } else {
                            let mut account = SpritesAccountUsage {
                                since: since.clone(),
                                cpu_hours: 0.0,
                                gb_hours: 0.0,
                                partial: false,
                                reported: false,
                                error: None,
                            };
                            match crate::agent::block_on_provider(|| async {
                                provider.account_usage_since(&since).await
                            }) {
                                Ok(reading) => {
                                    account.cpu_hours = finite_cpu_hours(reading.usage.cpu_seconds);
                                    account.gb_hours =
                                        finite_gb_hours(reading.usage.reserved_gb_hours);
                                    account.partial = reading.usage.partial;
                                    account.reported = reading.reported;
                                }
                                Err(error) => {
                                    account.error = Some(sanitize_text(&format!("{error:#}"), 256));
                                }
                            }
                            usage.account = Some(account);
                        }
                        match thegn_svc::sprites::ledger::owned(&scope) {
                            Err(error) => {
                                usage.error = Some(sanitize_text(&format!("{error:#}"), 256))
                            }
                            Ok(view) => {
                                for rec in view.recorded {
                                    let start = unix_to_rfc3339(
                                        thegn_svc::sprites::usage::usage_window_start(
                                            rec.created_at,
                                        ),
                                    );
                                    let read = crate::agent::block_on_provider(|| async {
                                        provider.usage_reading_since(&rec.name, &start).await
                                    });
                                    usage.rows.push(match read {
                                        Ok(Some(reading)) => sprites_row(
                                            &rec.name,
                                            &rec.state,
                                            &reading,
                                            price,
                                            usage_is_recent(rec.created_at, now),
                                        ),
                                        Ok(None) => SpritesUsageRow {
                                            name: rec.name.clone(),
                                            status: rec.state.clone(),
                                            cpu_hours: 0.0,
                                            gb_hours: 0.0,
                                            partial: false,
                                            recent: usage_is_recent(rec.created_at, now),
                                            reported: false,
                                            last_recorded_at: None,
                                            error: None,
                                            usd: None,
                                            unavailable: true,
                                        },
                                        Err(error) => SpritesUsageRow {
                                            name: rec.name.clone(),
                                            status: rec.state.clone(),
                                            cpu_hours: 0.0,
                                            gb_hours: 0.0,
                                            partial: false,
                                            recent: usage_is_recent(rec.created_at, now),
                                            reported: false,
                                            last_recorded_at: None,
                                            error: Some(sanitize_text(&format!("{error:#}"), 256)),
                                            usd: None,
                                            unavailable: false,
                                        },
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                usage.error =
                    Some("the sprites provider could not be built (token unavailable)".into())
            }
        }
        out.push(usage);
    }
    (!out.is_empty()).then_some(out)
}

fn finite_cpu_hours(cpu_seconds: f64) -> f64 {
    let cpu_hours = cpu_seconds / 3600.0;
    if cpu_hours.is_finite() {
        cpu_hours
    } else {
        0.0
    }
}

fn finite_gb_hours(gb_hours: f64) -> f64 {
    if gb_hours.is_finite() { gb_hours } else { 0.0 }
}

fn sprites_row(
    name: &str,
    status: &str,
    reading: &thegn_svc::sprites::usage::UsageReading,
    price: Option<f64>,
    recent: bool,
) -> SpritesUsageRow {
    let u = &reading.usage;
    let cpu_hours = finite_cpu_hours(u.cpu_seconds);
    let gb_hours = finite_gb_hours(u.reserved_gb_hours);
    let pending = usage_is_pending(recent, reading.reported, cpu_hours, gb_hours);
    SpritesUsageRow {
        name: name.to_string(),
        status: status.to_string(),
        cpu_hours,
        gb_hours,
        partial: u.partial || pending,
        recent,
        reported: reading.reported,
        last_recorded_at: reading
            .last_recorded_at
            .as_deref()
            .map(|value| sanitize_text(value, 40)),
        error: None,
        usd: if reading.reported && !pending {
            price.map(|p| cpu_hours * p).filter(|v| v.is_finite())
        } else {
            None
        },
        unavailable: false,
    }
}

/// The `fleet cost` Sprites block (pure). Appended after [`render_cost`].
fn render_sprites(usage: &[SpritesUsage]) -> String {
    let mut out = Vec::new();
    for env in usage {
        out.push(format!(
            "sprites (usage from the Sprites API; unpriced): {}",
            env.env
        ));
        if let Some(shared_with) = &env.account_shared_with {
            out.push(format!("  account: same account as {shared_with}"));
        } else if let Some(account) = &env.account {
            if let Some(error) = &account.error {
                out.push(format!(
                    "  account usage unavailable: {}",
                    sanitize_text(error, 256)
                ));
            } else if !account.reported {
                out.push("  account: no usage reported yet".into());
            } else {
                out.push(format!(
                    "  account (all sprites, since {}): CPU {:.2} h, {:.2} GB h{}",
                    account.since,
                    account.cpu_hours,
                    account.gb_hours,
                    if account.partial { " (partial)" } else { "" },
                ));
            }
        }
        if let Some(error) = &env.error {
            out.push(format!("  unavailable: {}", sanitize_text(error, 256)));
            continue;
        }
        if env.rows.is_empty() {
            out.push("  no sandboxes recorded by this host".into());
            continue;
        }
        let mut rows = vec![
            ["NAME", "STATUS", "CPU H", "GB H", "PARTIAL", "USD"]
                .map(String::from)
                .to_vec(),
        ];
        for row in &env.rows {
            let pending = usage_is_pending(row.recent, row.reported, row.cpu_hours, row.gb_hours);
            let (cpu, gb) = if row.unavailable || row.error.is_some() {
                ("?".to_string(), "?".to_string())
            } else if !row.reported || pending {
                ("-".to_string(), "-".to_string())
            } else {
                (
                    format!("{:.2}", row.cpu_hours),
                    format!("{:.2}", row.gb_hours),
                )
            };
            rows.push(vec![
                row.name.clone(),
                row.status.clone(),
                cpu,
                gb,
                if row.partial {
                    "yes"
                } else if row.recent {
                    "recent"
                } else {
                    "no"
                }
                .to_string(),
                row.usd.map_or_else(|| "-".to_string(), |v| usd(v, 4)),
            ]);
        }
        for line in table(&rows).lines() {
            out.push(format!("  {line}"));
        }
        let unreported: Vec<&str> = env
            .rows
            .iter()
            .filter(|row| !row.reported && !row.unavailable && row.error.is_none())
            .map(|row| row.name.as_str())
            .collect();
        if !unreported.is_empty() {
            out.push(format!(
                "  no usage reported yet: {} (Sprites records usage with a delay; run thegn fleet cost again later)",
                unreported.join(", ")
            ));
        }
        let pending: Vec<&str> = env
            .rows
            .iter()
            .filter(|row| usage_is_pending(row.recent, row.reported, row.cpu_hours, row.gb_hours))
            .map(|row| row.name.as_str())
            .collect();
        if !pending.is_empty() {
            out.push(format!(
                "  usage not recorded yet: {} (the sprite is younger than the usage lag; run thegn fleet cost again later)",
                pending.join(", ")
            ));
        }
        for row in &env.rows {
            if let Some(error) = &row.error {
                out.push(format!(
                    "  usage error: {}: {}",
                    row.name,
                    sanitize_text(error, 256)
                ));
            }
        }
        if let Some(latest) = env
            .rows
            .iter()
            .filter_map(|row| row.last_recorded_at.as_deref())
            .map(|value| sanitize_text(value, 40))
            .max()
        {
            out.push(format!("  latest usage record: {latest}"));
        }
    }
    out.join("\n")
}

/// One Vercel account's current-session accrual (`fleet cost` only).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct VercelUsage {
    envs: Vec<String>,
    /// The listing, the ledger and every per-sandbox read succeeded.
    complete: bool,
    /// Why the account could not be read at all.
    error: Option<String>,
    rows: Vec<VercelUsageRow>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct VercelUsageRow {
    name: String,
    status: String,
    session_usd: Option<f64>,
    snapshots_usd_month: f64,
    snapshot_bytes: u64,
}

impl From<thegn_svc::vercel::usage::UsageRow> for VercelUsageRow {
    fn from(row: thegn_svc::vercel::usage::UsageRow) -> Self {
        VercelUsageRow {
            session_usd: row.session.map(|a| a.cpu_usd + a.memory_usd),
            name: row.name,
            status: row.status,
            snapshots_usd_month: row.snapshots_usd_month,
            snapshot_bytes: row.snapshot_bytes,
        }
    }
}

/// The Vercel section's data: one entry per account (envs sharing an account
/// fingerprint are read once). `None` when no vercel env is configured, so the
/// rest of `fleet cost` is byte-identical. Reads only; never resumes.
fn vercel_usage(cfg: &Config) -> Option<Vec<VercelUsage>> {
    let mut groups: Vec<(
        String,
        VercelUsage,
        Option<thegn_svc::vercel::VercelProvider>,
    )> = Vec::new();
    for (name, envc) in &cfg.env {
        if envc.provider.provider.trim() != "vercel" {
            continue;
        }
        let provider = crate::provider_factory::vercel_provider_for(&envc.provider, "");
        let key = match provider.as_ref().map(|p| p.account()) {
            Some(Ok(account)) => format!("account:{account}"),
            _ => format!("env:{name}"),
        };
        if let Some((_, usage, _)) = groups.iter_mut().find(|(k, _, _)| *k == key) {
            usage.envs.push(name.clone());
            continue;
        }
        let usage = VercelUsage {
            envs: vec![name.clone()],
            complete: false,
            error: None,
            rows: Vec::new(),
        };
        groups.push((key, usage, provider));
    }
    if groups.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(groups.len());
    for (_, mut usage, provider) in groups {
        match provider {
            None => {
                usage.error = Some(
                    "the vercel provider could not be built (token or scope unavailable)".into(),
                );
            }
            Some(provider) => {
                match crate::agent::block_on_provider(|| async { provider.usage_report().await }) {
                    Ok((rows, complete)) => {
                        usage.complete = complete;
                        usage.rows = rows.into_iter().map(VercelUsageRow::from).collect();
                    }
                    Err(error) => usage.error = Some(format!("{error:#}")),
                }
            }
        }
        out.push(usage);
    }
    Some(out)
}

/// The `fleet cost` Vercel block (pure). Appended after [`render_cost`].
fn render_vercel(usage: &[VercelUsage]) -> String {
    let mut out = Vec::new();
    for account in usage {
        out.push(format!(
            "vercel (current session; estimate): {}",
            account.envs.join(", ")
        ));
        if let Some(error) = &account.error {
            out.push(format!("  unavailable: {error}"));
            continue;
        }
        if account.rows.is_empty() {
            out.push("  no sandboxes".into());
        } else {
            let mut rows = vec![
                ["NAME", "STATUS", "SESSION $", "SNAPSHOTS $/MONTH"]
                    .map(String::from)
                    .to_vec(),
            ];
            for row in &account.rows {
                rows.push(vec![
                    row.name.clone(),
                    row.status.clone(),
                    row.session_usd
                        .map_or_else(|| "?".to_string(), |v| usd(v, 4)),
                    usd(row.snapshots_usd_month, 2),
                ]);
            }
            for line in table(&rows).lines() {
                out.push(format!("  {line}"));
            }
        }
        if !account.complete {
            out.push("  incomplete: some sandboxes could not be read".into());
        }
    }
    out.push(
        "earlier sessions are not included; the vercel $/H above is the if-running-flat-out bound"
            .into(),
    );
    out.join("\n")
}

fn remote_agents_with(
    view: &FleetView,
    lookup: &dyn Fn(&str) -> Option<thegn_core::issue::AgentDispatch>,
) -> std::collections::BTreeMap<String, FleetAgent> {
    let mut agents = std::collections::BTreeMap::new();
    for row in &view.rows {
        let Some(worktree) = row.worktree.as_deref() else {
            continue;
        };
        let Some(dispatch) = lookup(worktree) else {
            continue;
        };
        agents.insert(
            sanitize_text(worktree, 4096),
            FleetAgent {
                agent: sanitize_text(&dispatch.agent_name, 32),
                status: sanitize_text(dispatch.status.as_str(), 32),
                issue: sanitize_text(&dispatch.issue_id, 32),
                stage: dispatch
                    .stage
                    .as_deref()
                    .map(|stage| sanitize_text(stage, 32)),
            },
        );
    }
    agents
}

fn remote_agents(view: &FleetView) -> std::collections::BTreeMap<String, FleetAgent> {
    match thegn_core::db::Db::open_read_only_current() {
        Ok(Some(db)) => remote_agents_with(view, &|worktree| {
            db.latest_active_dispatch_for_worktree(worktree)
                .ok()
                .flatten()
        }),
        Ok(None) => {
            tracing::debug!("fleet list: state database is unavailable for remote agents");
            std::collections::BTreeMap::new()
        }
        Err(error) => {
            tracing::debug!(%error, "fleet list: could not open state database for remote agents");
            std::collections::BTreeMap::new()
        }
    }
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    if let Action::Inspect(args) = &action {
        return super::fleet_inspect::run(cfg, args.clone());
    }
    let (only_env, json, list) = match action {
        Action::List { env, json } => (env, json, true),
        Action::Cost { json } => (None, json, false),
        Action::Inspect(_) => unreachable!(),
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
    let vercel = if list { None } else { vercel_usage(cfg) };
    let sprites = if list { None } else { sprites_usage(cfg) };
    let agents = if list {
        remote_agents(&view)
    } else {
        std::collections::BTreeMap::new()
    };
    if json {
        return super::emit_json(&FleetReport {
            generated_ms: now_ms,
            view,
            cost,
            vercel_usage: vercel,
            sprites_usage: sprites,
            agents,
        });
    }
    let text = if list {
        render_list(&view, &cost, now_ms, &agents)
    } else {
        let text = render_cost(&view, &cost);
        let text = match vercel.as_deref() {
            Some(usage) => format!("{text}\n\n{}", render_vercel(usage)),
            None => text,
        };
        match sprites.as_deref() {
            Some(usage) => format!("{text}\n\n{}", render_sprites(usage)),
            None => text,
        }
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
pub(crate) fn table(rows: &[Vec<String>]) -> String {
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
fn render_list(
    view: &FleetView,
    cost: &FleetCost,
    now_ms: i64,
    agents: &std::collections::BTreeMap<String, FleetAgent>,
) -> String {
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
            row.worktree.clone().unwrap_or_else(|| {
                if row.unrecorded {
                    "UNRECORDED".into()
                } else {
                    "ORPHAN".into()
                }
            }),
        ]);
    }
    let mut out = vec![if view.rows.is_empty() && !view.partial.is_empty() {
        "No provider sandboxes confirmed; inventory is incomplete.".to_string()
    } else if view.rows.is_empty() {
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
    if !view.down.is_empty() {
        out.push(String::new());
        out.push("not running (never provisioned, or env down)".into());
        let mut down = vec![
            ["WORKTREE", "PROVIDER", "ENV", "ID"]
                .map(String::from)
                .to_vec(),
        ];
        for rec in &view.down {
            down.push(vec![
                rec.worktree.clone(),
                rec.provider.clone(),
                rec.env.clone(),
                rec.id.clone(),
            ]);
        }
        out.push(table(&down));
    }
    partial_section(view, &mut out);
    out.push(String::new());
    out.push(format!(
        "{} sandbox(es), {} orphan(s), {} missing, {} down, {} unrecorded",
        view.rows.len(),
        view.orphans,
        view.missing.len(),
        view.down.len(),
        view.unrecorded,
    ));
    if view.unrecorded > 0 {
        out.push(
            "review unrecorded sprites: thegn env reap --provider sprites --unrecorded".into(),
        );
    }
    if !agents.is_empty() {
        out.push(String::new());
        out.push("remote agents:".into());
        let mut agent_rows = vec![
            ["WORKTREE", "AGENT", "STATUS", "ISSUE"]
                .map(String::from)
                .to_vec(),
        ];
        for (worktree, agent) in agents {
            agent_rows.push(vec![
                worktree.clone(),
                agent.agent.clone(),
                agent.status.clone(),
                agent.issue.clone(),
            ]);
        }
        out.push(table(&agent_rows));
    }
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
                    unrecorded: false,
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
            ..FleetView::default()
        }
    }

    const NOW: i64 = 2 * 3_600_000 + 5 * 60_000;

    #[test]
    fn usage_is_recent_window() {
        let now = 10_000;
        assert!(usage_is_recent(now - SPRITES_USAGE_LAG_SECS + 1, now));
        assert!(!usage_is_recent(now - SPRITES_USAGE_LAG_SECS, now));
        assert!(!usage_is_recent(now - SPRITES_USAGE_LAG_SECS - 1, now));
    }

    #[test]
    fn usage_is_pending_only_for_a_fresh_reported_zero() {
        assert!(usage_is_pending(true, true, 0.0, 0.0));
        assert!(
            !usage_is_pending(false, true, 0.0, 0.0),
            "an old zero is a measurement"
        );
        assert!(
            !usage_is_pending(true, false, 0.0, 0.0),
            "an unreported row is already marked unavailable"
        );
        assert!(!usage_is_pending(true, true, 1.5, 0.0));
        assert!(!usage_is_pending(true, true, 0.0, 2.0));
    }

    #[test]
    fn list_table_aligns_marks_orphans_and_unpriced() {
        let view = view();
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let text = render_list(&view, &cost, NOW, &Default::default());
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
            text.contains("2 sandbox(es), 1 orphan(s), 1 missing, 0 down, 0 unrecorded"),
            "{text}"
        );
        assert!(text.contains("1 unpriced"), "{text}");
        assert!(text.contains(fleet_cost::PRICE_TABLE_DATE), "{text}");
        assert!(text.lines().all(|line| line == line.trim_end()));
    }

    #[test]
    fn render_lists_down_and_unrecorded_with_review_hint() {
        let mut view = view();
        view.rows.push(FleetRow {
            provider: "sprites".into(),
            env: "sp".into(),
            id: "tg-unrecorded".into(),
            unrecorded: true,
            ..FleetRow::default()
        });
        view.unrecorded = 1;
        view.down.push(RecordedSandbox {
            worktree: "/wt/down".into(),
            provider: "sprites".into(),
            env: "sp".into(),
            id: "tg-down".into(),
        });
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let text = render_list(&view, &cost, NOW, &Default::default());
        assert!(text.contains("UNRECORDED"), "{text}");
        assert!(
            text.contains("not running (never provisioned, or env down)"),
            "{text}"
        );
        assert!(text.contains("WORKTREE  PROVIDER  ENV  ID"), "{text}");
        assert!(text.contains("/wt/down  sprites   sp   tg-down"), "{text}");
        assert!(
            text.contains("3 sandbox(es), 1 orphan(s), 1 missing, 1 down, 1 unrecorded"),
            "{text}"
        );
        assert!(
            text.contains(
                "review unrecorded sprites: thegn env reap --provider sprites --unrecorded"
            ),
            "{text}"
        );
    }

    #[test]
    fn list_marks_an_empty_unavailable_inventory_as_incomplete() {
        let view = FleetView {
            partial: vec![PartialEnv {
                env: "vps".into(),
                reason: "unavailable: timeout".into(),
            }],
            ..FleetView::default()
        };
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let text = render_list(&view, &cost, NOW, &Default::default());
        assert!(
            text.contains("No provider sandboxes confirmed; inventory is incomplete."),
            "{text}"
        );
        assert!(!text.contains("No provider sandboxes listed."), "{text}");
        assert!(text.contains("vps: unavailable: timeout"), "{text}");
        assert!(text.contains("0 sandbox(es)"), "{text}");
    }

    #[test]
    fn empty_list_and_cost_summary() {
        let empty = FleetView::default();
        let cost = fleet_cost::fleet_cost(&empty, TABLE, &Default::default(), NOW);
        assert!(
            render_list(&empty, &cost, NOW, &Default::default())
                .starts_with("No provider sandboxes listed.")
        );
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

    fn vercel_fixture() -> Vec<VercelUsage> {
        vec![
            VercelUsage {
                envs: vec!["vx".into(), "vx-alt".into()],
                complete: true,
                error: None,
                rows: vec![
                    VercelUsageRow {
                        name: "tg-run".into(),
                        status: "running".into(),
                        session_usd: Some(0.0425),
                        snapshots_usd_month: 0.16,
                        snapshot_bytes: 2_000_000_000,
                    },
                    VercelUsageRow {
                        name: "tg-gone-long".into(),
                        status: "gone".into(),
                        session_usd: None,
                        snapshots_usd_month: 0.0,
                        snapshot_bytes: 0,
                    },
                ],
            },
            VercelUsage {
                envs: vec!["vbroken".into()],
                complete: false,
                error: Some("no token".into()),
                rows: Vec::new(),
            },
        ]
    }

    #[test]
    fn vercel_cost_block_renders_rows_and_unavailable_accounts() {
        let text = render_vercel(&vercel_fixture());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0], "vercel (current session; estimate): vx, vx-alt",
            "{text}"
        );
        assert!(lines[1].starts_with("  NAME"), "{text}");
        let session_col = lines[1].find("SESSION $").unwrap();
        assert_eq!(lines[2].find("$0.0425"), Some(session_col), "{text}");
        assert!(
            lines[2].contains("$0.16") && lines[2].contains("running"),
            "{text}"
        );
        assert_eq!(lines[3].find("?"), Some(session_col), "unpriced: {text}");
        assert_eq!(
            lines[4], "vercel (current session; estimate): vbroken",
            "{text}"
        );
        assert_eq!(lines[5], "  unavailable: no token", "{text}");
        assert!(text.contains("earlier sessions are not included"), "{text}");
        assert!(text.lines().all(|line| line == line.trim_end()));
    }

    #[test]
    fn no_vercel_env_keeps_the_report_byte_identical() {
        let cfg = Config::default();
        assert!(vercel_usage(&cfg).is_none());
        let view = view();
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let json = serde_json::to_value(FleetReport {
            generated_ms: NOW,
            view: view.clone(),
            cost: cost.clone(),
            vercel_usage: None,
            sprites_usage: None,
            agents: Default::default(),
        })
        .unwrap();
        assert!(json.get("vercel_usage").is_none(), "{json}");
        let with = serde_json::to_value(FleetReport {
            generated_ms: NOW,
            view,
            cost,
            vercel_usage: Some(vercel_fixture()),
            sprites_usage: None,
            agents: Default::default(),
        })
        .unwrap();
        assert_eq!(with["vercel_usage"][0]["rows"][0]["name"], "tg-run");
        assert_eq!(
            with["vercel_usage"][0]["rows"][1]["session_usd"],
            serde_json::Value::Null
        );
        assert_eq!(with["vercel_usage"][1]["error"], "no token");
    }

    fn sprites_fixture() -> Vec<SpritesUsage> {
        vec![SpritesUsage {
            env: "sp".into(),
            error: None,
            account: None,
            account_shared_with: None,
            rows: vec![
                sprites_row(
                    "tg-a",
                    "ready",
                    &thegn_svc::sprites::usage::UsageReading {
                        usage: thegn_svc::sprites::usage::SpriteUsage {
                            cpu_seconds: 5400.0,
                            reserved_gb_hours: 3.0,
                            partial: true,
                        },
                        last_recorded_at: Some("2026-01-01T02:00:00Z".into()),
                        reported: true,
                    },
                    None,
                    false,
                ),
                sprites_row(
                    "tg-b",
                    "ready",
                    &thegn_svc::sprites::usage::UsageReading {
                        usage: thegn_svc::sprites::usage::SpriteUsage {
                            cpu_seconds: 3600.0,
                            reserved_gb_hours: 1.0,
                            partial: false,
                        },
                        last_recorded_at: Some("2026-01-01T02:00:00Z".into()),
                        reported: true,
                    },
                    Some(0.5),
                    false,
                ),
            ],
        }]
    }

    #[test]
    fn sprites_cost_block_renders_usage_and_price() {
        let text = render_sprites(&sprites_fixture());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0], "sprites (usage from the Sprites API; unpriced): sp",
            "{text}"
        );
        assert!(lines[1].starts_with("  NAME"), "{text}");
        assert!(
            lines[2].contains("1.50") && lines[2].contains("3.00") && lines[2].contains("yes"),
            "{text}"
        );
        assert!(lines[3].contains("$0.5000"), "{text}");
        assert!(!text.contains("NaN"), "{text}");
    }

    #[test]
    fn render_sprites_prints_the_account_row() {
        let usage = vec![SpritesUsage {
            env: "sp".into(),
            error: None,
            account: Some(SpritesAccountUsage {
                since: "2026-01-01T00:00:00Z".into(),
                cpu_hours: 12.5,
                gb_hours: 6.25,
                partial: false,
                reported: true,
                error: None,
            }),
            account_shared_with: None,
            rows: Vec::new(),
        }];

        let text = render_sprites(&usage);
        assert!(
            text.contains(
                "account (all sprites, since 2026-01-01T00:00:00Z): CPU 12.50 h, 6.25 GB h"
            ),
            "{text}"
        );
    }

    #[test]
    fn render_sprites_prints_partial_account_row() {
        let usage = vec![SpritesUsage {
            env: "sp".into(),
            error: None,
            account: Some(SpritesAccountUsage {
                since: "2026-01-01T00:00:00Z".into(),
                cpu_hours: 1.0,
                gb_hours: 2.0,
                partial: true,
                reported: true,
                error: None,
            }),
            account_shared_with: None,
            rows: Vec::new(),
        }];

        assert!(render_sprites(&usage).contains("GB h (partial)"));
    }

    #[test]
    fn render_sprites_prints_unreported_account_row() {
        let usage = vec![SpritesUsage {
            env: "sp".into(),
            error: None,
            account: Some(SpritesAccountUsage {
                since: "2026-01-01T00:00:00Z".into(),
                cpu_hours: 0.0,
                gb_hours: 0.0,
                partial: false,
                reported: false,
                error: None,
            }),
            account_shared_with: None,
            rows: Vec::new(),
        }];

        assert!(render_sprites(&usage).contains("account: no usage reported yet"));
    }

    #[test]
    fn two_envs_on_one_account_read_it_once() {
        let mut first = std::collections::BTreeMap::new();
        assert_eq!(account_shared_with(&mut first, "sp", "acct-a"), None);
        assert_eq!(
            account_shared_with(&mut first, "sp-alt", "acct-a"),
            Some("sp".to_string())
        );
        assert_eq!(account_shared_with(&mut first, "other", "acct-b"), None);
    }

    fn zero_reading(reported: bool) -> thegn_svc::sprites::usage::UsageReading {
        thegn_svc::sprites::usage::UsageReading {
            usage: thegn_svc::sprites::usage::SpriteUsage {
                cpu_seconds: 0.0,
                reserved_gb_hours: 0.0,
                partial: false,
            },
            last_recorded_at: Some("2026-01-01T00:05:00Z".into()),
            reported,
        }
    }

    #[test]
    fn a_fresh_reported_zero_reads_as_partial_not_zero() {
        let row = sprites_row("tg-new", "ready", &zero_reading(true), Some(0.5), true);

        assert!(row.partial, "a fresh zero is not a measurement");
        assert_eq!(row.usd, None, "an unrecorded figure is not priced");
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["partial"], true);
        assert_eq!(json["reported"], true);
        assert_eq!(json["usd"], serde_json::Value::Null);

        let usage = vec![SpritesUsage {
            env: "sp".into(),
            error: None,
            account: None,
            account_shared_with: None,
            rows: vec![row],
        }];
        let text = render_sprites(&usage);
        assert!(
            text.lines()
                .any(|line| line.contains("tg-new") && line.contains("yes")),
            "the row is marked partial: {text}"
        );
        assert!(
            text.contains(
                "usage not recorded yet: tg-new (the sprite is younger than the usage lag; run thegn fleet cost again later)"
            ),
            "{text}"
        );
        assert!(!text.contains("0.00"), "{text}");
    }

    #[test]
    fn an_old_reported_zero_stays_a_real_zero() {
        let row = sprites_row("tg-idle", "ready", &zero_reading(true), Some(0.5), false);

        assert!(!row.partial);
        assert_eq!(row.usd, Some(0.0));
        let usage = vec![SpritesUsage {
            env: "sp".into(),
            error: None,
            account: None,
            account_shared_with: None,
            rows: vec![row],
        }];
        let text = render_sprites(&usage);
        assert!(
            text.lines()
                .any(|line| line.contains("tg-idle") && line.contains("0.00")),
            "{text}"
        );
        assert!(!text.contains("usage not recorded yet"), "{text}");
    }

    #[test]
    fn unreported_sprite_shows_dash_and_note() {
        let usage = vec![SpritesUsage {
            env: "sp".into(),
            error: None,
            account: None,
            account_shared_with: None,
            rows: vec![SpritesUsageRow {
                name: "tg-late".into(),
                status: "ready".into(),
                cpu_hours: 0.0,
                gb_hours: 0.0,
                partial: false,
                recent: true,
                reported: false,
                last_recorded_at: None,
                error: None,
                usd: None,
                unavailable: false,
            }],
        }];

        let text = render_sprites(&usage);
        assert!(text.contains("tg-late"), "{text}");
        assert!(
            text.lines()
                .any(|line| line.contains("tg-late") && line.contains("recent")),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|line| line.contains("tg-late") && line.matches('-').count() >= 2),
            "{text}"
        );
        assert!(
            text.contains(
                "no usage reported yet: tg-late (Sprites records usage with a delay; run thegn fleet cost again later)"
            ),
            "{text}"
        );
        assert!(!text.contains("0.00"), "{text}");
    }

    #[test]
    fn one_failed_usage_read_keeps_other_rows() {
        let usage = vec![SpritesUsage {
            env: "sp".into(),
            error: None,
            account: None,
            account_shared_with: None,
            rows: vec![
                SpritesUsageRow {
                    name: "tg-good".into(),
                    status: "ready".into(),
                    cpu_hours: 1.5,
                    gb_hours: 2.0,
                    partial: false,
                    recent: false,
                    reported: true,
                    last_recorded_at: Some("2026-01-01T02:00:00Z".into()),
                    error: None,
                    usd: None,
                    unavailable: false,
                },
                SpritesUsageRow {
                    name: "tg-bad".into(),
                    status: "ready".into(),
                    cpu_hours: 0.0,
                    gb_hours: 0.0,
                    partial: false,
                    recent: false,
                    reported: false,
                    last_recorded_at: None,
                    error: Some("request failed".into()),
                    usd: None,
                    unavailable: false,
                },
            ],
        }];

        let text = render_sprites(&usage);
        assert!(text.contains("tg-good"), "{text}");
        assert!(text.contains("tg-bad"), "{text}");
        assert!(
            text.contains("usage error: tg-bad: request failed"),
            "{text}"
        );
        assert!(!text.contains("no usage reported yet"), "{text}");
    }

    #[test]
    fn json_rows_carry_reported() {
        let row = &sprites_fixture()[0].rows[0];
        let json = serde_json::to_value(row).unwrap();

        assert_eq!(json["reported"], true);
        assert_eq!(json["last_recorded_at"], "2026-01-01T02:00:00Z");
    }

    #[test]
    fn remote_agents_join_by_worktree() {
        let view = view();
        let agents = remote_agents_with(&view, &|worktree| {
            (worktree == "/wt/a").then_some(thegn_core::issue::AgentDispatch {
                id: 1,
                issue_id: "THE-2020".into(),
                worktree_path: worktree.into(),
                agent_name: "coder\n".into(),
                dispatched_at_ms: 0,
                status: thegn_core::issue::AgentDispatchStatus::Running,
                stage: Some("code".into()),
                parent_id: None,
                session_id: None,
                artifact_path: None,
                note: None,
                chunk_path: None,
                report: None,
                exit_code: None,
                exited_at_ms: None,
            })
        });

        assert_eq!(
            agents.get("/wt/a"),
            Some(&FleetAgent {
                agent: "coder".into(),
                status: "running".into(),
                issue: "THE-2020".into(),
                stage: Some("code".into()),
            })
        );
        assert!(!agents.contains_key("/wt/gone"));
    }

    #[test]
    fn control_report_is_the_two_account_list_envelope_without_usage_calls() {
        // THE-2020's no-credentials acceptance, synthetic: two fly accounts
        // (one listing the other's sandbox again), a sprites account, and a
        // fake provider whose token cannot resolve — composed through the
        // same report the daemon serves at `fleet.list`.
        let vm = |env: &str, id: &str| fleet_cost::SandboxRow {
            provider: "fly".into(),
            env: env.into(),
            id: id.into(),
            state: Some("started".into()),
            size: Some("shared-cpu-1x:256MB".into()),
            created_ms: Some(0),
            ..fleet_cost::SandboxRow::default()
        };
        let inventory = |env: &str, provider: &str, rows| fleet_cost::EnvInventory {
            env: env.into(),
            provider: provider.into(),
            completeness: fleet_cost::Completeness::Complete,
            rows,
        };
        let view = fleet_cost::join(
            vec![
                inventory(
                    "fly-acme",
                    "fly",
                    vec![vm("fly-acme", "tg-a"), vm("fly-acme", "tg-b\u{7}")],
                ),
                // The same account read through a second env: `tg-a` lists
                // again and must collapse to one authoritative row.
                inventory("fly-eu", "fly", vec![vm("fly-eu", "tg-a")]),
                inventory(
                    "sprites-prod",
                    "sprites",
                    vec![fleet_cost::SandboxRow {
                        provider: "sprites".into(),
                        env: "sprites-prod".into(),
                        id: "tg-c".into(),
                        ..fleet_cost::SandboxRow::default()
                    }],
                ),
                fleet_cost::EnvInventory {
                    env: "ghost".into(),
                    provider: "sprites".into(),
                    completeness: fleet_cost::Completeness::Unavailable(
                        "unavailable: API token SPRITES_GHOST could not be resolved".into(),
                    ),
                    rows: vec![],
                },
            ],
            vec![RecordedSandbox {
                worktree: "/wt/a".into(),
                provider: "fly".into(),
                env: "fly-acme".into(),
                id: "tg-a".into(),
            }],
        );
        let agents = remote_agents_with(&view, &|worktree| {
            (worktree == "/wt/a").then_some(thegn_core::issue::AgentDispatch {
                id: 1,
                issue_id: "THE-2020".into(),
                worktree_path: worktree.into(),
                agent_name: "coder\n".into(),
                dispatched_at_ms: 0,
                status: thegn_core::issue::AgentDispatchStatus::Running,
                stage: Some("code".into()),
                parent_id: None,
                session_id: None,
                artifact_path: None,
                note: None,
                chunk_path: None,
                report: None,
                exit_code: None,
                exited_at_ms: None,
            })
        });
        let cost =
            fleet_cost::fleet_cost(&view, fleet_cost::BUNDLED_PRICES, &Default::default(), NOW);
        let snapshot = crate::fleet_read::FleetSnapshot {
            generated_ms: NOW,
            view,
            cost,
            ai: Default::default(),
        };
        let json = fleet_list_json(&snapshot, agents).unwrap();

        // The list envelope: exactly the list fields, no usage sections.
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["agents", "cost", "generated_ms", "view"]);
        assert_eq!(json["generated_ms"], NOW, "an explicit timestamp");
        // One row per authoritative VM: the duplicate across accounts
        // collapsed, and the ledger row joined to its worktree.
        let rows = json["view"]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["worktree"], "/wt/a");
        assert_eq!(rows[0]["provider"], "fly");
        assert_eq!(
            json["view"]["orphans"], 2,
            "tg-b and the sprites row are unjoined"
        );
        // The unreachable fake provider is one partial row with its reason.
        assert_eq!(json["view"]["partial"][0]["env"], "ghost");
        assert!(
            json["view"]["partial"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("SPRITES_GHOST could not be resolved")
        );
        // Agent rows joined by worktree.
        assert_eq!(json["agents"]["/wt/a"]["agent"], "coder");
        // A control character in a provider string never reaches the response.
        assert_eq!(rows[1]["id"], "tg-b");
        assert!(!rows[1]["id"].as_str().unwrap().contains('\u{7}'));
        // The estimate's provenance is carried through unchanged.
        assert_eq!(
            json["cost"]["per_row"].as_array().unwrap().len(),
            rows.len()
        );
        assert!(
            json["cost"]["price_table_date"]
                .as_str()
                .is_some_and(|date| !date.is_empty())
        );
    }

    #[test]
    fn control_report_of_an_unconfigured_fleet_reads_no_provider() {
        // No `[env.*]` entries: the real path assembles offline (no network,
        // no credentials) and still returns the explicit list envelope.
        let _state = crate::testenv::IsolatedState::new();
        let cfg: Config = toml::from_str("").expect("the empty config parses");
        let json = control_report(&cfg).unwrap();
        assert!(
            json["generated_ms"].as_i64().is_some(),
            "an explicit timestamp"
        );
        assert_eq!(json["view"]["rows"].as_array().map(Vec::len), Some(0));
        assert!(json.get("vercel_usage").is_none());
        assert!(json.get("sprites_usage").is_none());
        assert!(json.get("agents").is_none(), "no rows to join");
    }

    #[test]
    fn render_list_appends_agents_section_only_when_present() {
        let view = view();
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let mut agents = std::collections::BTreeMap::new();
        agents.insert(
            "/wt/a".into(),
            FleetAgent {
                agent: "coder".into(),
                status: "running".into(),
                issue: "THE-2020".into(),
                stage: Some("code".into()),
            },
        );

        let text = render_list(&view, &cost, NOW, &agents);
        assert!(text.contains("remote agents:"), "{text}");
        assert!(text.contains("WORKTREE  AGENT  STATUS   ISSUE"), "{text}");
        assert!(
            text.contains("/wt/a     coder  running  THE-2020"),
            "{text}"
        );
        assert!(!render_list(&view, &cost, NOW, &Default::default()).contains("remote agents:"));
    }

    #[test]
    fn sprites_json_shape_and_absent_without_env() {
        let cfg = Config::default();
        assert!(sprites_usage(&cfg).is_none());
        let view = view();
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        let without = serde_json::to_value(FleetReport {
            generated_ms: NOW,
            view: view.clone(),
            cost: cost.clone(),
            vercel_usage: None,
            sprites_usage: None,
            agents: Default::default(),
        })
        .unwrap();
        assert!(without.get("sprites_usage").is_none());
        let with = serde_json::to_value(FleetReport {
            generated_ms: NOW,
            view,
            cost,
            vercel_usage: None,
            sprites_usage: Some(sprites_fixture()),
            agents: Default::default(),
        })
        .unwrap();
        assert_eq!(with["sprites_usage"][0]["rows"][0]["name"], "tg-a");
        assert_eq!(with["sprites_usage"][0]["rows"][0]["partial"], true);
        assert_eq!(
            with["sprites_usage"][0]["rows"][0]["usd"],
            serde_json::Value::Null
        );
        assert_eq!(with["sprites_usage"][0]["rows"][1]["usd"], 0.5);
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
