//! Fleet inventory join and cost estimation (`thegn fleet list|cost`).
//!
//! Pure: the host fetches each env's provider inventory and the worktree
//! bindings, then [`join`] reconciles them into a [`FleetView`] and
//! [`fleet_cost`] prices it. Nothing here does I/O.
//!
//! Prices are **estimates**. A rate comes from `[placement.price]` (keyed
//! `"<provider>:<size>"` or `"<provider>"`, hourly USD) or else from the
//! bundled [`BUNDLED_PRICES`] table dated [`PRICE_TABLE_DATE`]. An unknown
//! size is *unpriced* — never guessed — and is counted in
//! [`FleetCost::unpriced`].
//!
//! Accrual has no stop/start history: `accrued` is rate × age (an upper bound
//! for a sandbox that spent time stopped), while the hourly burn and the
//! projections count only sandboxes whose state is not a known stopped state.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use crate::config_placement::FleetBudget;

/// The date the bundled price table was last checked against the vendors'
/// pricing pages.
pub const PRICE_TABLE_DATE: &str = "2026-11-15";

/// One bundled list price. `size` is the provider's size key (Fly:
/// `shared-cpu-1x:256MB`, the shape `thegn_svc` reports for a machine; VPS:
/// the vendor slug); an empty `size` is a provider-wide flat rate. `region`
/// narrows a row to one region; `None` applies everywhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceRow {
    pub provider: &'static str,
    pub size: &'static str,
    pub region: Option<&'static str>,
    pub usd_per_hour: f64,
    pub source: &'static str,
}

const FLY_PRICING: &str = "https://fly.io/docs/about/pricing/";
const DO_PRICING: &str = "https://www.digitalocean.com/pricing/droplets";
const VERCEL_PRICING: &str =
    "vercel.com/docs/sandbox/pricing (iad1, Pro; upper bound = 100% active CPU + 2 GB/vCPU memory)";

const fn row(
    provider: &'static str,
    size: &'static str,
    usd_per_hour: f64,
    source: &'static str,
) -> PriceRow {
    PriceRow {
        provider,
        size,
        region: None,
        usd_per_hour,
        source,
    }
}

/// Bundled list prices (USD per hour, monthly list price / 730). Only rows
/// whose vendor list price is known are present; Hetzner (EUR-denominated),
/// Sprites and Daytona (usage-metered), and machine0 are left unpriced until
/// a `[placement.price]` entry covers them.
pub const BUNDLED_PRICES: &[PriceRow] = &[
    // Fly Machines: shared CPU presets (thegn's `size` presets) and the
    // common 1x memory steps; performance presets. Base-region list prices.
    row("fly", "shared-cpu-1x:256MB", 0.0027, FLY_PRICING),
    row("fly", "shared-cpu-1x:512MB", 0.0044, FLY_PRICING),
    row("fly", "shared-cpu-1x:1024MB", 0.0078, FLY_PRICING),
    row("fly", "shared-cpu-2x:512MB", 0.0053, FLY_PRICING),
    row("fly", "shared-cpu-4x:1024MB", 0.0107, FLY_PRICING),
    row("fly", "shared-cpu-8x:2048MB", 0.0213, FLY_PRICING),
    row("fly", "performance-1x:2048MB", 0.0425, FLY_PRICING),
    row("fly", "performance-2x:4096MB", 0.0849, FLY_PRICING),
    row("fly", "performance-4x:8192MB", 0.1699, FLY_PRICING),
    // DigitalOcean basic (regular) droplets.
    row("digitalocean", "s-1vcpu-512mb-10gb", 0.00595, DO_PRICING),
    row("digitalocean", "s-1vcpu-1gb", 0.00893, DO_PRICING),
    row("digitalocean", "s-1vcpu-2gb", 0.01786, DO_PRICING),
    row("digitalocean", "s-2vcpu-2gb", 0.02679, DO_PRICING),
    row("digitalocean", "s-2vcpu-4gb", 0.03571, DO_PRICING),
    row("digitalocean", "s-4vcpu-8gb", 0.07143, DO_PRICING),
    // Vercel Sandbox: usage-based, so these are upper bounds (n vCPU at 100%
    // active CPU plus 2 GB/vCPU provisioned memory); a stopped state is free.
    row("vercel", "1vcpu", 0.1704, VERCEL_PRICING),
    row("vercel", "2vcpu", 2.0 * 0.1704, VERCEL_PRICING),
    row("vercel", "4vcpu", 4.0 * 0.1704, VERCEL_PRICING),
    row("vercel", "8vcpu", 8.0 * 0.1704, VERCEL_PRICING),
    row("vercel", "16vcpu", 16.0 * 0.1704, VERCEL_PRICING),
    row("vercel", "32vcpu", 32.0 * 0.1704, VERCEL_PRICING),
];

/// Where a [`Rate`] came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateSource {
    /// A `[placement.price]` entry.
    Config,
    /// The bundled table, as of `date`.
    Bundled { date: String },
}

/// An hourly USD rate and its provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rate {
    pub usd_per_hour: f64,
    pub source: RateSource,
}

fn usable(rate: f64) -> bool {
    rate.is_finite() && rate >= 0.0
}

/// Resolve a rate. Precedence: `[placement.price]` `"provider:size"`, then
/// `"provider"`, then a bundled row for (provider, size, region), then a
/// bundled region-less row for (provider, size), then a bundled provider-wide
/// row (empty size). `None` is unpriced. Non-finite or negative config values
/// are ignored.
pub fn rate_for(
    bundled: &[PriceRow],
    overrides: &BTreeMap<String, f64>,
    provider: &str,
    size: Option<&str>,
    region: Option<&str>,
) -> Option<Rate> {
    let size = size.map(str::trim).filter(|s| !s.is_empty());
    let config = |key: &str| {
        overrides
            .get(key)
            .copied()
            .filter(|r| usable(*r))
            .map(|usd_per_hour| Rate {
                usd_per_hour,
                source: RateSource::Config,
            })
    };
    if let Some(rate) = size.and_then(|size| config(&format!("{provider}:{size}"))) {
        return Some(rate);
    }
    if let Some(rate) = config(provider) {
        return Some(rate);
    }
    let bundled_rate = |row: &PriceRow| Rate {
        usd_per_hour: row.usd_per_hour,
        source: RateSource::Bundled {
            date: PRICE_TABLE_DATE.to_string(),
        },
    };
    let find = |want_size: &str, want_region: Option<&str>| {
        bundled.iter().find(|row| {
            row.provider == provider
                && row.size == want_size
                && row.region == want_region
                && usable(row.usd_per_hour)
        })
    };
    if let Some(size) = size {
        if let Some(region) = region
            && let Some(row) = find(size, Some(region))
        {
            return Some(bundled_rate(row));
        }
        if let Some(row) = find(size, None) {
            return Some(bundled_rate(row));
        }
        return None;
    }
    find("", None).map(bundled_rate)
}

