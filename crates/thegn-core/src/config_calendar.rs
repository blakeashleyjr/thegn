//! The `[calendar]` config family — the month popup's display settings, the
//! `[[calendar.clocks]]` world clocks, and the `[[calendar.accounts]]` event
//! sources. Kept in a sibling module (rather than the god-file `config.rs`) per
//! the keep-god-files-flat guidance; `config.rs` re-exports everything here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::calendar::{ResolvedClock, resolve_zone};
use crate::config::{config_enum, config_warn};

/// The floor on any calendar refresh interval, in seconds.
///
/// Applied in [`CalendarAccount::refresh_secs`] rather than at the ticker, so
/// *every* caller inherits it and a misconfigured `0` can never spin a poll
/// loop against a provider's rate limit. (The `[pr_queue] poll_secs` lesson,
/// moved one layer up.)
pub const MIN_REFRESH_SECS: u64 = 60;

/// `[calendar]` — the calendar popup, its world clocks, and its event sources.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CalendarConfig {
    // --- scalars (must serialize before any sub-table or array for TOML) ---
    /// Master switch for event fetching. The month grid and world clocks need
    /// no provider, so the popup stays useful with this off — only the sync
    /// and the agenda go away.
    pub enabled: bool,
    /// Which day a week starts on: `auto` (from the locale), `monday`,
    /// `sunday`, or `saturday`.
    pub week_start: WeekStart,
    /// Clock display: `auto` (from the locale), `12`, or `24`.
    pub time_format: TimeFormat,
    /// IANA zone to treat as "home" for day boundaries and clock deltas.
    /// Empty means the system's local zone.
    pub home_zone: String,
    /// Show the configured-start week-number gutter down the left of the grid.
    /// Week 1 is the configured-start week containing January 4; Monday-first
    /// is ISO-8601, while Sunday- and Saturday-first use their row boundary.
    /// `auto` uses the locale-resolved `week_start` boundary.
    pub show_week_numbers: bool,
    /// Always render six week-rows, padding short months.
    ///
    /// On by default: a popup whose height changes as you page Feb→Mar is
    /// geometry damage, which forces a full frame and visibly jitters the box.
    pub show_six_weeks: bool,
    /// Show the per-day agenda block under the grid.
    pub show_agenda: bool,
    /// Agenda rows shown before the popup scrolls.
    pub agenda_rows: usize,
    /// Mark days that have events with a dot in the grid.
    pub show_event_markers: bool,
    /// Cache lifetime (seconds) before a background re-fetch is worthwhile.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub ttl_secs: u64,
    /// Default seconds between syncs, for accounts that don't set their own.
    /// Floored at [`MIN_REFRESH_SECS`].
    #[schemars(range(max = "crate::time_policy::MAX_CADENCE_SECS"))]
    pub refresh_interval_secs: u64,
    /// Admission budget: the most events (plus deletions) in the sync window
    /// one account fetch may bring in, between 1 and 10000; the account's
    /// byte budget scales with it. A source over the cap is refused whole —
    /// its previous cache and sync cursor are kept and a warning names the
    /// account — rather than truncated. 0 is rejected by validation and runs
    /// as the default; it never means unlimited.
    #[schemars(range(min = 1, max = 10000))]
    pub max_events: usize,
    /// How far back to fetch and keep events.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_DAYS"))]
    pub horizon_past_days: u32,
    /// How far ahead to fetch events.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_DAYS"))]
    pub horizon_future_days: u32,
    /// Raise notifications ahead of events that carry reminders.
    pub reminders_enabled: bool,
    /// Reminder offsets (minutes before start) for events whose source supplies
    /// none of their own.
    pub reminder_default_mins: Vec<u32>,

    // --- arrays of tables (must serialize after scalars) ---
    /// `[[calendar.clocks]]` — the world-clock rows.
    pub clocks: Vec<WorldClock>,
    /// `[[calendar.accounts]]` — where events come from.
    pub accounts: Vec<CalendarAccount>,
}

impl Default for CalendarConfig {
    fn default() -> Self {
        CalendarConfig {
            enabled: true,
            week_start: WeekStart::Auto,
            time_format: TimeFormat::Auto,
            home_zone: String::new(),
            show_week_numbers: false,
            show_six_weeks: true,
            show_agenda: true,
            agenda_rows: 6,
            show_event_markers: true,
            ttl_secs: 900,
            refresh_interval_secs: 900,
            max_events: crate::calendar::admission::DEFAULT_MAX_EVENTS,
            horizon_past_days: 90,
            horizon_future_days: 365,
            reminders_enabled: true,
            reminder_default_mins: vec![10],
            // Empty by default: a user who configures no clocks still gets the
            // synthesized home row, so the block is never blank.
            clocks: Vec::new(),
            accounts: Vec::new(),
        }
    }
}

