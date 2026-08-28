//! The Usage section (optional `[usage]` feature): AI-account rate-limit
//! windows, one block per tracked account. Hidden unless `[usage] enabled`;
//! empty until the first poll lands.
//!
//! The three width tiers answer three different questions:
//!   * **Normal** — "how much have I got left?" One row per account: its
//!     worst window as a bar, the percentage, and the reset countdown.
//!   * **Half** — "on which window?" Every window of every account, indented
//!     under the account, plus the plan.
//!   * **Full** — "which account is this, exactly?" The above plus identity
//!     (org, seat, rate-limit tier, credential home) summarised on one facts
//!     line below the numbers, the host-wide token rollup, and a legend.
//!
//! Every decision that is not layout — which account leads, what a window is
//! called in plain language, what tone a percentage gets, the reset/forecast
//! phrases, the shared name-column width — is made by
//! [`thegn_core::usage_view::build`], the same model the `Alt-u` overlay
//! renders, so the two surfaces cannot drift. The section only projects the
//! view into [`PanelRow`]s: accounts worst-first, one aligned metric line per
//! limit, facts below the numbers, legend on the last row.

use thegn_core::theme::Hue;
use thegn_core::usage::{UsageState, UsageTone};
use thegn_core::usage_view::{self, AccountView, MetricRow};

use crate::seg::{Line, Seg, seg, sp};
use thegn_core::termcaps::Glyph;

use super::{PanelRow, SectionCtx, bar_segs, d, g, g2, hint_row, hue, rule, t};

/// Bar width at the resting panel width. The deeper tiers get more room.
const BAR_W: usize = 10;
const BAR_W_DEEP: usize = 16;

/// Map a `usage_view` tone to a theme hue: core states severity, the host
/// picks the colour. The view tones against the **configured** thresholds
/// already — the same numbers the statusbar badge reads — so a window that is
/// amber in the bar is amber in the overlay and on the badge too.
fn tone(t: UsageTone) -> Tone {
    match t {
        UsageTone::Ok => Tone(Hue::Green),
        UsageTone::Warn => Tone(Hue::Amber),
        UsageTone::Crit => Tone(Hue::Red),
    }
}

/// A resolved hue, so the call sites read as `tone(..).0` rather than repeating
/// the match.
struct Tone(Hue);

/// One metric line from the shared view:
/// `5-hour window ▓▓▓░░  94%  resets in 2h 14m  runs out in 3h 12m`.
///
/// The name is already padded to the view's shared width in display cells, so
/// the bars line up down the screen across accounts whose labels differ in
/// length; the reset countdown and the exhaustion forecast share the row, so
/// one limit is one line and a forecast can never double the row count.
fn metric_row(ctx: &SectionCtx, m: &MetricRow, fg: Tone, indent: usize) -> PanelRow {
    let bar_w = if ctx.deep() { BAR_W_DEEP } else { BAR_W };
    let mut segs: Vec<Seg> = Vec::new();
    if indent > 0 {
        segs.push(sp(indent));
    }
    segs.push(seg(d(), m.name.clone()));
    segs.extend(bar_segs(m.frac, bar_w, hue(fg.0)));
    segs.push(seg(t(), format!(" {}", m.pct)));
    if !m.resets.is_empty() {
        segs.push(seg(g(), format!("  {}", m.resets)));
    }
    if !m.forecast.is_empty() {
        // Toned like the bar: a forecast is only emitted when the window is on
        // course to exhaust, which is exactly the urgency the hue conveys.
        segs.push(seg(hue(fg.0), format!("  {}", m.forecast)));
    }
    PanelRow::plain(Line::segs(segs))
}

