//! The LLM-proxy dashboard overlay (V 299): live spend, tokens-per-second,
//! per-backend/route/scope breakdowns, budgets, and cooling backends — the TUI
//! rendering of the same `thegn_core::proxy::stats` rollup the daemon's
//! `/stats` endpoint and `thegn proxy stats` serve. A child module of
//! `detail` (like `ci_drill`) so it reaches the private `DetailOverlay` fields.
//!
//! Opened by `Action::OpenProxyDash` (`Ctrl Alt l` / palette): the loop paints
//! a loading shell instantly, `crate::actions::spawn_proxy_dash` gathers the
//! DB data off-loop, and [`apply_proxy_dash`] fills the live overlay when the
//! payload lands on the refresh channel — DB reads never touch the loop.

use super::{Cell, DetailContent, DetailOverlay, Placement, Section, SectionsDetail, TableSection};
use crate::chrome::S;
use crate::seg::Tok;
use thegn_core::db::ProxyBudgetRow;
use thegn_core::proxy::stats::{NamedAgg, Rollup};
use thegn_core::theme::Hue;

/// The overlay title — also the marker [`apply_proxy_dash`] uses to recognise
/// a still-open dashboard when its async payload lands.
const TITLE: &str = "LLM Proxy";

/// The off-loop-gathered dashboard data, carried (boxed) over the loop's
/// refresh channel.
#[derive(Debug, Clone)]
pub struct ProxyDashPayload {
    /// Rollup window (seconds) the stats cover.
    pub since_secs: i64,
    pub rollup: Rollup,
    pub budgets: Vec<ProxyBudgetRow>,
    /// Active (unrevoked) virtual keys.
    pub keys_active: usize,
    /// Cooling backends: `(backend:model, reason, next_probe_ms)`.
    pub cooling: Vec<(String, String, i64)>,
}

/// The instant loading shell the loop opens before the off-loop gather lands.
pub fn proxy_dash_loading(cols: usize, rows: usize) -> DetailOverlay {
    let _ = (cols, rows); // centered placement self-positions
    DetailOverlay {
        title: TITLE.to_string(),
        content: DetailContent::Sections(SectionsDetail {
            sections: vec![Section::Heading {
                label: "gathering stats\u{2026}".into(),
                note: None,
            }],
        }),
        cols: 78,
        rows: 5,
        placement: Placement::Center,
        scroll: 0,
        sel: 0,
        hint: None,
        pending_ci: None,
        live_ci: None,
    }
}