impl CalendarConfig {
    /// Enabled accounts with a real provider.
    pub fn active_accounts(&self) -> Vec<CalendarAccount> {
        if !self.enabled {
            return Vec::new();
        }
        self.accounts
            .iter()
            .filter(|a| a.enabled && a.provider != CalendarProviderKind::None)
            .cloned()
            .collect()
    }

    /// The typed per-account admission budget every calendar backend is
    /// built with.
    ///
    /// An out-of-range `max_events` is adjusted (a legacy 0 becomes the
    /// default, never unlimited; above the ceiling becomes the ceiling) and
    /// warned about; `thegn config validate` reports it as an error.
    pub fn admission_budget(&self) -> crate::calendar::AdmissionBudget {
        let (budget, problem) = crate::calendar::AdmissionBudget::clamped(self.max_events);
        if let Some(problem) = problem {
            config_warn(&format!(
                "calendar.max_events: {problem} — using {}",
                budget.max_events()
            ));
        }
        budget
    }

    /// The configured clocks whose zone this build's tzdb actually knows.
    ///
    /// An unknown zone warns and is dropped rather than failing startup — a
    /// typo costs one clock row, the same warn-and-skip contract unknown bar
    /// widget ids get. `thegn config validate` reports it properly, with a
    /// did-you-mean.
    pub fn active_clocks(&self) -> Vec<ResolvedClock> {
        self.clocks
            .iter()
            .filter(|c| c.enabled)
            .filter_map(|c| match resolve_zone(&c.zone) {
                Some(zone) => Some(ResolvedClock {
                    label: c.label.clone(),
                    zone,
                    format: c.format.clone(),
                    is_home: false,
                }),
                None => {
                    config_warn(&format!(
                        "calendar.clocks: unknown IANA time zone {:?} — skipping this clock",
                        c.zone
                    ));
                    None
                }
            })
            .collect()
    }

    /// The home zone, or `None` to mean "whatever the system's local zone is".
    ///
    /// A configured-but-unknown zone warns and falls back, rather than silently
    /// pretending the user asked for local.
    pub fn home_zone(&self) -> Option<chrono_tz::Tz> {
        if self.home_zone.trim().is_empty() {
            return None;
        }
        match resolve_zone(&self.home_zone) {
            Some(z) => Some(z),
            None => {
                config_warn(&format!(
                    "calendar.home_zone: unknown IANA time zone {:?} — using the system zone",
                    self.home_zone
                ));
                None
            }
        }
    }

    /// Seconds between calendar syncs, or `None` when nothing is configured —
    /// in which case the ticker emits no calendar slot at all and a user
    /// without a calendar pays nothing.
    pub fn poll_secs(&self) -> Option<u64> {
        let accounts = self.active_accounts();
        if accounts.is_empty() {
            return None;
        }
        // The ticker runs one pass for all accounts, so it must tick at the
        // shortest interval any of them asks for.
        accounts.iter().map(|a| a.refresh_secs(self)).min()
    }

    /// Configured week start, or `None` for `auto`.
    pub fn week_start_pref(&self) -> Option<chrono::Weekday> {
        match self.week_start {
            WeekStart::Auto => None,
            WeekStart::Monday => Some(chrono::Weekday::Mon),
            WeekStart::Sunday => Some(chrono::Weekday::Sun),
            WeekStart::Saturday => Some(chrono::Weekday::Sat),
        }
    }

    /// Configured 12-hour preference, or `None` for `auto`.
    pub fn twelve_hour_pref(&self) -> Option<bool> {
        match self.time_format {
            TimeFormat::Auto => None,
            TimeFormat::H12 => Some(true),
            TimeFormat::H24 => Some(false),
        }
    }

    /// Reminder offsets to use for an event whose source supplied none.
    pub fn default_reminders(&self) -> Vec<crate::calendar::Reminder> {
        self.reminder_default_mins
            .iter()
            .map(|m| crate::calendar::Reminder { minutes_before: *m })
            .collect()
    }
}

/// A `[[calendar.clocks]]` entry — one world-clock row in the popup.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WorldClock {
    /// Display label. Empty derives one from the zone (`America/New_York` →
    /// `New York`).
    pub label: String,
    /// IANA zone name, e.g. `"Asia/Tokyo"`. Required.
    pub zone: String,
    /// strftime override for this row; empty inherits `[calendar] time_format`.
    pub format: String,
    /// Show this clock? Disabled rows stay in config but are skipped.
    pub enabled: bool,
    /// Always show the date, not just the ±1d marker when it differs from home.
    pub show_date: bool,
}

