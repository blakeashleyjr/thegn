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

use super::{
    Cell, DetailContent, DetailOverlay, Placement, Section, SectionsDetail, TableSection, spacer,
};
use crate::chrome::S;
use crate::seg::Tok;
use thegn_core::config::UsageConfig;
use thegn_core::termcaps::Glyph;
use thegn_core::theme::Hue;
use thegn_core::usage::{AccountUsage, UsageTone};
use thegn_core::usage_view::{self, MetricRow};

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
///
/// `cfg` carries the **configured** `[usage] warn_percent` / `crit_percent` —
/// the same thresholds the panel section and the statusbar badge tone against,
/// so a window cannot be amber in one surface and green in another.
pub fn usage_overlay(
    accounts: &[AccountUsage],
    history: &std::collections::BTreeMap<String, Vec<(i64, f32)>>,
    tokens: Option<&TokenRollupView>,
    cfg: &UsageConfig,
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
            cfg,
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
    cfg: &UsageConfig,
) -> bool {
    if let Some(ov) = slot.as_mut()
        && ov.title == TITLE
    {
        let refreshed = usage_overlay(accounts, history, tokens, cfg);
        ov.content = refreshed.content;
        ov.rows = refreshed.rows;
        ov.scroll = 0;
        return true;
    }
    false
}

/// Map a `usage_view` tone to a theme token: green healthy, amber warning, red
/// critical. The severity was already decided — against the caller's
/// **configured** thresholds, by `usage_view::build` — this only picks the
/// colour. Never re-tone a raw percent here: that is how the overlay ended up
/// disagreeing with the panel and the badge (the old hard-wired `usage::tone`).
fn tone_tok(t: UsageTone) -> Tok {
    match t {
        UsageTone::Ok => Tok::Hue(Hue::Green),
        UsageTone::Warn => Tok::Hue(Hue::Amber),
        UsageTone::Crit => Tok::Hue(Hue::Red),
    }
}

