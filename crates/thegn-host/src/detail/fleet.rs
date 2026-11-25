//! The masthead `fleet` widget's popup: every provider VM with its state,
//! size, burn, accrued cost and owning worktree (or ORPHAN), the fleet totals
//! (per-provider hourly, today, month to date, 24 h / 30 d projections), the
//! envs that could not be read completely, and the row keys.
//!
//! The rows table carries a cursor (`↑↓`). A row bound to a worktree takes
//! `s` (stop: park the VM) and `d` (destroy: pull commits back, then delete),
//! both as [`DetailAction::FleetDown`]; the loop confirms and then runs
//! `thegn env down [--stop] --worktree=<wt>` (`fleet_down_argv`) in a
//! visible tab, so the CLI's pull-back and refusals are reused and never
//! forced. Orphan rows (no worktree) take no keys.
//!
//! Built from the loop-owned `FrameModel::fleet` snapshot only, so opening it
//! costs no I/O; `refresh_open` rebuilds it in place when a new read lands
//! (riding the `RefreshKind::Fleet` drain, no wake source of its own). A child
//! module of `detail` so it reaches the private [`DetailOverlay`] fields.

use super::{
    Cell, DetailAction, DetailOverlay, Placement, RowActs, Section, SectionsCursor, SectionsDetail,
    TableSection, spacer,
};
use crate::chrome::{FrameModel, S};
use crate::fleet_read::FleetSnapshot;
use crate::seg::Tok;
use thegn_core::placement::fleet_cost::{self, BudgetLevel};
use thegn_core::theme::Hue;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    Some(super::sections_detail(
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
    let detail = build_sections(snap, ctx.now_ms);
    let content: usize = detail.sections.iter().map(Section::height).sum();
    ov.rows = content.min(ctx.screen.rows.saturating_sub(3)).max(1);
    // Keeps the cursor on the same `(provider, id)` VM, or clamps it.
    ov.set_sections(detail);
    true
}

/// The confirm-menu tag for a stop (`true`) or destroy.
pub(crate) fn fleet_down_tag(stop: bool) -> &'static str {
    if stop { "fleet-stop" } else { "fleet-destroy" }
}

/// The inverse of [`fleet_down_tag`]: `Some(stop)` for a fleet tag, `None`
/// for any other confirm.
pub(crate) fn stop_for_tag(tag: &str) -> Option<bool> {
    match tag {
        "fleet-stop" => Some(true),
        "fleet-destroy" => Some(false),
        _ => None,
    }
}

/// The argv a confirmed stop/destroy runs: `thegn env down [--stop]
/// --worktree=<wt>`. Structured argv (no shell); the worktree rides inside
/// its `--worktree=` flag so a value starting with `-` can never parse as an
/// option. Never `--force`: a failed pull-back refuses the destroy.
pub(crate) fn fleet_down_argv(exe: &str, worktree: &str, stop: bool) -> Vec<String> {
    let mut argv = vec![exe.to_string(), "env".into(), "down".into()];
    if stop {
        argv.push("--stop".into());
    }
    argv.push(format!("--worktree={worktree}"));
    argv
}

/// The confirm menu's title and body for stopping/destroying `label` (bound
/// to `worktree`).
pub(crate) fn confirm_text(label: &str, worktree: &str, stop: bool) -> (String, String) {
    let verb = if stop { "Stop" } else { "Destroy" };
    let what = if stop {
        "Stop parks the VM; `thegn env up` resumes it."
    } else {
        "Destroy pulls the VM's commits back first, then deletes the VM. \
         If the pull-back fails it refuses and the VM stays."
    };
    (
        format!("{verb} {label}?"),
        format!(
            "{what} Panes in that worktree end. Runs `thegn env down{}` for {worktree} in a new tab.",
            if stop { " --stop" } else { "" }
        ),
    )
}

