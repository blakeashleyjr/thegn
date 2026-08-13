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

use serde::Deserialize;

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
}

impl UsageWindow {
    /// Build a window, clamping `used_percent` to `0..=100` and normalizing a
    /// millisecond `resets_at` to seconds.
    pub fn new(label: &str, used_percent: f32, resets_at: Option<i64>) -> Self {
        UsageWindow {
            label: label.to_string(),
            used_percent: used_percent.clamp(0.0, 100.0),
            resets_at: resets_at.map(epoch_secs),
        }
    }
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
}

impl AccountUsage {
    /// An `Ok` snapshot with the given windows.
    pub fn ok(
        provider: &str,
        label: &str,
        plan: Option<String>,
        windows: Vec<UsageWindow>,
    ) -> Self {
        AccountUsage {
            provider: provider.to_string(),
            account_label: label.to_string(),
            plan,
            windows,
            state: UsageState::Ok,
            note: None,
        }
    }

    /// A placeholder row shown while the gather is in flight.
    pub fn loading(provider: &str, label: &str) -> Self {
        AccountUsage {
            provider: provider.to_string(),
            account_label: label.to_string(),
            plan: None,
            windows: Vec::new(),
            state: UsageState::Loading,
            note: None,
        }
    }

    /// A row that couldn't be read, carrying a short reason.
    pub fn unavailable(provider: &str, label: &str, note: impl Into<String>) -> Self {
        AccountUsage {
            provider: provider.to_string(),
            account_label: label.to_string(),
            plan: None,
            windows: Vec::new(),
            state: UsageState::Unavailable,
            note: Some(note.into()),
        }
    }

    /// The most-consumed window's percentage, for a compact summary. `None` when
    /// there are no windows.
    pub fn peak_percent(&self) -> Option<f32> {
        self.windows
            .iter()
            .map(|w| w.used_percent)
            .fold(None, |acc, p| Some(acc.map_or(p, |a: f32| a.max(p))))
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

/// Tone ramp: `Crit` at ≥90%, `Warn` at ≥75%, else `Ok`.
pub fn tone(used_percent: f32) -> UsageTone {
    if used_percent >= 90.0 {
        UsageTone::Crit
    } else if used_percent >= 75.0 {
        UsageTone::Warn
    } else {
        UsageTone::Ok
    }
}

/// Bar fill fraction `0.0..=1.0` for a `used %`.
pub fn used_frac(used_percent: f32) -> f32 {
    (used_percent / 100.0).clamp(0.0, 1.0)
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

/// Normalize an epoch that might be milliseconds into seconds (13-digit values
/// are ms). Applied to every absolute `resets_at` we ingest.
fn epoch_secs(v: i64) -> i64 {
    if v > 1_000_000_000_000 { v / 1000 } else { v }
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
    if let Some(w) = &rl.primary {
        windows.push(UsageWindow::new(
            "session",
            w.used_percent.unwrap_or(0.0) as f32,
            w.reset_at(now),
        ));
    }
    if let Some(w) = &rl.secondary {
        windows.push(UsageWindow::new(
            "weekly",
            w.used_percent.unwrap_or(0.0) as f32,
            w.reset_at(now),
        ));
    }
    if windows.is_empty() {
        return None;
    }
    Some(AccountUsage::ok("codex", "Codex", rl.plan_type, windows))
}

// --- Claude: `/api/oauth/usage` body + `.credentials.json` token -----------

#[derive(Debug, Deserialize)]
struct ClaudeWindow {
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    five_hour: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_opus: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<ClaudeWindow>,
}

/// Parse Anthropic's `GET /api/oauth/usage` body into windows. `utilization` is
/// taken as a 0–100 percentage. Returns `None` when no window is present.
pub fn parse_claude_usage(bytes: &[u8], plan: Option<String>) -> Option<AccountUsage> {
    let u: ClaudeUsage = serde_json::from_slice(bytes).ok()?;
    let mut windows = Vec::new();
    for (label, w) in [
        ("5h", &u.five_hour),
        ("7d", &u.seven_day),
        ("7d opus", &u.seven_day_opus),
        ("7d sonnet", &u.seven_day_sonnet),
    ] {
        if let Some(w) = w {
            windows.push(UsageWindow::new(
                label,
                w.utilization.unwrap_or(0.0) as f32,
                w.resets_at,
            ));
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
}

/// Extract the OAuth token (+ plan) from a `.credentials.json` blob. Returns
/// `None` when there's no `claudeAiOauth.accessToken` (not logged in on this box).
pub fn parse_claude_credentials(bytes: &[u8]) -> Option<ClaudeCreds> {
    let raw: RawClaudeCreds = serde_json::from_slice(bytes).ok()?;
    let oauth = raw.oauth?;
    let token = oauth.access_token.filter(|t| !t.trim().is_empty())?;
    Some(ClaudeCreds {
        token,
        plan: oauth.subscription_type.filter(|p| !p.trim().is_empty()),
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
        let label = w
            .label
            .clone()
            .unwrap_or_else(|| format!("window {}", i + 1));
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
        sum.plan,
        windows,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(u.windows[1].label, "weekly");
        assert_eq!(u.windows[1].used_percent, 40.0);
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
}
