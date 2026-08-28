//! AI-account usage tracking — the pure, substrate-agnostic core (roadmap V 300,
//! "quota refresh tracking"). Models per-account rate-limit windows (session /
//! weekly / …) as `used %` + an absolute reset deadline, mirroring orca's
//! `account-usage-state.ts` (`usedPercent` / `resetsAt` / `loading` /
//! `unavailable`).
//!
//! **This module never touches the filesystem or the network** — that lives in
//! the `thegn-svc` seam. Here we only turn bytes a harness wrote (or a provider
//! returned) into [`AccountUsage`], and format windows for display. Keeping the
//! parsing/formatting pure makes it unit-testable against fixtures (the 95%
//! `thegn-core` coverage gate) and keeps clock/IO at the edges.
//!
//! Data sources differ per provider (only Codex persists its snapshot to disk):
//!   * **Codex** — the newest `~/.codex/sessions/…/rollout-*.jsonl` carries
//!     `event_msg`/`token_count` lines whose `rate_limits` is the same snapshot
//!     `/status` shows ([`parse_codex_rollup`]). Field drift is real: reset is
//!     either `resets_at` (absolute) or the older `resets_in_seconds`, and the
//!     field is sometimes literally `null` — all handled here.
//!   * **Claude** — no on-disk window state; the svc seam reads the OAuth token
//!     from `~/.claude/.credentials.json` ([`parse_claude_credentials`]) and
//!     GETs `/api/oauth/usage`, whose body this parses ([`parse_claude_usage`]).
//!   * **Antigravity** — no on-disk window state either; the svc seam live-fetches
//!     a quota summary this parses leniently ([`parse_antigravity_quota`]). The
//!     exact schema is unverified/version-sensitive, so it degrades to `None`.

use crate::resource_alert;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Whether an account's usage is known, still being gathered, or couldn't be read.
/// Mirrors orca's `loading` / `unavailable` bar states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageState {
    /// Windows are populated and current.
    Ok,
    /// A gather is in flight (the instant loading shell).
    Loading,
    /// No creds / disabled / unreadable / unparseable — show a dim note.
    Unavailable,
}

/// One rate-limit window for an account (e.g. a 5-hour session or a weekly cap).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// Short display label (`"session"`, `"weekly"`, `"5h"`, `"7d"`…).
    pub label: String,
    /// Fraction of the limit consumed, as a percentage `0.0..=100.0`.
    pub used_percent: f32,
    /// Absolute reset deadline, epoch **seconds**. `None` when unknown.
    pub resets_at: Option<i64>,
    /// The window's own length in minutes, when the provider states it (Codex
    /// sends `window_minutes`; Claude only implies it through the label). Lets
    /// the detail view print "5h window" rather than inferring from a label.
    pub window_minutes: Option<u32>,
}

impl UsageWindow {
    /// Build a window, clamping `used_percent` to `0..=100` and normalizing a
    /// millisecond `resets_at` to seconds.
    pub fn new(label: &str, used_percent: f32, resets_at: Option<i64>) -> Self {
        UsageWindow {
            label: label.to_string(),
            used_percent: used_percent.clamp(0.0, 100.0),
            resets_at: resets_at.map(epoch_secs),
            window_minutes: None,
        }
    }

    /// [`UsageWindow::new`] plus the provider-stated window length. A zero or
    /// absent length stays `None` — "not stated" must not render as "0m".
    pub fn with_len(
        label: &str,
        used_percent: f32,
        resets_at: Option<i64>,
        window_minutes: Option<u32>,
    ) -> Self {
        UsageWindow {
            window_minutes: window_minutes.filter(|m| *m > 0),
            ..UsageWindow::new(label, used_percent, resets_at)
        }
    }

    /// The window length as a compact human string (`"5h"`, `"7d"`), or `None`
    /// when the provider didn't state one.
    pub fn len_label(&self) -> Option<String> {
        self.window_minutes.map(fmt_window_len)
    }
}

/// Cumulative token counters a harness reports for itself. Codex publishes these
/// per session (`info.total_token_usage`); Claude does not, so they stay `None`
/// there and the host-wide transcript rollup covers it instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTotals {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning_output: u64,
    pub total: u64,
}

/// One tracked account's usage snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUsage {
    /// Provider id (`"claude"`, `"codex"`, `"antigravity"`).
    pub provider: String,
    /// Human label for the row (provider display name or account name).
    pub account_label: String,
    /// Plan tier when known (`"max"`, `"pro"`, Codex `plan_type`…).
    pub plan: Option<String>,
    /// Windows to render (empty when `state != Ok`).
    pub windows: Vec<UsageWindow>,
    pub state: UsageState,
    /// A short reason shown when `Unavailable` (`"not logged in"`, `"no session"`…).
    pub note: Option<String>,
    /// Stable identity of the account this row is about, used to dedupe rows
    /// that came from two paths to the same login and to key history samples.
    /// See [`AccountUsage::with_identity`].
    pub key: String,
    /// Account email, when the harness records one on disk.
    pub email: Option<String>,
    /// Organization display name (`"Ageless Humans Team"`).
    pub org: Option<String>,
    /// The account's seat within its org (`"team_standard"`, `"enterprise_…"`).
    pub seat_tier: Option<String>,
    /// The org's rate-limit tier (`"default_claude_max_20x"`), which is what
    /// actually determines how large these windows are.
    pub rate_limit_tier: Option<String>,
    /// The credential home this row was read from — the thing that makes two
    /// same-plan accounts distinguishable in the UI when nothing else does.
    pub home: Option<PathBuf>,
    /// Harness-reported cumulative token counters, when it publishes them.
    pub tokens: Option<TokenTotals>,
}

impl AccountUsage {
    /// The shared skeleton: provider + label, everything else empty. The three
    /// state constructors below differ only in `state`/`note`/`windows`, so they
    /// funnel through here rather than restating eleven fields each.
    fn blank(provider: &str, label: &str, state: UsageState) -> Self {
        AccountUsage {
            provider: provider.to_string(),
            account_label: label.to_string(),
            plan: None,
            windows: Vec::new(),
            state,
            note: None,
            // Falls back to the provider id so a row always has *some* key; a
            // real identity replaces it via `with_identity`.
            key: provider.to_string(),
            email: None,
            org: None,
            seat_tier: None,
            rate_limit_tier: None,
            home: None,
            tokens: None,
        }
    }

    /// An `Ok` snapshot with the given windows.
    pub fn ok(
        provider: &str,
        label: &str,
        plan: Option<String>,
        windows: Vec<UsageWindow>,
    ) -> Self {
        AccountUsage {
            plan,
            windows,
            ..AccountUsage::blank(provider, label, UsageState::Ok)
        }
    }

    /// A placeholder row shown while the gather is in flight.
    pub fn loading(provider: &str, label: &str) -> Self {
        AccountUsage::blank(provider, label, UsageState::Loading)
    }

    /// A row that couldn't be read, carrying a short reason.
    pub fn unavailable(provider: &str, label: &str, note: impl Into<String>) -> Self {
        AccountUsage {
            note: Some(note.into()),
            ..AccountUsage::blank(provider, label, UsageState::Unavailable)
        }
    }

    /// Attach the credential home this row was read from, and derive the row's
    /// key from it. Used for rows that have no parsed identity (Codex, or a
    /// Claude home whose `.claude.json` is missing) so they still dedupe by
    /// where they came from rather than colliding on the bare provider id.
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        self.key = format!("{}:{}", self.provider, home.display());
        self.home = Some(home);
        self
    }

    /// Fold a parsed account identity into the row: fills the display label and
    /// the identity fields, and replaces the key with the account's stable
    /// `(accountUuid, organizationUuid)` pair.
    ///
    /// The uuid pair — not `userID`, which is NOT unique: several of a user's
    /// profiles can share one while being genuinely different accounts.
    pub fn with_identity(mut self, id: &ClaudeIdentity) -> Self {
        if let Some(k) = id.key() {
            self.key = format!("{}:{k}", self.provider);
        }
        if let Some(label) = id.display_label() {
            self.account_label = label;
        }
        self.email = id.email.clone();
        self.org = id.org_name.clone();
        self.seat_tier = id.seat_tier.clone();
        self.rate_limit_tier = id.rate_limit_tier.clone();
        self
    }

    /// Attach harness-reported cumulative token counters.
    pub fn with_tokens(mut self, tokens: Option<TokenTotals>) -> Self {
        self.tokens = tokens;
        self
    }

    /// The most-consumed window's percentage, for a compact summary. `None` when
    /// there are no windows.
    pub fn peak_percent(&self) -> Option<f32> {
        self.windows
            .iter()
            .map(|w| w.used_percent)
            .fold(None, |acc, p| Some(acc.map_or(p, |a: f32| a.max(p))))
    }

    /// A chip-sized name for this account, for the statusbar badge.
    ///
    /// The full label is `email (Organization)` — 50-odd cells, and clipping it
    /// to a chip cuts mid-domain into something that reads as a broken address
    /// (`blake@ashleyjr`). The email's local part is both shorter and the half
    /// that actually distinguishes one account from another, so prefer it, then
    /// the org, then whatever the label is.
    pub fn short_label(&self) -> String {
        if let Some(local) = self
            .email
            .as_deref()
            .and_then(|e| e.split('@').next())
            .filter(|s| !s.trim().is_empty())
        {
            return local.to_string();
        }
        self.org
            .clone()
            .filter(|o| !o.trim().is_empty())
            .unwrap_or_else(|| self.account_label.clone())
    }

    /// The most-consumed window itself, for the badge's "87% · resets in 2h14m".
    pub fn peak_window(&self) -> Option<&UsageWindow> {
        self.windows
            .iter()
            .fold(None, |acc: Option<&UsageWindow>, w| match acc {
                Some(best) if best.used_percent >= w.used_percent => Some(best),
                _ => Some(w),
            })
    }
}