/// The danger confirm for the `s`/`d` row actions. Stop keeps the plain
/// confirm default; destroy pre-selects `cancel`, so `d` then Enter is a no-op.
pub(crate) fn confirm_menu(label: &str, worktree: &str, stop: bool) -> crate::menu::MenuOverlay {
    let (title, body) = confirm_text(label, worktree, stop);
    let tag = fleet_down_tag(stop);
    if stop {
        crate::menu::confirm_menu(title, body, tag, worktree.to_string(), true)
    } else {
        crate::menu::confirm_menu_default_cancel(title, body, tag, worktree.to_string())
    }
}

/// The status line once the `env down` tab is open.
pub(crate) fn started_status(worktree: &str, stop: bool) -> String {
    let verb = if stop { "stopping" } else { "destroying" };
    format!("fleet: {verb} the VM for {worktree} in a new tab")
}

/// The keys of one listed VM: `s`/`d` when it is bound to a worktree, none for
/// an orphan. The id is `provider/id`, the row identity a refresh keeps.
fn row_acts(row: &fleet_cost::FleetRow) -> RowActs {
    let label = format!("{}/{}", row.provider, row.id);
    let keys = match &row.worktree {
        Some(wt) => [('s', true), ('d', false)]
            .into_iter()
            .map(|(key, stop)| {
                (
                    key,
                    DetailAction::FleetDown {
                        worktree: wt.clone(),
                        label: label.clone(),
                        stop,
                    },
                )
            })
            .collect(),
        None => Vec::new(),
    };
    RowActs {
        id: label,
        enter: None,
        keys,
    }
}

/// Whether the open overlay (if any) is this popup.
pub(crate) fn is_open(slot: Option<&DetailOverlay>) -> bool {
    slot.is_some_and(|ov| ov.title == TITLE)
}

fn usd(value: f64, decimals: usize) -> String {
    format!("${value:.decimals$}")
}

/// Clip `s` to at most `max` cells keeping its TAIL, behind a leading
/// ellipsis glyph (ASCII fallback via caps). Width-aware: never splits a wide
/// glyph. Unchanged when it already fits.
fn clip_start(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let ell = crate::caps::active_glyphs().ellipsis;
    let budget = max.saturating_sub(UnicodeWidthStr::width(ell));
    if budget == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0usize;
    for c in s.chars().rev() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        tail.push(c);
        used += w;
    }
    tail.reverse();
    format!("{ell}{}", tail.into_iter().collect::<String>())
}

