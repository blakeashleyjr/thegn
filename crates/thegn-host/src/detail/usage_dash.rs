//! The AI-account usage overlay (V 300): per-account rate-limit windows
//! (session / weekly / …) rendered as usage bars with a `used %` and a "resets
//! in …" countdown — the TUI take on orca's account-usage view. A child module
//! of `detail` (like `ci_drill`) so it reaches the private `DetailOverlay`
//! fields.
//!
//! Opened by `Action::OpenUsage`: the loop paints a loading shell instantly,
//! `crate::actions::spawn_usage` gathers each harness's local state off-loop
//! (`thegn_svc::usage::gather`), and [`apply_usage`] fills the live overlay when
//! the payload lands on the refresh channel — file reads / live fetches never
//! touch the loop.

use super::{Cell, DetailContent, DetailOverlay, Placement, Section, SectionsDetail, TableSection};
use crate::chrome::S;
use crate::seg::Tok;
use thegn_core::theme::Hue;
use thegn_core::usage::{AccountUsage, UsageState, UsageTone};

/// The overlay title — also the marker [`apply_usage`] uses to recognise a
/// still-open usage overlay when its async payload lands.
const TITLE: &str = "Usage";

/// Bar width (cells) for each window's usage gauge.
const BAR_W: usize = 16;

/// The off-loop-gathered usage data, carried (boxed) over the loop's refresh
/// channel.
#[derive(Debug, Clone, Default)]
///
/// Carries no token rollup: that arrives on its own `RefreshKind`, because the
/// scan behind it is far slower than a gather and must never delay one.
pub struct UsagePayload {
    /// One entry per tracked account, in display order.
    pub accounts: Vec<AccountUsage>,
    /// Recent history per window, keyed `<account key>#<window label>`, oldest
    /// first — the sparkline and the exhaustion forecast. Read back off-loop
    /// after the samples are written, so the loop never touches the DB.
    pub history: std::collections::BTreeMap<String, Vec<(i64, f32)>>,
    /// Model-proxy spend rollup, computed off-loop from the audit tables during
    /// the same poll (only when `[model_proxy]` is enabled). `None` otherwise.
    pub proxy_spend: Option<thegn_core::proxy::stats::Rollup>,
}

/// A rollup plus what the scan had to leave out. The skip count travels with the
/// numbers so the UI can say the total is a floor — a truncated scan presented
/// as complete is worse than no scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRollupView {
    pub rollup: thegn_core::usage_tokens::TokenRollup,
    pub skipped: usize,
}

/// The history key for one window.
pub fn history_key(account_key: &str, window: &str) -> String {
    format!("{account_key}#{window}")
}

/// Overlay width. Wide enough for the identity grid: a credential home like
/// `~/.claude-profiles/regclaude2/.claude` is most of 60 cells on its own, and
/// the home is precisely the field that tells two same-plan accounts apart.
/// `Section::Grid` also only earns its two columns past ~88.
const BOX_COLS: usize = 88;

/// The overlay shell, with whatever content the caller has. Shared by the cold
/// "gathering…" open and the warm open that renders the last poll's data.
fn shell(content: DetailContent) -> DetailOverlay {
    let mut ov = DetailOverlay {
        title: TITLE.to_string(),
        content,
        cols: BOX_COLS,
        rows: 5,
        // Centered placement self-positions, so the overlay needs no anchor.
        placement: Placement::Center,
        scroll: 0,
        sel: 0,
        hint: None,
        pending_ci: None,
        monitor_tab: None,
        live_ci: None,
    };
    ov.rows = ov.content_rows().clamp(5, 30);
    ov
}

/// The instant loading shell, opened only when there is nothing cached yet.
pub fn usage_loading(cols: usize, rows: usize) -> DetailOverlay {
    let _ = (cols, rows); // centered placement self-positions
    shell(DetailContent::Sections(SectionsDetail {
        sections: vec![Section::Heading {
            label: "gathering usage\u{2026}".into(),
            note: None,
        }],
    }))
}