/// Cost of running at `rate` from `created_ms` to `now_ms` (0 for skew,
/// non-finite or non-positive rates). Same math as
/// [`crate::db_compute::accrue_cost`].
pub fn accrued_usd(rate_usd_per_hour: f64, created_ms: i64, now_ms: i64) -> f64 {
    crate::db_compute::accrue_cost(rate_usd_per_hour, created_ms, now_ms)
}

/// Start of the UTC day containing `now_ms`, in epoch ms.
pub fn utc_day_start_ms(now_ms: i64) -> i64 {
    now_ms - now_ms.rem_euclid(86_400_000)
}

/// Start of the UTC month containing `now_ms`, in epoch ms.
pub fn utc_month_start_ms(now_ms: i64) -> i64 {
    use chrono::{Datelike, TimeZone};
    let Some(now) = chrono::DateTime::from_timestamp_millis(now_ms) else {
        return utc_day_start_ms(now_ms);
    };
    chrono::Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .map_or_else(
            || utc_day_start_ms(now_ms),
            |start| start.timestamp_millis(),
        )
}

/// Cost since `period_start_ms`, clamped to `created_ms` when known. An
/// unknown creation time counts from the period start (an upper bound).
fn since_usd(rate: f64, created_ms: Option<i64>, period_start_ms: i64, now_ms: i64) -> f64 {
    let from = created_ms.map_or(period_start_ms, |c| c.max(period_start_ms));
    accrued_usd(rate, from, now_ms)
}

/// Cost so far today (UTC).
pub fn today_usd(rate: f64, created_ms: Option<i64>, now_ms: i64) -> f64 {
    since_usd(rate, created_ms, utc_day_start_ms(now_ms), now_ms)
}

/// Cost so far this UTC month.
pub fn month_to_date_usd(rate: f64, created_ms: Option<i64>, now_ms: i64) -> f64 {
    since_usd(rate, created_ms, utc_month_start_ms(now_ms), now_ms)
}

/// Projected spend over `hours` at the summed hourly `rates`.
pub fn projected_usd(rates: &[f64], hours: f64) -> f64 {
    sum_usd(rates.iter().copied().filter(|r| usable(*r))) * hours.max(0.0)
}

/// Sum from `+0.0`. `Iterator::sum` for floats starts at `-0.0`, so an empty
/// fleet would otherwise total `-0.0` and render as `$-0.0000`.
fn sum_usd(values: impl Iterator<Item = f64>) -> f64 {
    values.fold(0.0, |total, value| total + value)
}

/// One sandbox a provider listed for one env.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxRow {
    pub provider: String,
    pub env: String,
    pub id: String,
    pub name: Option<String>,
    pub state: Option<String>,
    pub size: Option<String>,
    pub region: Option<String>,
    pub created_ms: Option<i64>,
}

/// How completely an env's inventory was read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "reason")]
pub enum Completeness {
    /// Every sandbox was listed: absence of a recorded id is meaningful.
    Complete,
    /// Some rows are known; absence proves nothing.
    Partial(String),
    /// Nothing could be listed (no provider, unresolved token, timeout, …).
    Unavailable(String),
}

/// One env's inventory read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvInventory {
    pub env: String,
    pub provider: String,
    pub completeness: Completeness,
    pub rows: Vec<SandboxRow>,
}

/// A worktree's recorded provider sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedSandbox {
    pub worktree: String,
    pub provider: String,
    pub env: String,
    pub id: String,
}

/// A listed sandbox joined to its worktree (`None` = orphan).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRow {
    pub provider: String,
    pub env: String,
    pub id: String,
    pub name: Option<String>,
    pub state: Option<String>,
    pub size: Option<String>,
    pub region: Option<String>,
    pub created_ms: Option<i64>,
    pub worktree: Option<String>,
    /// Listed by an account-wide provider (sprites) with no worktree and no
    /// record of this host: not thegn's to judge, never an orphan.
    #[serde(default)]
    pub unrecorded: bool,
}

impl FleetRow {
    pub fn is_orphan(&self) -> bool {
        self.worktree.is_none() && !self.unrecorded
    }
}

/// An env whose inventory is not complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialEnv {
    pub env: String,
    pub reason: String,
}

/// The reconciled fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetView {
    /// Every listed sandbox, deduplicated by `(provider, id)`.
    pub rows: Vec<FleetRow>,
    /// How many rows have no worktree and are not marked `unrecorded`.
    pub orphans: usize,
    /// Recorded sandboxes the provider did not list (only judged for envs
    /// whose inventory was complete).
    pub missing: Vec<RecordedSandbox>,
    /// Recorded worktree sandboxes the provider does not list and this host
    /// never provisioned or already tore down (`env down`): not a warning.
    #[serde(default)]
    pub down: Vec<RecordedSandbox>,
    /// Rows marked `unrecorded`.
    #[serde(default)]
    pub unrecorded: usize,
    /// Envs whose inventory was partial or unavailable.
    pub partial: Vec<PartialEnv>,
}

/// Move every `missing` record for which `provisioned` is false into `down`
/// (order kept).
pub fn split_down(view: &mut FleetView, provisioned: impl Fn(&RecordedSandbox) -> bool) {
    let mut missing = Vec::with_capacity(view.missing.len());
    for record in view.missing.drain(..) {
        if provisioned(&record) {
            missing.push(record);
        } else {
            view.down.push(record);
        }
    }
    view.missing = missing;
}