/// Account heading: the label toned to the account's peak tone — the worst
/// account is both first and loudest — then the plan as a chip, or the reason
/// it can't be read.
fn account_heading(a: &AccountView) -> PanelRow {
    let fg = a.tone.map(|t| hue(tone(t).0)).unwrap_or_else(d);
    let mut head: Vec<Seg> = vec![seg(fg, a.label.clone())];
    match a.state {
        // The view's note for an Ok account IS the plan; keep the teal chip.
        UsageState::Ok => {
            if !a.note.trim().is_empty() {
                head.push(seg(hue(Hue::Teal), format!("  {}", a.note)));
            }
        }
        // Loading / Unavailable / no-windows: the note is the state's own
        // words ("…", "unavailable: …", "no windows reported"), always present.
        _ => head.push(seg(g2(), format!("  {}", a.note))),
    }
    PanelRow::plain(Line::segs(head))
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let mut rows: Vec<PanelRow> = Vec::new();
    // The full-width body leads with a seam, like the other full section
    // bodies, so the three width tiers render distinctly.
    if ctx.full() {
        rows.push(rule());
    }
    if ctx.model.usage.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g(),
            if ctx.model.usage_cfg.enabled {
                "gathering\u{2026}"
            } else {
                "usage tracking is off"
            },
        )])));
        rows.push(hint_row(&[("\u{21b5}", "open"), ("r", "refresh")]));
        if ctx.deep() && !ctx.model.usage_cfg.allow_network {
            // The single most common reason for a permanently empty section:
            // Claude publishes no window state to disk, so with the network
            // fetch off there is genuinely nothing to read.
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                g2(),
                "Claude needs [usage] allow_network = true".to_string(),
            )])));
        }
        return rows;
    }

    let now = thegn_core::util::now();
    // One shared view with the Alt-u overlay: worst-first ordering, plain
    // language, configured thresholds, aligned names — decided once, in core.
    // `build` never touches the input slice; the statusbar badge and the alert
    // handler key off its discovery order.
    let view = usage_view::build(
        &ctx.model.usage,
        &ctx.model.usage_history,
        &usage_view::ViewOpts {
            now,
            warn_percent: ctx.model.usage_cfg.warn_percent,
            crit_percent: ctx.model.usage_cfg.crit_percent,
            // The resting width answers "how much have I got left?" — the
            // single worst window per account; the deeper tiers show them all.
            peak_only: !ctx.deep(),
        },
    );
    for (i, a) in view.accounts.iter().enumerate() {
        // A blank line between accounts, but not before the first — the
        // separator is between blocks, not a top margin.
        if i > 0 && ctx.deep() {
            rows.push(PanelRow::blank());
        }
        rows.push(account_heading(a));
        for m in &a.rows {
            rows.push(metric_row(ctx, m, tone(m.tone), 2));
        }
        if ctx.full() && !a.facts.is_empty() {
            // The identity facts sit BELOW the numbers: the percentage the
            // reader came for leads, the org/seat/home tail follows.
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(2),
                seg(g(), a.facts.clone()),
            ])));
        }
    }
    if ctx.full() {
        token_rows(ctx, &mut rows);
    }
    if ctx.deep() {
        proxy_spend_rows(ctx, &mut rows);
    }
    if ctx.full() {
        // The legend closes the body, directly above the hint row: the reader
        // meets it after the numbers it explains. Not at Normal/Half — there
        // is no room, and the tiers are graded by exactly this detail.
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            d(),
            usage_view::legend().join(crate::caps::glyph(Glyph::Middot)),
        )])));
    }
    rows.push(hint_row(&[("\u{21b5}", "open"), ("r", "refresh")]));
    rows
}

/// The model-proxy spend block (`[model_proxy]`): cost and token totals by route
/// for the trailing week, beside the per-account quota windows. Absent when the
/// proxy is disabled (`model_proxy_spend` is `None`) or has served nothing.
fn proxy_spend_rows(ctx: &SectionCtx, rows: &mut Vec<PanelRow>) {
    let Some(r) = &ctx.model.model_proxy_spend else {
        return;
    };
    if r.totals.requests == 0 {
        return;
    }
    rows.push(PanelRow::blank());
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(g2(), "model proxy ".to_string()),
        seg(g(), "spend, last 7d".to_string()),
    ])));
    rows.push(PanelRow::plain(Line::segs(vec![
        sp(2),
        seg(t(), format!("${:.2}", r.totals.cost_usd)),
        seg(g2(), format!("   {} req", r.totals.requests)),
        seg(
            d(),
            format!(
                "   {} in / {} out",
                r.totals.input_tokens, r.totals.output_tokens
            ),
        ),
    ])));
    if ctx.full() {
        for na in r.by_route.iter().take(3) {
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(2),
                seg(g(), format!("{:<20}", na.name)),
                seg(d(), format!("${:.2}", na.agg.cost_usd)),
            ])));
        }
    }
}

