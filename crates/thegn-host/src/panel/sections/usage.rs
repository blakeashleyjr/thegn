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
//!     (org, seat, rate-limit tier, credential home), the provider-stated
//!     window length, and the absolute reset time.

use thegn_core::theme::Hue;
use thegn_core::usage::{AccountUsage, UsageState, UsageTone, UsageWindow};

use crate::seg::{Line, Seg, seg, sp};

use super::{PanelRow, SectionCtx, bar_segs, d, g, g2, hint_row, hue, rule, t};

/// Bar width at the resting panel width. The deeper tiers get more room.
const BAR_W: usize = 10;
const BAR_W_DEEP: usize = 16;

/// Tone a percentage against the *configured* thresholds — the same call the
/// statusbar badge makes, so a window that is amber in the bar is amber here.
fn tone(ctx: &SectionCtx, pct: f32) -> Tone {
    let cfg = &ctx.model.usage_cfg;
    match thegn_core::usage::tone_at(pct, cfg.warn_percent, cfg.crit_percent) {
        UsageTone::Ok => Tone(Hue::Green),
        UsageTone::Warn => Tone(Hue::Amber),
        UsageTone::Crit => Tone(Hue::Red),
    }
}

/// A resolved hue, so the call sites read as `tone(..).0` rather than repeating
/// the match.
struct Tone(Hue);

/// `resets in 2h 14m`, or empty when the provider didn't say.
fn resets_in(w: &UsageWindow, now: i64) -> String {
    thegn_core::usage::fmt_resets_in(w.resets_at, now)
        .map(|s| format!("resets in {s}"))
        .unwrap_or_default()
}

/// The one-line state note for an account that has no windows to draw.
fn state_note(a: &AccountUsage) -> Option<String> {
    match a.state {
        UsageState::Ok if a.windows.is_empty() => Some("no windows reported".into()),
        UsageState::Ok => None,
        UsageState::Loading => Some("\u{2026}".into()),
        UsageState::Unavailable => Some(
            a.note
                .clone()
                .map(|n| format!("unavailable: {n}"))
                .unwrap_or_else(|| "unavailable".into()),
        ),
    }
}

/// One window row: `label ▓▓▓░░ 87% resets in 2h 14m`, with the window length
/// and absolute reset added at Full width.
fn window_row(ctx: &SectionCtx, w: &UsageWindow, now: i64, indent: usize) -> PanelRow {
    let bar_w = if ctx.deep() { BAR_W_DEEP } else { BAR_W };
    let mut segs: Vec<Seg> = Vec::new();
    if indent > 0 {
        segs.push(sp(indent));
    }
    segs.push(seg(d(), format!("{:<8}", w.label)));
    segs.extend(bar_segs(
        thegn_core::usage::used_frac(w.used_percent),
        bar_w,
        hue(tone(ctx, w.used_percent).0),
    ));
    segs.push(seg(t(), format!(" {:>3.0}%", w.used_percent)));
    if ctx.full()
        && let Some(len) = w.len_label()
    {
        // The provider-stated window length, so "5h" is a fact rather than an
        // inference the reader makes from the label.
        segs.push(seg(g2(), format!(" /{len}")));
    }
    let reset = resets_in(w, now);
    if !reset.is_empty() {
        segs.push(seg(g(), format!("  {reset}")));
    }
    PanelRow::plain(Line::segs(segs))
}

/// The identity facts under an account, at Full width only — what tells two
/// same-plan accounts apart.
fn fact_rows(a: &AccountUsage, rows: &mut Vec<PanelRow>) {
    let mut fact = |k: &str, v: Option<&String>| {
        if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(2),
                seg(g2(), format!("{k:<6}")),
                seg(g(), v.clone()),
            ])));
        }
    };
    fact("org", a.org.as_ref());
    fact("seat", a.seat_tier.as_ref());
    fact("tier", a.rate_limit_tier.as_ref());
    let home = a.home.as_ref().map(|h| h.display().to_string());
    fact("home", home.as_ref());
    if let Some(tok) = a.tokens {
        let v = format!(
            "{} in / {} out / {} total",
            thegn_core::usage::fmt_tokens(tok.input),
            thegn_core::usage::fmt_tokens(tok.output),
            thegn_core::usage::fmt_tokens(tok.total),
        );
        fact("tokens", Some(&v));
    }
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
    for (i, a) in ctx.model.usage.iter().enumerate() {
        // A blank line between accounts, but not before the first — the
        // separator is between blocks, not a top margin.
        if i > 0 && ctx.deep() {
            rows.push(PanelRow::blank());
        }
        // Account heading: label plus the plan, or the reason it can't be read.
        let mut head: Vec<Seg> = vec![seg(t(), a.account_label.clone())];
        if let Some(plan) = a.plan.as_ref().filter(|p| !p.trim().is_empty()) {
            head.push(seg(hue(Hue::Teal), format!("  {plan}")));
        }
        if let Some(note) = state_note(a) {
            head.push(seg(g2(), format!("  {note}")));
        }
        rows.push(PanelRow::plain(Line::segs(head)));

        if ctx.full() {
            fact_rows(a, &mut rows);
        }
        if a.state != UsageState::Ok || a.windows.is_empty() {
            continue;
        }
        if ctx.deep() {
            // Every window, indented under its account.
            for w in &a.windows {
                rows.push(window_row(ctx, w, now, 2));
                if ctx.full()
                    && let Some(row) = forecast_row(ctx, a, w, now)
                {
                    rows.push(row);
                }
            }
        } else if let Some(w) = a.peak_window() {
            // At the resting width, only the window that is actually close to
            // its limit — the rest is detail the reader didn't ask for.
            rows.push(window_row(ctx, w, now, 2));
        }
    }
    if ctx.full() {
        token_rows(ctx, &mut rows);
    }
    if ctx.deep() {
        proxy_spend_rows(ctx, &mut rows);
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

/// `full in 3h 12m` under a window that is on course to exhaust before it
/// resets. Absent otherwise — including when the window resets first, which is
/// the common and uninteresting case.
fn forecast_row(ctx: &SectionCtx, a: &AccountUsage, w: &UsageWindow, now: i64) -> Option<PanelRow> {
    let hist = ctx
        .model
        .usage_history
        .get(&crate::detail::history_key(&a.key, &w.label))?;
    let eta = thegn_core::usage::forecast_exhaustion(hist, now, w.resets_at)?;
    let left = thegn_core::usage::fmt_resets_in(Some(eta), now)?;
    Some(PanelRow::plain(Line::segs(vec![
        sp(10),
        seg(hue(tone(ctx, w.used_percent).0), format!("full in {left}")),
        seg(g2(), "  at this rate".to_string()),
    ])))
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

    #[test]
    fn state_note_explains_every_non_ok_row() {
        let loading = AccountUsage::loading("claude", "x");
        assert_eq!(state_note(&loading).as_deref(), Some("\u{2026}"));
        let down = AccountUsage::unavailable("claude", "x", "token expired");
        assert_eq!(
            state_note(&down).as_deref(),
            Some("unavailable: token expired")
        );
        // An Ok account with no windows is its own case: the fetch worked and
        // the provider reported nothing, which is not the same as a failure.
        let empty = AccountUsage::ok("claude", "x", None, vec![]);
        assert_eq!(state_note(&empty).as_deref(), Some("no windows reported"));
        let good = AccountUsage::ok("claude", "x", None, vec![UsageWindow::new("5h", 1.0, None)]);
        assert_eq!(state_note(&good), None);
    }
}