/// Severity of a window's consumption, for tone selection at the render site
/// (the host maps this to a theme hue). Thresholds live here so they're testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageTone {
    Ok,
    Warn,
    Crit,
}

/// Default warn threshold (`[usage] warn_percent`).
pub const DEFAULT_WARN_PERCENT: f32 = 75.0;
/// Default critical threshold (`[usage] crit_percent`).
pub const DEFAULT_CRIT_PERCENT: f32 = 90.0;

/// Tone ramp at the default thresholds: `Crit` at ≥90%, `Warn` at ≥75%.
pub fn tone(used_percent: f32) -> UsageTone {
    tone_at(used_percent, DEFAULT_WARN_PERCENT, DEFAULT_CRIT_PERCENT)
}

/// Tone ramp at caller-supplied thresholds (`[usage] warn_percent` /
/// `crit_percent`). A `crit` below `warn` is treated as `warn` — a
/// misconfiguration must not make the critical band unreachable and leave a
/// maxed-out account rendering amber.
pub fn tone_at(used_percent: f32, warn: f32, crit: f32) -> UsageTone {
    let crit = crit.max(warn);
    if used_percent >= crit {
        UsageTone::Crit
    } else if used_percent >= warn {
        UsageTone::Warn
    } else {
        UsageTone::Ok
    }
}

/// Bar fill fraction `0.0..=1.0` for a `used %`.
pub fn used_frac(used_percent: f32) -> f32 {
    (used_percent / 100.0).clamp(0.0, 1.0)
}

/// The single most-consumed window across every tracked account — what the
/// statusbar badge shows. Returns the account's row index alongside the window
/// so the caller can label it. `None` when nothing has readable windows.
///
/// Ties keep the earlier account, so the badge doesn't flip between two equally
/// loaded accounts on successive polls.
pub fn peak_across<'a>(accounts: &'a [AccountUsage]) -> Option<(usize, &'a UsageWindow)> {
    accounts
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.peak_window().map(|w| (i, w)))
        .fold(
            None,
            |acc: Option<(usize, &'a UsageWindow)>, (i, w)| match acc {
                Some((_, best)) if best.used_percent >= w.used_percent => acc,
                _ => Some((i, w)),
            },
        )
}

/// Human "resets in …" string for an absolute deadline given `now` (epoch secs):
/// `"3h 54m"`, `"12m"`, `"45s"`, `"2d 3h"`, or `"now"` once elapsed. `None` when
/// the window has no known reset.
pub fn fmt_resets_in(resets_at: Option<i64>, now: i64) -> Option<String> {
    let at = resets_at?;
    let rem = at - now;
    if rem <= 0 {
        return Some("now".to_string());
    }
    Some(fmt_dur(rem as u64))
}

/// Format a positive second-count as a compact 2-unit duration.
fn fmt_dur(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

/// The shortest run of history a forecast may be drawn from. Two samples five
/// minutes apart is a slope; two samples five seconds apart is noise with a
/// denominator, and dividing by it produces confident nonsense.
const MIN_FORECAST_SPAN_SECS: i64 = 300;

/// Trim a window's history to the **current** run: the samples since the last
/// time the percentage dropped.
///
/// Load-bearing. A window resets to ~0 on its boundary, so history that spans a
/// reset contains a huge negative step; averaging across it yields a nonsense
/// (usually negative) burn rate. Only the run since the last reset describes
/// what is happening now.
pub fn current_run(samples: &[(i64, f32)]) -> &[(i64, f32)] {
    let start = samples
        .windows(2)
        .rposition(|p| p[1].1 < p[0].1)
        .map(|i| i + 1)
        .unwrap_or(0);
    &samples[start..]
}

/// Project when this window hits 100%, from its recent burn rate — the
/// unfinished "forecast" half of roadmap V 300.
///
/// `samples` is `(epoch_secs, used_percent)`, oldest first. Returns `None` —
/// meaning "nothing to predict", never "you are fine" — when the history is too
/// short or too brief to have a slope, when usage is flat or falling, or when
/// the projection lands after the window resets anyway (in which case the reset
/// is the answer, and the countdown already shows it).
pub fn forecast_exhaustion(
    samples: &[(i64, f32)],
    now: i64,
    resets_at: Option<i64>,
) -> Option<i64> {
    let run = current_run(samples);
    let (first, last) = (run.first()?, run.last()?);
    let span = last.0 - first.0;
    if span < MIN_FORECAST_SPAN_SECS {
        return None;
    }
    let rate = (last.1 - first.1) / span as f32; // percent per second
    if rate <= 0.0 {
        return None;
    }
    let remaining = (100.0 - last.1).max(0.0);
    // Round: `as i64` truncates, and f32 division lands a hair under the exact
    // answer often enough that a clean "1 hour" projection renders as 59m 59s.
    let eta = now + (remaining / rate).round() as i64;
    // Resetting before you run out is not a forecast worth showing.
    match resets_at {
        Some(r) if eta >= r => None,
        _ => Some(eta),
    }
}

/// A token count as a compact string: `812`, `45.3k`, `1.2M`, `3.4B`. Token
/// counts run to eight or nine digits, and a raw one in a narrow chrome column
/// is a number nobody reads.
pub fn fmt_tokens(n: u64) -> String {
    const UNITS: [(u64, char); 3] = [(1_000_000_000, 'B'), (1_000_000, 'M'), (1_000, 'k')];
    for (scale, suffix) in UNITS {
        if n >= scale {
            let v = n as f64 / scale as f64;
            // One decimal below 10 (`1.2M`), none above (`45M`) — the extra
            // digit stops mattering exactly when the column gets tight.
            return if v < 10.0 {
                format!("{v:.1}{suffix}")
            } else {
                format!("{v:.0}{suffix}")
            };
        }
    }
    n.to_string()
}

/// A window *length* in minutes as its single largest unit — `300` ⇒ `"5h"`,
/// `10080` ⇒ `"7d"`. Deliberately not [`fmt_dur`]: that formats a *remaining*
/// duration and always shows two units, so a 5-hour window would read "5h 0m".
///
/// Rounds rather than truncating, because providers report the window slightly
/// short of the round number — Codex sends `window_minutes: 299` for what its
/// own UI calls a 5-hour window, and "4h" (or "299m") would be wrong twice over.
fn fmt_window_len(minutes: u32) -> String {
    let m = u64::from(minutes);
    if m >= 1440 {
        format!("{}d", (m + 720) / 1440)
    } else if m >= 60 {
        format!("{}h", (m + 30) / 60)
    } else {
        format!("{m}m")
    }
}

/// Normalize an epoch that might be milliseconds into seconds (13-digit values
/// are ms). Applied to every absolute `resets_at` we ingest.
fn epoch_secs(v: i64) -> i64 {
    if v > 1_000_000_000_000 { v / 1000 } else { v }
}

// --- config types (see `config::UsageConfig`) ------------------------------

/// A `[[usage.accounts]]` entry — one credential home to track, on top of (or
/// instead of) what discovery finds. Mirrors the `[[accounts]]` precedent
/// ([`crate::account::Account`]), and like [`crate::config_issues::IssueAccount`]
/// every field is optional so a two-line entry works.
///
/// Three jobs: **add** a home discovery wouldn't find, **rename** one whose
/// auto-derived label is unhelpful, and **exclude** one (`enabled = false`) —
/// the last is why an entry that names an already-discovered dir is useful.
///
/// (A config type living with its domain logic, re-exported by `config` — the
/// same arrangement as `account::Account`, and what keeps the `config.rs`
/// god-file from growing.)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct UsageAccount {
    /// Stable id for this entry. Used as the display label when `label` is
    /// empty and no identity could be read from the home.
    pub name: String,
    /// Harness id: `"claude"`, `"codex"`, `"antigravity"`.
    pub provider: String,
    /// The credential home (`~` expanded) — e.g. `"~/.claude-profiles/work/.claude"`.
    pub dir: String,
    /// Display label override; empty falls back to `name`, then to the account
    /// identity read from the home.
    pub label: String,
    /// Track this home? `false` excludes it even if discovery finds it.
    pub enabled: bool,
}

impl Default for UsageAccount {
    fn default() -> Self {
        // `enabled` defaults to true so an entry that omits it is still tracked
        // (serde container `default` fills missing fields from this impl).
        UsageAccount {
            name: String::new(),
            provider: String::new(),
            dir: String::new(),
            label: String::new(),
            enabled: true,
        }
    }
}

