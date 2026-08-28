//! The usage **layout model** — one pure decision about ordering, naming,
//! tone, alignment and phrasing, shared by every surface that draws account
//! usage (the detail overlay and the panel section; THE-65).
//!
//! [`crate::usage`] owns the data: windows, accounts, resets, forecasts. This
//! module owns the *view*: which account leads, what a window is called in
//! plain language, how wide the name column is, what tone a percentage gets
//! under the **caller's** thresholds, and the exact `resets …` / `runs out …`
//! phrases. Plain data in, `String`s and enums out — no I/O, no clock (`now`
//! is a [`ViewOpts`] parameter), no substrate. Colours stay at the host
//! chokepoints: core states [`UsageTone`], the host maps it to a theme token.
//!
//! Two contracts here are load-bearing:
//!   * [`order`] returns **indices** — callers reorder a view, never the usage
//!     slice itself, whose order other consumers (the statusbar badge, the
//!     alert handler) key off.
//!   * [`MetricRow::history_key`] is byte-identical to the host's
//!     `format!("{account_key}#{window_label}")` history-map key — a mismatch
//!     there silently kills every sparkline and forecast.

use crate::usage::{self, AccountUsage, UsageState, UsageTone, UsageWindow};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Component, Path};
use unicode_width::UnicodeWidthStr;

/// How a surface wants its usage view shaped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewOpts {
    /// Epoch seconds; the caller passes [`crate::util::now`].
    pub now: i64,
    /// `[usage] warn_percent` — NOT the module defaults.
    pub warn_percent: f32,
    /// `[usage] crit_percent` — NOT the module defaults.
    pub crit_percent: f32,
    /// Only the peak window per account (the panel's resting width).
    pub peak_only: bool,
}

/// One limit's row: a padded plain-language name, a bar fill, a tone, and the
/// phrases that say what happens next.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricRow {
    /// Plain-language name, ALREADY padded to [`UsageView::name_w`] display
    /// cells.
    pub name: String,
    /// Right-aligned used percent, stable width: `" 94%"`, `"100%"`, `"  2%"`.
    pub pct: String,
    pub used_percent: f32,
    /// [`usage::used_frac`] of `used_percent` — the bar fill.
    pub frac: f32,
    pub tone: UsageTone,
    /// `"resets in 2h 14m"` — or `"resets now"` once elapsed, or empty when
    /// the provider stated no reset.
    pub resets: String,
    /// `"runs out in 3h 12m"` when [`usage::forecast_exhaustion`] yields one,
    /// else empty.
    pub forecast: String,
    /// `"{account key}#{window label}"` — the shared history-map key. Must
    /// stay byte-identical to what the sampler writes and the panel reads.
    pub history_key: String,
}

/// One account as a surface should show it: note, tone, facts line, rows.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountView {
    pub key: String,
    pub label: String,
    /// The plan (`"max"`), or `"unavailable: token expired"`, or `"…"` while
    /// loading, or `"no windows reported"`. Never empty for a non-Ok account.
    pub note: String,
    pub state: UsageState,
    /// Peak window's tone; `None` when there is nothing to tone (the caller
    /// draws those dim). Core states severity, the host picks the colour.
    pub tone: Option<UsageTone>,
    pub peak_percent: Option<f32>,
    /// One line: `org Acme · seat team_standard · regclaude2/.claude`.
    /// Empty when nothing is known.
    pub facts: String,
    pub rows: Vec<MetricRow>,
}

/// The full layout: accounts worst-first, one shared name-column width.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageView {
    /// Worst-first (see [`order`]).
    pub accounts: Vec<AccountView>,
    /// The one name-column width every row was padded to, in display cells.
    pub name_w: usize,
    /// `"8 accounts"` / `"1 account"`.
    pub summary: String,
}

/// Worst-first ordering over the accounts, as **indices into the input**.
///
/// Sort key: state rank (Ok-with-windows `0`, Ok-without-windows `1`,
/// `Loading` `2`, `Unavailable` `3`), then peak percent descending, then the
/// original index. The index tiebreak keeps equal accounts in discovery order
/// so the list does not flip between polls — the same reasoning as
/// [`usage::peak_across`]. The input slice is never cloned, reordered or
/// otherwise touched.
pub fn order(accounts: &[AccountUsage]) -> Vec<usize> {
    let mut ranked: Vec<(usize, u8, f32)> = accounts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let rank = match a.state {
                UsageState::Ok if !a.windows.is_empty() => 0,
                UsageState::Ok => 1,
                UsageState::Loading => 2,
                UsageState::Unavailable => 3,
            };
            // No windows to rank → sink to the bottom of the rank's band.
            let peak = a.peak_percent().unwrap_or(f32::NEG_INFINITY);
            (i, rank, peak)
        })
        .collect();
    ranked.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| b.2.total_cmp(&a.2)) // peak descending
            .then_with(|| a.0.cmp(&b.0)) // discovery order
    });
    ranked.into_iter().map(|(i, _, _)| i).collect()
}

