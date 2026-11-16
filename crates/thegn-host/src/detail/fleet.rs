//! The masthead `fleet` widget's popup: every provider VM with its state,
//! size, burn, accrued cost and owning worktree (or ORPHAN), the fleet totals
//! (per-provider hourly, today, month to date, 24 h / 30 d projections), the
//! envs that could not be read completely, and how to act on a row from the
//! CLI. Read-only: no row actions yet.
//!
//! Built from the loop-owned `FrameModel::fleet` snapshot only, so opening it
//! costs no I/O; [`refresh_open`] rebuilds it in place when a new read lands
//! (riding the `RefreshKind::Fleet` drain, no wake source of its own). A child
//! module of `detail` so it reaches the private [`DetailOverlay`] fields.

use super::{
    Cell, DetailContent, DetailOverlay, Placement, Section, SectionsDetail, TableSection, spacer,
    trunc,
};
use crate::chrome::{FrameModel, S};
use crate::fleet_read::FleetSnapshot;
use crate::seg::Tok;
use thegn_core::placement::fleet_cost;
use thegn_core::theme::Hue;

/// The overlay title — also the marker [`refresh_open`] (and the loop's
/// "popup open" check for the poll cadence) recognise the popup by.
pub(crate) const TITLE: &str = "Fleet";

/// Requested content width; clamped to the screen by `super::sections`.
const WANT_COLS: usize = 104;

/// Body rows shown before the tail collapses into `+N more`.
const MAX_ROWS: usize = 64;

/// Widest worktree cell (the path's tail is the informative part).
const WORKTREE_COLS: usize = 32;

/// Open the popup, or `None` before the first read (Enter is then a no-op).
pub(super) fn open(
    model: &FrameModel,
    ctx: &super::StatusCtx,
    near: Placement,
) -> Option<DetailOverlay> {
    let snap = model.fleet.as_deref()?;
    Some(super::sections(
        TITLE,
        WANT_COLS,
        build_sections(snap, ctx.now_ms),
        near,
        ctx.screen,
    ))
}

/// Rebuild the popup in place iff it is the one open. Returns `true` when it
/// repainted.
pub(crate) fn refresh_open(
    slot: &mut Option<DetailOverlay>,
    model: &FrameModel,
    ctx: &super::StatusCtx,
) -> bool {
    let Some(ov) = slot.as_mut() else {
        return false;
    };
    if ov.title != TITLE {
        return false;
    }
    let Some(snap) = model.fleet.as_deref() else {
        return false;
    };
    let secs = build_sections(snap, ctx.now_ms);
    let content: usize = secs.iter().map(Section::height).sum();
    ov.content = DetailContent::Sections(SectionsDetail { sections: secs });
    ov.rows = content.min(ctx.screen.rows.saturating_sub(3)).max(1);
    ov.scroll = ov.scroll.min(ov.scroll_max());
    true
}

/// Whether the open overlay (if any) is this popup.
pub(crate) fn is_open(slot: &Option<DetailOverlay>) -> bool {
    slot.as_ref().is_some_and(|ov| ov.title == TITLE)
}

fn usd(value: f64, decimals: usize) -> String {
    format!("${value:.decimals$}")
}

fn text(s: impl Into<String>, tone: Tok) -> Cell {
    Cell::Text(s.into(), tone)
}

fn dim() -> Tok {
    Tok::Slot(S::Dim)
}

fn heading(label: impl Into<String>, note: Option<String>) -> Section {
    Section::Heading {
        label: label.into(),
        note,
    }
}