/// The overlay rendered straight from cached model state — a warm open shows
/// real numbers on the first frame and refreshes underneath, rather than a
/// "gathering…" placeholder for data the loop already has.
pub fn usage_overlay(
    accounts: &[AccountUsage],
    history: &std::collections::BTreeMap<String, Vec<(i64, f32)>>,
    tokens: Option<&TokenRollupView>,
) -> DetailOverlay {
    shell(DetailContent::Sections(SectionsDetail {
        sections: usage_sections(
            &UsagePayload {
                accounts: accounts.to_vec(),
                history: history.clone(),
                // Transport-only on this path: the spend rollup reaches the UI
                // as `model.model_proxy_spend` (the panel's usage section), and
                // `usage_sections` never reads it, so the re-render carries none.
                proxy_spend: None,
            },
            tokens,
        ),
    }))
}

/// Re-render the live usage overlay from current state, iff the user still has
/// it open. Returns `true` when it did (repaint).
///
/// Called from both producers — the per-account gather and the token rollup —
/// so whichever lands second does not drop what the first delivered.
pub fn apply_usage(
    slot: &mut Option<DetailOverlay>,
    accounts: &[AccountUsage],
    history: &std::collections::BTreeMap<String, Vec<(i64, f32)>>,
    tokens: Option<&TokenRollupView>,
) -> bool {
    if let Some(ov) = slot.as_mut()
        && ov.title == TITLE
    {
        let refreshed = usage_overlay(accounts, history, tokens);
        ov.content = refreshed.content;
        ov.rows = refreshed.rows;
        ov.scroll = 0;
        return true;
    }
    false
}

/// Map a window's consumption tone to a theme token: green healthy, amber
/// warning, red critical.
fn tone_tok(pct: f32) -> Tok {
    match thegn_core::usage::tone(pct) {
        UsageTone::Ok => Tok::Hue(Hue::Green),
        UsageTone::Warn => Tok::Hue(Hue::Amber),
        UsageTone::Crit => Tok::Hue(Hue::Red),
    }
}

/// The heading row for one account: the account's own label (email + org where
/// known), with a right-aligned note carrying the plan or the reason it can't be
/// read. The plan lives in the note rather than in the label because the label
/// is what distinguishes eight accounts from each other and must not be pushed
/// off the edge by a "(max)" suffix.
fn account_heading(a: &AccountUsage) -> Section {
    match a.state {
        UsageState::Ok => match &a.plan {
            Some(plan) => Section::HeadingToned {
                label: a.account_label.clone(),
                note: plan.clone(),
                tone: Tok::Hue(Hue::Teal),
            },
            None => Section::Heading {
                label: a.account_label.clone(),
                note: None,
            },
        },
        UsageState::Loading => Section::Heading {
            label: a.account_label.clone(),
            note: Some("\u{2026}".to_string()),
        },
        UsageState::Unavailable => Section::HeadingToned {
            label: a.account_label.clone(),
            note: format!(
                "unavailable{}",
                a.note
                    .as_ref()
                    .map(|n| format!(": {n}"))
                    .unwrap_or_default()
            ),
            tone: Tok::Slot(S::Dim),
        },
    }
}

/// The identity/plan facts under an account's heading — what tells two `max`
/// accounts apart. Only non-empty facts are emitted, so a bare row stays bare
/// rather than showing a column of "unknown".
fn account_facts(a: &AccountUsage) -> Option<Section> {
    let mut cells: Vec<(String, String, Tok)> = Vec::new();
    let mut push = |k: &str, v: Option<&String>| {
        if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
            cells.push((k.to_string(), v.clone(), Tok::Slot(S::Dim)));
        }
    };
    push("org", a.org.as_ref());
    push("seat", a.seat_tier.as_ref());
    push("tier", a.rate_limit_tier.as_ref());
    // The home is the last-resort discriminator: two logins to the same org can
    // otherwise render identically.
    let home = a.home.as_ref().map(|h| h.display().to_string());
    push("home", home.as_ref());
    if let Some(t) = a.tokens {
        cells.push((
            "tokens".into(),
            format!(
                "{} in / {} out",
                thegn_core::usage::fmt_tokens(t.input),
                thegn_core::usage::fmt_tokens(t.output)
            ),
            Tok::Slot(S::Dim),
        ));
    }
    (!cells.is_empty()).then_some(Section::Grid { cols: 2, cells })
}