/// For each row with no worktree, `ours(row) == Some(false)` marks it
/// `unrecorded`. `None` (unknown) leaves it an orphan. Recomputes `orphans`
/// and `unrecorded`.
pub fn mark_unrecorded(view: &mut FleetView, ours: impl Fn(&FleetRow) -> Option<bool>) {
    for row in &mut view.rows {
        row.unrecorded = if row.worktree.is_none() {
            ours(row) == Some(false)
        } else {
            false
        };
    }
    view.orphans = view.rows.iter().filter(|row| row.is_orphan()).count();
    view.unrecorded = view.rows.iter().filter(|row| row.unrecorded).count();
}

/// Reconcile inventories with recorded bindings. Two envs sharing one account
/// may list the same sandbox: the first listing wins. A recorded id binds by
/// `(provider, id)` regardless of env; it is *missing* only when its own env's
/// inventory was complete and no env listed it.
pub fn join(inventory: Vec<EnvInventory>, recorded: Vec<RecordedSandbox>) -> FleetView {
    let mut view = FleetView::default();
    let mut seen = BTreeSet::new();
    let mut complete_envs = BTreeSet::new();
    for env in inventory {
        match &env.completeness {
            Completeness::Complete => {
                complete_envs.insert(env.env.clone());
            }
            Completeness::Partial(reason) | Completeness::Unavailable(reason) => {
                view.partial.push(PartialEnv {
                    env: env.env.clone(),
                    reason: reason.clone(),
                });
            }
        }
        for row in env.rows {
            if !seen.insert((row.provider.clone(), row.id.clone())) {
                continue;
            }
            view.rows.push(FleetRow {
                provider: row.provider,
                env: row.env,
                id: row.id,
                name: row.name,
                state: row.state,
                size: row.size,
                region: row.region,
                created_ms: row.created_ms,
                worktree: None,
                unrecorded: false,
            });
        }
    }
    for rec in recorded {
        match view
            .rows
            .iter_mut()
            .find(|row| row.provider == rec.provider && row.id == rec.id)
        {
            Some(row) => {
                if row.worktree.is_none() {
                    row.worktree = Some(rec.worktree);
                }
            }
            None if complete_envs.contains(&rec.env) => view.missing.push(rec),
            None => {}
        }
    }
    view.orphans = view.rows.iter().filter(|row| row.is_orphan()).count();
    view
}

/// Whether a provider state means the sandbox is not currently burning
/// compute (storage may still bill).
pub fn is_stopped_state(state: Option<&str>) -> bool {
    state.is_some_and(|s| {
        matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "stopped"
                | "stopping"
                | "suspended"
                | "destroyed"
                | "destroying"
                | "off"
                | "archived"
                | "warm"
                | "cold"
        )
    })
}

/// One row's cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowCost {
    pub provider: String,
    pub id: String,
    pub rate: Option<Rate>,
    /// Hourly burn now (0 when stopped; `None` when unpriced).
    pub burn_usd_per_hour: Option<f64>,
    /// Rate × age (`None` when unpriced or the creation time is unknown).
    pub accrued_usd: Option<f64>,
    pub today_usd: Option<f64>,
    pub mtd_usd: Option<f64>,
}

/// The fleet's cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetCost {
    pub per_row: Vec<RowCost>,
    pub per_provider_hourly: BTreeMap<String, f64>,
    pub total_hourly: f64,
    pub today: f64,
    pub mtd: f64,
    pub projected_24h: f64,
    pub projected_30d: f64,
    pub unpriced: usize,
    pub price_table_date: String,
    /// `[placement.fleet_budget]` breaches, filled by [`apply_budgets`]
    /// (empty until then, and when no budget is set or none is reached).
    #[serde(default)]
    pub budget: Vec<BudgetBreach>,
    /// Sprites spend read from the usage API, filled by
    /// [`apply_sprites_usage`]. `None` when no usage was read (or a
    /// `[placement.price]` rate prices the sprites directly).
    #[serde(default)]
    pub sprites: Option<SpritesSpend>,
}

/// Sprites bill by usage, not by the hour: published CPU rate, USD per
/// CPU-hour (`sprites.dev/pricing`, as of [`PRICE_TABLE_DATE`]).
pub const SPRITES_CPU_USD_PER_HOUR: f64 = 0.07;
/// Published memory rate, USD per reserved GB-hour.
pub const SPRITES_GB_USD_PER_HOUR: f64 = 0.04375;

/// One Sprites account's usage-API readings, in hours. Account-wide: it
/// covers every sprite on the account, including those another host created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SpritesAccountUsage {
    pub today_cpu_hours: f64,
    pub today_gb_hours: f64,
    pub mtd_cpu_hours: f64,
    pub mtd_gb_hours: f64,
    /// Usage over a recent window, the basis of the `$/h` estimate.
    pub recent_cpu_hours: f64,
    pub recent_gb_hours: f64,
    /// Length of that window in hours (`0` = not read; no burn estimate).
    pub recent_window_hours: f64,
}

/// Sprites spend priced from usage. The burn is an estimate: recent usage
/// at the published rates, spread over the window it was read for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SpritesSpend {
    pub today_usd: f64,
    pub mtd_usd: f64,
    /// Estimated burn, USD/h (always an estimate: label it `est.`).
    pub burn_usd_per_hour: f64,
    /// Sprites listed on the account that this host never recorded; their
    /// usage is included above.
    pub unrecorded: usize,
}

/// USD for `cpu_hours` and `gb_hours` at the published Sprites rates.
/// Non-finite or negative input counts as zero.
pub fn sprites_usd(cpu_hours: f64, gb_hours: f64) -> f64 {
    let clean = |v: f64| if v.is_finite() && v > 0.0 { v } else { 0.0 };
    clean(cpu_hours) * SPRITES_CPU_USD_PER_HOUR + clean(gb_hours) * SPRITES_GB_USD_PER_HOUR
}