/// The section stack (pure; `now_ms` is injected for the age column).
fn build_sections(snap: &FleetSnapshot, now_ms: i64) -> Vec<Section> {
    let (view, cost) = (&snap.view, &snap.cost);
    let mut secs = Vec::new();
    let age_note = crate::cmd::fleet::age(Some(snap.generated_ms), now_ms);
    secs.push(heading(
        "provider VMs",
        Some(format!(
            "{} listed · {} orphan · {} missing · read {age_note} ago",
            view.rows.len(),
            view.orphans,
            view.missing.len()
        )),
    ));

    if view.rows.is_empty() {
        secs.push(heading("No provider sandboxes listed.", None));
    } else {
        let mut rows = Vec::new();
        for (index, row) in view.rows.iter().enumerate().take(MAX_ROWS) {
            let priced = cost.per_row.get(index).filter(|c| c.id == row.id);
            let rate = priced
                .and_then(|c| c.burn_usd_per_hour)
                .map_or_else(|| "?".to_string(), |r| usd(r, 4));
            let accrued = priced
                .and_then(|c| c.accrued_usd)
                .map_or_else(|| "?".to_string(), |v| usd(v, 2));
            let stopped = fleet_cost::is_stopped_state(row.state.as_deref());
            let state_tone = if stopped {
                Tok::Slot(S::Ghost)
            } else {
                Tok::Hue(Hue::Green)
            };
            let worktree = match &row.worktree {
                Some(wt) => text(trunc(wt.clone(), WORKTREE_COLS), dim()),
                None => text("ORPHAN", Tok::Hue(Hue::Amber)),
            };
            rows.push(vec![
                text(format!("{}/{}", row.provider, row.env), Tok::Slot(S::Text)),
                text(row.id.clone(), Tok::Slot(S::Text)),
                text(row.state.clone().unwrap_or_else(|| "-".into()), state_tone),
                text(row.size.clone().unwrap_or_else(|| "-".into()), dim()),
                text(rate, Tok::Slot(S::Text)),
                text(accrued, dim()),
                worktree,
                text(crate::cmd::fleet::age(row.created_ms, now_ms), dim()),
            ]);
        }
        secs.push(Section::Table(TableSection {
            header: [
                "provider/env",
                "id",
                "state",
                "size",
                "$/h",
                "accrued",
                "worktree",
                "age",
            ]
            .map(String::from)
            .to_vec(),
            rows,
            sel: None,
        }));
        let more = view.rows.len().saturating_sub(MAX_ROWS);
        if more > 0 {
            secs.push(heading(format!("+{more} more"), None));
        }
    }

    if !view.missing.is_empty() {
        secs.push(spacer());
        secs.push(heading("recorded, not listed by the provider", None));
        secs.push(Section::Table(TableSection {
            header: ["provider/env", "id", "worktree"]
                .map(String::from)
                .to_vec(),
            rows: view
                .missing
                .iter()
                .take(MAX_ROWS)
                .map(|rec| {
                    vec![
                        text(format!("{}/{}", rec.provider, rec.env), Tok::Slot(S::Text)),
                        text(rec.id.clone(), Tok::Hue(Hue::Amber)),
                        text(trunc(rec.worktree.clone(), WORKTREE_COLS), dim()),
                    ]
                })
                .collect(),
            sel: None,
        }));
    }

    secs.push(spacer());
    secs.push(heading("totals", None));
    let mut totals: Vec<(String, String, Tok)> = cost
        .per_provider_hourly
        .iter()
        .map(|(provider, hourly)| (format!("{provider} $/h"), usd(*hourly, 4), dim()))
        .collect();
    totals.extend([
        (
            "burn".to_string(),
            format!("{}/h", usd(cost.total_hourly, 4)),
            Tok::Slot(S::Text),
        ),
        ("today (UTC)".to_string(), usd(cost.today, 2), dim()),
        ("month to date".to_string(), usd(cost.mtd, 2), dim()),
        ("next 24h".to_string(), usd(cost.projected_24h, 2), dim()),
        ("next 30d".to_string(), usd(cost.projected_30d, 2), dim()),
    ]);
    if cost.unpriced > 0 {
        totals.push((
            "unpriced".to_string(),
            cost.unpriced.to_string(),
            Tok::Hue(Hue::Amber),
        ));
    }
    secs.push(Section::KeyVal(totals));

    if !view.partial.is_empty() {
        secs.push(spacer());
        secs.push(heading("incomplete envs", None));
        secs.push(Section::KeyVal(
            view.partial
                .iter()
                .map(|env| (env.env.clone(), env.reason.clone(), Tok::Hue(Hue::Amber)))
                .collect(),
        ));
    }

    secs.push(spacer());
    secs.push(heading(
        format!(
            "estimates · price table {} · [placement.price] overrides",
            cost.price_table_date
        ),
        None,
    ));
    secs.push(heading(
        "stop: thegn env down --stop <worktree> · destroy: thegn env down <worktree>",
        None,
    ));
    secs
}

#[cfg(test)]
mod tests {
    use super::{Cell, MAX_ROWS, Section, Tok, build_sections};
    use crate::fleet_read::FleetSnapshot;
    use thegn_core::placement::fleet_cost::{
        self, FleetRow, FleetView, PartialEnv, PriceRow, RecordedSandbox,
    };

    const TABLE: &[PriceRow] = &[PriceRow {
        provider: "fly",
        size: "shared-cpu-1x:256MB",
        region: None,
        usd_per_hour: 0.5,
        source: "test",
    }];

    // 2 h 5 min after the epoch: no UTC day/month boundary assumptions beyond
    // the injected clock.
    const NOW: i64 = 2 * 3_600_000 + 5 * 60_000;