/// One window's row: label, bar, used %, window length, and the reset countdown.
fn window_row(w: &thegn_core::usage::UsageWindow, now: i64) -> Vec<Cell> {
    let reset = thegn_core::usage::fmt_resets_in(w.resets_at, now)
        .map(|s| format!("resets in {s}"))
        .unwrap_or_default();
    vec![
        Cell::Text(w.label.clone(), Tok::Slot(S::Dim)),
        Cell::Bar(
            thegn_core::usage::used_frac(w.used_percent),
            BAR_W,
            tone_tok(w.used_percent),
        ),
        Cell::Text(format!("{:>3.0}%", w.used_percent), Tok::Slot(S::Text)),
        // The provider-stated window length, so "5h" is a fact rather than an
        // inference the reader has to make from the label.
        Cell::Text(
            w.len_label().map(|l| format!("/{l}")).unwrap_or_default(),
            Tok::Slot(S::Ghost),
        ),
        Cell::Text(reset, Tok::Slot(S::Dim)),
    ]
}

/// The trend + forecast row for one window, when there is enough history to say
/// anything. Silent otherwise — an empty sparkline is worse than no sparkline.
fn trend_row(
    p: &UsagePayload,
    key: &str,
    w: &thegn_core::usage::UsageWindow,
    now: i64,
) -> Option<Section> {
    let hist = p.history.get(key)?;
    if hist.len() < 2 {
        return None;
    }
    let spark: Vec<f32> = thegn_core::usage::current_run(hist)
        .iter()
        .map(|(_, pct)| *pct)
        .collect();
    if spark.len() < 2 {
        return None;
    }
    // The forecast is the point of the trend: "climbing" is interesting,
    // "climbing, and you run out at 16:40" is actionable.
    let cur = match thegn_core::usage::forecast_exhaustion(hist, now, w.resets_at) {
        Some(eta) => thegn_core::usage::fmt_resets_in(Some(eta), now)
            .map(|s| format!("full in {s}"))
            .unwrap_or_default(),
        None => String::new(),
    };
    Some(Section::Sparkrow {
        label: format!("{:<8}", w.label),
        spark,
        cur,
        tone: tone_tok(w.used_percent),
    })
}

/// The host-wide transcript rollup block. Its heading says "host-wide" because
/// these totals genuinely cannot be attributed to an account, and a number
/// sitting under a list of accounts would otherwise read as if they could.
fn token_sections(v: &TokenRollupView) -> Vec<Section> {
    use thegn_core::usage::fmt_tokens;
    let r = &v.rollup;
    if r.records == 0 {
        return Vec::new();
    }
    let note = match v.skipped {
        0 => format!("{} responses", r.records),
        n => format!("{} responses (+{n} files not read)", r.records),
    };
    let mut secs = vec![Section::HeadingToned {
        label: "local tokens \u{2014} host-wide, not per account".into(),
        note,
        tone: Tok::Slot(S::Dim),
    }];
    secs.push(Section::Grid {
        cols: 2,
        cells: vec![
            (
                "input".into(),
                fmt_tokens(r.total.input),
                Tok::Slot(S::Text),
            ),
            (
                "output".into(),
                fmt_tokens(r.total.output),
                Tok::Slot(S::Text),
            ),
            (
                "cache read".into(),
                fmt_tokens(r.total.cache_read),
                Tok::Slot(S::Dim),
            ),
            (
                "cache write".into(),
                fmt_tokens(r.total.cache_creation),
                Tok::Slot(S::Dim),
            ),
        ],
    });
    let top = r.top_models(4);
    if !top.is_empty() {
        secs.push(Section::Table(TableSection {
            header: Vec::new(),
            rows: top
                .iter()
                .map(|(model, t)| {
                    vec![
                        Cell::Text(model.clone(), Tok::Slot(S::Dim)),
                        Cell::Text(fmt_tokens(t.total()), Tok::Slot(S::Text)),
                    ]
                })
                .collect(),
            sel: None,
        }));
    }
    secs
}