impl Default for WorldClock {
    fn default() -> Self {
        WorldClock {
            label: String::new(),
            zone: String::new(),
            format: String::new(),
            // True so an entry that omits it still shows (serde container
            // `default` fills missing fields from this impl).
            enabled: true,
            show_date: false,
        }
    }
}

/// A `[[calendar.accounts]]` entry — one event source.
///
/// One flat struct with the union of every provider's fields, deliberately not
/// an internally-tagged enum: it matches `[[issue_accounts]]`, keeps the TOML
/// flat, and means only the fields relevant to `provider` are read (the rest
/// stay at their empty default).
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CalendarAccount {
    /// Stable id for this source, e.g. `"work"`. Also the cache key, so
    /// renaming an account orphans its cached rows until the next sync.
    pub name: String,
    /// Which backend this account talks to.
    pub provider: CalendarProviderKind,
    /// Sync this account? Disabled entries stay in config but are skipped.
    pub enabled: bool,
    /// Semantic hue for this source's events (`"teal"`, `"amber"`, …). Empty
    /// picks one by position. Never an RGB value — the theme resolves it.
    pub color: String,
    /// Refuse writes to this source. True by default; this pass has no writing
    /// backend at all, so it is currently belt-and-braces.
    pub read_only: bool,
    /// Seconds between syncs; `0` inherits `[calendar] refresh_interval_secs`.
    /// Always floored at [`MIN_REFRESH_SECS`].
    #[schemars(range(max = "crate::time_policy::MAX_CADENCE_SECS"))]
    pub refresh_interval_secs: u64,

    /// `ics`: path to a `.ics` file, or to a directory of them (which is the
    /// vdir layout vdirsyncer and khal already write).
    pub path: String,
    /// `ics_url`: an HTTP(S) or `webcal://` subscription URL. Webcal is
    /// normalized to HTTPS during validation.
    /// `caldav`: an HTTP(S) collection URL.
    pub url: String,
    /// `caldav`: username for Basic auth.
    pub username: String,
    /// `ics_url` / `caldav`: secret ref (`"env:VAR"` / `"file:PATH"`), resolved
    /// at fetch time. Note the URL itself is often a credential too.
    pub token: String,
    /// Permit intentional local/LAN calendar destinations. This is a trusted
    /// security boundary; unspecified, multicast, and broadcast destinations
    /// remain refused, and ambient proxies are never used.
    pub allow_private_network: bool,
    /// `caldav`: restrict to specific collections; empty means all.
    pub calendar_ids: Vec<String>,

    /// `command`: argv of the plugin to run. Argv, not a shell string, so no
    /// quoting rules are involved.
    pub command: Vec<String>,
    /// `command`: working directory; empty means inherit.
    pub cwd: String,
    /// `command`: extra environment for the child.
    pub env: BTreeMap<String, String>,
    /// `command`: capabilities granted to this plugin (`"network:example.com"`,
    /// `"run:khal"`). A plugin requesting more than this is denied and audited.
    pub capabilities: Vec<String>,
    /// Seconds before a fetch is abandoned and the child's process group killed.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub timeout_secs: u64,
}

