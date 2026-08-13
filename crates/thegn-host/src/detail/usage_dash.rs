//! The AI-account usage overlay (V 300): per-account rate-limit windows
//! (session / weekly / …) rendered as usage bars with a `used %` and a "resets
//! in …" countdown — the TUI take on orca's account-usage view. A child module
//! of `detail` (like `proxy_dash`) so it reaches the private `DetailOverlay`
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
#[derive(Debug, Clone)]
pub struct UsagePayload {
    /// One entry per tracked provider, in display order.
    pub accounts: Vec<AccountUsage>,
}

/// The instant loading shell the loop opens before the off-loop gather lands.
pub fn usage_loading(cols: usize, rows: usize) -> DetailOverlay {
    let _ = (cols, rows); // centered placement self-positions
    DetailOverlay {
        title: TITLE.to_string(),
        content: DetailContent::Sections(SectionsDetail {
            sections: vec![Section::Heading {
                label: "gathering usage\u{2026}".into(),
                note: None,
            }],
        }),
        cols: 60,
        rows: 5,
        placement: Placement::Center,
        scroll: 0,
        sel: 0,
        hint: None,
        pending_ci: None,
        live_ci: None,
    }
}

/// Deliver the async usage payload into the live overlay, iff the user still has
/// it open. Returns `true` when it filled (repaint).
pub fn apply_usage(slot: &mut Option<DetailOverlay>, p: UsagePayload) -> bool {
    if let Some(ov) = slot.as_mut()
        && ov.title == TITLE
    {
        ov.content = DetailContent::Sections(SectionsDetail {
            sections: usage_sections(&p),
        });
        ov.rows = ov.content_rows().clamp(5, 30);
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

/// The heading row for one account: `provider (plan)`, with a right-aligned note
/// carrying the loading/unavailable state.
fn account_heading(a: &AccountUsage) -> Section {
    let label = match &a.plan {
        Some(plan) => format!("{} ({plan})", a.account_label),
        None => a.account_label.clone(),
    };
    let note = match a.state {
        UsageState::Loading => Some("\u{2026}".to_string()),
        UsageState::Unavailable => Some(format!(
            "unavailable{}",
            a.note
                .as_ref()
                .map(|n| format!(": {n}"))
                .unwrap_or_default()
        )),
        UsageState::Ok => None,
    };
    Section::Heading { label, note }
}

fn usage_sections(p: &UsagePayload) -> Vec<Section> {
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
        if a.state != UsageState::Ok || a.windows.is_empty() {
            continue;
        }
        let rows: Vec<Vec<Cell>> = a
            .windows
            .iter()
            .map(|w| {
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
                    Cell::Text(reset, Tok::Slot(S::Dim)),
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
        }
    }

    #[test]
    fn loading_then_fill_swaps_content_in_place() {
        let mut slot = Some(usage_loading(120, 40));
        assert_eq!(slot.as_ref().unwrap().title, TITLE);
        assert!(apply_usage(&mut slot, payload()));
        let ov = slot.unwrap();
        let DetailContent::Sections(d) = &ov.content else {
            panic!("expected sections");
        };
        // Codex heading + its window table + Claude (unavailable) heading = 3.
        assert_eq!(d.sections.len(), 3);
        // The Codex window table has two rows (session + weekly), each a bar row.
        let Section::Table(t) = &d.sections[1] else {
            panic!("expected a window table after the codex heading");
        };
        assert_eq!(t.rows.len(), 2);
        assert!(matches!(t.rows[0][1], Cell::Bar(..)));
        // The unavailable Claude account renders only a heading (no table).
        assert!(matches!(d.sections[2], Section::Heading { .. }));
    }

    #[test]
    fn fill_ignores_other_overlays() {
        // A different overlay (e.g. a CI drill) must not be clobbered by a late
        // usage payload.
        let mut ov = usage_loading(80, 24);
        ov.title = "CI \u{25b8} something".into();
        let mut slot = Some(ov);
        assert!(!apply_usage(&mut slot, payload()));
        // And an empty slot (user closed it) is a no-op.
        let mut none = None;
        assert!(!apply_usage(&mut none, payload()));
    }

    #[test]
    fn empty_accounts_shows_a_note() {
        let secs = usage_sections(&UsagePayload { accounts: vec![] });
        assert_eq!(secs.len(), 1);
        assert!(matches!(secs[0], Section::Heading { .. }));
    }
}