fn usage_sections(p: &UsagePayload, tokens: Option<&TokenRollupView>) -> Vec<Section> {
    if p.accounts.is_empty() {
        return vec![Section::Heading {
            label: "no usage data \u{2014} set [usage] enabled = true".into(),
            note: None,
        }];
    }
    let now = thegn_core::util::now();
    let mut secs = Vec::new();
    for a in &p.accounts {
        secs.push(account_heading(a));
        if let Some(facts) = account_facts(a) {
            secs.push(facts);
        }
        if a.state != UsageState::Ok || a.windows.is_empty() {
            continue;
        }
        secs.push(Section::Table(TableSection {
            header: Vec::new(),
            rows: a.windows.iter().map(|w| window_row(w, now)).collect(),
            sel: None,
        }));
        for w in &a.windows {
            if let Some(row) = trend_row(p, &history_key(&a.key, &w.label), w, now) {
                secs.push(row);
            }
        }
    }
    if let Some(v) = tokens {
        secs.extend(token_sections(v));
    }
    secs
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::usage::UsageWindow;

    fn payload() -> UsagePayload {
        UsagePayload {
            accounts: vec![
                AccountUsage::ok(
                    "codex",
                    "Codex",
                    Some("plus".into()),
                    vec![
                        UsageWindow::new("session", 20.0, None),
                        UsageWindow::new("weekly", 95.0, None),
                    ],
                ),
                AccountUsage::unavailable("claude", "Claude", "network off"),
            ],
            ..Default::default()
        }
    }

    /// Re-render an open overlay from a payload, as the loop does.
    fn apply(slot: &mut Option<DetailOverlay>, p: &UsagePayload) -> bool {
        apply_usage(slot, &p.accounts, &p.history, None)
    }

    #[test]
    fn loading_then_fill_swaps_content_in_place() {
        let mut slot = Some(usage_loading(120, 40));
        assert_eq!(slot.as_ref().unwrap().title, TITLE);
        assert!(apply(&mut slot, &payload()));
        let ov = slot.unwrap();
        let DetailContent::Sections(d) = &ov.content else {
            panic!("expected sections");
        };
        // Codex heading + its window table + Claude (unavailable) heading = 3.
        // Neither account carries identity facts, so no Grid block appears.
        assert_eq!(d.sections.len(), 3);
        // The Codex window table has two rows (session + weekly), each a bar row.
        let Section::Table(t) = &d.sections[1] else {
            panic!("expected a window table after the codex heading");
        };
        assert_eq!(t.rows.len(), 2);
        assert!(matches!(t.rows[0][1], Cell::Bar(..)));
        // The unavailable Claude account renders only a heading (no table), and
        // it is toned so the reason reads as a state, not as data.
        assert!(matches!(d.sections[2], Section::HeadingToned { .. }));
    }

    #[test]
    fn fill_ignores_other_overlays() {
        // A different overlay (e.g. a CI drill) must not be clobbered by a late
        // usage payload.
        let mut ov = usage_loading(80, 24);
        ov.title = "CI \u{25b8} something".into();
        let mut slot = Some(ov);
        assert!(!apply(&mut slot, &payload()));
        // And an empty slot (user closed it) is a no-op.
        let mut none = None;
        assert!(!apply(&mut none, &payload()));
    }

    #[test]
    fn empty_accounts_shows_a_note() {
        let secs = usage_sections(&UsagePayload::default(), None);
        assert_eq!(secs.len(), 1);
        assert!(matches!(secs[0], Section::Heading { .. }));
    }

    #[test]
    fn identity_facts_render_only_what_is_known() {
        // Nothing known → no grid at all, rather than a column of "unknown".
        let bare = AccountUsage::ok("claude", "x", None, vec![]);
        assert!(account_facts(&bare).is_none());

        let rich = AccountUsage::ok("claude", "x", None, vec![])
            .with_home("/home/u/.claude-profiles/work/.claude")
            .with_identity(&thegn_core::usage::ClaudeIdentity {
                org_name: Some("Acme".into()),
                seat_tier: Some("team_standard".into()),
                ..Default::default()
            });
        let Some(Section::Grid { cells, .. }) = account_facts(&rich) else {
            panic!("expected a facts grid");
        };
        let keys: Vec<&str> = cells.iter().map(|(k, _, _)| k.as_str()).collect();
        assert_eq!(keys, ["org", "seat", "home"]);
        // The home is the last-resort discriminator between two same-plan
        // accounts, so it must actually be shown.
        assert!(cells.iter().any(|(_, v, _)| v.contains("work")));
    }

    #[test]
    fn window_rows_carry_length_and_countdown() {
        let now = 1_000_000;
        let w = UsageWindow::with_len("5h", 40.0, Some(now + 3600), Some(300));
        let cells = window_row(&w, now);
        let text: Vec<String> = cells
            .iter()
            .filter_map(|c| match c {
                Cell::Text(s, _) => Some(s.clone()),
                Cell::Bar(..) => None,
            })
            .collect();
        assert!(text.iter().any(|s| s.contains("5h")));
        assert!(text.iter().any(|s| s.contains("40%")), "{text:?}");
        assert!(text.iter().any(|s| s == "/5h"), "{text:?}");
        assert!(text.iter().any(|s| s.contains("resets in 1h")), "{text:?}");
        // An unstated length leaves an empty cell, never "/0m".
        let bare = UsageWindow::new("5h", 40.0, None);
        let cells = window_row(&bare, now);
        assert!(matches!(&cells[3], Cell::Text(s, _) if s.is_empty()));
    }

    #[test]
    fn a_trend_needs_two_points_in_the_current_run() {
        let now = 10_000;
        let w = UsageWindow::new("5h", 40.0, None);
        let mut p = UsagePayload::default();
        let key = history_key("acct", "5h");
        // No history at all.
        assert!(trend_row(&p, &key, &w, now).is_none());
        // One point is not a trend.
        p.history.insert(key.clone(), vec![(0, 10.0)]);
        assert!(trend_row(&p, &key, &w, now).is_none());
        // Two points, but the second is a fresh window — the current run is one
        // point long, so still nothing to draw.
        p.history.insert(key.clone(), vec![(0, 90.0), (60, 1.0)]);
        assert!(trend_row(&p, &key, &w, now).is_none());
        // A real run draws, and carries the forecast.
        p.history
            .insert(key.clone(), vec![(0, 10.0), (1200, 30.0), (2400, 50.0)]);
        let Some(Section::Sparkrow { spark, cur, .. }) = trend_row(&p, &key, &w, now) else {
            panic!("expected a sparkrow");
        };
        assert_eq!(spark, vec![10.0, 30.0, 50.0]);
        assert!(cur.starts_with("full in"), "{cur}");
    }

    #[test]
    fn token_block_says_host_wide_and_admits_truncation() {
        use thegn_core::usage_tokens::{TokenRollup, TranscriptTokens};
        let mut rollup = TokenRollup {
            records: 3,
            total: TranscriptTokens {
                input: 1_000,
                output: 200,
                cache_read: 5_000,
                cache_creation: 300,
                thinking: 10,
            },
            ..Default::default()
        };
        rollup.by_model.insert(
            "claude-opus-4-8".into(),
            TranscriptTokens {
                input: 1_000,
                ..Default::default()
            },
        );
        let secs = token_sections(&TokenRollupView {
            rollup: rollup.clone(),
            skipped: 0,
        });
        let Section::HeadingToned { label, note, .. } = &secs[0] else {
            panic!("expected a heading");
        };
        // The disclaimer is the point: these totals pool several accounts'
        // transcripts and cannot be attributed to any one of them.
        assert!(label.contains("host-wide"), "{label}");
        assert_eq!(note, "3 responses");

        // A truncated scan must say so rather than present a floor as a total.
        let secs = token_sections(&TokenRollupView {
            rollup: rollup.clone(),
            skipped: 12,
        });
        let Section::HeadingToned { note, .. } = &secs[0] else {
            panic!("expected a heading");
        };
        assert!(note.contains("+12 files not read"), "{note}");

        // Nothing counted → no block at all.
        assert!(
            token_sections(&TokenRollupView {
                rollup: TokenRollup::default(),
                skipped: 0
            })
            .is_empty()
        );
    }
}