/// `[usage.alerts]` — warn when an account approaches its limit.
///
/// Deliberately the same knob vocabulary as `[stats.alerts]`
/// ([`crate::config::StatsAlertsConfig`]): `sustain_secs` / `repeat_secs` /
/// `clear_margin` / `notify_clear` mean exactly what they mean there, and the
/// evaluator in [`crate::usage_alert`] implements the same rules. Two threshold
/// tables that behave differently is how a user learns to distrust both.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct UsageAlertsConfig {
    pub enabled: bool,
    /// Also record to the notification inbox (not just an in-app toast). On by
    /// default here, unlike `[stats.alerts]`: a quota you are about to exhaust
    /// is worth finding later, and it fires at most a handful of times a week —
    /// where a pegged CPU during a build would fire every sample.
    pub notify: bool,
    /// Seconds past a threshold before firing. Usage moves in poll-sized steps,
    /// so this is mostly a guard against a single bad API response.
    pub sustain_secs: u32,
    /// Minimum seconds between repeats of a standing alert. `0` disables repeats.
    pub repeat_secs: u32,
    /// Fractional retreat below the threshold required before an alert clears,
    /// so a window hovering on the line cannot flap.
    pub clear_margin: f32,
    /// Emit an event when an alert clears (usually because the window reset).
    pub notify_clear: bool,
    /// Percent used. `0` disables that level. Left at 0/0 these inherit
    /// `[usage] warn_percent` / `crit_percent`, so the alert lines and the bar
    /// colors have one place to be set and cannot disagree.
    pub used: resource_alert::AlertRule,
}

impl Default for UsageAlertsConfig {
    fn default() -> Self {
        UsageAlertsConfig {
            enabled: true,
            notify: true,
            sustain_secs: 0,
            repeat_secs: 3600,
            clear_margin: 0.05,
            notify_clear: false,
            // Zero = inherit `[usage] warn_percent` / `crit_percent`; see
            // `UsageConfig::effective_alerts`.
            used: resource_alert::AlertRule {
                warn: 0.0,
                critical: 0.0,
            },
        }
    }
}

// --- credential-home discovery (pure) --------------------------------------

/// Where a candidate credential home came from. The order of this enum is the
/// precedence order: an explicitly configured home beats the same directory
/// found by scanning, so its label and `enabled` flag win the dedup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HomeOrigin {
    /// A `[[usage.accounts]]` entry, or an `[[accounts]]` credential home.
    Configured,
    /// The harness's own default home (`$CLAUDE_CONFIG_DIR` / `~/.claude`).
    Default,
    /// Found by scanning a profiles root (`~/.claude-profiles/*/.claude`).
    Discovered,
}

/// A credential home the svc seam offered up for consideration, before dedup.
#[derive(Debug, Clone, PartialEq)]
pub struct HomeCandidate {
    pub provider: String,
    pub dir: PathBuf,
    pub origin: HomeOrigin,
    /// Display label from config, when one was given.
    pub label: Option<String>,
    /// `false` excludes this home (a `[[usage.accounts]] enabled = false`).
    pub enabled: bool,
}

/// A credential home that survived dedup and will be gathered from.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageHome {
    pub provider: String,
    pub dir: PathBuf,
    pub origin: HomeOrigin,
    pub label: Option<String>,
}

impl UsageHome {
    /// The fallback row label when no identity can be parsed out of the home:
    /// the configured label, else something recognisable from the path.
    ///
    /// Path-derived labels take the **parent** directory when the leaf is the
    /// harness's generic config dir name — `~/.claude-profiles/regclaude2/.claude`
    /// is meaningful as "regclaude2" and useless as "claude", and every
    /// discovered home would otherwise carry the identical label.
    pub fn fallback_label(&self, generic_leaf: &str) -> String {
        if let Some(l) = self.label.as_deref().filter(|l| !l.trim().is_empty()) {
            return l.to_string();
        }
        let leaf = self
            .dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if leaf == generic_leaf
            && let Some(parent) = self
                .dir
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .filter(|s| !s.is_empty())
        {
            return parent;
        }
        if leaf.is_empty() {
            self.dir.display().to_string()
        } else {
            leaf
        }
    }
}

/// Does `provider` have an **explicitly configured** credential home — a
/// `[[usage.accounts]]` entry that is enabled and names a `dir`?
///
/// This is the "explicit beats implicit" rule the svc seam applies before it
/// offers a provider's *default* home (`$CLAUDE_CONFIG_DIR` / `~/.claude`) as a
/// candidate: an explicit account dir is the user telling us where that provider
/// lives, so the default home is a zero-config fallback for providers that were
/// left unconfigured, not an extra home to scan on top. Adding both means the
/// ambient home is read even when the config never mentioned it — which
/// double-counts sessions and tokens whenever the two overlap, and makes
/// discovery non-hermetic (its result depends on what happens to exist under
/// `$HOME`).
///
/// Disabled entries do **not** count: `enabled = false` says "skip this home",
/// not "the provider lives here", so it must not also suppress the fallback.
pub fn has_configured_home(accounts: &[UsageAccount], provider: &str) -> bool {
    accounts
        .iter()
        .any(|a| a.enabled && a.provider.trim() == provider && !a.dir.trim().is_empty())
}

/// Reduce the candidate homes to the set worth gathering: drop disabled ones,
/// dedup by `(provider, dir)` keeping the highest-precedence origin, and return
/// them in a stable order (configured, then default, then discovered; ties
/// broken by path so the UI's row order never shuffles between polls).
///
/// Pure — the directory listing that produces the candidates lives in the svc
/// seam. Dedup here is by **path**; two paths that turn out to be the same
/// *account* are collapsed later by [`dedupe_by_identity`], which needs the
/// identity files read first.
pub fn discover_homes(candidates: &[HomeCandidate]) -> Vec<UsageHome> {
    let mut best: Vec<HomeCandidate> = Vec::new();
    for c in candidates {
        match best
            .iter_mut()
            .find(|b| b.provider == c.provider && b.dir == c.dir)
        {
            // Same home seen twice: the more specific origin wins, and a
            // disabling entry wins outright — an explicit opt-out must not be
            // undone by the scanner finding the same directory.
            Some(b) => {
                if !c.enabled {
                    b.enabled = false;
                }
                if c.origin < b.origin {
                    b.origin = c.origin;
                    b.label = c.label.clone();
                }
            }
            None => best.push(c.clone()),
        }
    }
    best.retain(|c| c.enabled);
    best.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then(a.origin.cmp(&b.origin))
            .then(a.dir.cmp(&b.dir))
    });
    best.into_iter()
        .map(|c| UsageHome {
            provider: c.provider,
            dir: c.dir,
            origin: c.origin,
            label: c.label,
        })
        .collect()
}

/// Collapse rows that resolved to the same account, keeping the most informative
/// one. Two credential homes can hold the same login — `~/.claude` and a profile
/// copy of it — and showing that account twice would double-count it in the
/// badge's peak and raise two warnings for one quota.
///
/// "Most informative" = an `Ok` row beats a non-`Ok` one, and among equals the
/// one with more windows wins; ties keep the first, so [`discover_homes`]'s
/// precedence ordering decides.
pub fn dedupe_by_identity(rows: Vec<AccountUsage>) -> Vec<AccountUsage> {
    let mut out: Vec<AccountUsage> = Vec::new();
    for row in rows {
        match out.iter_mut().find(|r| r.key == row.key) {
            Some(existing) => {
                let better = (row.state == UsageState::Ok, row.windows.len())
                    > (existing.state == UsageState::Ok, existing.windows.len());
                if better {
                    *existing = row;
                }
            }
            None => out.push(row),
        }
    }
    out
}

// --- Provider-text hygiene --------------------------------------------------
// Every parser below turns provider bytes (an HTTP body, a harness's JSON/JSONL
// on disk) into domain strings. Those strings reach `Change::Text` — and a
// control byte in a `Change::Text` is not inert: termwiz acts on `\r`/`\n`, so
// `group = "weekly\rZAP"` paints "ZAP" at column 0 of the chrome outside the
// popup's clip rect (the weather incident), and a control byte disagrees
// between the width models that size and truncate chrome text (`seg::cut`
// counts it 0, `seg_width` counts it 1). An oversized string blows out the
// column/chip width it is measured against. So every provider-supplied
// *string* is filtered and bounded here, at the seam where provider data
// becomes domain data — which also keeps the `{account}#{label}` history keys
// consistent, since both the sampler that writes them and the views that read
// them key off the same sanitized label.

/// Maximum chars any provider-supplied string may carry into the UI. Window
/// labels (`"7-day window (opus)"`), plans and org names are short by nature;
/// the cap only bites on hostile or bloated input.
const MAX_TEXT_CHARS: usize = 64;

/// Control characters dropped, length capped, trimmed.
fn safe_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// [`safe_text`] over an optional field: `None` stays `None`; a value that is
/// empty — or was nothing but control characters/whitespace — becomes `None`,
/// so a blank never renders as data.
fn safe_field(v: Option<String>) -> Option<String> {
    let s = safe_text(&v?);
    (!s.is_empty()).then_some(s)
}

// --- Codex: newest rollout `token_count` / `rate_limits` -------------------

#[derive(Debug, Deserialize)]
struct CodexWindow {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<i64>,
    #[serde(default)]
    resets_in_seconds: Option<f64>,
    #[serde(default)]
    window_minutes: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct CodexRateLimits {
    #[serde(default)]
    primary: Option<CodexWindow>,
    #[serde(default)]
    secondary: Option<CodexWindow>,
    #[serde(default)]
    plan_type: Option<String>,
}

/// Codex's cumulative token counters (`payload.info.total_token_usage`). Field
/// names are Codex's own, hence the `_tokens` suffixes.
#[derive(Debug, Deserialize)]
struct CodexTokenUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cached_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    reasoning_output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

impl From<CodexTokenUsage> for TokenTotals {
    fn from(u: CodexTokenUsage) -> Self {
        TokenTotals {
            input: u.input_tokens.unwrap_or(0),
            cached_input: u.cached_input_tokens.unwrap_or(0),
            output: u.output_tokens.unwrap_or(0),
            reasoning_output: u.reasoning_output_tokens.unwrap_or(0),
            // Codex sends its own total; only derive one when it didn't, so we
            // never contradict the harness about its own arithmetic.
            total: u
                .total_tokens
                .unwrap_or_else(|| u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0)),
        }
    }
}