/// The plain-language name for a window, from `window_minutes` first and the
/// provider label second.
///
/// A stated length renders as a whole number of minutes/hours/days —
/// `"5-hour window"`, `"45-minute window"` — never `"1.5-hour"`: minutes that
/// don't divide evenly into their unit fall back to the label. A model-scoped
/// qualifier in the label survives in parentheses (`"7d opus"` → `"7-day
/// window (opus)"`). With **no** stated length the provider's label passes
/// through verbatim (`"window 1"`, `"limit"`) — never invent a duration.
pub fn metric_name(w: &UsageWindow) -> String {
    // A zero length means "not stated" (same rule as `UsageWindow::with_len`).
    let Some(mins) = w.window_minutes.filter(|m| *m > 0) else {
        return w.label.clone();
    };
    let (unit, div) = if mins < 60 {
        ("minute", 1)
    } else if mins < 1440 {
        ("hour", 60)
    } else {
        ("day", 1440)
    };
    if mins % div != 0 {
        return w.label.clone();
    }
    let mut name = format!("{}-{unit} window", mins / div);
    if let Some(q) = qualifier(&w.label) {
        name.push_str(&format!(" ({q})"));
    }
    name
}

/// The model-scoped tail of a provider label, when there is one: `"7d opus"`
/// → `"opus"`, `"weekly Fable"` → `"Fable"`. The qualifier is whatever
/// remains after stripping a known leading base token; anything else
/// (`"window 1"`, `"limit"`) is not a qualifier.
fn qualifier(label: &str) -> Option<&str> {
    const BASE_TOKENS: [&str; 4] = ["session", "weekly", "5h", "7d"];
    let (head, rest) = label.split_once(' ')?;
    let rest = rest.trim();
    (BASE_TOKENS.contains(&head) && !rest.is_empty()).then_some(rest)
}

/// Build the whole view: select, name, measure, pad, tone, phrase, order.
///
/// `history` maps the shared `"{key}#{label}"` key to a window's samples
/// (`(epoch_secs, used_percent)`, oldest first) — the same map the sampler
/// writes. Accounts come back worst-first; nothing in the inputs is mutated.
pub fn build(
    accounts: &[AccountUsage],
    history: &BTreeMap<String, Vec<(i64, f32)>>,
    opts: &ViewOpts,
) -> UsageView {
    // Pass 1 — select each account's windows (all of them, or the peak only,
    // never `windows.first()`: a fresh 5-hour window at 2% must not hide a
    // 7-day window at 91%).
    let selected: Vec<Vec<&UsageWindow>> = accounts
        .iter()
        .map(|a| {
            if opts.peak_only {
                a.peak_window().into_iter().collect()
            } else {
                a.windows.iter().collect()
            }
        })
        .collect();
    // One width across every selected window of every account, in display
    // cells — that is what lines the bar/percent columns up down the screen.
    let name_w = selected
        .iter()
        .flatten()
        .map(|w| metric_name(w).width())
        .max()
        .unwrap_or(0);

    let summary = {
        let n = accounts.len();
        format!("{n} account{}", if n == 1 { "" } else { "s" })
    };

    let mut view = Vec::with_capacity(accounts.len());
    for i in order(accounts) {
        let a = &accounts[i];
        let rows = selected[i]
            .iter()
            .map(|w| {
                let history_key = format!("{}#{}", a.key, w.label);
                let hist = history.get(&history_key).map(Vec::as_slice).unwrap_or(&[]);
                let phrase = |prefix: &str, s: String| {
                    if s == "now" {
                        format!("{prefix} now")
                    } else {
                        format!("{prefix} in {s}")
                    }
                };
                MetricRow {
                    name: pad_to(&metric_name(w), name_w),
                    pct: format!("{:>3.0}%", w.used_percent.clamp(0.0, 100.0)),
                    used_percent: w.used_percent,
                    frac: usage::used_frac(w.used_percent),
                    tone: usage::tone_at(w.used_percent, opts.warn_percent, opts.crit_percent),
                    resets: usage::fmt_resets_in(w.resets_at, opts.now)
                        .map(|s| phrase("resets", s))
                        .unwrap_or_default(),
                    forecast: usage::forecast_exhaustion(hist, opts.now, w.resets_at)
                        .and_then(|eta| usage::fmt_resets_in(Some(eta), opts.now))
                        .map(|s| phrase("runs out", s))
                        .unwrap_or_default(),
                    history_key,
                }
            })
            .collect();
        view.push(AccountView {
            key: a.key.clone(),
            label: a.account_label.clone(),
            note: note_for(a),
            state: a.state,
            tone: a
                .peak_window()
                .map(|w| usage::tone_at(w.used_percent, opts.warn_percent, opts.crit_percent)),
            peak_percent: a.peak_percent(),
            facts: facts_for(a),
            rows,
        });
    }
    UsageView {
        accounts: view,
        name_w,
        summary,
    }
}