/// One KeyVal row per budget breach: the burn against its limit, red at Max
/// (new sandboxes are refused), amber at Warn.
fn budget_rows(breaches: &[fleet_cost::BudgetBreach]) -> Vec<(String, String, Tok)> {
    breaches
        .iter()
        .map(|b| {
            let base = format!("{}/h of {}/h", usd(b.hourly, 4), usd(b.limit, 4));
            let provider = thegn_core::pipeline_report::display_cell(&b.provider);
            match b.level {
                BudgetLevel::Max => (
                    provider,
                    format!("{base} · new sandboxes refused"),
                    Tok::Hue(Hue::Red),
                ),
                BudgetLevel::Warn => (provider, base, Tok::Hue(Hue::Amber)),
            }
        })
        .collect()
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

/// The section stack and its row cursor (pure; `now_ms` is injected for the
/// age column).
fn build_sections(snap: &FleetSnapshot, now_ms: i64) -> SectionsDetail {
    let (view, cost) = (&snap.view, &snap.cost);
    let mut secs = Vec::new();
    let mut cursor = None;
    if !cost.budget.is_empty() {
        secs.push(heading("budget", None));
        secs.push(Section::KeyVal(budget_rows(&cost.budget)));
        secs.push(spacer());
    }
    let age_note = crate::cmd::fleet::age(Some(snap.generated_ms), now_ms);
    secs.push(heading(
        "provider VMs",
        Some(format!(
            "{} listed · {} orphan · {} missing · {} down · {} unrecorded · read {age_note} ago",
            view.rows.len(),
            view.orphans,
            view.missing.len(),
            view.down.len(),
            view.unrecorded,
        )),
    ));

    if view.rows.is_empty() && !view.partial.is_empty() {
        secs.push(heading(
            "No provider sandboxes confirmed; inventory is incomplete.",
            None,
        ));
    } else if view.rows.is_empty() {
        secs.push(heading("No provider sandboxes listed.", None));
    } else {
        let mut rows = Vec::new();
        let mut acts = Vec::new();
        for (index, row) in view.rows.iter().enumerate().take(MAX_ROWS) {
            acts.push(row_acts(row));
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
                Some(wt) => text(clip_start(wt, WORKTREE_COLS), dim()),
                None if row.unrecorded => text("UNRECORDED", dim()),
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
        cursor = Some(SectionsCursor {
            table: secs.len(),
            rows: acts,
        });
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
                        text(clip_start(&rec.worktree, WORKTREE_COLS), dim()),
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
        "↑↓ select · s stop · d destroy (pulls commits back first) · esc",
        None,
    ));
    if view.orphans > 0 {
        secs.push(heading(
            "orphan: not bound to a worktree; remove it with the provider's console",
            None,
        ));
    }
    SectionsDetail {
        sections: secs,
        cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BudgetLevel, Cell, DetailAction, Hue, MAX_ROWS, S, Section, Tok, UnicodeWidthStr,
        build_sections, clip_start, confirm_menu, confirm_text, fleet_down_argv, fleet_down_tag,
        refresh_open, started_status, stop_for_tag,
    };
    use crate::fleet_read::FleetSnapshot;
    use thegn_core::placement::fleet_cost::{
        self, FleetRow, FleetView, PartialEnv, PriceRow, RecordedSandbox,
    };
    use thegn_svc::provider::{InventoryReport, SandboxInfo};

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

    fn breach(level: BudgetLevel, hourly: f64, limit: f64) -> fleet_cost::BudgetBreach {
        fleet_cost::BudgetBreach {
            provider: "fly".into(),
            level,
            hourly,
            limit,
        }
    }

    #[test]
    fn budget_section_leads_with_toned_rows_for_warn_and_max() {
        let mut sn = snap(FleetView::default());
        sn.cost.budget = vec![
            breach(BudgetLevel::Warn, 1.0, 0.5),
            breach(BudgetLevel::Max, 2.0, 1.5),
        ];
        let secs = build_sections(&sn, NOW).sections;
        assert!(matches!(&secs[0], Section::Heading { label, .. } if label == "budget"));
        let Section::KeyVal(pairs) = &secs[1] else {
            panic!("keyval")
        };
        assert_eq!(pairs[0].0, "fly");
        assert_eq!(pairs[0].1, "$1.0000/h of $0.5000/h");
        assert_eq!(pairs[0].2, Tok::Hue(Hue::Amber));
        assert_eq!(pairs[1].1, "$2.0000/h of $1.5000/h · new sandboxes refused");
        assert_eq!(pairs[1].2, Tok::Hue(Hue::Red));
        assert_theme_tokens_only(&secs);
    }

    #[test]
    fn no_budget_section_when_no_breach() {
        let secs = build_sections(&snap(FleetView::default()), NOW).sections;
        assert!(!texts(&secs).iter().any(|l| l.starts_with("budget:")));
    }

    #[test]
    fn clip_start_keeps_tail_and_respects_width() {
        assert_eq!(clip_start("/wt/a", 32), "/wt/a");
        let out = clip_start("/very/long/path/to/worktree-name", 12);
        assert!(out.ends_with("tree-name"), "{out}");
        assert!(UnicodeWidthStr::width(out.as_str()) <= 12, "{out}");
        assert!(out.starts_with(crate::caps::active_glyphs().ellipsis));
        let wide = clip_start("\u{4f60}\u{597d}\u{4e16}\u{754c}", 6);
        assert!(UnicodeWidthStr::width(wide.as_str()) <= 6, "{wide}");
        assert!(wide.ends_with('\u{754c}'));
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
            ..FleetView::default()
        };
        let secs = build_sections(&snap(view), NOW).sections;
        let lines = texts(&secs);
        assert_eq!(
            lines[0],
            "provider VMs: 2 listed · 1 orphan · 1 missing · 0 down · 0 unrecorded · read 1m ago"
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
                .any(|l| l == "↑↓ select · s stop · d destroy (pulls commits back first) · esc: ")
        );
        assert!(lines.iter().any(|l| l.starts_with(
            "orphan: not bound to a worktree; remove it with the provider's console"
        )));
        assert_theme_tokens_only(&secs);
    }

    #[test]
    fn down_and_unrecorded_counts_are_neutral() {
        let mut unrecorded = row("tg-unrecorded", None);
        unrecorded.unrecorded = true;
        let view = FleetView {
            rows: vec![unrecorded],
            unrecorded: 1,
            down: vec![RecordedSandbox {
                worktree: "/wt/down".into(),
                provider: "fly".into(),
                env: "fly-dev".into(),
                id: "tg-down".into(),
            }],
            ..FleetView::default()
        };
        let detail = build_sections(&snap(view), NOW);
        let lines = texts(&detail.sections);
        assert!(lines[0].contains("1 down · 1 unrecorded"), "{lines:?}");
        let Section::Table(table) = detail
            .sections
            .iter()
            .find(|section| matches!(section, Section::Table(_)))
            .unwrap()
        else {
            unreachable!()
        };
        let cell = table
            .rows
            .iter()
            .flatten()
            .find_map(|cell| match cell {
                Cell::Text(label, tone) if label == "UNRECORDED" => Some(tone),
                _ => None,
            })
            .expect("unrecorded worktree marker");
        assert_eq!(*cell, Tok::Slot(S::Dim));
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
        )
        .sections;
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
        let detail = build_sections(&snap(FleetView::default()), NOW);
        assert!(detail.cursor.is_none(), "no rows, no cursor");
        let lines = texts(&detail.sections);
        assert!(lines.iter().any(|l| l == "No provider sandboxes listed.: "));
        assert!(lines.iter().any(|l| l == "burn=$0.0000/h"));
        assert!(!lines.iter().any(|l| l.starts_with("orphan:")));
    }

    #[test]
    fn provider_inventory_display_keeps_page_rows_identity_and_offline_state() {
        let complete = crate::fleet_read::env_inventory(
            "vps",
            "hetzner",
            Ok(InventoryReport::Complete(vec![
                SandboxInfo {
                    id: "tg-first".into(),
                    name: Some("101".into()),
                    state: Some("running".into()),
                    ..SandboxInfo::default()
                },
                SandboxInfo {
                    id: "tg-selected".into(),
                    name: Some("202".into()),
                    state: Some("off".into()),
                    ..SandboxInfo::default()
                },
            ])),
        );
        let offline = crate::fleet_read::env_inventory(
            "do",
            "digitalocean",
            Err("temporary provider timeout".into()),
        );
        let view = fleet_cost::join(
            vec![complete, offline],
            vec![
                RecordedSandbox {
                    worktree: "/wt/first".into(),
                    provider: "hetzner".into(),
                    env: "vps".into(),
                    id: "tg-first".into(),
                },
                RecordedSandbox {
                    worktree: "/wt/selected".into(),
                    provider: "hetzner".into(),
                    env: "vps".into(),
                    id: "tg-selected".into(),
                },
                RecordedSandbox {
                    worktree: "/wt/unreadable".into(),
                    provider: "digitalocean".into(),
                    env: "do".into(),
                    id: "tg-not-provably-missing".into(),
                },
            ],
        );
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.orphans, 0);
        assert!(
            view.missing.is_empty(),
            "offline inventory cannot prove absence"
        );
        assert_eq!(view.partial.len(), 1);
        assert_eq!(view.partial[0].env, "do");

        let detail = build_sections(&snap(view), NOW);
        let lines = texts(&detail.sections);
        assert!(
            lines.iter().any(|line| line.starts_with(
                "provider VMs: 2 listed · 0 orphan · 0 missing · 0 down · 0 unrecorded"
            )),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("|tg-first|")));
        assert!(lines.iter().any(|line| line.contains("|tg-selected|")));
        assert!(
            lines
                .iter()
                .any(|line| line == "do=unavailable: temporary provider timeout")
        );
        let cursor = detail.cursor.expect("two provider rows remain selectable");
        assert_eq!(cursor.rows[1].id, "hetzner/tg-selected");
        assert!(matches!(
            cursor.rows[1].keys.as_slice(),
            [
                ('s', DetailAction::FleetDown { worktree, label, stop: true }),
                ('d', DetailAction::FleetDown { worktree: destroy_worktree, label: destroy_label, stop: false })
            ] if worktree == "/wt/selected"
                && destroy_worktree == "/wt/selected"
                && label == "hetzner/tg-selected"
                && destroy_label == "hetzner/tg-selected"
        ));

        let offline_only = crate::fleet_read::env_inventory(
            "vps",
            "hetzner",
            Err("temporary provider timeout".into()),
        );
        let empty_detail = build_sections(
            &snap(fleet_cost::join(
                vec![offline_only],
                vec![RecordedSandbox {
                    worktree: "/wt/unreadable".into(),
                    provider: "hetzner".into(),
                    env: "vps".into(),
                    id: "tg-known".into(),
                }],
            )),
            NOW,
        );
        let lines = texts(&empty_detail.sections);
        assert!(
            lines.iter().any(|line| {
                line == "No provider sandboxes confirmed; inventory is incomplete.: "
            })
        );
        assert!(
            !lines
                .iter()
                .any(|line| line == "No provider sandboxes listed.: ")
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "vps=unavailable: temporary provider timeout")
        );
        assert!(empty_detail.cursor.is_none());
    }

    #[test]
    fn a_bound_row_gets_stop_and_destroy_and_an_orphan_gets_none() {
        let view = FleetView {
            rows: vec![row("tg-a", Some("/wt/a")), row("tg-orphan", None)],
            orphans: 1,
            ..FleetView::default()
        };
        let detail = build_sections(&snap(view), NOW);
        let cursor = detail.cursor.expect("rows table has a cursor");
        assert!(
            matches!(detail.sections[cursor.table], Section::Table(_)),
            "the cursor points at the rows table"
        );
        assert_eq!(cursor.rows.len(), 2);
        let bound = &cursor.rows[0];
        assert_eq!(bound.id, "fly/tg-a");
        assert_eq!(bound.enter, None);
        assert_eq!(
            bound.keys,
            [
                (
                    's',
                    DetailAction::FleetDown {
                        worktree: "/wt/a".into(),
                        label: "fly/tg-a".into(),
                        stop: true,
                    }
                ),
                (
                    'd',
                    DetailAction::FleetDown {
                        worktree: "/wt/a".into(),
                        label: "fly/tg-a".into(),
                        stop: false,
                    }
                ),
            ]
        );
        let orphan = &cursor.rows[1];
        assert_eq!(orphan.id, "fly/tg-orphan");
        assert!(orphan.keys.is_empty() && orphan.enter.is_none());
    }

    #[test]
    fn confirm_tags_round_trip_to_the_env_down_argv() {
        for stop in [true, false] {
            let tag = fleet_down_tag(stop);
            assert_eq!(stop_for_tag(tag), Some(stop));
        }
        assert_eq!(fleet_down_tag(true), "fleet-stop");
        assert_eq!(fleet_down_tag(false), "fleet-destroy");
        assert_eq!(stop_for_tag("git-op"), None);
        assert_eq!(stop_for_tag(""), None);

        let argv = |stop| fleet_down_argv("/bin/thegn", "/wt/a", stop);
        assert_eq!(
            argv(stop_for_tag("fleet-stop").unwrap()),
            ["/bin/thegn", "env", "down", "--stop", "--worktree=/wt/a"]
        );
        assert_eq!(
            argv(stop_for_tag("fleet-destroy").unwrap()),
            ["/bin/thegn", "env", "down", "--worktree=/wt/a"]
        );
        // Never forced; a dash-leading worktree stays inside its flag.
        for stop in [true, false] {
            let a = fleet_down_argv("thegn", "-rf", stop);
            assert!(!a.iter().any(|s| s == "--force"), "{a:?}");
            assert_eq!(a.last().map(String::as_str), Some("--worktree=-rf"));
        }
    }

    #[test]
    fn confirm_text_names_the_vm_and_the_consequences() {
        let (title, body) = confirm_text("fly/tg-a", "/wt/a", true);
        assert_eq!(title, "Stop fly/tg-a?");
        assert!(body.contains("`thegn env up` resumes it"), "{body}");
        assert!(body.contains("Panes in that worktree end"), "{body}");
        assert!(body.contains("env down --stop` for /wt/a"), "{body}");
        let (title, body) = confirm_text("fly/tg-a", "/wt/a", false);
        assert_eq!(title, "Destroy fly/tg-a?");
        assert!(body.contains("pulls the VM's commits back first"), "{body}");
        assert!(body.contains("If the pull-back fails it refuses"), "{body}");
        assert!(!body.contains("--force"), "{body}");
        assert!(started_status("/wt/a", false).contains("destroying"));
    }

    #[test]
    fn destroy_confirm_defaults_to_cancel() {
        use crate::menu::{MenuChoice, MenuOutcome};
        use termwiz::input::{KeyCode, Modifiers};

        let mut destroy = confirm_menu("fly/tg-a", "/wt/a", false);
        assert!(matches!(
            destroy.items()[destroy.selected()].choice,
            MenuChoice::Dismiss
        ));
        assert_eq!(
            destroy.handle_key(&KeyCode::Enter, Modifiers::NONE),
            MenuOutcome::Pick(MenuChoice::Dismiss),
            "d then Enter must not destroy"
        );
        let stop = confirm_menu("fly/tg-a", "/wt/a", true);
        assert!(matches!(
            stop.items()[stop.selected()].choice,
            MenuChoice::Confirm { .. }
        ));
    }

    #[test]
    fn cursor_identity_survives_refresh_open() {
        use crate::chrome::FrameModel;
        use crate::detail::{DetailOutcome, StatusCtx};
        use crate::telemetry::TelemetryHistory;
        use termwiz::input::{KeyCode, Modifiers};

        let hist = TelemetryHistory::default();
        let ctx = StatusCtx::new_for_test(&hist);
        let three = FleetView {
            rows: vec![
                row("vm-a", Some("/wt/a")),
                row("vm-b", Some("/wt/b")),
                row("vm-c", Some("/wt/c")),
            ],
            ..FleetView::default()
        };
        let mut model = FrameModel {
            fleet: Some(std::sync::Arc::new(snap(three))),
            ..Default::default()
        };
        let mut slot = super::open(&model, &ctx, crate::detail::Placement::Center);
        let ov = slot.as_mut().expect("popup");
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
        assert_eq!(ov.sel, 2, "on vm-c");

        // vm-a disappears: the cursor follows vm-c to its new index.
        model.fleet = Some(std::sync::Arc::new(snap(FleetView {
            rows: vec![row("vm-b", Some("/wt/b")), row("vm-c", Some("/wt/c"))],
            ..FleetView::default()
        })));
        assert!(refresh_open(&mut slot, &model, &ctx));
        let ov = slot.as_mut().expect("still open");
        assert_eq!(ov.sel, 1);
        assert!(matches!(
            ov.handle_key(&KeyCode::Char('s'), Modifiers::NONE),
            DetailOutcome::Act(DetailAction::FleetDown { ref worktree, stop: true, .. })
                if worktree == "/wt/c"
        ));

        // vm-c disappears too: the cursor clamps onto the last row.
        model.fleet = Some(std::sync::Arc::new(snap(FleetView {
            rows: vec![row("vm-b", Some("/wt/b"))],
            ..FleetView::default()
        })));
        assert!(refresh_open(&mut slot, &model, &ctx));
        assert_eq!(slot.as_ref().map(|ov| ov.sel), Some(0));
    }
}