impl CodexWindow {
    /// Resolve the absolute reset deadline: prefer `resets_at`, else `now +
    /// resets_in_seconds` (the older relative form).
    fn reset_at(&self, now: i64) -> Option<i64> {
        self.resets_at
            .or_else(|| self.resets_in_seconds.map(|s| now + s as i64))
    }
}

/// Parse the account's usage from a Codex rollout `.jsonl` blob: scan every line,
/// keep the **last** `token_count` event whose `rate_limits` is non-null, and map
/// its `primary`/`secondary` to `session`/`weekly` windows. Returns `None` when
/// no line carries rate-limit data (the `"rate_limits": null` case included), so
/// the caller can render the account `Unavailable`.
pub fn parse_codex_rollup(bytes: &[u8], now: i64) -> Option<AccountUsage> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut latest: Option<CodexRateLimits> = None;
    let mut tokens: Option<TokenTotals> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // Lines are `event_msg` records, optionally wrapped with a top-level
        // `timestamp`. The rate-limit snapshot rides on `token_count` payloads.
        let payload = v.get("payload").unwrap_or(&v);
        if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
            continue;
        }
        // `total_token_usage` is already cumulative for the session, so the last
        // one wins — summing across lines would multiply the count by the number
        // of turns.
        if let Some(tu) = payload.pointer("/info/total_token_usage")
            && let Ok(parsed) = serde_json::from_value::<CodexTokenUsage>(tu.clone())
        {
            tokens = Some(parsed.into());
        }
        match payload.get("rate_limits") {
            Some(rl) if !rl.is_null() => {
                if let Ok(parsed) = serde_json::from_value::<CodexRateLimits>(rl.clone()) {
                    latest = Some(parsed);
                }
            }
            _ => {}
        }
    }
    let rl = latest?;
    let mut windows = Vec::new();
    for (label, w) in [("session", &rl.primary), ("weekly", &rl.secondary)] {
        if let Some(w) = w {
            windows.push(UsageWindow::with_len(
                label,
                w.used_percent.unwrap_or(0.0) as f32,
                w.reset_at(now),
                w.window_minutes,
            ));
        }
    }
    if windows.is_empty() {
        return None;
    }
    // `plan_type` is provider-authored (it rides the rollout JSONL) — same
    // hygiene as the HTTP-body parsers above.
    Some(AccountUsage::ok("codex", "Codex", safe_field(rl.plan_type), windows).with_tokens(tokens))
}

// --- Claude: `/api/oauth/usage` body + `.credentials.json` token -----------

/// A `resets_at` as the endpoint actually sends it.
///
/// It is an **RFC-3339 string** (`"2026-08-22T23:39:59.604885+00:00"`), not the
/// epoch integer this originally assumed. That mismatch failed the whole
/// struct's deserialization, so every Claude account reported "no windows" —
/// silently, because the parser's contract is to degrade rather than error. The
/// integer arm stays for defensiveness and because Codex does send epochs.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Instant {
    Epoch(i64),
    Rfc3339(String),
}

impl Instant {
    fn to_epoch(&self) -> Option<i64> {
        match self {
            Instant::Epoch(v) => Some(*v),
            Instant::Rfc3339(s) => chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.timestamp()),
        }
    }
}

fn epoch_of(v: &Option<Instant>) -> Option<i64> {
    v.as_ref().and_then(Instant::to_epoch)
}

#[derive(Debug, Deserialize)]
struct ClaudeWindow {
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<Instant>,
}

/// One entry of the response's `limits` array — the self-describing form, and
/// the preferred one: it names each window (`kind`/`group`), covers the
/// model-scoped caps the flat fields have no slot for, and grows without this
/// parser needing a new field per window.
#[derive(Debug, Deserialize)]
struct ClaudeLimit {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<Instant>,
    #[serde(default)]
    scope: Option<ClaudeScope>,
}

#[derive(Debug, Deserialize)]
struct ClaudeScope {
    #[serde(default)]
    model: Option<ClaudeScopeModel>,
}

#[derive(Debug, Deserialize)]
struct ClaudeScopeModel {
    #[serde(default)]
    display_name: Option<String>,
}

impl ClaudeLimit {
    /// `"session"`, `"weekly"`, `"weekly Fable"` — the group, narrowed by the
    /// scoped model's display name when there is one. Without the model name two
    /// weekly rows would render identically and the scoped cap would look like a
    /// duplicate of the overall one.
    fn label(&self) -> String {
        // Provider strings — filtered and bounded before they become a window's
        // name (see the provider-text hygiene note above).
        let base = [self.group.as_deref(), self.kind.as_deref()]
            .into_iter()
            .flatten()
            .map(safe_text)
            .find(|b| !b.is_empty())
            .unwrap_or_else(|| "limit".to_string());
        match self
            .scope
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .and_then(|m| m.display_name.as_deref())
            .map(safe_text)
            .filter(|n| !n.is_empty())
        {
            Some(model) => format!("{base} {model}"),
            None => base,
        }
    }

    /// The window's length, inferred from its group. The endpoint doesn't state
    /// one, but the group names are the length.
    fn window_minutes(&self) -> Option<u32> {
        let key = self.group.as_deref().or(self.kind.as_deref())?;
        if key.starts_with("session") {
            Some(FIVE_HOURS)
        } else if key.starts_with("weekly") {
            Some(SEVEN_DAYS)
        } else {
            None
        }
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeUsage {
    /// The self-describing form. Preferred when present and non-empty.
    #[serde(default)]
    limits: Vec<ClaudeLimit>,
    #[serde(default)]
    five_hour: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_opus: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<ClaudeWindow>,
}

/// Minutes in the fixed-length Claude windows. The endpoint states no length,
/// but each field/group name *is* the length, so it is known here rather than
/// left for the UI to infer from a label string.
const FIVE_HOURS: u32 = 5 * 60;
const SEVEN_DAYS: u32 = 7 * 24 * 60;

/// Parse Anthropic's `GET /api/oauth/usage` body into windows. `utilization` is
/// taken as a 0–100 percentage. Returns `None` when no window is present.
pub fn parse_claude_usage(bytes: &[u8], plan: Option<String>) -> Option<AccountUsage> {
    let u: ClaudeUsage = serde_json::from_slice(bytes).ok()?;
    // Prefer `limits[]`: it names its own windows and covers the model-scoped
    // caps, so a new window kind appears without a parser change. The flat
    // fields are the fallback for responses that don't carry it.
    let mut windows: Vec<UsageWindow> = u
        .limits
        .iter()
        .map(|l| {
            UsageWindow::with_len(
                &l.label(),
                l.percent.unwrap_or(0.0) as f32,
                epoch_of(&l.resets_at),
                l.window_minutes(),
            )
        })
        .collect();
    if windows.is_empty() {
        for (label, minutes, w) in [
            ("5h", FIVE_HOURS, &u.five_hour),
            ("7d", SEVEN_DAYS, &u.seven_day),
            ("7d opus", SEVEN_DAYS, &u.seven_day_opus),
            ("7d sonnet", SEVEN_DAYS, &u.seven_day_sonnet),
        ] {
            // Most of these fields are present but `null` on any given account
            // (`seven_day_opus` &c.); a null window is not a window.
            if let Some(w) = w {
                windows.push(UsageWindow::with_len(
                    label,
                    w.utilization.unwrap_or(0.0) as f32,
                    epoch_of(&w.resets_at),
                    Some(minutes),
                ));
            }
        }
    }
    if windows.is_empty() {
        return None;
    }
    Some(AccountUsage::ok("claude", "Claude", plan, windows))
}

/// The bits of `~/.claude/.credentials.json` the fetch seam needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeCreds {
    /// OAuth bearer for `/api/oauth/usage`.
    pub token: String,
    /// Subscription tier (`"max"`, `"pro"`), shown as the plan label.
    pub plan: Option<String>,
    /// Rate-limit tier as the credentials file records it. A fallback for the
    /// same field in `.claude.json`, which some homes don't have.
    pub rate_limit_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawClaudeCreds {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<RawClaudeOauth>,
}

#[derive(Debug, Deserialize)]
struct RawClaudeOauth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

/// Extract the OAuth token (+ plan) from a `.credentials.json` blob. Returns
/// `None` when there's no `claudeAiOauth.accessToken` (not logged in on this box).
pub fn parse_claude_credentials(bytes: &[u8]) -> Option<ClaudeCreds> {
    let raw: RawClaudeCreds = serde_json::from_slice(bytes).ok()?;
    let oauth = raw.oauth?;
    let token = oauth.access_token.filter(|t| !t.trim().is_empty())?;
    Some(ClaudeCreds {
        token,
        plan: safe_field(oauth.subscription_type),
        rate_limit_tier: safe_field(oauth.rate_limit_tier),
    })
}

// --- Claude: account identity from `.claude.json` --------------------------

/// Who a Claude credential home belongs to, read from `<home>/.claude.json`'s
/// `oauthAccount`. This is what makes several logins on one box distinguishable
/// — without it every row renders as an indistinguishable "Claude".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeIdentity {
    pub account_uuid: Option<String>,
    pub organization_uuid: Option<String>,
    pub email: Option<String>,
    pub org_name: Option<String>,
    /// `"claude_max"` / `"claude_team"` / `"claude_enterprise"`.
    pub org_type: Option<String>,
    pub seat_tier: Option<String>,
    /// `"default_claude_max_20x"`, `"default_raven"`, … — the org-level tier
    /// that actually sizes the rate-limit windows.
    pub rate_limit_tier: Option<String>,
}

impl ClaudeIdentity {
    /// The stable dedup key: the account **and** organization uuids together.
    /// Both matter — one login can hold seats in several orgs, and those are
    /// separate quota pools. `None` when neither uuid was recorded.
    pub fn key(&self) -> Option<String> {
        match (&self.account_uuid, &self.organization_uuid) {
            (None, None) => None,
            (a, o) => Some(format!(
                "{}/{}",
                a.as_deref().unwrap_or("-"),
                o.as_deref().unwrap_or("-")
            )),
        }
    }