/// Fold Sprites usage into `cost`: today and month-to-date from real usage,
/// a `$/h` burn estimate from recent usage, and the sprites rows (which have
/// no bundled rate) no longer count as unpriced. Pure. A no-op when no usage
/// was read, or when a `[placement.price]` rate already prices a sprites row
/// (the override wins; adding usage would double count).
pub fn apply_sprites_usage(cost: &mut FleetCost, view: &FleetView, usage: &[SpritesAccountUsage]) {
    if usage.is_empty() {
        return;
    }
    if cost
        .per_row
        .iter()
        .any(|row| row.provider == "sprites" && row.rate.is_some())
    {
        return;
    }
    let mut spend = SpritesSpend {
        unrecorded: view
            .rows
            .iter()
            .filter(|row| row.provider == "sprites" && row.unrecorded)
            .count(),
        ..SpritesSpend::default()
    };
    for account in usage {
        spend.today_usd += sprites_usd(account.today_cpu_hours, account.today_gb_hours);
        spend.mtd_usd += sprites_usd(account.mtd_cpu_hours, account.mtd_gb_hours);
        if account.recent_window_hours.is_finite() && account.recent_window_hours > 0.0 {
            spend.burn_usd_per_hour +=
                sprites_usd(account.recent_cpu_hours, account.recent_gb_hours)
                    / account.recent_window_hours;
        }
    }
    let covered = cost
        .per_row
        .iter()
        .filter(|row| row.provider == "sprites" && row.rate.is_none())
        .count();
    cost.unpriced = cost.unpriced.saturating_sub(covered);
    cost.today += spend.today_usd;
    cost.mtd += spend.mtd_usd;
    cost.total_hourly += spend.burn_usd_per_hour;
    cost.projected_24h += spend.burn_usd_per_hour * 24.0;
    cost.projected_30d += spend.burn_usd_per_hour * 24.0 * 30.0;
    if spend.burn_usd_per_hour > 0.0 {
        *cost
            .per_provider_hourly
            .entry("sprites".to_string())
            .or_default() += spend.burn_usd_per_hour;
    }
    cost.sprites = Some(spend);
}

/// Which `[placement.fleet_budget.<provider>]` limit a burn reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLevel {
    /// At or above `warn_hourly` (the badge turns amber).
    Warn,
    /// At or above `max_hourly` (new sandboxes are refused).
    Max,
}

impl BudgetLevel {
    pub fn label(self) -> &'static str {
        match self {
            BudgetLevel::Warn => "warn",
            BudgetLevel::Max => "max",
        }
    }
}

/// One provider whose estimated hourly burn reached a budget limit. Only the
/// highest level reached is reported per provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetBreach {
    pub provider: String,
    pub level: BudgetLevel,
    /// The provider's estimated burn, USD/h.
    pub hourly: f64,
    /// The limit it reached, USD/h.
    pub limit: f64,
}

/// Float slack for budget comparisons: a burn within this of a limit counts
/// as reaching it (summed rates are not exact).
pub const BUDGET_EPSILON: f64 = 1e-9;

/// A limit is set when it is a finite, positive amount (`0` = off; negative or
/// non-finite values are refused by `config validate` and ignored here).
fn limit_set(limit: f64) -> bool {
    limit.is_finite() && limit > 0.0
}

fn reaches(hourly: f64, limit: f64) -> bool {
    limit_set(limit) && hourly + BUDGET_EPSILON >= limit
}

/// A provider's estimated burn (an unlisted or unpriced provider burns 0).
fn provider_hourly(cost: &FleetCost, provider: &str) -> f64 {
    cost.per_provider_hourly
        .get(provider)
        .copied()
        .filter(|h| h.is_finite())
        .unwrap_or(0.0)
}

/// Compare each provider's burn against its budget and record the breaches
/// in [`FleetCost::budget`] (replacing any earlier result). Pure.
pub fn apply_budgets(cost: &mut FleetCost, budgets: &BTreeMap<String, FleetBudget>) {
    cost.budget = budgets
        .iter()
        .filter_map(|(provider, budget)| {
            let hourly = provider_hourly(cost, provider);
            let (level, limit) = if reaches(hourly, budget.max_hourly) {
                (BudgetLevel::Max, budget.max_hourly)
            } else if reaches(hourly, budget.warn_hourly) {
                (BudgetLevel::Warn, budget.warn_hourly)
            } else {
                return None;
            };
            Some(BudgetBreach {
                provider: provider.clone(),
                level,
                hourly,
                limit,
            })
        })
        .collect();
}

/// May a NEW sandbox be created on `provider`? `Err` when its `max_hourly` is
/// set and the estimated burn is at or above it. Pure.
pub fn admit_new(
    cost: &FleetCost,
    provider: &str,
    budgets: &BTreeMap<String, FleetBudget>,
) -> Result<(), String> {
    let Some(max) = budgets
        .get(provider)
        .map(|b| b.max_hourly)
        .filter(|m| limit_set(*m))
    else {
        return Ok(());
    };
    let hourly = provider_hourly(cost, provider);
    if reaches(hourly, max) {
        return Err(format!(
            "{provider} burn ${hourly:.2}/h is at its max_hourly ${max:.2}/h \
             ([placement.fleet_budget.{provider}]); stop or destroy a sandbox \
             (`thegn fleet list`) or raise the cap"
        ));
    }
    Ok(())
}