/// The note line for one account. Never empty for a non-Ok account: a Loading
/// row reads `"…"`, an Unavailable one reads `"unavailable: <reason>"`, and an
/// Ok row with no windows reads `"no windows reported"` rather than nothing.
fn note_for(a: &AccountUsage) -> String {
    match a.state {
        UsageState::Loading => "…".to_string(),
        UsageState::Unavailable => match a.note.as_deref().map(str::trim).filter(|n| !n.is_empty())
        {
            Some(reason) => format!("unavailable: {reason}"),
            None => "unavailable".to_string(),
        },
        UsageState::Ok if a.windows.is_empty() => "no windows reported".to_string(),
        UsageState::Ok => a.plan.clone().unwrap_or_default(),
    }
}

/// The identity/plan facts as **one line**, not a grid:
/// `org Acme · seat team_standard · tier default_claude_max_20x · regclaude2/.claude`.
///
/// The credential home is abbreviated to its last two path components — that
/// tail is the discriminator that tells two same-plan accounts apart, at a
/// tenth of the width. Absent and whitespace-only fields are skipped, so a
/// bare account gets an empty string rather than a column of "unknown".
/// The home is taken from the snapshot; `$HOME` is never read.
fn facts_for(a: &AccountUsage) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |label: &str, v: Option<&str>| {
        if let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) {
            parts.push(format!("{label} {v}"));
        }
    };
    push("org", a.org.as_deref());
    push("seat", a.seat_tier.as_deref());
    push("tier", a.rate_limit_tier.as_deref());
    if let Some(home) = a.home.as_deref() {
        let tail = home_tail(home);
        if !tail.is_empty() {
            parts.push(tail);
        }
    }
    parts.join(" · ")
}

/// The last two components of a path (`…/regclaude2/.claude` →
/// `regclaude2/.claude`); a shorter path renders whatever it has.
fn home_tail(home: &Path) -> String {
    let comps: Vec<&OsStr> = home
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    match comps.as_slice() {
        [] => String::new(),
        [only] => only.to_string_lossy().into_owned(),
        [.., a, b] => format!("{}/{}", a.to_string_lossy(), b.to_string_lossy()),
    }
}

/// Pad a name to `width` **display cells**, not chars — `format!("{:<n$}")`
/// counts chars and drifts on wide glyphs, the same reason the host's grid
/// drawing measures with `unicode-width`.
fn pad_to(name: &str, width: usize) -> String {
    let cells = name.width();
    let mut padded = String::with_capacity(name.len() + width.saturating_sub(cells));
    padded.push_str(name);
    for _ in cells..width {
        padded.push(' ');
    }
    padded
}