    /// The row label: `email (org)`, degrading to whichever half exists.
    pub fn display_label(&self) -> Option<String> {
        match (self.email.as_deref(), self.org_name.as_deref()) {
            (Some(e), Some(o)) if e != o => Some(format!("{e} ({o})")),
            (Some(e), _) => Some(e.to_string()),
            (None, Some(o)) => Some(o.to_string()),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawClaudeConfig {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<RawOauthAccount>,
}

#[derive(Debug, Deserialize)]
struct RawOauthAccount {
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
    #[serde(rename = "organizationUuid")]
    organization_uuid: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
    #[serde(rename = "organizationName")]
    organization_name: Option<String>,
    #[serde(rename = "organizationType")]
    organization_type: Option<String>,
    #[serde(rename = "seatTier")]
    seat_tier: Option<String>,
    #[serde(rename = "organizationRateLimitTier")]
    org_rate_limit_tier: Option<String>,
    #[serde(rename = "userRateLimitTier")]
    user_rate_limit_tier: Option<String>,
}

/// Parse `<claude home>/.claude.json` into a [`ClaudeIdentity`]. Returns `None`
/// when the blob isn't JSON or carries no `oauthAccount` — that file is large
/// and full of unrelated state, so everything here is best-effort and optional.
pub fn parse_claude_identity(bytes: &[u8]) -> Option<ClaudeIdentity> {
    let raw: RawClaudeConfig = serde_json::from_slice(bytes).ok()?;
    let a = raw.oauth_account?;
    // Every field is provider-authored text bound for the chrome; `safe_field`
    // also subsumes the old blank-out rule (present-but-empty ⇒ `None`).
    Some(ClaudeIdentity {
        account_uuid: safe_field(a.account_uuid),
        organization_uuid: safe_field(a.organization_uuid),
        email: safe_field(a.email_address),
        org_name: safe_field(a.organization_name),
        org_type: safe_field(a.organization_type),
        seat_tier: safe_field(a.seat_tier),
        // Org tier first: it is what sizes the windows. The user tier is the
        // fallback for accounts (enterprise seats) that only carry the latter.
        rate_limit_tier: safe_field(a.org_rate_limit_tier).or(safe_field(a.user_rate_limit_tier)),
    })
}

// --- Antigravity: live quota summary (schema unverified, lenient) ----------

#[derive(Debug, Deserialize)]
struct AgWindow {
    #[serde(default, alias = "name", alias = "label", alias = "displayName")]
    label: Option<String>,
    #[serde(
        default,
        alias = "usedPercent",
        alias = "used_percent",
        alias = "utilization",
        alias = "usage"
    )]
    used_percent: Option<f64>,
    #[serde(
        default,
        alias = "resetsAt",
        alias = "resets_at",
        alias = "resetTime",
        alias = "resetTimeMs"
    )]
    resets_at: Option<i64>,
    #[serde(default, alias = "resetsInSeconds", alias = "resets_in_seconds")]
    resets_in_seconds: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct AgSummary {
    #[serde(
        default,
        alias = "quotas",
        alias = "pools",
        alias = "limits",
        alias = "rateLimits"
    )]
    windows: Vec<AgWindow>,
    #[serde(default, alias = "planType", alias = "plan")]
    plan: Option<String>,
}