/// The trend row for one window — only when it carries a forecast (the
/// actionable half of the trend; a sparkline with an empty value said nothing)
/// AND the current run has enough history to draw one. The series is
/// `current_run` — the samples since the window's last reset — so a freshly
/// reset window doesn't plot its predecessor's climb. An empty sparkline is
/// worse than no sparkline.
fn trend_row(p: &UsagePayload, r: &MetricRow) -> Option<Section> {
    if r.forecast.is_empty() {
        return None;
    }
    let hist = p.history.get(&r.history_key)?;
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
    Some(Section::Sparkrow {
        // The same padded name the metric row carries, so the spark aligns
        // with the window it continues.
        label: r.name.clone(),
        spark,
        cur: r.forecast.clone(),
        tone: tone_tok(r.tone),
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
        label_tone: Tok::Slot(S::Dim),
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

/// The overlay body: a worst-first projection of `usage_view::build` — the one
/// layout model shared with the panel section. Per account: a toned heading
/// (worst account first and loudest), one aligned metric line per limit, the
/// identity facts **below** the numbers, a sparkline only where a forecast
/// exists, and a blank between accounts; then the token rollup and a legend
/// footer. Pure: everything here is a function of the payload the refresh
/// channel already delivered.
fn usage_sections(
    p: &UsagePayload,
    tokens: Option<&TokenRollupView>,
    cfg: &UsageConfig,
) -> Vec<Section> {
    if p.accounts.is_empty() {
        return vec![Section::Heading {
            label: "no usage data \u{2014} set [usage] enabled = true".into(),
            note: None,
        }];
    }
    let now = thegn_core::util::now();
    // Toned against the **caller's** configured thresholds, never the module
    // defaults — the panel and the badge read the same config.
    let view = usage_view::build(
        &p.accounts,
        &p.history,
        &usage_view::ViewOpts {
            now,
            warn_percent: cfg.warn_percent,
            crit_percent: cfg.crit_percent,
            // The overlay is the deep surface: every window, not the peak only.
            peak_only: false,
        },
    );
    let middot = crate::caps::glyph(Glyph::Middot);
    let mut secs = vec![Section::Heading {
        label: "usage".into(),
        note: Some(format!("{} {middot} worst first", view.summary)),
    }];
    for (i, a) in view.accounts.iter().enumerate() {
        // A blank BETWEEN accounts — never before the first, never after the
        // last. Without it the blocks run together and nothing is greppable.
        if i > 0 {
            secs.push(spacer());
        }
        // The account's peak tone on both the label (bold) and the note, so
        // the leading — worst — account is the loudest thing on screen.
        // `None` (loading / unavailable / bare) stays dim.
        let tone = a.tone.map(tone_tok).unwrap_or(Tok::Slot(S::Dim));
        secs.push(Section::HeadingToned {
            label: a.label.clone(),
            label_tone: tone,
            note: a.note.clone(),
            tone,
        });
        if !a.rows.is_empty() {
            // One table per account, but every name arrives pre-padded to the
            // view's shared `name_w` — so the bar and % columns line up down
            // the whole overlay without touching the drawing code.
            secs.push(Section::Table(TableSection {
                header: Vec::new(),
                rows: a.rows.iter().map(metric_cells).collect(),
                sel: None,
            }));
            for r in &a.rows {
                if let Some(row) = trend_row(p, r) {
                    secs.push(row);
                }
            }
        }
        // The identity facts close the block, below the numbers: what tells
        // two same-plan accounts apart, not what the reader opened the overlay
        // for. One dim row, gone entirely when nothing is known.
        if !a.facts.is_empty() {
            secs.push(Section::Heading {
                label: a.facts.clone(),
                note: None,
            });
        }
    }
    if let Some(v) = tokens {
        secs.extend(token_sections(v));
    }
    // The legend is a trailing Section, not `ov.hint` — a Sections popup never
    // draws the hint.
    secs.push(Section::Heading {
        label: usage_view::legend().join(middot),
        note: None,
    });
    secs
}

/// One metric row's cells: dim pre-padded name, the gauge, the used %, the
/// reset countdown, and — when the burn rate has an ETA — the forecast in
/// ghost. The name column width comes from the view, so every account's bar
/// and % land in the same column.
fn metric_cells(r: &MetricRow) -> Vec<Cell> {
    vec![
        Cell::Text(r.name.clone(), Tok::Slot(S::Dim)),
        Cell::Bar(r.frac, BAR_W, tone_tok(r.tone)),
        Cell::Text(r.pct.clone(), Tok::Slot(S::Text)),
        Cell::Text(r.resets.clone(), Tok::Slot(S::Dim)),
        Cell::Text(r.forecast.clone(), Tok::Slot(S::Ghost)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::usage::UsageWindow;

    fn cfg() -> UsageConfig {
        UsageConfig::default()
    }

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
        apply_usage(slot, &p.accounts, &p.history, None, &cfg())
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
        // usage heading / Codex heading / its window table / spacer / Claude
        // (unavailable) heading / legend. Neither account carries identity
        // facts, so no facts line appears.
        assert_eq!(d.sections.len(), 6);
        assert!(matches!(&d.sections[0], Section::Heading { .. }));
        // The Codex window table has two rows (session + weekly), each a bar
        // row, and sits under a toned heading.
        assert!(matches!(d.sections[1], Section::HeadingToned { .. }));
        let Section::Table(t) = &d.sections[2] else {
            panic!("expected a window table after the codex heading");
        };
        assert_eq!(t.rows.len(), 2);
        assert!(matches!(t.rows[0][1], Cell::Bar(..)));
        // A blank between the accounts, then the unavailable Claude heading —
        // toned so the reason reads as a state, not as data.
        assert!(matches!(&d.sections[3], Section::Heading { label, .. } if label.is_empty()));
        assert!(matches!(d.sections[4], Section::HeadingToned { .. }));
        // The legend closes the overlay (a Sections popup never draws `hint`).
        assert!(matches!(d.sections[5], Section::Heading { .. }));
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
        let secs = usage_sections(&UsagePayload::default(), None, &cfg());
        assert_eq!(secs.len(), 1);
        assert!(matches!(secs[0], Section::Heading { .. }));
    }

    #[test]
    fn worst_account_leads_with_a_loud_label_and_unavailable_sinks() {
        // One account over crit_percent, one healthy, one unavailable.
        let p = UsagePayload {
            accounts: vec![
                AccountUsage::ok("low", "Low", None, vec![UsageWindow::new("5h", 10.0, None)]),
                AccountUsage::ok(
                    "crit",
                    "Crit",
                    Some("max".into()),
                    vec![UsageWindow::new("weekly", 95.0, None)],
                ),
                AccountUsage::unavailable("dead", "Dead", "token expired"),
            ],
            ..Default::default()
        };
        let secs = usage_sections(&p, None, &cfg());
        let toned: Vec<(&str, Tok)> = secs
            .iter()
            .filter_map(|s| match s {
                Section::HeadingToned {
                    label, label_tone, ..
                } => Some((label.as_str(), *label_tone)),
                _ => None,
            })
            .collect();
        assert_eq!(toned.len(), 3);
        // The crit account is the first HeadingToned emitted, and its label is
        // red — the dim slot would bury the one number the user opened the
        // overlay for.
        assert_eq!(toned[0].0, "Crit");
        assert_eq!(
            toned[0].1,
            Tok::Hue(Hue::Red),
            "over crit_percent must not tone the label dim"
        );
        // Healthy middle, Unavailable last.
        assert_eq!(toned[1].0, "Low");
        assert_eq!(toned[2].0, "Dead");
        assert_eq!(toned[2].1, Tok::Slot(S::Dim), "unavailable stays dim");
    }

    #[test]
    fn name_column_width_is_shared_across_accounts() {
        // Differently-wide window names ("5-hour window" vs "7-day window");
        // both tables' name cells must come out one shared width so the bars
        // and percentages line up down the overlay.
        let p = UsagePayload {
            accounts: vec![
                AccountUsage::ok(
                    "a",
                    "A",
                    None,
                    vec![UsageWindow::with_len("5h", 10.0, None, Some(300))],
                ),
                AccountUsage::ok(
                    "b",
                    "B",
                    None,
                    vec![UsageWindow::with_len("weekly", 50.0, None, Some(10_080))],
                ),
            ],
            ..Default::default()
        };
        let secs = usage_sections(&p, None, &cfg());
        let widths: Vec<usize> = secs
            .iter()
            .filter_map(|s| match s {
                Section::Table(t) => Some(t.rows.iter().filter_map(|r| match &r[0] {
                    Cell::Text(s, _) => Some(crate::seg::cells(s)),
                    _ => None,
                })),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(widths, [13, 13], "every name padded to the shared width");
    }

    #[test]
    fn density_counts_are_pinned_and_sparkrows_carry_forecasts_only() {
        let now = thegn_core::util::now();
        let mut p = UsagePayload::default();
        // Account A: two windows — one whose burn projects an ETA (rising
        // samples ten minutes apart, reset far off) and one that is falling,
        // so it has history but NO forecast.
        p.accounts.push(AccountUsage::ok(
            "a",
            "A",
            Some("max".into()),
            vec![
                UsageWindow::with_len("5h", 30.0, Some(now + 86_400), Some(300)),
                UsageWindow::with_len("weekly", 50.0, Some(now + 86_400), Some(10_080)),
            ],
        ));
        p.accounts.push(AccountUsage::unavailable("b", "B", "off"));
        p.history
            .insert(history_key("a", "5h"), vec![(now - 600, 10.0), (now, 30.0)]);
        p.history.insert(
            history_key("a", "weekly"),
            vec![(now - 600, 50.0), (now, 30.0)], // falling: no forecast
        );
        let secs = usage_sections(&p, None, &cfg());
        // usage heading / A heading / A table (2 rows) / ONE sparkrow /
        // spacer / B heading / legend. A regression that re-adds a row per
        // window breaks the pinned count here.
        assert_eq!(secs.len(), 7);
        assert_eq!(
            secs.iter()
                .filter(|s| matches!(s, Section::Sparkrow { .. }))
                .count(),
            1,
            "a sparkline only where a forecast exists"
        );
        // The forecastless window's history was present — its absence from the
        // stack is the §1.2 fix, not missing data.
        let Some(Section::Sparkrow { cur, .. }) =
            secs.iter().find(|s| matches!(s, Section::Sparkrow { .. }))
        else {
            panic!("expected the forecast sparkrow");
        };
        assert!(cur.starts_with("runs out in"), "{cur}");
    }

    #[test]
    fn facts_follow_the_table_and_vanish_when_nothing_is_known() {
        let rich = AccountUsage::ok(
            "a",
            "A",
            Some("max".into()),
            vec![UsageWindow::new("5h", 10.0, None)],
        )
        .with_home("/home/u/.claude-profiles/work/.claude")
        .with_identity(&thegn_core::usage::ClaudeIdentity {
            org_name: Some("Acme".into()),
            ..Default::default()
        });
        let bare = AccountUsage::ok("b", "B", None, vec![]);
        let p = UsagePayload {
            accounts: vec![rich, bare],
            ..Default::default()
        };
        let secs = usage_sections(&p, None, &cfg());
        // usage heading, then rich: heading, table, facts — then spacer,
        // bare heading, legend.
        assert_eq!(secs.len(), 7);
        let table_idx = secs
            .iter()
            .position(|s| matches!(s, Section::Table(_)))
            .expect("a table");
        let Some(Section::Heading { label, note: None }) = secs.get(table_idx + 1) else {
            panic!("expected the one-row facts line right after the table");
        };
        // The home survives as the discriminator between same-plan accounts.
        assert!(label.contains("org Acme"), "{label}");
        assert!(label.contains("work/.claude"), "{label}");
        // The bare account renders heading → legend with no facts line.
        let Some(Section::Heading { label, note: None }) = secs.get(table_idx + 2) else {
            panic!("expected the spacer after the facts line");
        };
        assert!(label.is_empty(), "the spacer draws blank, got {label:?}");
        assert!(matches!(secs[table_idx + 3], Section::HeadingToned { .. }));
        assert!(matches!(secs[table_idx + 4], Section::Heading { .. }));
    }

    #[test]
    fn legend_closes_the_overlay() {
        let p = UsagePayload {
            accounts: vec![AccountUsage::ok("a", "A", None, vec![])],
            ..Default::default()
        };
        let secs = usage_sections(&p, None, &cfg());
        let Some(Section::Heading { label, note: None }) = secs.last() else {
            panic!("expected a plain legend heading last");
        };
        for part in usage_view::legend() {
            assert!(label.contains(part), "{part} missing from {label}");
        }
        // Joined with the caps middot glyph, never a baked literal.
        assert!(label.contains(crate::caps::glyph(Glyph::Middot)), "{label}");
    }

    #[test]
    fn bar_tones_follow_the_configured_thresholds() {
        // §1.6 regression pin: the same 70% window under two `[usage]`
        // thresholds. It must not be green in the overlay while the panel and
        // the badge (which read the config) show amber.
        let p = UsagePayload {
            accounts: vec![AccountUsage::ok(
                "a",
                "A",
                None,
                vec![UsageWindow::new("5h", 70.0, None)],
            )],
            ..Default::default()
        };
        let bar_tone = |cfg: &UsageConfig| {
            usage_sections(&p, None, cfg)
                .iter()
                .filter_map(|s| match s {
                    Section::Table(t) => t.rows.iter().find_map(|r| match &r[1] {
                        Cell::Bar(_, _, tone) => Some(*tone),
                        _ => None,
                    }),
                    _ => None,
                })
                .next()
                .expect("a bar cell")
        };
        let hot = UsageConfig {
            warn_percent: 60.0,
            ..UsageConfig::default()
        };
        assert_eq!(bar_tone(&hot), Tok::Hue(Hue::Amber), "70% at warn 60");
        assert_eq!(
            bar_tone(&UsageConfig::default()),
            Tok::Hue(Hue::Green),
            "70% at the 75/90 defaults"
        );
    }

    #[test]
    fn spacers_sit_between_accounts_never_at_the_edges() {
        let p = UsagePayload {
            accounts: vec![
                AccountUsage::ok("a", "A", None, vec![UsageWindow::new("5h", 10.0, None)]),
                AccountUsage::ok("b", "B", None, vec![UsageWindow::new("5h", 20.0, None)]),
                AccountUsage::ok("c", "C", None, vec![UsageWindow::new("5h", 30.0, None)]),
            ],
            ..Default::default()
        };
        let secs = usage_sections(&p, None, &cfg());
        let blanks: Vec<usize> = secs
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, Section::Heading { label, .. } if label.is_empty()))
            .map(|(i, _)| i)
            .collect();
        // usage / A h / A table / — / B h / B table / — / C h / C table / legend
        assert_eq!(blanks, [3, 6], "one blank between each pair of accounts");
        // Neither a top margin nor a trailing one.
        assert!(!matches!(&secs[0], Section::Heading { label, .. } if label.is_empty()));
        assert!(
            !secs
                .last()
                .is_some_and(|s| matches!(s, Section::Heading { label, .. } if label.is_empty()))
        );
    }

    #[test]
    fn metric_rows_carry_plain_names_pct_and_countdown() {
        let now = thegn_core::util::now();
        let p = UsagePayload {
            accounts: vec![AccountUsage::ok(
                "claude",
                "Claude",
                Some("max".into()),
                vec![UsageWindow::with_len(
                    "5h",
                    40.0,
                    Some(now + 3600),
                    Some(300),
                )],
            )],
            ..Default::default()
        };
        let secs = usage_sections(&p, None, &cfg());
        // usage / heading / table / legend
        assert_eq!(secs.len(), 4);
        let Section::Table(t) = &secs[2] else {
            panic!("expected the metric table");
        };
        assert_eq!(t.rows.len(), 1);
        // The window's length is the name, not a ghost "/5h" fragment to
        // assemble mentally.
        let Cell::Text(name, _) = &t.rows[0][0] else {
            panic!("expected a name cell");
        };
        assert_eq!(name.trim_end(), "5-hour window");
        assert!(matches!(&t.rows[0][1], Cell::Bar(..)));
        let Cell::Text(pct, _) = &t.rows[0][2] else {
            panic!("expected a pct cell");
        };
        assert_eq!(pct.trim(), "40%");
        let Cell::Text(reset, _) = &t.rows[0][3] else {
            panic!("expected a reset cell");
        };
        assert!(reset.starts_with("resets in "), "{reset}");
    }

    #[test]
    fn a_trend_needs_a_forecast_and_two_points_in_the_current_run() {
        let now = thegn_core::util::now();
        let mut p = UsagePayload::default();
        let acct = AccountUsage::ok(
            "acct",
            "Acct",
            None,
            vec![UsageWindow::with_len(
                "5h",
                30.0,
                Some(now + 86_400),
                Some(300),
            )],
        );
        let opts = usage_view::ViewOpts {
            now,
            warn_percent: 75.0,
            crit_percent: 90.0,
            peak_only: false,
        };
        let key = history_key("acct", "5h");

        // No history at all → no row.
        let r = usage_view::build(std::slice::from_ref(&acct), &p.history, &opts).accounts[0].rows
            [0]
        .clone();
        assert!(trend_row(&p, &r).is_none());

        // One point is not a trend.
        p.history.insert(key.clone(), vec![(now - 600, 10.0)]);
        let r = usage_view::build(std::slice::from_ref(&acct), &p.history, &opts).accounts[0].rows
            [0]
        .clone();
        assert!(trend_row(&p, &r).is_none());

        // Rising samples ten minutes apart: a forecast ⇒ exactly one row, and
        // the spark carries the current run.
        p.history
            .insert(key.clone(), vec![(now - 600, 10.0), (now, 30.0)]);
        let r = usage_view::build(std::slice::from_ref(&acct), &p.history, &opts).accounts[0].rows
            [0]
        .clone();
        let Some(Section::Sparkrow { spark, cur, .. }) = trend_row(&p, &r) else {
            panic!("expected a sparkrow");
        };
        assert_eq!(spark, vec![10.0, 30.0]);
        assert!(cur.starts_with("runs out in"), "{cur}");

        // Falling samples: no forecast ⇒ no row, even with enough points —
        // the old decorative-squiggle case.
        p.history
            .insert(key.clone(), vec![(now - 600, 30.0), (now, 10.0)]);
        let r = usage_view::build(std::slice::from_ref(&acct), &p.history, &opts).accounts[0].rows
            [0]
        .clone();
        assert!(trend_row(&p, &r).is_none(), "no forecast, no sparkline");
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