/// Deliver the async dashboard payload into the live overlay, iff the user
/// still has the dashboard open. Returns `true` when it filled (repaint).
pub fn apply_proxy_dash(slot: &mut Option<DetailOverlay>, p: ProxyDashPayload) -> bool {
    if let Some(ov) = slot.as_mut()
        && ov.title == TITLE
    {
        ov.content = DetailContent::Sections(SectionsDetail {
            sections: dash_sections(&p),
        });
        ov.rows = ov.content_rows().clamp(5, 30);
        ov.scroll = 0;
        return true;
    }
    false
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn agg_table(aggs: &[NamedAgg]) -> Section {
    let header = ["", "req", "tok in/out", "cost", "tok/s", "p95"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let rows: Vec<Vec<Cell>> = aggs
        .iter()
        .take(8)
        .map(|n| {
            let tone = if n.agg.failed > 0 && n.agg.ok == 0 {
                Tok::Hue(Hue::Red)
            } else {
                Tok::Slot(S::Text)
            };
            vec![
                Cell::Text(n.name.clone(), tone),
                Cell::Text(n.agg.requests.to_string(), Tok::Slot(S::Dim)),
                Cell::Text(
                    format!(
                        "{}/{}",
                        fmt_tokens(n.agg.input_tokens),
                        fmt_tokens(n.agg.output_tokens)
                    ),
                    Tok::Slot(S::Dim),
                ),
                Cell::Text(format!("${:.4}", n.agg.cost_usd), Tok::Slot(S::Dim)),
                Cell::Text(format!("{:.1}", n.agg.tokens_per_sec), Tok::Slot(S::Text)),
                Cell::Text(format!("{}ms", n.agg.duration_p95_ms), Tok::Slot(S::Dim)),
            ]
        })
        .collect();
    Section::Table(TableSection { header, rows })
}

fn dash_sections(p: &ProxyDashPayload) -> Vec<Section> {
    let t = &p.rollup.totals;
    let window = if p.since_secs % 3600 == 0 {
        format!("{}h", p.since_secs / 3600)
    } else {
        format!("{}s", p.since_secs)
    };
    let mut throughput = format!("{:.1} tok/s", t.tokens_per_sec);
    if let Some(last) = t.last_tokens_per_sec {
        throughput.push_str(&format!("  (last {last:.1})"));
    }
    let mut secs = vec![Section::KeyVal(vec![
        (
            format!("requests ({window})"),
            format!("{} ({} ok, {} failed)", t.requests, t.ok, t.failed),
            if t.failed > 0 {
                Tok::Hue(Hue::Amber)
            } else {
                Tok::Slot(S::Text)
            },
        ),
        (
            "tokens".into(),
            format!(
                "{} in / {} out",
                fmt_tokens(t.input_tokens),
                fmt_tokens(t.output_tokens)
            ),
            Tok::Slot(S::Text),
        ),
        (
            "spend".into(),
            format!("${:.4}", t.cost_usd),
            Tok::Slot(S::Text),
        ),
        (
            "latency".into(),
            format!(
                "p50 {}ms  p95 {}ms  ttfb {}ms",
                t.duration_p50_ms, t.duration_p95_ms, t.avg_ttfb_ms
            ),
            Tok::Slot(S::Dim),
        ),
        ("throughput".into(), throughput, Tok::Hue(Hue::Green)),
        (
            "virtual keys".into(),
            format!("{} active", p.keys_active),
            Tok::Slot(S::Dim),
        ),
    ])];
    for (label, aggs) in [
        ("backends", &p.rollup.by_backend),
        ("routes", &p.rollup.by_route),
        ("scopes", &p.rollup.by_scope),
    ] {
        if !aggs.is_empty() {
            secs.push(Section::Heading {
                label: label.into(),
                note: None,
            });
            secs.push(agg_table(aggs));
        }
    }
    if !p.budgets.is_empty() {
        secs.push(Section::Heading {
            label: "budgets".into(),
            note: None,
        });
        let rows: Vec<Vec<Cell>> = p
            .budgets
            .iter()
            .take(8)
            .map(|b| {
                let caps = match (b.limit_tokens, b.limit_cost) {
                    (None, None) => "no caps".to_string(),
                    (tk, c) => format!(
                        "{} tok / {}",
                        tk.map(|v| fmt_tokens(v.max(0) as u64))
                            .unwrap_or_else(|| "-".into()),
                        c.map(|v| format!("${v:.2}")).unwrap_or_else(|| "-".into())
                    ),
                };
                let tone = if b.killed {
                    Tok::Hue(Hue::Red)
                } else {
                    Tok::Slot(S::Text)
                };
                vec![
                    Cell::Text(b.scope.clone(), tone),
                    Cell::Text(b.period.clone(), Tok::Slot(S::Dim)),
                    Cell::Text(
                        format!(
                            "{} tok ${:.4}",
                            fmt_tokens(b.spent_tokens.max(0) as u64),
                            b.spent_cost
                        ),
                        Tok::Slot(S::Dim),
                    ),
                    Cell::Text(
                        if b.killed { "KILLED".to_string() } else { caps },
                        Tok::Slot(S::Dim),
                    ),
                ]
            })
            .collect();
        secs.push(Section::Table(TableSection {
            header: Vec::new(),
            rows,
        }));
    }
    if !p.cooling.is_empty() {
        secs.push(Section::Heading {
            label: "cooling backends".into(),
            note: None,
        });
        let now_ms = thegn_core::util::now() * 1000;
        let rows: Vec<Vec<Cell>> = p
            .cooling
            .iter()
            .map(|(ident, reason, next_probe)| {
                let wait = ((next_probe - now_ms) / 1000).max(0);
                vec![
                    Cell::Text(ident.clone(), Tok::Hue(Hue::Amber)),
                    Cell::Text(reason.clone(), Tok::Slot(S::Dim)),
                    Cell::Text(format!("retry in {wait}s"), Tok::Slot(S::Dim)),
                ]
            })
            .collect();
        secs.push(Section::Table(TableSection {
            header: Vec::new(),
            rows,
        }));
    }
    secs
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::db::ProxyRequestRow;
    use thegn_core::proxy::stats::rollup;

    fn payload() -> ProxyDashPayload {
        let rows = vec![ProxyRequestRow {
            ts_ms: 1,
            route: "standard".into(),
            backend: "nano-gpt".into(),
            outcome: "ok".into(),
            input_tokens: 100,
            output_tokens: 50,
            cost_usd: 0.01,
            duration_ms: 1000,
            ..Default::default()
        }];
        ProxyDashPayload {
            since_secs: 86_400,
            rollup: rollup(&rows),
            budgets: vec![ProxyBudgetRow {
                scope: "global".into(),
                period: "monthly".into(),
                spent_tokens: 150,
                spent_cost: 0.01,
                limit_tokens: None,
                limit_cost: Some(5.0),
                reset_ms: 0,
                killed: false,
            }],
            keys_active: 2,
            cooling: vec![("openrouter:m".into(), "HTTP 429".into(), i64::MAX / 2)],
        }
    }

    #[test]
    fn loading_then_fill_swaps_content_in_place() {
        let mut slot = Some(proxy_dash_loading(120, 40));
        assert_eq!(slot.as_ref().unwrap().title, TITLE);
        assert!(apply_proxy_dash(&mut slot, payload()));
        let ov = slot.unwrap();
        let DetailContent::Sections(d) = &ov.content else {
            panic!("expected sections");
        };
        // Header + backends/routes/scopes headings+tables + budgets + cooling.
        assert!(d.sections.len() >= 9, "got {}", d.sections.len());
        // The header block carries the tokens/sec figure.
        let Section::KeyVal(pairs) = &d.sections[0] else {
            panic!("expected keyval header");
        };
        assert!(
            pairs
                .iter()
                .any(|(k, v, _)| k == "throughput" && v.contains("tok/s"))
        );
    }

    #[test]
    fn fill_ignores_other_overlays() {
        // A different overlay (e.g. a CI drill) must not be clobbered by a late
        // proxy payload.
        let mut ov = proxy_dash_loading(80, 24);
        ov.title = "CI \u{25b8} something".into();
        let mut slot = Some(ov);
        assert!(!apply_proxy_dash(&mut slot, payload()));
        // And an empty slot (user closed it) is a no-op.
        let mut none = None;
        assert!(!apply_proxy_dash(&mut none, payload()));
    }

    #[test]
    fn token_formatting_is_compact() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(15_000), "15k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }
}