    fn snap(view: FleetView) -> FleetSnapshot {
        let cost = fleet_cost::fleet_cost(&view, TABLE, &Default::default(), NOW);
        FleetSnapshot {
            generated_ms: NOW - 60_000,
            view,
            cost,
        }
    }

    fn row(id: &str, worktree: Option<&str>) -> FleetRow {
        FleetRow {
            provider: "fly".into(),
            env: "fly-dev".into(),
            id: id.into(),
            state: Some("started".into()),
            size: Some("shared-cpu-1x:256MB".into()),
            created_ms: Some(0),
            worktree: worktree.map(String::from),
            ..FleetRow::default()
        }
    }

    fn texts(secs: &[Section]) -> Vec<String> {
        secs.iter()
            .flat_map(|s| match s {
                Section::Table(t) => t
                    .rows
                    .iter()
                    .map(|r| {
                        r.iter()
                            .map(|c| match c {
                                Cell::Text(s, _) => s.trim().to_string(),
                                Cell::Bar(..) => String::new(),
                            })
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .collect::<Vec<_>>(),
                Section::Heading { label, note } => {
                    vec![format!("{label}: {}", note.clone().unwrap_or_default())]
                }
                Section::KeyVal(pairs) => {
                    pairs.iter().map(|(k, v, _)| format!("{k}={v}")).collect()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    fn assert_theme_tokens_only(secs: &[Section]) {
        for s in secs {
            match s {
                Section::Table(t) => {
                    for cell in t.rows.iter().flatten() {
                        let Cell::Text(_, tok) = cell else {
                            panic!("text cells only")
                        };
                        assert!(
                            matches!(tok, Tok::Slot(_) | Tok::Hue(_)),
                            "colour literal in {tok:?}"
                        );
                    }
                }
                Section::KeyVal(pairs) => {
                    for (_, _, tok) in pairs {
                        assert!(
                            matches!(tok, Tok::Slot(_) | Tok::Hue(_)),
                            "colour literal in {tok:?}"
                        );
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn sections_render_rows_totals_partials_and_hints_with_theme_tokens() {
        let view = FleetView {
            rows: vec![row("tg-a", Some("/wt/a")), row("tg-orphan", None)],
            orphans: 1,
            missing: vec![RecordedSandbox {
                worktree: "/wt/gone".into(),
                provider: "fly".into(),
                env: "fly-dev".into(),
                id: "tg-gone".into(),
            }],
            partial: vec![PartialEnv {
                env: "do".into(),
                reason: "unavailable: no token".into(),
            }],
        };
        let secs = build_sections(&snap(view), NOW);
        let lines = texts(&secs);
        assert_eq!(
            lines[0],
            "provider VMs: 2 listed · 1 orphan · 1 missing · read 1m ago"
        );
        assert_eq!(
            lines[1],
            "fly/fly-dev|tg-a|started|shared-cpu-1x:256MB|$0.5000|$1.04|/wt/a|2h5m"
        );
        assert!(lines[2].contains("|ORPHAN|"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "fly/fly-dev|tg-gone|/wt/gone"));
        assert!(lines.iter().any(|l| l == "fly $/h=$1.0000"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "burn=$1.0000/h"));
        assert!(lines.iter().any(|l| l == "next 24h=$24.00"));
        assert!(lines.iter().any(|l| l == "do=unavailable: no token"));
        assert!(
            lines.iter().any(
                |l| l.contains(fleet_cost::PRICE_TABLE_DATE) && l.contains("[placement.price]")
            )
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("thegn env down --stop <worktree>"))
        );
        assert_theme_tokens_only(&secs);
    }

    #[test]
    fn rows_cap_at_sixty_four_with_a_more_line() {
        let rows: Vec<FleetRow> = (0..MAX_ROWS + 6)
            .map(|i| row(&format!("vm-{i}"), Some("/wt")))
            .collect();
        let secs = build_sections(
            &snap(FleetView {
                rows,
                ..FleetView::default()
            }),
            NOW,
        );
        let table = secs
            .iter()
            .find_map(|s| match s {
                Section::Table(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(table.rows.len(), MAX_ROWS);
        assert!(texts(&secs).iter().any(|l| l == "+6 more: "));
    }

    #[test]
    fn an_empty_fleet_says_so() {
        let lines = texts(&build_sections(&snap(FleetView::default()), NOW));
        assert!(lines.iter().any(|l| l == "No provider sandboxes listed.: "));
        assert!(lines.iter().any(|l| l == "burn=$0.0000/h"));
    }
}