/// The host-wide transcript rollup. Headed as host-wide because these totals
/// genuinely cannot be attributed to an account, and a number sitting under a
/// list of accounts would otherwise read as if they could.
fn token_rows(ctx: &SectionCtx, rows: &mut Vec<PanelRow>) {
    use thegn_core::usage::fmt_tokens;
    let Some(v) = &ctx.model.usage_tokens else {
        return;
    };
    let r = &v.rollup;
    if r.records == 0 {
        return;
    }
    rows.push(PanelRow::blank());
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(g2(), "local tokens ".to_string()),
        seg(g(), "host-wide, not per account".to_string()),
    ])));
    let note = match v.skipped {
        0 => format!("{} responses", r.records),
        // A truncated scan presented as a total is worse than no scan.
        n => format!("{} responses (+{n} files not read)", r.records),
    };
    rows.push(PanelRow::plain(Line::segs(vec![
        sp(2),
        seg(t(), format!("{} in ", fmt_tokens(r.total.total_input()))),
        seg(t(), format!("/ {} out", fmt_tokens(r.total.output))),
        seg(g2(), format!("   {note}")),
    ])));
    for (model, tok) in r.top_models(3) {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(2),
            seg(g(), format!("{model:<24}")),
            seg(d(), fmt_tokens(tok.total())),
        ])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::layout::PanelWidth;
    use crate::panel::{PanelUi, Section};
    use std::collections::BTreeMap;
    use thegn_core::usage::{AccountUsage, UsageWindow};

    /// A fixed clock: every phrase below is computed against it.
    const NOW: i64 = 1_800_000_000;

    /// The peak window is what the resting width shows, so this is the
    /// load-bearing choice: a 7-day window at 91% must not be hidden behind a
    /// freshly-reset 5-hour one at 2%.
    #[test]
    fn resting_width_shows_the_window_nearest_its_limit() {
        let a = AccountUsage::ok(
            "claude",
            "work",
            None,
            vec![
                UsageWindow::new("5h", 2.0, None),
                UsageWindow::new("7d", 91.0, None),
            ],
        );
        assert_eq!(a.peak_window().map(|w| w.label.as_str()), Some("7d"));
    }

    // ── harness ────────────────────────────────────────────────────────────

    /// Render the Usage section at `width` against a model carrying `usage`
    /// (and optionally history), flattened to one String per row — the same
    /// shape the shared `render` harness in `panel/sections/mod.rs` produces,
    /// mirrored here because that harness is private to its module.
    fn render_with(
        width: PanelWidth,
        usage: Vec<AccountUsage>,
        history: BTreeMap<String, Vec<(i64, f32)>>,
    ) -> Vec<String> {
        let mut m = FrameModel::default();
        m.usage = usage;
        m.usage_history = history;
        let u = PanelUi {
            open: Section::Usage,
            width,
            ..Default::default()
        };
        let (cols, rows) = match width {
            PanelWidth::Normal => (39, 28),
            PanelWidth::Half => (75, 32),
            PanelWidth::Full => (150, 38),
        };
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols,
            rows,
        };
        content(&ctx)
            .iter()
            .map(|r| match &r.line {
                Line::Blank => String::new(),
                Line::Fill { ch, .. } => ch.to_string(),
                Line::Segs(v) => v.iter().map(|s| s.text.clone()).collect(),
                Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
                    let flat = |v: &[Seg]| v.iter().map(|s| s.text.clone()).collect::<String>();
                    format!("{} {}", flat(l), flat(r))
                }
            })
            .collect()
    }

    fn render(width: PanelWidth, usage: Vec<AccountUsage>) -> Vec<String> {
        render_with(width, usage, BTreeMap::new())
    }

    fn ok(key: &str, label: &str, windows: Vec<UsageWindow>) -> AccountUsage {
        AccountUsage {
            key: key.into(),
            ..AccountUsage::ok("claude", label, None, windows)
        }
    }

    /// The gauge alphabet — fill, eighth-block rungs and track — derived by
    /// sweeping `bar_track` across a cell boundary, so no glyph literal is
    /// written here (the glyph and caret ratchets scan test code too).
    fn gauge_alphabet() -> Vec<char> {
        let mut chars: Vec<char> = Vec::new();
        let mut feed = |(bar, track): (String, String)| {
            chars.extend(bar.chars());
            chars.extend(track.chars());
        };
        feed(crate::caps::bar_track(0.0, 8)); // the track
        feed(crate::caps::bar_track(0.5, 8)); // full blocks
        for k in 1..=7u32 {
            // k/8 of one cell: each rung of the eighth-block ladder.
            feed(crate::caps::bar_track(k as f32 / 64.0, 8));
        }
        chars.sort_unstable();
        chars.dedup();
        chars
    }

    /// Column of the first gauge cell (fill, rung, or track) — where the bar
    /// starts on a metric row.
    fn bar_col(line: &str) -> Option<usize> {
        let alphabet = gauge_alphabet();
        line.char_indices()
            .find(|(_, c)| alphabet.contains(c))
            .map(|(i, _)| i)
    }

    fn bar_rows(rows: &[String]) -> Vec<&String> {
        rows.iter().filter(|r| bar_col(r).is_some()).collect()
    }

    // ── the view's notes ───────────────────────────────────────────────────

    /// `AccountView::note` carries the old `state_note` helper's four cases —
    /// asserted on the shared view now, since the heading renders it verbatim.
    #[test]
    fn view_note_explains_every_account_state() {
        let good = ok("a", "A", vec![UsageWindow::new("5h", 1.0, None)]);
        let empty = ok("b", "B", vec![]);
        let loading = AccountUsage::loading("codex", "C");
        let down = AccountUsage::unavailable("claude", "D", "token expired");
        let v = usage_view::build(
            &[good, empty, loading, down],
            &BTreeMap::new(),
            &usage_view::ViewOpts {
                now: NOW,
                warn_percent: 75.0,
                crit_percent: 90.0,
                peak_only: false,
            },
        );
        let note = |key: &str| {
            v.accounts
                .iter()
                .find(|a| a.key == key)
                .map(|a| a.note.clone())
                .unwrap()
        };
        // Ok with windows: the note is the plan — empty when the provider
        // stated none, so the heading carries no chip.
        assert_eq!(note("a"), "");
        // An Ok account with no windows is its own case: the fetch worked and
        // the provider reported nothing, which is not the same as a failure.
        assert_eq!(note("b"), "no windows reported");
        assert_eq!(note("codex"), "\u{2026}");
        assert_eq!(note("claude"), "unavailable: token expired");
    }

    // ── ordering ───────────────────────────────────────────────────────────

    /// Accounts render worst-first: the one nearest its limit leads, the
    /// unreadable one sinks to the bottom. The list is a ranking, not a roster.
    #[test]
    fn normal_width_lists_the_account_nearest_its_limit_first() {
        let near = ok("a", "Near", vec![UsageWindow::new("7d", 91.0, None)]);
        let mid = ok("b", "Mid", vec![UsageWindow::new("5h", 40.0, None)]);
        let down = AccountUsage::unavailable("claude", "Down", "not logged in");
        let rows = render(PanelWidth::Normal, vec![mid, down, near]);
        let pos = |label: &str| rows.iter().position(|r| r.contains(label)).unwrap();
        assert!(
            pos("Near") < pos("Mid") && pos("Mid") < pos("Down"),
            "worst first: {rows:?}"
        );
    }

    // ── the three width tiers ──────────────────────────────────────────────

    /// Normal: one metric row per account (the peak only). Half: one per
    /// window. Full: the same rows plus the facts line and the legend.
    #[test]
    fn width_tiers_grade_the_detail_peak_all_and_full() {
        let a = ok(
            "a",
            "A",
            vec![
                UsageWindow::new("5h", 2.0, None),
                UsageWindow::new("7d", 91.0, None),
            ],
        );
        let b = ok("b", "B", vec![UsageWindow::new("session", 30.0, None)]);
        let org = AccountUsage {
            org: Some("Acme".into()),
            ..ok("c", "C", vec![UsageWindow::new("7d", 12.0, None)])
        };
        let usage = vec![a, b, org];

        let normal = render(PanelWidth::Normal, usage.clone());
        assert_eq!(bar_rows(&normal).len(), 3, "peak only: {normal:?}");

        let half = render(PanelWidth::Half, usage.clone());
        assert_eq!(bar_rows(&half).len(), 4, "every window: {half:?}");
        assert!(
            !half.iter().any(|r| r.contains("org Acme")),
            "facts are a Full-width tier: {half:?}"
        );

        let full = render(PanelWidth::Full, usage.clone());
        assert_eq!(bar_rows(&full).len(), 4, "same metric rows: {full:?}");
        assert!(
            full.iter().any(|r| r.contains("org Acme")),
            "facts line: {full:?}"
        );
        assert!(
            full.iter().any(|r| r.contains("worst first")),
            "legend: {full:?}"
        );
        // The facts sit BELOW the numbers, per the scannable layout.
        let last_bar = full.iter().rposition(|r| bar_col(r).is_some()).unwrap();
        let facts = full.iter().position(|r| r.contains("org Acme")).unwrap();
        assert!(facts > last_bar, "facts after the metric rows: {full:?}");
    }

    // ── alignment ──────────────────────────────────────────────────────────

    /// Two accounts whose window names differ in width still start their bars
    /// at the same column — the view pads every name to one shared width.
    #[test]
    fn half_width_bars_line_up_across_differently_named_windows() {
        let a = ok(
            "a",
            "A",
            vec![UsageWindow::with_len("5h", 10.0, None, Some(300))],
        );
        let b = ok("b", "B", vec![UsageWindow::new("weekly", 20.0, None)]);
        let rows = render(PanelWidth::Half, vec![a, b]);
        let cols: Vec<usize> = bar_rows(&rows)
            .into_iter()
            .filter_map(|r| bar_col(r))
            .collect();
        assert_eq!(cols.len(), 2, "{rows:?}");
        assert_eq!(cols[0], cols[1], "aligned bars: {rows:?}");
    }

    // ── plain language ─────────────────────────────────────────────────────

    /// A 300-minute window reads `5-hour window`, not the provider's `5h`.
    #[test]
    fn windows_read_in_plain_language() {
        let a = ok(
            "a",
            "A",
            vec![UsageWindow::with_len("5h", 10.0, None, Some(300))],
        );
        let rows = render(PanelWidth::Normal, vec![a]);
        assert!(rows.iter().any(|r| r.contains("5-hour window")), "{rows:?}");
        assert!(
            rows.iter().all(|r| !r.contains("5h")),
            "no provider shorthand: {rows:?}"
        );
    }

    // ── the forecast tail ──────────────────────────────────────────────────

    /// A forecasting window's exhaustion lands on the SAME row as its bar —
    /// one line per limit — and no second row is emitted for it.
    #[test]
    fn forecast_lives_on_the_metric_row_not_beside_it() {
        let a = ok(
            "a",
            "A",
            vec![UsageWindow::new("7d", 50.0, Some(NOW + 400_000))],
        );
        // Two rising samples ten minutes apart: a span, a slope, a forecast.
        let mut history = BTreeMap::new();
        history.insert("a#7d".into(), vec![(NOW - 600, 10.0), (NOW, 50.0)]);
        let rows = render_with(PanelWidth::Normal, vec![a], history);
        let forecasting: Vec<&String> = rows.iter().filter(|r| r.contains("runs out in")).collect();
        assert_eq!(forecasting.len(), 1, "no extra row: {rows:?}");
        assert!(
            bar_col(forecasting[0]).is_some(),
            "on the bar's row: {}",
            forecasting[0]
        );
    }
}
