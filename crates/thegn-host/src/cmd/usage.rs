//! `thegn usage [--json] [--tokens]` — the per-account AI usage snapshot the
//! overlay and statusbar badge show, for scripts and terminals.
//!
//! Read-only client of [`thegn_svc::usage::gather`], the same call the UI
//! makes: it obeys `[usage] enabled` and `[usage] allow_network` and never
//! errors (an unreadable account is a row with a note). No credential is read
//! or printed here; every field below already lives on `AccountUsage`.
//!
//! `--json` emits one document (`UsageReport`) with these stable fields:
//! - `generated_ms`: epoch ms of the snapshot.
//! - `enabled`: `[usage] enabled`. `false` means `accounts` is empty and
//!   `tokens` is `null`.
//! - `accounts[]`: `provider`, `account` (display label), `plan`, `state`
//!   (`"ok"` | `"loading"` | `"unavailable"`), `note`, `email`, `org`, `home`
//!   (credential home path, a string), `windows[]`.
//! - `accounts[].windows[]`: `label`, `used_pct` (0–100), `resets_at` (epoch
//!   **seconds**, `null` when unknown).
//! - `tokens` (only with `--tokens`, `null` otherwise or when rollups are
//!   off): `input`, `output`, `cache_read`, `cache_creation`, `thinking`,
//!   `records`, `skipped` (non-zero means the totals are a floor). Host-wide:
//!   transcripts carry no account, so these are never per-account.

use anyhow::Result;
use std::fmt::Write as _;
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_core::pipeline_report::display_cell;
use thegn_core::usage::{AccountUsage, UsageState, fmt_resets_in, fmt_tokens};

/// Printed (plain mode) when `[usage] enabled = false`.
const DISABLED_NOTE: &str = "usage tracking is off ([usage] enabled = false)";

#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// Emit one JSON document instead of the table.
    #[arg(long)]
    pub json: bool,
    /// Also report host-wide token totals from local transcripts.
    #[arg(long)]
    pub tokens: bool,
}

#[derive(serde::Serialize)]
struct WindowRow {
    label: String,
    used_pct: f32,
    resets_at: Option<i64>,
}

#[derive(serde::Serialize)]
struct AccountRow {
    provider: String,
    account: String,
    plan: Option<String>,
    state: &'static str,
    note: Option<String>,
    email: Option<String>,
    org: Option<String>,
    windows: Vec<WindowRow>,
    home: Option<String>,
}

#[derive(serde::Serialize)]
struct TokensRow {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    thinking: u64,
    records: usize,
    skipped: usize,
}

#[derive(serde::Serialize)]
struct UsageReport {
    generated_ms: i64,
    enabled: bool,
    accounts: Vec<AccountRow>,
    tokens: Option<TokensRow>,
}

fn state_name(state: UsageState) -> &'static str {
    match state {
        UsageState::Ok => "ok",
        UsageState::Loading => "loading",
        UsageState::Unavailable => "unavailable",
    }
}

fn tokens_row(result: &thegn_svc::usage::RollupResult) -> TokensRow {
    let t = &result.rollup.total;
    TokensRow {
        input: t.input,
        output: t.output,
        cache_read: t.cache_read,
        cache_creation: t.cache_creation,
        thinking: t.thinking,
        records: result.rollup.records,
        skipped: result.skipped,
    }
}

fn build_report(
    enabled: bool,
    accounts: &[AccountUsage],
    tokens: Option<TokensRow>,
    now_ms: i64,
) -> UsageReport {
    let accounts = accounts
        .iter()
        .map(|a| AccountRow {
            provider: a.provider.clone(),
            account: a.account_label.clone(),
            plan: a.plan.clone(),
            state: state_name(a.state),
            note: a.note.clone(),
            email: a.email.clone(),
            org: a.org.clone(),
            windows: a
                .windows
                .iter()
                .map(|w| WindowRow {
                    label: w.label.clone(),
                    used_pct: w.used_percent,
                    resets_at: w.resets_at,
                })
                .collect(),
            home: a.home.as_ref().map(|h| h.display().to_string()),
        })
        .collect();
    UsageReport {
        generated_ms: now_ms,
        enabled,
        accounts,
        tokens,
    }
}