/// Best-effort parse of an Antigravity quota-summary response. The real schema is
/// undocumented and version-sensitive, so this is deliberately lenient (field
/// aliases + a couple of common nesting keys) and returns `None` — rendering the
/// account `Unavailable` — whenever it can't find windows.
pub fn parse_antigravity_quota(bytes: &[u8], now: i64) -> Option<AccountUsage> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let root = v
        .get("quotaSummary")
        .or_else(|| v.get("userQuota"))
        .cloned()
        .unwrap_or(v);
    let sum: AgSummary = serde_json::from_value(root).ok()?;
    let mut windows = Vec::new();
    for (i, w) in sum.windows.iter().enumerate() {
        let label = safe_field(w.label.clone()).unwrap_or_else(|| format!("window {}", i + 1));
        let resets_at = w
            .resets_at
            .or_else(|| w.resets_in_seconds.map(|s| now + s as i64));
        windows.push(UsageWindow::new(
            &label,
            w.used_percent.unwrap_or(0.0) as f32,
            resets_at,
        ));
    }
    if windows.is_empty() {
        return None;
    }
    Some(AccountUsage::ok(
        "antigravity",
        "Antigravity",
        safe_field(sum.plan),
        windows,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(provider: &str, dir: &str, origin: HomeOrigin) -> HomeCandidate {
        HomeCandidate {
            provider: provider.into(),
            dir: PathBuf::from(dir),
            origin,
            label: None,
            enabled: true,
        }
    }

    #[test]
    fn current_run_starts_after_the_last_reset() {
        // A reset is a drop; only what came after it describes now.
        let s = [(0, 10.0), (60, 40.0), (120, 2.0), (180, 9.0), (240, 20.0)];
        assert_eq!(current_run(&s), &s[2..]);
        // No drop → the whole history is the run.
        let rising = [(0, 1.0), (60, 2.0)];
        assert_eq!(current_run(&rising), &rising[..]);
        // Two resets → only the newest run.
        let twice = [(0, 50.0), (60, 1.0), (120, 60.0), (180, 3.0)];
        assert_eq!(current_run(&twice), &twice[3..]);
        assert!(current_run(&[]).is_empty());
    }

    #[test]
    fn forecast_projects_the_burn_rate_forward() {
        // 20% over 20 minutes = 1%/min; 60% left ⇒ 60 minutes out.
        let s = [(0, 20.0), (1200, 40.0)];
        let eta = forecast_exhaustion(&s, 1200, None).expect("a forecast");
        assert_eq!(eta, 1200 + 3600);
    }

    #[test]
    fn forecast_declines_to_guess_without_evidence() {
        // Too few points.
        assert_eq!(forecast_exhaustion(&[(0, 50.0)], 0, None), None);
        assert_eq!(forecast_exhaustion(&[], 0, None), None);
        // A long-enough span but flat, or falling: nothing to project.
        assert_eq!(
            forecast_exhaustion(&[(0, 50.0), (600, 50.0)], 600, None),
            None
        );
        assert_eq!(
            forecast_exhaustion(&[(0, 50.0), (600, 40.0)], 600, None),
            None
        );
        // Two samples seconds apart are noise with a denominator — a 1% jump
        // over 10s would "predict" exhaustion in eight minutes.
        assert_eq!(
            forecast_exhaustion(&[(0, 50.0), (10, 51.0)], 10, None),
            None
        );
    }

    #[test]
    fn forecast_is_silent_when_the_window_resets_first() {
        // 1%/min with 60% left = 60 minutes out, but the window resets in 10.
        let s = [(0, 20.0), (1200, 40.0)];
        let now = 1200;
        assert_eq!(forecast_exhaustion(&s, now, Some(now + 600)), None);
        // Reset comfortably after the projection → the projection stands.
        assert!(forecast_exhaustion(&s, now, Some(now + 99_999)).is_some());
    }

    #[test]
    fn forecast_ignores_history_from_before_a_reset() {
        // Without the run-trim, the drop at t=1200 averages to a NEGATIVE rate
        // across the whole history and the forecast silently disappears.
        let s = [(0, 90.0), (1200, 2.0), (2400, 12.0)];
        let eta = forecast_exhaustion(&s, 2400, None).expect("a forecast");
        // 10% over 20 min = 0.5%/min; 88% left ⇒ 176 minutes.
        assert_eq!(eta, 2400 + 176 * 60);
    }

    #[test]
    fn fmt_tokens_stays_narrow_at_every_magnitude() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(812), "812");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(45_300), "45k");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
        assert_eq!(fmt_tokens(45_000_000), "45M");
        assert_eq!(fmt_tokens(3_400_000_000), "3.4B");
    }

    #[test]
    fn tone_at_honors_configured_thresholds() {
        assert_eq!(tone_at(50.0, 60.0, 80.0), UsageTone::Ok);
        assert_eq!(tone_at(60.0, 60.0, 80.0), UsageTone::Warn);
        assert_eq!(tone_at(80.0, 60.0, 80.0), UsageTone::Crit);
        // A crit BELOW warn must not make the critical band unreachable — a
        // maxed-out account has to still render red, not amber.
        assert_eq!(tone_at(95.0, 90.0, 10.0), UsageTone::Crit);
        assert_eq!(tone_at(50.0, 90.0, 10.0), UsageTone::Ok);
    }

    #[test]
    fn window_length_rounds_to_the_provider_s_intended_unit() {
        // Codex reports 299 minutes for what it calls a 5-hour window.
        assert_eq!(
            UsageWindow::with_len("s", 0.0, None, Some(299)).len_label(),
            Some("5h".into())
        );
        assert_eq!(
            UsageWindow::with_len("s", 0.0, None, Some(300)).len_label(),
            Some("5h".into())
        );
        assert_eq!(
            UsageWindow::with_len("w", 0.0, None, Some(10080)).len_label(),
            Some("7d".into())
        );
        assert_eq!(
            UsageWindow::with_len("m", 0.0, None, Some(45)).len_label(),
            Some("45m".into())
        );
        // Zero and absent both mean "not stated" — never "0m".
        assert_eq!(
            UsageWindow::with_len("z", 0.0, None, Some(0)).len_label(),
            None
        );
        assert_eq!(UsageWindow::new("z", 0.0, None).len_label(), None);
    }

    #[test]
    fn claude_identity_parses_and_keys_on_both_uuids() {
        let blob = br#"{"userID":"shared","oauthAccount":{
            "accountUuid":"acc-1","organizationUuid":"org-1",
            "emailAddress":"a@example.com","organizationName":"Acme",
            "organizationType":"claude_team","seatTier":"team_standard",
            "organizationRateLimitTier":"default_raven","userRateLimitTier":"ignored"}}"#;
        let id = parse_claude_identity(blob).expect("identity");
        assert_eq!(id.key().as_deref(), Some("acc-1/org-1"));
        assert_eq!(id.display_label().as_deref(), Some("a@example.com (Acme)"));
        assert_eq!(id.seat_tier.as_deref(), Some("team_standard"));
        // The ORG tier wins: it is what sizes the windows.
        assert_eq!(id.rate_limit_tier.as_deref(), Some("default_raven"));
        // No oauthAccount / not JSON → None.
        assert!(parse_claude_identity(b"{}").is_none());
        assert!(parse_claude_identity(b"nope").is_none());
    }

    #[test]
    fn claude_identity_degrades_field_by_field() {
        // Only a user tier → it is the fallback for the org tier.
        let only_user = br#"{"oauthAccount":{"userRateLimitTier":"default_claude_max_5x"}}"#;
        let id = parse_claude_identity(only_user).expect("identity");
        assert_eq!(id.rate_limit_tier.as_deref(), Some("default_claude_max_5x"));
        // No uuids at all → no key, so the row falls back to its home path.
        assert_eq!(id.key(), None);
        assert_eq!(id.display_label(), None);
        // Blank strings are not values.
        let blanks = br#"{"oauthAccount":{"emailAddress":"  ","organizationName":"Acme"}}"#;
        let id = parse_claude_identity(blanks).expect("identity");
        assert_eq!(id.email, None);
        assert_eq!(id.display_label().as_deref(), Some("Acme"));
        // Email == org name (a personal org) renders once, not "x (x)".
        let same = br#"{"oauthAccount":{"emailAddress":"a@b.c","organizationName":"a@b.c"}}"#;
        let id = parse_claude_identity(same).expect("identity");
        assert_eq!(id.display_label().as_deref(), Some("a@b.c"));
    }

    #[test]
    fn account_rows_carry_home_and_identity() {
        let row = AccountUsage::unavailable("claude", "Claude", "x")
            .with_home("/home/u/.claude-profiles/work/.claude");
        // Without an identity the key is the home, so two homes never collide.
        assert_eq!(row.key, "claude:/home/u/.claude-profiles/work/.claude");
        let id = ClaudeIdentity {
            account_uuid: Some("a".into()),
            organization_uuid: Some("o".into()),
            email: Some("e@x.io".into()),
            org_name: Some("Org".into()),
            seat_tier: Some("seat".into()),
            rate_limit_tier: Some("tier".into()),
            ..Default::default()
        };
        let row = row.with_identity(&id);
        assert_eq!(row.key, "claude:a/o");
        assert_eq!(row.account_label, "e@x.io (Org)");
        assert_eq!(row.rate_limit_tier.as_deref(), Some("tier"));
        // The home survives the identity fold — it is the tiebreaker the user
        // reads when two accounts otherwise look alike.
        assert!(row.home.is_some());
    }

    #[test]
    fn explicit_account_dir_decides_whether_the_default_home_applies() {
        let acct = |provider: &str, dir: &str, enabled: bool| UsageAccount {
            name: "a".into(),
            provider: provider.into(),
            dir: dir.into(),
            enabled,
            ..Default::default()
        };
        let configured = [acct("claude", "/h/p/work/.claude", true)];
        // Explicit dir → the provider's home is configured, so the svc seam
        // must NOT also offer that provider's default home.
        assert!(has_configured_home(&configured, "claude"));
        // No account at all → the default home stays the zero-config fallback.
        assert!(!has_configured_home(&[], "claude"));
        // One provider's config never speaks for another.
        assert!(!has_configured_home(&configured, "codex"));
        // A dir-less entry is not a home, and a disabled one says "skip this
        // home", not "the provider lives here" — neither suppresses the default.
        assert!(!has_configured_home(
            &[acct("claude", "  ", true)],
            "claude"
        ));
        assert!(!has_configured_home(
            &[acct("claude", "/h/p/work/.claude", false)],
            "claude"
        ));
    }

    #[test]
    fn discover_homes_dedupes_by_path_and_ranks_by_origin() {
        let homes = discover_homes(&[
            cand("claude", "/h/.claude", HomeOrigin::Discovered),
            cand("claude", "/h/.claude", HomeOrigin::Default),
            cand("claude", "/h/p/b/.claude", HomeOrigin::Discovered),
            cand("claude", "/h/p/a/.claude", HomeOrigin::Discovered),
            cand("codex", "/h/.codex", HomeOrigin::Default),
        ]);
        let seen: Vec<_> = homes.iter().map(|h| h.dir.display().to_string()).collect();
        // claude before codex; within claude, Default before Discovered; within
        // an origin, path order — a stable row order across polls.
        assert_eq!(
            seen,
            [
                "/h/.claude",
                "/h/p/a/.claude",
                "/h/p/b/.claude",
                "/h/.codex"
            ]
        );
        assert_eq!(homes[0].origin, HomeOrigin::Default);
    }

    #[test]
    fn discover_homes_respects_explicit_config() {
        let labeled = HomeCandidate {
            label: Some("work".into()),
            ..cand("claude", "/h/p/w/.claude", HomeOrigin::Configured)
        };
        let disabled = HomeCandidate {
            enabled: false,
            ..cand("claude", "/h/p/x/.claude", HomeOrigin::Configured)
        };
        let homes = discover_homes(&[
            cand("claude", "/h/p/w/.claude", HomeOrigin::Discovered),
            labeled,
            // An explicit opt-out must survive the scanner finding it too,
            // whichever order the two arrive in.
            cand("claude", "/h/p/x/.claude", HomeOrigin::Discovered),
            disabled,
        ]);
        assert_eq!(homes.len(), 1);
        assert_eq!(homes[0].label.as_deref(), Some("work"));
        assert_eq!(homes[0].origin, HomeOrigin::Configured);
    }

    #[test]
    fn fallback_label_uses_the_profile_dir_not_the_generic_leaf() {
        let home = UsageHome {
            provider: "claude".into(),
            dir: PathBuf::from("/h/.claude-profiles/regclaude2/.claude"),
            origin: HomeOrigin::Discovered,
            label: None,
        };
        // ".claude" is the same for every profile — the parent is the name.
        assert_eq!(home.fallback_label(".claude"), "regclaude2");
        // A configured label always wins.
        let labeled = UsageHome {
            label: Some("work".into()),
            ..home.clone()
        };
        assert_eq!(labeled.fallback_label(".claude"), "work");
        // A blank label is not a label.
        let blank = UsageHome {
            label: Some("  ".into()),
            ..home.clone()
        };
        assert_eq!(blank.fallback_label(".claude"), "regclaude2");
        // A non-generic leaf stands on its own.
        let adopted = UsageHome {
            dir: PathBuf::from("/h/.codex-work"),
            ..home
        };
        assert_eq!(adopted.fallback_label(".codex"), ".codex-work");
    }

    #[test]
    fn dedupe_by_identity_keeps_the_most_informative_row() {
        let win = |n| vec![UsageWindow::new("5h", n, None)];
        let mut a = AccountUsage::ok("claude", "A", None, win(10.0));
        a.key = "claude:acc/org".into();
        let mut stale = AccountUsage::unavailable("claude", "A-copy", "network off");
        stale.key = "claude:acc/org".into();
        let mut other = AccountUsage::ok("claude", "B", None, win(20.0));
        other.key = "claude:other/org".into();

        // The Ok row wins regardless of which order the two arrive in.
        let out = dedupe_by_identity(vec![stale.clone(), a.clone(), other.clone()]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].account_label, "A");
        assert_eq!(out[0].state, UsageState::Ok);
        let out = dedupe_by_identity(vec![a.clone(), stale, other]);
        assert_eq!(out[0].account_label, "A");

        // Among two Ok rows, more windows wins.
        let mut rich = AccountUsage::ok(
            "claude",
            "A-rich",
            None,
            vec![
                UsageWindow::new("5h", 1.0, None),
                UsageWindow::new("7d", 2.0, None),
            ],
        );
        rich.key = "claude:acc/org".into();
        let out = dedupe_by_identity(vec![a, rich]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].account_label, "A-rich");
    }

    #[test]
    fn short_label_stays_chip_sized_and_readable() {
        let id = |email: Option<&str>, org: Option<&str>| ClaudeIdentity {
            email: email.map(str::to_string),
            org_name: org.map(str::to_string),
            ..Default::default()
        };
        let acct = |identity: &ClaudeIdentity| {
            AccountUsage::ok("claude", "fallback", None, vec![]).with_identity(identity)
        };
        // The email's LOCAL PART: the full label is `email (Organization)`,
        // which a chip clips mid-domain into a broken-looking address.
        assert_eq!(
            acct(&id(Some("blake@ashleyjr.com"), Some("Acme"))).short_label(),
            "blake"
        );
        // No email → the org.
        assert_eq!(acct(&id(None, Some("Acme"))).short_label(), "Acme");
        // Neither → whatever the row is labelled (a profile dir name).
        let bare = AccountUsage::ok("claude", "regclaude2", None, vec![]);
        assert_eq!(bare.short_label(), "regclaude2");
    }

    #[test]
    fn peak_across_finds_the_worst_window_and_keeps_ties_stable() {
        let acct = |label: &str, pcts: &[f32]| {
            AccountUsage::ok(
                "claude",
                label,
                None,
                pcts.iter()
                    .map(|p| UsageWindow::new("w", *p, None))
                    .collect(),
            )
        };
        let rows = vec![
            acct("a", &[10.0, 40.0]),
            acct("b", &[87.0, 12.0]),
            acct("c", &[5.0]),
        ];
        let (i, w) = peak_across(&rows).expect("a peak");
        assert_eq!(i, 1);
        assert_eq!(w.used_percent, 87.0);
        // A tie keeps the earlier account, so the badge doesn't flip per poll.
        let tied = vec![acct("a", &[50.0]), acct("b", &[50.0])];
        assert_eq!(peak_across(&tied).map(|(i, _)| i), Some(0));
        // Rows with no windows (loading / unavailable) contribute nothing.
        assert!(peak_across(&[AccountUsage::loading("claude", "x")]).is_none());
        assert!(peak_across(&[]).is_none());
    }

    #[test]
    fn tone_and_frac_thresholds() {
        assert_eq!(tone(10.0), UsageTone::Ok);
        assert_eq!(tone(74.9), UsageTone::Ok);
        assert_eq!(tone(75.0), UsageTone::Warn);
        assert_eq!(tone(89.9), UsageTone::Warn);
        assert_eq!(tone(90.0), UsageTone::Crit);
        assert_eq!(tone(150.0), UsageTone::Crit);
        assert_eq!(used_frac(0.0), 0.0);
        assert_eq!(used_frac(50.0), 0.5);
        assert_eq!(used_frac(250.0), 1.0);
        assert_eq!(used_frac(-5.0), 0.0);
    }

    #[test]
    fn window_clamps_percent_and_normalizes_ms_epoch() {
        let w = UsageWindow::new("s", 140.0, Some(1_700_000_000_000));
        assert_eq!(w.used_percent, 100.0);
        assert_eq!(w.resets_at, Some(1_700_000_000)); // ms → s
        let w2 = UsageWindow::new("s", -3.0, Some(1_700_000_000));
        assert_eq!(w2.used_percent, 0.0);
        assert_eq!(w2.resets_at, Some(1_700_000_000)); // already seconds
    }

    #[test]
    fn fmt_resets_in_units() {
        let now = 1_000_000;
        assert_eq!(fmt_resets_in(None, now), None);
        assert_eq!(fmt_resets_in(Some(now - 10), now).as_deref(), Some("now"));
        assert_eq!(fmt_resets_in(Some(now + 45), now).as_deref(), Some("45s"));
        assert_eq!(
            fmt_resets_in(Some(now + 12 * 60), now).as_deref(),
            Some("12m")
        );
        assert_eq!(
            fmt_resets_in(Some(now + 3 * 3600 + 54 * 60), now).as_deref(),
            Some("3h 54m")
        );
        assert_eq!(
            fmt_resets_in(Some(now + 2 * 86_400 + 3 * 3600), now).as_deref(),
            Some("2d 3h")
        );
    }

    #[test]
    fn account_helpers_and_peak() {
        let loading = AccountUsage::loading("codex", "Codex");
        assert_eq!(loading.state, UsageState::Loading);
        assert_eq!(loading.peak_percent(), None);
        let un = AccountUsage::unavailable("claude", "Claude", "not logged in");
        assert_eq!(un.state, UsageState::Unavailable);
        assert_eq!(un.note.as_deref(), Some("not logged in"));
        let ok = AccountUsage::ok(
            "codex",
            "Codex",
            None,
            vec![
                UsageWindow::new("session", 20.0, None),
                UsageWindow::new("weekly", 63.5, None),
            ],
        );
        assert_eq!(ok.peak_percent(), Some(63.5));
    }

    #[test]
    fn codex_parses_new_resets_at_form() {
        let now = 1_000_000;
        let blob = br#"{"timestamp":"x","type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":{"primary":{"used_percent":12.5,"window_minutes":299,"resets_at":1700000000},"secondary":{"used_percent":40.0,"resets_at":1700500000},"plan_type":"plus"}}}"#;
        let u = parse_codex_rollup(blob, now).expect("parsed");
        assert_eq!(u.provider, "codex");
        assert_eq!(u.plan.as_deref(), Some("plus"));
        assert_eq!(u.windows.len(), 2);
        assert_eq!(u.windows[0].label, "session");
        assert_eq!(u.windows[0].used_percent, 12.5);
        assert_eq!(u.windows[0].resets_at, Some(1_700_000_000));
        assert_eq!(u.windows[0].len_label().as_deref(), Some("5h"));
        assert_eq!(u.windows[1].label, "weekly");
        assert_eq!(u.windows[1].used_percent, 40.0);
        // The secondary window stated no length here.
        assert_eq!(u.windows[1].window_minutes, None);
    }

    #[test]
    fn codex_takes_the_last_cumulative_token_total_not_a_sum() {
        let now = 1_000_000;
        // `total_token_usage` is already cumulative — summing across turns would
        // multiply the count by the number of lines.
        let blob = concat!(
            r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"reasoning_output_tokens":4,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":1.0}}}}"#,
            "\n",
            r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":90,"output_tokens":25,"reasoning_output_tokens":9,"total_tokens":325}},"rate_limits":{"primary":{"used_percent":2.0}}}}"#,
            "\n",
        );
        let t = parse_codex_rollup(blob.as_bytes(), now)
            .expect("parsed")
            .tokens
            .expect("tokens");
        assert_eq!(t.input, 300);
        assert_eq!(t.cached_input, 90);
        assert_eq!(t.output, 25);
        assert_eq!(t.reasoning_output, 9);
        assert_eq!(t.total, 325);
        // A payload with no token block leaves the counters absent, not zeroed —
        // "not reported" and "reported zero" are different claims.
        let bare =
            r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":1.0}}}}"#;
        assert_eq!(
            parse_codex_rollup(bare.as_bytes(), now)
                .expect("parsed")
                .tokens,
            None
        );
    }

    #[test]
    fn codex_derives_a_total_only_when_the_harness_omitted_one() {
        let now = 1;
        let blob = r#"{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":7,"output_tokens":3}},"rate_limits":{"primary":{"used_percent":1.0}}}}"#;
        let t = parse_codex_rollup(blob.as_bytes(), now)
            .expect("parsed")
            .tokens
            .expect("tokens");
        assert_eq!(t.total, 10);
    }

    #[test]
    fn codex_parses_legacy_resets_in_seconds_and_keeps_last_nonnull() {
        let now = 1_000_000;
        // First a null rate_limits (must be skipped), then a legacy relative form,
        // then a newer line that should win (last non-null).
        let blob = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":null}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":5.0,"resets_in_seconds":600}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":9.0,"resets_in_seconds":1800},"secondary":{"used_percent":1.0,"resets_in_seconds":275281}}}}"#,
            "\n",
        );
        let u = parse_codex_rollup(blob.as_bytes(), now).expect("parsed");
        assert_eq!(u.windows.len(), 2);
        assert_eq!(u.windows[0].used_percent, 9.0); // last line won
        assert_eq!(u.windows[0].resets_at, Some(now + 1800));
        assert_eq!(u.windows[1].resets_at, Some(now + 275_281));
    }

    #[test]
    fn codex_none_when_no_ratelimit_lines() {
        let now = 1;
        // Only null / non-token_count lines → nothing to show.
        let blob = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":null}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","text":"hi"}}"#,
            "\n",
            "not json at all\n",
        );
        assert!(parse_codex_rollup(blob.as_bytes(), now).is_none());
        assert!(parse_codex_rollup(b"", now).is_none());
    }

    #[test]
    fn claude_usage_maps_windows() {
        let blob = br#"{"five_hour":{"utilization":37,"resets_at":1700000000},"seven_day":{"utilization":12,"resets_at":1700600000},"seven_day_opus":{"utilization":80,"resets_at":1700600000}}"#;
        let u = parse_claude_usage(blob, Some("max".into())).expect("parsed");
        assert_eq!(u.provider, "claude");
        assert_eq!(u.plan.as_deref(), Some("max"));
        assert_eq!(u.windows.len(), 3);
        assert_eq!(u.windows[0].label, "5h");
        assert_eq!(u.windows[0].used_percent, 37.0);
        assert_eq!(u.windows[2].label, "7d opus");
        assert_eq!(u.windows[2].used_percent, 80.0);
        // No windows → None.
        assert!(parse_claude_usage(b"{}", None).is_none());
    }

    #[test]
    fn claude_parses_the_real_rfc3339_reset_and_limits_array() {
        // The shape the endpoint actually returns. `resets_at` is an RFC-3339
        // STRING — assuming an epoch integer failed the whole struct, so every
        // Claude account reported "no windows" with no error anywhere.
        let blob = br#"{
          "five_hour":{"utilization":19.0,"resets_at":"2026-08-22T23:39:59.604885+00:00"},
          "seven_day":{"utilization":47.0,"resets_at":"2026-08-24T12:59:59.604907+00:00"},
          "seven_day_opus":null,"seven_day_sonnet":null,
          "limits":[
            {"kind":"session","group":"session","percent":19,
             "resets_at":"2026-08-22T23:39:59.604885+00:00","scope":null},
            {"kind":"weekly_all","group":"weekly","percent":47,
             "resets_at":"2026-08-24T12:59:59.604907+00:00","scope":null},
            {"kind":"weekly_scoped","group":"weekly","percent":13,
             "resets_at":"2026-08-24T12:59:59.604907+00:00",
             "scope":{"model":{"id":null,"display_name":"Fable"}}}
          ]}"#;
        let u = parse_claude_usage(blob, Some("max".into())).expect("parsed");
        // `limits[]` wins over the flat fields: it names its windows and covers
        // the model-scoped cap the flat fields have no slot for.
        assert_eq!(u.windows.len(), 3);
        assert_eq!(u.windows[0].label, "session");
        assert_eq!(u.windows[0].used_percent, 19.0);
        assert_eq!(u.windows[0].len_label().as_deref(), Some("5h"));
        assert_eq!(u.windows[1].label, "weekly");
        // Without the model name the two weekly rows would render identically
        // and the scoped cap would look like a duplicate.
        assert_eq!(u.windows[2].label, "weekly Fable");
        assert_eq!(u.windows[2].used_percent, 13.0);
        assert_eq!(u.windows[2].len_label().as_deref(), Some("7d"));
        // 2026-08-22T23:39:59Z
        assert_eq!(u.windows[0].resets_at, Some(1_787_442_000 - 1));
    }

    #[test]
    fn claude_falls_back_to_flat_fields_without_a_limits_array() {
        let blob = br#"{"five_hour":{"utilization":37,"resets_at":"2026-08-22T23:39:59Z"},
                        "seven_day_opus":{"utilization":80,"resets_at":1700600000}}"#;
        let u = parse_claude_usage(blob, None).expect("parsed");
        assert_eq!(u.windows.len(), 2);
        assert_eq!(u.windows[0].label, "5h");
        assert_eq!(u.windows[0].resets_at, Some(1_787_441_999));
        // An epoch integer still parses — Codex sends those, and defensiveness
        // here costs nothing.
        assert_eq!(u.windows[1].label, "7d opus");
        assert_eq!(u.windows[1].resets_at, Some(1_700_600_000));
        // An unparseable stamp loses the countdown, not the window.
        let bad = br#"{"five_hour":{"utilization":5,"resets_at":"not a date"}}"#;
        let u = parse_claude_usage(bad, None).expect("parsed");
        assert_eq!(u.windows[0].used_percent, 5.0);
        assert_eq!(u.windows[0].resets_at, None);
    }

    #[test]
    fn claude_null_windows_are_not_windows() {
        // Most of the flat fields are present-but-null on any given account.
        let blob = br#"{"five_hour":null,"seven_day":null,
                        "seven_day_opus":null,"seven_day_sonnet":null,"limits":[]}"#;
        assert!(parse_claude_usage(blob, None).is_none());
    }

    #[test]
    fn claude_credentials_extracts_token_and_plan() {
        let blob = br#"{"claudeAiOauth":{"accessToken":"sk-oauth-abc","refreshToken":"r","subscriptionType":"max"}}"#;
        let c = parse_claude_credentials(blob).expect("creds");
        assert_eq!(c.token, "sk-oauth-abc");
        assert_eq!(c.plan.as_deref(), Some("max"));
        // Missing / blank token → None.
        assert!(parse_claude_credentials(br#"{"claudeAiOauth":{"accessToken":""}}"#).is_none());
        assert!(parse_claude_credentials(b"{}").is_none());
        assert!(parse_claude_credentials(b"nonsense").is_none());
    }

    #[test]
    fn antigravity_lenient_parse_with_aliases_and_nesting() {
        let now = 1_000_000;
        let blob = br#"{"quotaSummary":{"planType":"pro","quotas":[
            {"name":"5h","usedPercent":40,"resetsAt":1700000000},
            {"label":"weekly","utilization":10,"resetsInSeconds":3600}
        ]}}"#;
        let u = parse_antigravity_quota(blob, now).expect("parsed");
        assert_eq!(u.provider, "antigravity");
        assert_eq!(u.plan.as_deref(), Some("pro"));
        assert_eq!(u.windows.len(), 2);
        assert_eq!(u.windows[0].label, "5h");
        assert_eq!(u.windows[0].used_percent, 40.0);
        assert_eq!(u.windows[1].label, "weekly");
        assert_eq!(u.windows[1].resets_at, Some(now + 3600));
        // Unknown shape → None (caller renders Unavailable).
        assert!(parse_antigravity_quota(b"{}", now).is_none());
        assert!(parse_antigravity_quota(b"garbage", now).is_none());
    }

    // --- provider-text hygiene (the weather incident, at the usage seams) ------
    // A control byte in a `Change::Text` is acted on by the terminal: `\r`
    // repaints at column 0 of the chrome outside every clip rect. Provider
    // strings must arrive filtered and bounded.

    #[test]
    fn claude_limit_labels_carry_no_control_bytes_and_stay_bounded() {
        let long = "x".repeat(200);
        let blob = format!(
            r#"{{"limits":[
            {{"group":"weekly\rZAP","kind":"weekly_all","percent":10,
              "scope":{{"model":{{"display_name":"{long}\n"}}}}}},
            {{"group":"\n\t","kind":"session","percent":20}},
            {{"kind":"limit","percent":30}}
        ]}}"#
        );
        let u = parse_claude_usage(blob.as_bytes(), None).expect("parsed");
        let (w0, w1, w2) = (&u.windows[0], &u.windows[1], &u.windows[2]);
        // The `\r` is gone (the terminal would act on it); the surviving text
        // and the (now 64-char-capped) model tail survive.
        assert_eq!(w0.label, format!("weeklyZAP {}", "x".repeat(64)));
        assert!(!w0.label.chars().any(char::is_control));
        // A group that was nothing but control characters falls through to the
        // kind, then the neutral "limit" — never an empty label.
        assert_eq!(w1.label, "session");
        assert_eq!(w2.label, "limit");
    }

    #[test]
    fn identity_and_credential_fields_carry_no_control_bytes() {
        let long = "o".repeat(300);
        let blob = format!(
            r#"{{"oauthAccount":{{"accountUuid":"a","organizationUuid":"o",
            "emailAddress":"blake@example.com\r","organizationName":"{long}\n",
            "seatTier":"team_standard","userRateLimitTier":"\n\r"}}}}"#
        );
        let id = parse_claude_identity(blob.as_bytes()).expect("identity");
        assert_eq!(id.email.as_deref(), Some("blake@example.com"));
        assert!(
            id.org_name
                .as_ref()
                .is_none_or(|s| { s.chars().count() <= 64 && !s.chars().any(char::is_control) })
        );
        assert_eq!(id.seat_tier.as_deref(), Some("team_standard"));
        // A tier that was nothing but control characters is absent, not blank.
        assert_eq!(id.rate_limit_tier, None);

        let creds = parse_claude_credentials(
            br#"{"claudeAiOauth":{"accessToken":"t","subscriptionType":"pro\r"}}"#,
        )
        .expect("creds");
        assert_eq!(creds.plan.as_deref(), Some("pro"));
    }

    #[test]
    fn antigravity_and_codex_provider_strings_are_sanitized() {
        let now = 1_000_000;
        let long = "y".repeat(120);
        let blob =
            format!(r#"{{"planType":"pro\rX","quotas":[{{"label":"{long}\n","usedPercent":5}}]}}"#);
        let u = parse_antigravity_quota(blob.as_bytes(), now).expect("parsed");
        assert_eq!(u.plan.as_deref(), Some("proX"));
        assert_eq!(u.windows[0].label.chars().count(), 64);
        assert!(!u.windows[0].label.chars().any(char::is_control));

        let blob = br#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12,"window_minutes":299},"plan_type":"plus\nZAP"}}}"#;
        let u = parse_codex_rollup(blob, now).expect("parsed");
        assert_eq!(u.plan.as_deref(), Some("plusZAP"));
    }
}