/// Price a [`FleetView`] at `now_ms`.
pub fn fleet_cost(
    view: &FleetView,
    bundled: &[PriceRow],
    overrides: &BTreeMap<String, f64>,
    now_ms: i64,
) -> FleetCost {
    let mut cost = FleetCost {
        price_table_date: PRICE_TABLE_DATE.to_string(),
        ..FleetCost::default()
    };
    let mut burns = Vec::new();
    for row in &view.rows {
        let rate = rate_for(
            bundled,
            overrides,
            &row.provider,
            row.size.as_deref(),
            row.region.as_deref(),
        );
        let Some(rate) = rate else {
            cost.unpriced += 1;
            cost.per_row.push(RowCost {
                provider: row.provider.clone(),
                id: row.id.clone(),
                rate: None,
                burn_usd_per_hour: None,
                accrued_usd: None,
                today_usd: None,
                mtd_usd: None,
            });
            continue;
        };
        let hourly = rate.usd_per_hour;
        let burn = if is_stopped_state(row.state.as_deref()) {
            0.0
        } else {
            hourly
        };
        burns.push(burn);
        *cost
            .per_provider_hourly
            .entry(row.provider.clone())
            .or_default() += burn;
        let today = today_usd(hourly, row.created_ms, now_ms);
        let mtd = month_to_date_usd(hourly, row.created_ms, now_ms);
        cost.today += today;
        cost.mtd += mtd;
        cost.per_row.push(RowCost {
            provider: row.provider.clone(),
            id: row.id.clone(),
            rate: Some(rate),
            burn_usd_per_hour: Some(burn),
            accrued_usd: row.created_ms.map(|c| accrued_usd(hourly, c, now_ms)),
            today_usd: Some(today),
            mtd_usd: Some(mtd),
        });
    }
    cost.total_hourly = sum_usd(burns.iter().copied());
    cost.projected_24h = projected_usd(&burns, 24.0);
    cost.projected_30d = projected_usd(&burns, 24.0 * 30.0);
    cost
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn ms(rfc3339: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    const TABLE: &[PriceRow] = &[
        PriceRow {
            provider: "p",
            size: "s",
            region: None,
            usd_per_hour: 1.0,
            source: "x",
        },
        PriceRow {
            provider: "p",
            size: "s",
            region: Some("r"),
            usd_per_hour: 2.0,
            source: "x",
        },
        PriceRow {
            provider: "p",
            size: "",
            region: None,
            usd_per_hour: 0.5,
            source: "x",
        },
        PriceRow {
            provider: "bad",
            size: "s",
            region: None,
            usd_per_hour: f64::NAN,
            source: "x",
        },
    ];

    fn bundled(v: f64) -> Option<Rate> {
        Some(Rate {
            usd_per_hour: v,
            source: RateSource::Bundled {
                date: PRICE_TABLE_DATE.to_string(),
            },
        })
    }

    fn config(v: f64) -> Option<Rate> {
        Some(Rate {
            usd_per_hour: v,
            source: RateSource::Config,
        })
    }

    #[test]
    fn rate_precedence_table() {
        let none = BTreeMap::new();
        let both: BTreeMap<String, f64> = [("p:s".to_string(), 9.0), ("p".to_string(), 8.0)]
            .into_iter()
            .collect();
        let flat: BTreeMap<String, f64> = [("p".to_string(), 8.0)].into_iter().collect();
        type Case<'a> = (
            &'a BTreeMap<String, f64>,
            Option<&'a str>,
            Option<&'a str>,
            Option<Rate>,
        );
        let cases: &[Case<'_>] = &[
            (&both, Some("s"), Some("r"), config(9.0)),
            (&flat, Some("s"), Some("r"), config(8.0)),
            (&none, Some("s"), Some("r"), bundled(2.0)),
            (&none, Some("s"), Some("other"), bundled(1.0)),
            (&none, Some("s"), None, bundled(1.0)),
            (&none, None, None, bundled(0.5)),
            (&none, Some("  "), None, bundled(0.5)),
            (&none, Some("unknown"), None, None),
        ];
        for (overrides, size, region, want) in cases {
            assert_eq!(
                &rate_for(TABLE, overrides, "p", *size, *region),
                want,
                "{size:?} {region:?}"
            );
        }
    }

    #[test]
    fn unusable_rates_are_unpriced() {
        let bad: BTreeMap<String, f64> =
            [("q".to_string(), -1.0), ("q:s".to_string(), f64::INFINITY)]
                .into_iter()
                .collect();
        assert_eq!(rate_for(TABLE, &bad, "q", Some("s"), None), None);
        assert_eq!(
            rate_for(TABLE, &BTreeMap::new(), "bad", Some("s"), None),
            None
        );
        assert_eq!(
            rate_for(TABLE, &BTreeMap::new(), "nobody", None, None),
            None
        );
    }

    fn sprite_row(id: &str, unrecorded: bool) -> FleetRow {
        FleetRow {
            provider: "sprites".into(),
            id: id.into(),
            unrecorded,
            ..FleetRow::default()
        }
    }

    #[test]
    fn sprites_usage_prices_spend_and_counts_unrecorded() {
        let view = FleetView {
            rows: vec![sprite_row("mine", false), sprite_row("theirs", true)],
            unrecorded: 1,
            ..FleetView::default()
        };
        let mut cost = fleet_cost(&view, BUNDLED_PRICES, &BTreeMap::new(), 0);
        assert_eq!(cost.unpriced, 2);
        let usage = SpritesAccountUsage {
            today_cpu_hours: 2.0,
            today_gb_hours: 4.0,
            mtd_cpu_hours: 10.0,
            mtd_gb_hours: 20.0,
            recent_cpu_hours: 1.0,
            recent_gb_hours: 2.0,
            recent_window_hours: 2.0,
        };
        apply_sprites_usage(&mut cost, &view, &[usage]);
        let today = 2.0 * 0.07 + 4.0 * 0.04375;
        assert!(close(cost.today, today));
        assert!(close(cost.mtd, 10.0 * 0.07 + 20.0 * 0.04375));
        let burn = (0.07 + 2.0 * 0.04375) / 2.0;
        assert!(close(cost.total_hourly, burn));
        assert!(close(cost.per_provider_hourly["sprites"], burn));
        assert!(close(cost.projected_24h, burn * 24.0));
        assert_eq!(cost.unpriced, 0);
        let spend = cost.sprites.unwrap();
        assert_eq!(spend.unrecorded, 1);
        assert!(close(spend.today_usd, today));
    }

    #[test]
    fn sprites_usage_without_a_window_has_no_burn_and_garbage_is_zero() {
        let view = FleetView::default();
        let mut cost = fleet_cost(&view, BUNDLED_PRICES, &BTreeMap::new(), 0);
        let usage = SpritesAccountUsage {
            today_cpu_hours: f64::NAN,
            today_gb_hours: -1.0,
            mtd_cpu_hours: 1.0,
            recent_cpu_hours: 5.0,
            recent_window_hours: 0.0,
            ..SpritesAccountUsage::default()
        };
        apply_sprites_usage(&mut cost, &view, &[usage]);
        assert!(close(cost.today, 0.0));
        assert!(close(cost.mtd, 0.07));
        assert!(close(cost.total_hourly, 0.0));
        assert!(!cost.per_provider_hourly.contains_key("sprites"));
        let mut untouched = fleet_cost(&view, BUNDLED_PRICES, &BTreeMap::new(), 0);
        apply_sprites_usage(&mut untouched, &view, &[]);
        assert_eq!(untouched.sprites, None);
    }

    #[test]
    fn sprites_config_price_wins_over_usage() {
        let view = FleetView {
            rows: vec![sprite_row("a", false)],
            ..FleetView::default()
        };
        let price: BTreeMap<String, f64> = [("sprites".to_string(), 0.5)].into_iter().collect();
        let mut cost = fleet_cost(&view, BUNDLED_PRICES, &price, 0);
        let before = cost.clone();
        let usage = SpritesAccountUsage {
            today_cpu_hours: 1.0,
            recent_cpu_hours: 1.0,
            recent_window_hours: 1.0,
            ..SpritesAccountUsage::default()
        };
        apply_sprites_usage(&mut cost, &view, &[usage]);
        assert_eq!(cost, before);
    }

    #[test]
    fn vercel_rows_priced_and_unknown_size_unpriced() {
        let o = BTreeMap::new();
        let r = rate_for(BUNDLED_PRICES, &o, "vercel", Some("2vcpu"), None).unwrap();
        assert!(close(r.usd_per_hour, 0.3408));
        assert!(matches!(r.source, RateSource::Bundled { .. }));
        for (n, want) in [
            ("1vcpu", 0.1704),
            ("4vcpu", 0.6816),
            ("8vcpu", 1.3632),
            ("16vcpu", 2.7264),
            ("32vcpu", 5.4528),
        ] {
            let r = rate_for(BUNDLED_PRICES, &o, "vercel", Some(n), None).unwrap();
            assert!(close(r.usd_per_hour, want), "{n}");
        }
        assert_eq!(
            rate_for(BUNDLED_PRICES, &o, "vercel", Some("3vcpu"), None),
            None
        );
    }

    #[test]
    fn bundled_table_is_well_formed() {
        for row in BUNDLED_PRICES {
            assert!(
                row.usd_per_hour > 0.0 && row.usd_per_hour.is_finite(),
                "{row:?}"
            );
            assert!(
                row.source.starts_with("https://") || row.source.starts_with("vercel.com/"),
                "{row:?}"
            );
            assert!(!row.size.is_empty(), "{row:?}");
        }
        let fly = rate_for(
            BUNDLED_PRICES,
            &BTreeMap::new(),
            "fly",
            Some("shared-cpu-2x:512MB"),
            Some("iad"),
        );
        assert!(fly.is_some_and(|r| close(r.usd_per_hour, 0.0053)));
        assert_eq!(
            rate_for(
                BUNDLED_PRICES,
                &BTreeMap::new(),
                "hetzner",
                Some("cx22"),
                None
            ),
            None
        );
    }

    #[test]
    fn accrual_and_boundaries() {
        assert!(close(accrued_usd(2.0, 0, 1_800_000), 1.0));
        assert!(close(accrued_usd(2.0, 10, 5), 0.0));
        let now = ms("2026-11-15T06:30:00Z");
        assert_eq!(utc_day_start_ms(now), ms("2026-11-15T00:00:00Z"));
        assert_eq!(utc_month_start_ms(now), ms("2026-11-01T00:00:00Z"));
        assert_eq!(utc_month_start_ms(i64::MAX), utc_day_start_ms(i64::MAX));
    }

    #[test]
    fn accrual_across_a_month_boundary() {
        let created = ms("2026-10-31T22:00:00Z");
        let now = ms("2026-11-01T02:00:00Z");
        assert!(close(accrued_usd(1.0, created, now), 4.0));
        assert!(close(month_to_date_usd(1.0, Some(created), now), 2.0));
        assert!(close(today_usd(1.0, Some(created), now), 2.0));
    }

    #[test]
    fn mtd_for_a_vm_created_last_month_and_unknown_creation() {
        let now = ms("2026-11-03T00:00:00Z");
        let created = ms("2026-09-20T00:00:00Z");
        assert!(close(month_to_date_usd(0.5, Some(created), now), 24.0));
        assert!(close(month_to_date_usd(0.5, None, now), 24.0));
        let fresh = ms("2026-11-02T12:00:00Z");
        assert!(close(month_to_date_usd(1.0, Some(fresh), now), 12.0));
        assert!(close(today_usd(1.0, None, now), 0.0));
    }

    #[test]
    fn projections() {
        assert!(close(projected_usd(&[1.0, 0.5, f64::NAN], 24.0), 36.0));
        assert!(close(projected_usd(&[1.0], -3.0), 0.0));
        assert!(close(projected_usd(&[], 720.0), 0.0));
    }

    fn sandbox(env: &str, id: &str) -> SandboxRow {
        SandboxRow {
            provider: "fly".into(),
            env: env.into(),
            id: id.into(),
            ..SandboxRow::default()
        }
    }

    fn recorded(wt: &str, env: &str, id: &str) -> RecordedSandbox {
        RecordedSandbox {
            worktree: wt.into(),
            provider: "fly".into(),
            env: env.into(),
            id: id.into(),
        }
    }

    #[test]
    fn join_orphan_missing_duplicate_and_partial() {
        let inventory = vec![
            EnvInventory {
                env: "a".into(),
                provider: "fly".into(),
                completeness: Completeness::Complete,
                rows: vec![sandbox("a", "bound"), sandbox("a", "orphan")],
            },
            EnvInventory {
                env: "b".into(),
                provider: "fly".into(),
                completeness: Completeness::Complete,
                rows: vec![sandbox("b", "bound")],
            },
            EnvInventory {
                env: "c".into(),
                provider: "fly".into(),
                completeness: Completeness::Partial("row cap".into()),
                rows: vec![],
            },
            EnvInventory {
                env: "d".into(),
                provider: "sprites".into(),
                completeness: Completeness::Unavailable("no token".into()),
                rows: vec![],
            },
        ];
        let view = join(
            inventory,
            vec![
                recorded("/wt/one", "b", "bound"),
                recorded("/wt/dup", "a", "bound"),
                recorded("/wt/gone", "a", "gone"),
                recorded("/wt/unknown", "c", "gone-too"),
                recorded("/wt/unconfigured", "zzz", "x"),
            ],
        );
        assert_eq!(view.rows.len(), 2, "duplicate across envs collapses");
        assert_eq!(view.rows[0].env, "a");
        assert_eq!(view.rows[0].worktree.as_deref(), Some("/wt/one"));
        assert!(view.rows[1].is_orphan());
        assert_eq!(view.orphans, 1);
        assert_eq!(view.missing, vec![recorded("/wt/gone", "a", "gone")]);
        assert_eq!(
            view.partial,
            vec![
                PartialEnv {
                    env: "c".into(),
                    reason: "row cap".into()
                },
                PartialEnv {
                    env: "d".into(),
                    reason: "no token".into()
                },
            ]
        );
    }

    #[test]
    fn split_down_moves_only_unprovisioned_missing() {
        let mut view = FleetView {
            missing: vec![
                recorded("/wt/kept", "a", "kept"),
                recorded("/wt/down", "a", "down"),
                recorded("/wt/kept-2", "a", "kept-2"),
            ],
            ..FleetView::default()
        };
        split_down(&mut view, |record| record.id != "down");
        assert_eq!(
            view.missing,
            vec![
                recorded("/wt/kept", "a", "kept"),
                recorded("/wt/kept-2", "a", "kept-2"),
            ]
        );
        assert_eq!(view.down, vec![recorded("/wt/down", "a", "down")]);
    }

    #[test]
    fn mark_unrecorded_excludes_rows_from_orphans_and_unknown_stays_orphan() {
        let mut view = FleetView {
            rows: vec![
                FleetRow {
                    id: "known".into(),
                    ..FleetRow::default()
                },
                FleetRow {
                    id: "unknown".into(),
                    ..FleetRow::default()
                },
            ],
            ..FleetView::default()
        };
        mark_unrecorded(&mut view, |row| match row.id.as_str() {
            "known" => Some(false),
            "unknown" => None,
            _ => Some(true),
        });
        assert!(view.rows[0].unrecorded);
        assert!(!view.rows[0].is_orphan());
        assert!(!view.rows[1].unrecorded);
        assert!(view.rows[1].is_orphan());
        assert_eq!(view.orphans, 1);
        assert_eq!(view.unrecorded, 1);
    }

    #[test]
    fn bound_rows_are_never_unrecorded() {
        let mut view = FleetView {
            rows: vec![FleetRow {
                id: "bound".into(),
                worktree: Some("/wt/bound".into()),
                ..FleetRow::default()
            }],
            ..FleetView::default()
        };
        mark_unrecorded(&mut view, |_| Some(false));
        assert!(!view.rows[0].unrecorded);
        assert_eq!(view.orphans, 0);
        assert_eq!(view.unrecorded, 0);
    }

    #[test]
    fn fleet_view_without_new_fields_deserializes() {
        let old = r#"{
            "rows": [{"provider":"fly","env":"e","id":"a",
                "name":null,"state":null,"size":null,"region":null,
                "created_ms":null,"worktree":null}],
            "orphans": 1,
            "missing": [],
            "partial": []
        }"#;
        let view: FleetView = serde_json::from_str(old).unwrap();
        assert_eq!(view.unrecorded, 0);
        assert!(view.down.is_empty());
        assert!(!view.rows[0].unrecorded);
    }

    #[test]
    fn stopped_states() {
        assert!(is_stopped_state(Some(" Stopped ")));
        assert!(is_stopped_state(Some("suspended")));
        assert!(is_stopped_state(Some("warm")));
        assert!(is_stopped_state(Some("COLD")));
        assert!(!is_stopped_state(Some("started")));
        assert!(!is_stopped_state(None));
    }

    #[test]
    fn fleet_cost_totals_and_unpriced() {
        let now = ms("2026-11-15T12:00:00Z");
        let view = FleetView {
            rows: vec![
                FleetRow {
                    provider: "p".into(),
                    id: "run".into(),
                    size: Some("s".into()),
                    state: Some("started".into()),
                    created_ms: Some(ms("2026-11-15T10:00:00Z")),
                    ..FleetRow::default()
                },
                FleetRow {
                    provider: "p".into(),
                    id: "stopped".into(),
                    size: Some("s".into()),
                    state: Some("stopped".into()),
                    created_ms: Some(ms("2026-11-14T12:00:00Z")),
                    ..FleetRow::default()
                },
                FleetRow {
                    provider: "p".into(),
                    id: "unpriced".into(),
                    size: Some("huge".into()),
                    ..FleetRow::default()
                },
            ],
            ..FleetView::default()
        };
        let cost = fleet_cost(&view, TABLE, &BTreeMap::new(), now);
        assert_eq!(cost.unpriced, 1);
        assert_eq!(cost.per_row.len(), 3);
        assert!(close(cost.total_hourly, 1.0));
        assert!(close(cost.per_provider_hourly["p"], 1.0));
        assert!(close(cost.projected_24h, 24.0));
        assert!(close(cost.projected_30d, 720.0));
        // today: run 2h + stopped 12h (accrual ignores stop history)
        assert!(close(cost.today, 14.0));
        // mtd: run 2h + stopped 24h
        assert!(close(cost.mtd, 26.0));
        assert!(close(cost.per_row[1].accrued_usd.unwrap(), 24.0));
        assert_eq!(cost.per_row[1].burn_usd_per_hour, Some(0.0));
        assert_eq!(cost.per_row[2].rate, None);
        assert_eq!(cost.price_table_date, PRICE_TABLE_DATE);
    }

    #[test]
    fn fleet_cost_and_view_json_round_trip() {
        let view = FleetView {
            rows: vec![FleetRow {
                provider: "p".into(),
                id: "a".into(),
                size: Some("s".into()),
                created_ms: Some(0),
                ..FleetRow::default()
            }],
            orphans: 1,
            ..FleetView::default()
        };
        let cost = fleet_cost(&view, TABLE, &BTreeMap::new(), 3_600_000);
        let json = serde_json::to_value(&cost).unwrap();
        for key in [
            "per_row",
            "per_provider_hourly",
            "total_hourly",
            "today",
            "mtd",
            "projected_24h",
            "projected_30d",
            "unpriced",
            "price_table_date",
            "budget",
        ] {
            assert!(json.get(key).is_some(), "{key}");
        }
        assert_eq!(
            json["per_row"][0]["rate"]["source"]["bundled"]["date"],
            PRICE_TABLE_DATE
        );
        let back: FleetCost = serde_json::from_value(json).unwrap();
        assert_eq!(back, cost);
        let view_json = serde_json::to_string(&view).unwrap();
        assert_eq!(serde_json::from_str::<FleetView>(&view_json).unwrap(), view);
        let src = serde_json::to_value(RateSource::Config).unwrap();
        assert_eq!(src, "config");
        let c = serde_json::to_value(Completeness::Partial("x".into())).unwrap();
        assert_eq!(c["kind"], "partial");
    }

    #[test]
    fn an_empty_fleet_totals_positive_zero() {
        let cost = fleet_cost(&FleetView::default(), TABLE, &BTreeMap::new(), 3_600_000);
        for total in [cost.total_hourly, cost.projected_24h, cost.projected_30d] {
            assert!(total == 0.0 && total.is_sign_positive(), "{total:?}");
        }
        assert_eq!(format!("{:.4}", cost.total_hourly), "0.0000");
    }

    fn burn(pairs: &[(&str, f64)]) -> FleetCost {
        FleetCost {
            per_provider_hourly: pairs.iter().map(|(p, h)| ((*p).to_string(), *h)).collect(),
            ..FleetCost::default()
        }
    }

    fn budgets(rows: &[(&str, f64, f64)]) -> BTreeMap<String, FleetBudget> {
        rows.iter()
            .map(|(p, warn_hourly, max_hourly)| {
                (
                    (*p).to_string(),
                    FleetBudget {
                        warn_hourly: *warn_hourly,
                        max_hourly: *max_hourly,
                    },
                )
            })
            .collect()
    }

    /// (burn, warn, max) → expected (level, limit).
    type BudgetCase = (f64, f64, f64, Option<(BudgetLevel, f64)>);

    #[test]
    fn apply_budgets_table() {
        let cases: &[BudgetCase] = &[
            (5.0, 0.0, 0.0, None),
            (1.0, 2.0, 3.0, None),
            (2.0, 2.0, 3.0, Some((BudgetLevel::Warn, 2.0))),
            (2.0 - 1e-12, 2.0, 3.0, Some((BudgetLevel::Warn, 2.0))),
            (2.0 - 1e-6, 2.0, 3.0, None),
            (2.5, 2.0, 3.0, Some((BudgetLevel::Warn, 2.0))),
            (3.0, 2.0, 3.0, Some((BudgetLevel::Max, 3.0))),
            (0.1 + 0.2, 0.0, 0.3, Some((BudgetLevel::Max, 0.3))),
            (9.0, 2.0, 0.0, Some((BudgetLevel::Warn, 2.0))),
            (9.0, 0.0, 3.0, Some((BudgetLevel::Max, 3.0))),
            (9.0, f64::NAN, -1.0, None),
            (0.0, 0.0, 0.0, None),
        ];
        for (hourly, warn, max, want) in cases {
            let mut cost = burn(&[("fly", *hourly)]);
            apply_budgets(&mut cost, &budgets(&[("fly", *warn, *max)]));
            let got = cost.budget.first().map(|b| (b.level, b.limit));
            assert_eq!(got, *want, "burn {hourly} warn {warn} max {max}");
            if let Some(b) = cost.budget.first() {
                assert_eq!(b.provider, "fly");
                assert!((b.hourly - hourly).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn apply_budgets_unpriced_or_absent_provider_burns_zero_and_replaces() {
        let mut cost = burn(&[("fly", 4.0), ("digitalocean", f64::NAN)]);
        apply_budgets(&mut cost, &BTreeMap::new());
        assert!(cost.budget.is_empty(), "no budget ⇒ no breach");
        apply_budgets(
            &mut cost,
            &budgets(&[
                ("fly", 1.0, 0.0),
                ("hetzner", 0.5, 1.0),
                ("digitalocean", 0.5, 0.0),
            ]),
        );
        assert_eq!(cost.budget.len(), 1, "{:?}", cost.budget);
        assert_eq!(cost.budget[0].provider, "fly");
        // A second application replaces, never appends.
        apply_budgets(&mut cost, &budgets(&[("fly", 1.0, 0.0)]));
        assert_eq!(cost.budget.len(), 1);
        assert_eq!(BudgetLevel::Warn.label(), "warn");
        assert_eq!(BudgetLevel::Max.label(), "max");
        let json = serde_json::to_value(&cost.budget[0]).unwrap();
        assert_eq!(json["level"], "warn");
        assert_eq!(json["provider"], "fly");
    }

    #[test]
    fn admit_new_table() {
        let b = budgets(&[("fly", 1.0, 3.0), ("digitalocean", 1.0, 0.0)]);
        // No budget for the provider at all.
        assert_eq!(
            admit_new(&burn(&[("hetzner", 99.0)]), "hetzner", &b),
            Ok(())
        );
        // Warn-only budget never refuses.
        assert_eq!(
            admit_new(&burn(&[("digitalocean", 99.0)]), "digitalocean", &b),
            Ok(())
        );
        // Under, unpriced/absent (0 burn), and just under beyond epsilon.
        assert_eq!(admit_new(&burn(&[("fly", 2.0)]), "fly", &b), Ok(()));
        assert_eq!(admit_new(&burn(&[]), "fly", &b), Ok(()));
        assert_eq!(admit_new(&burn(&[("fly", f64::NAN)]), "fly", &b), Ok(()));
        assert_eq!(admit_new(&burn(&[("fly", 3.0 - 1e-6)]), "fly", &b), Ok(()));
        // At (within epsilon) and over.
        for hourly in [3.0 - 1e-12, 3.0, 3.2] {
            let err = admit_new(&burn(&[("fly", hourly)]), "fly", &b).unwrap_err();
            assert!(err.starts_with("fly burn $"), "{err}");
            assert!(err.contains("max_hourly $3.00/h"), "{err}");
        }
        assert!(
            admit_new(&burn(&[("fly", 3.2)]), "fly", &b)
                .unwrap_err()
                .contains("$3.20/h")
        );
    }
}