/// Left-aligned columns, two spaces apart, trailing spaces trimmed.
fn table(rows: &[Vec<String>]) -> String {
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

/// One row per account: provider, label, plan, state, peak `used %`, reset
/// countdown. Unavailable rows carry their note in the last column. Every
/// untrusted string goes through `display_cell`.
fn render_table(accounts: &[AccountUsage], now_secs: i64) -> String {
    let mut rows = vec![
        ["PROVIDER", "ACCOUNT", "PLAN", "STATE", "USED", "RESETS"]
            .iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<_>>(),
    ];
    for a in accounts {
        let peak = a.peak_window();
        let used = peak.map_or_else(|| "-".to_string(), |w| format!("{:.0}%", w.used_percent));
        let resets = peak
            .and_then(|w| fmt_resets_in(w.resets_at, now_secs))
            .unwrap_or_else(|| "-".to_string());
        let last = match (a.state, a.note.as_deref()) {
            (UsageState::Unavailable, Some(note)) => display_cell(note),
            _ => resets,
        };
        rows.push(vec![
            display_cell(&a.provider),
            display_cell(&a.account_label),
            display_cell(a.plan.as_deref().unwrap_or("-")),
            state_name(a.state).to_string(),
            used,
            last,
        ]);
    }
    table(&rows)
}

fn render_tokens(t: &TokensRow) -> String {
    let mut line = format!(
        "tokens (host-wide, recent transcripts): input {}  output {}  cache read {}  cache write {}  thinking {}  from {} responses",
        fmt_tokens(t.input),
        fmt_tokens(t.output),
        fmt_tokens(t.cache_read),
        fmt_tokens(t.cache_creation),
        fmt_tokens(t.thinking),
        t.records,
    );
    if t.skipped > 0 {
        write!(line, " (floor: {} transcripts skipped)", t.skipped)
            .expect("writing to String cannot fail");
    }
    line
}

pub fn run(cfg: &Config, args: Args) -> Result<()> {
    let now_ms = thegn_core::util::now_ms();
    if !cfg.usage.enabled {
        if args.json {
            return super::emit_json(&build_report(false, &[], None, now_ms));
        }
        outln!("{DISABLED_NOTE}");
        return Ok(());
    }
    let accounts = thegn_svc::usage::gather(&cfg.usage);
    let tokens = if args.tokens {
        thegn_svc::usage::token_rollup(&cfg.usage)
            .as_ref()
            .map(tokens_row)
    } else {
        None
    };
    if args.json {
        return super::emit_json(&build_report(true, &accounts, tokens, now_ms));
    }
    if accounts.is_empty() {
        outln!("no AI accounts found");
    } else {
        outln!("{}", render_table(&accounts, now_ms / 1000));
    }
    if args.tokens {
        match &tokens {
            Some(t) => outln!("{}", render_tokens(t)),
            None => outln!("token rollups are off ([usage] token_rollups = false)"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::usage::UsageWindow;

    fn fixture() -> Vec<AccountUsage> {
        let mut ok = AccountUsage::ok(
            "claude",
            "me@example.com",
            Some("max".into()),
            vec![
                UsageWindow::new("5h", 40.0, Some(1_000 + 3_600)),
                UsageWindow::new("weekly", 87.0, Some(1_000 + 90_000)),
            ],
        )
        .with_home("/home/x/.claude");
        ok.email = Some("me@example.com".into());
        ok.org = Some("Org".into());
        vec![
            ok,
            AccountUsage::unavailable("codex", "codex", "not logged in"),
            AccountUsage::loading("antigravity", "ag"),
        ]
    }

    #[test]
    fn json_report_covers_ok_unavailable_loading() {
        let report = build_report(true, &fixture(), None, 42);
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["generated_ms"], 42);
        assert!(v["tokens"].is_null());
        let a = &v["accounts"];
        assert_eq!(a[0]["state"], "ok");
        assert_eq!(a[0]["provider"], "claude");
        assert_eq!(a[0]["account"], "me@example.com");
        assert_eq!(a[0]["plan"], "max");
        assert_eq!(a[0]["org"], "Org");
        assert_eq!(a[0]["home"], "/home/x/.claude");
        assert_eq!(a[0]["windows"][1]["label"], "weekly");
        assert_eq!(a[0]["windows"][1]["used_pct"], 87.0);
        assert_eq!(a[0]["windows"][0]["resets_at"], 4_600);
        assert_eq!(a[1]["state"], "unavailable");
        assert_eq!(a[1]["note"], "not logged in");
        assert!(a[1]["windows"].as_array().unwrap().is_empty());
        assert_eq!(a[2]["state"], "loading");
    }

    #[test]
    fn disabled_report_is_empty_and_flagged() {
        let v = serde_json::to_value(build_report(false, &[], None, 1)).unwrap();
        assert_eq!(v["enabled"], false);
        assert!(v["accounts"].as_array().unwrap().is_empty());
        assert!(v["tokens"].is_null());
        assert_eq!(
            DISABLED_NOTE,
            "usage tracking is off ([usage] enabled = false)"
        );
    }

    #[test]
    fn table_aligns_columns_and_shows_peak_window() {
        let text = render_table(&fixture(), 1_000);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("PROVIDER"));
        // Peak window is weekly (87%), resetting in 1d 1h.
        assert!(lines[1].contains("87%"), "{}", lines[1]);
        assert!(lines[1].contains("1d 1h"), "{}", lines[1]);
        assert!(lines[2].ends_with("not logged in"), "{}", lines[2]);
        // The STATE column starts at the same offset on every row.
        let col = lines[0].find("STATE").unwrap();
        assert_eq!(&lines[1][col..col + 2], "ok");
        assert_eq!(&lines[2][col..col + 11], "unavailable");
        assert_eq!(&lines[3][col..col + 7], "loading");
    }

    #[test]
    fn table_sanitizes_escape_laden_labels() {
        let rows = vec![AccountUsage::unavailable(
            "claude",
            "evil\x1b]52;c;ZQ==\x07",
            "bad\x1b[2J",
        )];
        let text = render_table(&rows, 0);
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\x07'));
        assert!(text.contains('\u{fffd}'));
    }

    #[test]
    fn tokens_line_notes_floor_only_when_skipped() {
        let mut t = TokensRow {
            input: 1_500,
            output: 2_000_000,
            cache_read: 0,
            cache_creation: 0,
            thinking: 0,
            records: 7,
            skipped: 0,
        };
        let plain = render_tokens(&t);
        assert!(plain.contains("input 1.5k"), "{plain}");
        assert!(plain.contains("from 7 responses"), "{plain}");
        assert!(!plain.contains("floor"));
        t.skipped = 3;
        assert!(render_tokens(&t).ends_with("(floor: 3 transcripts skipped)"));
    }

    fn parse(args: &[&str]) -> Args {
        use clap::Parser as _;
        match crate::Cli::try_parse_from(args).map(|c| c.command) {
            Ok(Some(crate::Command::Usage(a))) => a,
            _ => panic!("expected a usage command"),
        }
    }

    #[test]
    fn usage_flags_parse_independently() {
        let a = parse(&["thegn", "usage"]);
        assert!(!a.json && !a.tokens);
        let a = parse(&["thegn", "usage", "--json"]);
        assert!(a.json && !a.tokens);
        let a = parse(&["thegn", "usage", "--tokens"]);
        assert!(!a.json && a.tokens);
        let a = parse(&["thegn", "usage", "--json", "--tokens"]);
        assert!(a.json && a.tokens);
    }
}