/// The legend parts, **unjoined** — the host joins them with the caps middot,
/// so no separator glyph is baked in here.
pub fn legend() -> &'static [&'static str] {
    &[
        "bar = share of the limit used",
        "% = used now",
        "worst first",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{DEFAULT_CRIT_PERCENT, DEFAULT_WARN_PERCENT};
    use std::path::PathBuf;

    fn opts(now: i64) -> ViewOpts {
        ViewOpts {
            now,
            warn_percent: DEFAULT_WARN_PERCENT,
            crit_percent: DEFAULT_CRIT_PERCENT,
            peak_only: false,
        }
    }

    fn win(label: &str, pct: f32, minutes: Option<u32>) -> UsageWindow {
        UsageWindow::with_len(label, pct, None, minutes)
    }

    fn acct(key: &str, label: &str, windows: Vec<UsageWindow>) -> AccountUsage {
        AccountUsage {
            key: key.to_string(),
            ..AccountUsage::ok("claude", label, None, windows)
        }
    }

    // --- order -------------------------------------------------------------

    #[test]
    fn order_is_worst_first_across_states_and_keeps_discovery_order() {
        let low = acct("a", "low", vec![win("5h", 10.0, Some(300))]);
        let high = acct("b", "high", vec![win("5h", 90.0, Some(300))]);
        let mid = acct("c", "mid", vec![win("5h", 50.0, Some(300))]);
        let loading = AccountUsage::loading("codex", "Codex");
        let broken = AccountUsage::unavailable("claude", "Claude", "not logged in");
        let bare = acct("d", "bare", vec![]); // Ok without windows
        let accounts = vec![low, high, mid, loading, broken, bare];
        let snapshot = accounts.clone();

        let order = order(&accounts);
        // Ok-with-windows worst first, by peak percent.
        assert_eq!(&order[..3], &[1, 2, 0]);
        // Ok-without-windows, then Loading, then Unavailable.
        assert_eq!(order[3], 5);
        assert_eq!(order[4], 3);
        assert_eq!(order[5], 4);
        // `order` returns indices; the input slice is untouched.
        assert_eq!(accounts, snapshot);
    }

    #[test]
    fn order_ties_keep_discovery_order() {
        let a = acct("a", "a", vec![win("5h", 50.0, Some(300))]);
        let b = acct("b", "b", vec![win("7d", 50.0, Some(10080))]);
        let c = acct("c", "c", vec![win("5h", 50.0, Some(300))]);
        assert_eq!(order(&[a, b, c]), vec![0, 1, 2]);
    }

    // --- metric_name ---------------------------------------------------------

    #[test]
    fn metric_name_speaks_plain_language() {
        let n = |label: &str, minutes: Option<u32>| {
            metric_name(&UsageWindow::with_len(label, 0.0, None, minutes))
        };
        assert_eq!(n("session", Some(300)), "5-hour window");
        assert_eq!(n("5h", Some(300)), "5-hour window");
        assert_eq!(n("weekly", Some(10080)), "7-day window");
        assert_eq!(n("7d", Some(10080)), "7-day window");
        assert_eq!(n("7d opus", Some(10080)), "7-day window (opus)");
        assert_eq!(n("weekly Fable", Some(10080)), "7-day window (Fable)");
        // Sub-hour lengths read in minutes.
        assert_eq!(n("5m", Some(45)), "45-minute window");
        // No stated length → the provider's label verbatim, never an invented
        // duration.
        assert_eq!(n("window 1", None), "window 1");
        assert_eq!(n("limit", None), "limit");
        // A zero length means "not stated" too.
        let zero = UsageWindow {
            window_minutes: Some(0),
            ..UsageWindow::new("odd", 0.0, None)
        };
        assert_eq!(metric_name(&zero), "odd");
        // Minutes that don't divide evenly into their unit fall back to the
        // label — no "1.5-hour".
        assert_eq!(n("session", Some(90)), "session");
    }

    // --- build: alignment ----------------------------------------------------

    #[test]
    fn build_pads_every_name_to_one_display_width() {
        let a = acct("a", "a", vec![win("7d opus", 10.0, Some(10080))]);
        // A verbatim label with a wide glyph: chars ≠ cells, and only cells
        // line the columns up.
        let b = acct("b", "b", vec![UsageWindow::new("枠", 30.0, None)]);
        let v = build(&[a, b], &BTreeMap::new(), &opts(0));
        assert_eq!(v.name_w, "7-day window (opus)".width());
        let mut saw_wide_glyph = false;
        for acc in &v.accounts {
            for row in &acc.rows {
                assert_eq!(row.name.width(), v.name_w, "padded in display cells");
                if row.name.chars().count() != row.name.width() {
                    saw_wide_glyph = true;
                }
            }
        }
        assert!(
            saw_wide_glyph,
            "the wide-glyph row is what this test is for"
        );
        // Worst first: b (30%) leads, a (10%) follows; the widest name carries
        // no trailing padding, everything else pads up to it.
        assert_eq!(v.accounts[0].rows[0].name, format!("枠{}", " ".repeat(17)));
        assert_eq!(v.accounts[1].rows[0].name, "7-day window (opus)");
    }

    // --- build: tone -----------------------------------------------------------

    #[test]
    fn build_tones_against_the_caller_s_thresholds() {
        let a = acct("a", "a", vec![win("5h", 70.0, Some(300))]);
        let defaults = opts(0);
        let configured = ViewOpts {
            warn_percent: 60.0,
            ..defaults
        };
        // 70% is green at the defaults (warn 75) and amber at warn 60 — the
        // same number must not have two colours between the surfaces.
        assert_eq!(
            build(&[a.clone()], &BTreeMap::new(), &defaults).accounts[0].rows[0].tone,
            UsageTone::Ok
        );
        assert_eq!(
            build(&[a.clone()], &BTreeMap::new(), &configured).accounts[0].rows[0].tone,
            UsageTone::Warn
        );
        // The account-level tone follows the same thresholds.
        assert_eq!(
            build(&[a], &BTreeMap::new(), &configured).accounts[0].tone,
            Some(UsageTone::Warn)
        );
    }

    // --- build: peak_only ------------------------------------------------------

    #[test]
    fn build_peak_only_takes_the_peak_window_not_the_first() {
        let a = acct(
            "a",
            "a",
            vec![win("5h", 2.0, Some(300)), win("7d", 91.0, Some(10080))],
        );
        let v = build(
            &[a],
            &BTreeMap::new(),
            &ViewOpts {
                peak_only: true,
                ..opts(0)
            },
        );
        assert_eq!(v.accounts[0].rows.len(), 1);
        assert_eq!(v.accounts[0].rows[0].used_percent, 91.0);
        assert_eq!(v.accounts[0].rows[0].name, "7-day window");
    }

    // --- build: resets -----------------------------------------------------------

    #[test]
    fn build_resets_phrases() {
        let now = 1_000_000;
        let a = acct(
            "a",
            "a",
            vec![
                UsageWindow::with_len("5h", 50.0, Some(now + 3600), Some(300)),
                UsageWindow::with_len("5h", 50.0, Some(now - 60), Some(300)),
                UsageWindow::with_len("5h", 50.0, None, Some(300)),
            ],
        );
        let v = build(&[a], &BTreeMap::new(), &opts(now));
        let resets: Vec<&str> = v.accounts[0]
            .rows
            .iter()
            .map(|r| r.resets.as_str())
            .collect();
        assert_eq!(resets, ["resets in 1h 0m", "resets now", ""]);
    }

    // --- build: forecast ---------------------------------------------------------

    #[test]
    fn build_forecast_phrases() {
        let now = 100_000;
        let climbing = vec![(now - 600, 50.0), (now, 60.0)]; // 10% over 600s
        let flat = vec![(now - 600, 50.0), (now, 50.0)];
        let brief = vec![(now - 60, 50.0), (now, 60.0)]; // span < 300s
        let mut history = BTreeMap::new();
        history.insert("a#5h".to_string(), climbing.clone());
        history.insert("a#7d".to_string(), climbing);
        history.insert("a#45m".to_string(), flat);
        history.insert("a#session".to_string(), brief);
        let a = acct(
            "a",
            "a",
            vec![
                win("5h", 60.0, Some(300)), // climbing, no reset → forecast
                UsageWindow::with_len("7d", 60.0, Some(now + 100), Some(10080)), // resets first
                win("45m", 60.0, Some(45)), // flat → no slope
                win("session", 60.0, Some(300)), // too brief
            ],
        );
        let v = build(&[a], &history, &opts(now));
        let forecasts: Vec<&str> = v.accounts[0]
            .rows
            .iter()
            .map(|r| r.forecast.as_str())
            .collect();
        // 40% left at 1%/60s → 2400s.
        assert_eq!(forecasts, ["runs out in 40m", "", "", ""]);
    }

    #[test]
    fn build_forecast_at_exhaustion_reads_runs_out_now() {
        let now = 100_000;
        let mut history = BTreeMap::new();
        history.insert("a#5h".to_string(), vec![(now - 600, 90.0), (now, 100.0)]);
        let a = acct("a", "a", vec![win("5h", 100.0, Some(300))]);
        let v = build(&[a], &history, &opts(now));
        // eta == now: "runs out in now" is not a sentence.
        assert_eq!(v.accounts[0].rows[0].forecast, "runs out now");
    }

    // --- history_key ---------------------------------------------------------

    #[test]
    fn history_key_matches_the_shared_format_and_finds_the_series() {
        let now = 100_000;
        let a = acct("claude:uuid-1", "a", vec![win("5h", 60.0, Some(300))]);
        let v = build(&[a.clone()], &BTreeMap::new(), &opts(now));
        let key = v.accounts[0].rows[0].history_key.clone();
        assert_eq!(key, "claude:uuid-1#5h");
        // A caller that inserts with `format!("{key}#{label}")` is found.
        let mut history = BTreeMap::new();
        history.insert(key, vec![(now - 600, 50.0), (now, 60.0)]);
        let v = build(&[a], &history, &opts(now));
        assert_eq!(v.accounts[0].rows[0].forecast, "runs out in 40m");
    }

    // --- facts -----------------------------------------------------------------

    #[test]
    fn facts_skip_what_is_unknown_and_keep_the_field_order() {
        let v = build(&[acct("a", "a", vec![])], &BTreeMap::new(), &opts(0));
        assert_eq!(v.accounts[0].facts, "");

        let rich = AccountUsage {
            org: Some("Acme".into()),
            seat_tier: Some("team_standard".into()),
            rate_limit_tier: Some("default_claude_max_20x".into()),
            home: Some(PathBuf::from(
                "/home/blake/.claude-profiles/regclaude2/.claude",
            )),
            ..acct("a", "a", vec![])
        };
        let v = build(&[rich.clone()], &BTreeMap::new(), &opts(0));
        assert_eq!(
            v.accounts[0].facts,
            "org Acme · seat team_standard · tier default_claude_max_20x · regclaude2/.claude"
        );

        // Whitespace-only is as good as absent; a one-component home renders
        // what it has; a root home contributes nothing.
        let trimmed = AccountUsage {
            org: Some("   ".into()),
            seat_tier: Some(" \t ".into()),
            home: Some(PathBuf::from("claude")),
            ..rich.clone()
        };
        let v = build(&[trimmed], &BTreeMap::new(), &opts(0));
        assert_eq!(v.accounts[0].facts, "tier default_claude_max_20x · claude");

        let rooted = AccountUsage {
            home: Some(PathBuf::from("/")),
            ..rich
        };
        let v = build(&[rooted], &BTreeMap::new(), &opts(0));
        assert_eq!(
            v.accounts[0].facts,
            "org Acme · seat team_standard · tier default_claude_max_20x"
        );
    }

    // --- notes / non-Ok accounts ------------------------------------------------

    #[test]
    fn notes_describe_the_state_and_never_vanish_for_non_ok_rows() {
        let planned = AccountUsage {
            plan: Some("max".into()),
            ..acct("a", "a", vec![win("5h", 10.0, Some(300))])
        };
        let bare = acct("b", "b", vec![]);
        let loading = AccountUsage::loading("codex", "Codex");
        let broken = AccountUsage::unavailable("claude", "Claude", "token expired");
        let silent = AccountUsage::unavailable("codex", "Codex", "");
        let accounts = [planned, bare, loading, broken, silent];
        let v = build(&accounts, &BTreeMap::new(), &opts(0));
        let notes: Vec<&str> = v.accounts.iter().map(|a| a.note.as_str()).collect();
        assert_eq!(
            notes,
            [
                "max",
                "no windows reported",
                "…",
                "unavailable: token expired",
                "unavailable"
            ]
        );
    }

    #[test]
    fn non_ok_accounts_have_no_rows_and_no_tone() {
        let loading = AccountUsage::loading("codex", "Codex");
        let broken = AccountUsage::unavailable("claude", "Claude", "not logged in");
        let v = build(&[loading, broken], &BTreeMap::new(), &opts(0));
        assert_eq!(v.name_w, 0);
        for a in &v.accounts {
            assert!(a.rows.is_empty());
            assert_eq!(a.tone, None);
            assert_eq!(a.peak_percent, None);
        }
    }

    // --- summary ----------------------------------------------------------------

    #[test]
    fn summary_counts_accounts() {
        let one = [acct("a", "a", vec![])];
        assert_eq!(build(&one, &BTreeMap::new(), &opts(0)).summary, "1 account");
        let two = [acct("a", "a", vec![]), acct("b", "b", vec![])];
        assert_eq!(
            build(&two, &BTreeMap::new(), &opts(0)).summary,
            "2 accounts"
        );
        let none: [AccountUsage; 0] = [];
        let v = build(&none, &BTreeMap::new(), &opts(0));
        assert!(v.accounts.is_empty());
        assert_eq!(v.summary, "0 accounts");
        assert_eq!(v.name_w, 0);
    }

    // --- legend -----------------------------------------------------------------

    #[test]
    fn legend_is_unjoined_parts_without_a_separator_glyph() {
        let parts = legend();
        assert!(!parts.is_empty());
        assert!(parts.iter().all(|p| !p.contains('·')));
    }
}