impl std::fmt::Debug for CalendarAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Subscription URLs frequently contain credentials in their query.
        // Keep config diagnostics useful without ever making Debug a secret
        // disclosure path.
        f.debug_struct("CalendarAccount")
            .field("name", &self.name)
            .field("provider", &self.provider)
            .field("enabled", &self.enabled)
            .field("color", &self.color)
            .field("read_only", &self.read_only)
            .field("refresh_interval_secs", &self.refresh_interval_secs)
            .field("path", &self.path)
            .field(
                "url",
                &if self.url.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field(
                "username",
                &if self.username.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field(
                "token",
                &if self.token.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("allow_private_network", &self.allow_private_network)
            .field("calendar_ids", &self.calendar_ids)
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("capabilities", &self.capabilities)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl Default for CalendarAccount {
    fn default() -> Self {
        CalendarAccount {
            name: String::new(),
            provider: CalendarProviderKind::None,
            enabled: true,
            color: String::new(),
            read_only: true,
            refresh_interval_secs: 0,
            path: String::new(),
            url: String::new(),
            username: String::new(),
            token: String::new(),
            allow_private_network: false,
            calendar_ids: Vec::new(),
            command: Vec::new(),
            cwd: String::new(),
            env: BTreeMap::new(),
            capabilities: Vec::new(),
            timeout_secs: 20,
        }
    }
}

impl CalendarAccount {
    /// This account's sync interval, inheriting from `[calendar]` and always
    /// floored at [`MIN_REFRESH_SECS`].
    pub fn refresh_secs(&self, cfg: &CalendarConfig) -> u64 {
        let v = if self.refresh_interval_secs > 0 {
            self.refresh_interval_secs
        } else {
            cfg.refresh_interval_secs
        };
        v.max(MIN_REFRESH_SECS)
    }

    /// Whether syncing this account needs the network.
    ///
    /// Load-bearing for the offline gate: unlike issues or PRs, a calendar
    /// account may be a local file or a subprocess, which must keep syncing
    /// while offline. The refresher gates per account on this rather than
    /// skipping the whole pass.
    pub fn is_network_backed(&self) -> bool {
        matches!(
            self.provider,
            CalendarProviderKind::IcsUrl | CalendarProviderKind::CalDav
        )
    }

    /// `"<provider>:<name>"` — the source id events from this account carry.
    pub fn source_id(&self) -> crate::calendar::SourceId {
        crate::calendar::SourceId(format!("{}:{}", self.provider.as_str(), self.name))
    }

    /// The configured hue, if it names one.
    pub fn hue(&self) -> Option<crate::theme::Hue> {
        match self.color.trim().to_ascii_lowercase().as_str() {
            "teal" => Some(crate::theme::Hue::Teal),
            "magenta" => Some(crate::theme::Hue::Magenta),
            "purple" => Some(crate::theme::Hue::Purple),
            "green" => Some(crate::theme::Hue::Green),
            "amber" => Some(crate::theme::Hue::Amber),
            "red" => Some(crate::theme::Hue::Red),
            "blue" => Some(crate::theme::Hue::Blue),
            "orange" => Some(crate::theme::Hue::Orange),
            _ => None,
        }
    }
}

config_enum! {
    /// Where a calendar account's events come from.
    pub enum CalendarProviderKind : "calendar provider" {
        None    = "none",
        Ics     = "ics",
        IcsUrl  = "ics_url" | "webcal" | "url",
        CalDav  = "caldav",
        Command = "command" | "exec" | "subprocess",
    } default = None;
}

config_enum! {
    /// Which day the calendar's week starts on.
    pub enum WeekStart : "week start" {
        Auto     = "auto",
        Monday   = "monday" | "mon",
        Sunday   = "sunday" | "sun",
        Saturday = "saturday" | "sat",
    } default = Auto;
}

config_enum! {
    /// 12- or 24-hour clock display.
    pub enum TimeFormat : "time format" {
        Auto = "auto",
        H12  = "12" | "12h" | "h12",
        H24  = "24" | "24h" | "h24",
    } default = Auto;
}

/// Validate `[calendar]`, returning one message per problem.
///
/// Zone names can't be a `config_enum!` — there are ~600 of them, the schema
/// would balloon, and the list rots with every tzdb update — so they are
/// checked here instead, with a did-you-mean built from the bundled database.
pub fn validate_calendar(cfg: &CalendarConfig) -> Vec<String> {
    let mut out = Vec::new();
    if let Err(problem) = crate::calendar::AdmissionBudget::new(cfg.max_events) {
        out.push(format!("calendar.max_events: {problem}"));
    }
    if !cfg.home_zone.trim().is_empty() && resolve_zone(&cfg.home_zone).is_none() {
        out.push(format!(
            "calendar.home_zone: unknown IANA time zone {:?}{}",
            cfg.home_zone,
            did_you_mean(&cfg.home_zone)
        ));
    }
    for (i, c) in cfg.clocks.iter().enumerate() {
        if c.zone.trim().is_empty() {
            out.push(format!(
                "calendar.clocks[{i}].zone: required (an IANA zone name)"
            ));
        } else if resolve_zone(&c.zone).is_none() {
            out.push(format!(
                "calendar.clocks[{i}].zone: unknown IANA time zone {:?}{}",
                c.zone,
                did_you_mean(&c.zone)
            ));
        }
        if !c.format.is_empty()
            && let Err(e) = crate::config::validate_strftime(&c.format)
        {
            out.push(format!("calendar.clocks[{i}].format: {e}"));
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for (i, a) in cfg.accounts.iter().enumerate() {
        if a.name.trim().is_empty() {
            out.push(format!(
                "calendar.accounts[{i}].name: required (the cache key)"
            ));
        } else if seen.contains(&a.name.as_str()) {
            // Duplicates would silently share cache rows and clobber each other.
            out.push(format!(
                "calendar.accounts[{i}].name: duplicate {:?} — names are cache keys and must be unique",
                a.name
            ));
        } else {
            seen.push(&a.name);
        }
        match a.provider {
            CalendarProviderKind::Ics if a.path.trim().is_empty() => out.push(format!(
                "calendar.accounts[{i}].path: required for provider \"ics\""
            )),
            CalendarProviderKind::IcsUrl | CalendarProviderKind::CalDav => {
                if a.url.trim().is_empty() {
                    out.push(format!(
                        "calendar.accounts[{i}].url: required for provider {:?}",
                        a.provider.as_str()
                    ));
                } else if let Err(reason) = validate_remote_calendar_url(
                    &a.url,
                    matches!(a.provider, CalendarProviderKind::IcsUrl),
                ) {
                    out.push(format!("calendar.accounts[{i}].url: {reason}"));
                }
            }
            CalendarProviderKind::Command if a.command.is_empty() => out.push(format!(
                "calendar.accounts[{i}].command: required for provider \"command\""
            )),
            _ => {}
        }
        for c in &a.capabilities {
            if !c.contains(':') {
                out.push(format!(
                    "calendar.accounts[{i}].capabilities: {c:?} must be \"kind:target\", e.g. \"run:khal\""
                ));
            }
        }
    }
    out
}

/// Parse and normalize a remote calendar URL without echoing it in a
/// diagnostic. Query strings are intentionally retained because subscribed
/// feeds commonly use signed query credentials, but userinfo and fragments
/// are rejected so they cannot become an accidental secret/logging surface.
pub fn normalize_remote_calendar_url(
    raw: &str,
    allow_webcal: bool,
) -> Result<String, &'static str> {
    let raw = raw.trim();
    // `Url::set_scheme` intentionally refuses a non-special → special
    // transition.  Normalize the legacy webcal spelling before parsing so the
    // resulting URL is a normal HTTPS URL while retaining its authority,
    // path, query, and percent-encoding exactly as the URL parser defines them.
    let parse_raw = match raw.split_once(':') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("webcal") => {
            if !allow_webcal {
                return Err("must use http:// or https:// (webcal:// is accepted for ics_url)");
            }
            format!("https:{rest}")
        }
        _ => raw.to_owned(),
    };
    let url = url::Url::parse(&parse_raw).map_err(|_| "must be a valid URL")?;
    let scheme = url.scheme().to_ascii_lowercase();
    let accepted = matches!(scheme.as_str(), "http" | "https");
    if !accepted {
        return Err("must use http:// or https:// (webcal:// is accepted for ics_url)");
    }
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("must not contain URL userinfo or a fragment");
    }
    Ok(url.to_string())
}

fn validate_remote_calendar_url(raw: &str, allow_webcal: bool) -> Result<(), &'static str> {
    normalize_remote_calendar_url(raw, allow_webcal).map(|_| ())
}

/// Advisory display diagnostics. Raw account names remain semantic cache keys;
/// legacy labels are accepted, and every draw still applies the safe projection.
pub fn display_warnings(cfg: &CalendarConfig) -> Vec<String> {
    let mut out = Vec::new();
    for (i, c) in cfg.clocks.iter().enumerate() {
        if crate::calendar::display::DisplayText::new(
            &c.label,
            crate::calendar::display::Field::ClockLabel,
        )
        .as_str()
            != c.label
        {
            out.push(format!("calendar.clocks[{i}].label: display text contains controls or exceeds the 64 scalar/cell budget; it will be sanitized"));
        }
    }
    for (i, a) in cfg.accounts.iter().enumerate() {
        if crate::calendar::display::DisplayText::new(
            &a.name,
            crate::calendar::display::Field::Calendar,
        )
        .as_str()
            != a.name
        {
            out.push(format!("calendar.accounts[{i}].name: display text contains controls or exceeds the 64 scalar/cell budget; it will be sanitized"));
        }
    }
    out
}

/// `, did you mean "America/New_York"?` — or nothing when we have no idea.
fn did_you_mean(name: &str) -> String {
    match crate::calendar::tz::suggest_zones(name, 3).as_slice() {
        [] => String::new(),
        [one] => format!(", did you mean {one:?}?"),
        many => format!(
            ", did you mean one of {}?",
            many.iter()
                .map(|s| format!("{s:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
#[path = "config_calendar_tests.rs"]
mod tests;
