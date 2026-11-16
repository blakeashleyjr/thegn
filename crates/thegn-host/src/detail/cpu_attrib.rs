//! The `cpu`/`mem` popups' attribution tree (THE-2025): who is using the
//! machine, workspace → worktree → pane → process, then thegn itself and
//! everything outside it. The tree is built by the pure
//! [`thegn_core::cpu_attrib::build`]; this module maps the frame model into its
//! inputs and renders the rows as a [`TableSection`].
//!
//! Inert by itself: it reads the last published process snapshot and pane
//! index. The loop refreshes it in place (like the status modal) only while the
//! popup is open, which is also the only time the sampler runs.

use thegn_core::cpu_attrib::{
    self, AttribInputs, AttribRow, Owner, PaneUsage, RowKind, ScopeUsage, SelfUsage, SortKey,
    TopProc, Usage,
};
use thegn_core::theme::Hue;

use super::status_modal::human_bytes;
use super::{Cell, DetailContent, DetailOverlay, Section, SectionsDetail, TableSection, trunc};
use crate::chrome::{FrameModel, S};
use crate::seg::Tok;

/// Title of the `cpu` popup.
pub(crate) const TITLE: &str = "CPU";
/// Title of the `mem` popup (unchanged from before the breakdown existed).
pub(crate) const MEM_TITLE: &str = "Memory";
/// Requested popup width; `sections` clamps it to the screen.
pub(crate) const COLS: usize = 60;
/// Label of the heading the attribution sections start at. `refresh_open`
/// replaces everything from here down and leaves the graphs above alone.
const HEADING: &str = "by workspace";
/// Widest label cell (indent included) before it is ellipsized.
const LABEL_CELLS: usize = 30;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The sort a popup title implies, or `None` for any other popup.
fn sort_for(title: &str) -> Option<SortKey> {
    match title {
        TITLE => Some(SortKey::Cpu),
        MEM_TITLE => Some(SortKey::Mem),
        _ => None,
    }
}

/// Whether `slot` holds the `cpu` or `mem` popup (the loop's
/// sample-while-open gate).
pub(crate) fn is_open(slot: &Option<DetailOverlay>) -> bool {
    slot.as_ref()
        .is_some_and(|ov| sort_for(&ov.title).is_some())
}

fn usage(cpu: Option<f32>, rss: Option<u64>) -> Usage {
    Usage::new(cpu, rss)
}

fn totals_usage(t: &thegn_metrics::ProcOwnerTotals) -> Usage {
    usage(t.cpu_pct.map(|c| c as f32), t.rss_bytes)
}

/// Map the sampler's `thegn.slice` scopes onto the builder's input: a named
/// `thegn-<kind>-<slug>-…` unit carries its kind and worktree slug; the
/// unnamed sum (and any name thegn did not mint) carries neither.
fn scope_usage(scopes: &[thegn_metrics::ScopeTotals]) -> Vec<ScopeUsage> {
    scopes
        .iter()
        .map(|scope| {
            let parsed = thegn_core::sandbox_cpucap::parse_scope_unit(&scope.unit);
            ScopeUsage {
                kind: parsed.as_ref().map(|(kind, _)| *kind),
                worktree: parsed.map(|(_, slug)| slug),
                cpu_pct: scope.cpu_pct,
                rss: scope.rss,
            }
        })
        .collect()
}

/// Map the frame model onto the core builder's inputs and build the tree.
fn tree(model: &FrameModel, sort: SortKey) -> cpu_attrib::AttribTree {
    let snap = &model.procs;
    let stats = &model.stats;
    let summary = snap.summary.as_ref();
    let panes: Option<Vec<PaneUsage>> = summary.map(|s| {
        s.pane_totals
            .iter()
            .map(|(pane_id, t)| PaneUsage {
                pane_id: *pane_id,
                usage: totals_usage(t),
            })
            .collect()
    });
    let procs: Vec<TopProc> = snap
        .procs
        .iter()
        .map(|p| TopProc {
            pid: p.pid,
            name: p.name.clone(),
            cpu_pct: p.cpu_pct,
            rss: p.rss_bytes,
            owner: match p.owner {
                thegn_metrics::ProcOwner::Pane(id) => Owner::Pane(id),
                thegn_metrics::ProcOwner::ThegnSelf => Owner::Ui,
                thegn_metrics::ProcOwner::ThegnDaemon => Owner::Daemon,
                thegn_metrics::ProcOwner::Other => Owner::Other,
            },
        })
        .collect();
    // The full pass's owner classes include each process's descendants (a
    // language server under the UI); the stats sampler's self figures are the
    // fallback when no full pass exists.
    let thegn = match summary {
        Some(s) => SelfUsage {
            ui: totals_usage(&s.owners.ui),
            daemon: totals_usage(&s.owners.daemon),
            other: totals_usage(&s.owners.other),
        },
        None => SelfUsage {
            ui: usage(stats.self_cpu_pct, stats.self_rss_bytes),
            daemon: usage(stats.daemon_cpu_pct, stats.daemon_rss_bytes),
            other: Usage::default(),
        },
    };
    let system = usage(
        stats.cpu_pct.map(f32::from),
        stats
            .mem_gib
            .map(|(used, _)| (f64::from(used.max(0.0)) * GIB) as u64),
    );
    let scopes = summary.map(|s| scope_usage(&s.scopes)).unwrap_or_default();
    cpu_attrib::build(
        &AttribInputs {
            owners: &model.pane_owners,
            panes: panes.as_deref(),
            procs: &procs,
            thegn,
            system,
            cores: stats.cpu_cores.len(),
            scopes: &scopes,
        },
        sort,
        cpu_attrib::TOP_PER_PANE,
    )
}

/// The one-line status under the heading: what the figures are and whether
/// they are live.
fn status_note(model: &FrameModel, tree: &cpu_attrib::AttribTree) -> String {
    let snap = &model.procs;
    let mut note = if snap.total == 0 {
        "open the breakdown to start sampling".to_string()
    } else if tree.partial {
        "per-pane totals unavailable on this platform".to_string()
    } else {
        format!("sampling while open · {} processes", snap.total)
    };
    if snap.total > 0 && !snap.primed {
        note.push_str(" · cpu warming up");
    }
    if tree.clamped {
        note.push_str(" · outside clamped");
    }
    note
}

fn label_tone(kind: RowKind) -> Tok {
    match kind {
        RowKind::Workspace | RowKind::Worktree | RowKind::Pane => Tok::Slot(S::Text),
        RowKind::Thegn | RowKind::Job => Tok::Hue(Hue::Green),
        RowKind::Proc | RowKind::Outside | RowKind::More => Tok::Slot(S::Dim),
    }
}

fn dash() -> String {
    "—".to_string()
}

fn render_row(r: &AttribRow) -> Vec<Cell> {
    let indent = "  ".repeat(usize::from(r.depth));
    let label = trunc(format!("{indent}{}", r.label), LABEL_CELLS);
    let num = if r.kind == RowKind::Proc {
        Tok::Slot(S::Dim)
    } else {
        Tok::Slot(S::Text)
    };
    vec![
        Cell::Text(label, label_tone(r.kind)),
        Cell::Text(r.cpu_pct.map_or_else(dash, |c| format!("{c:.0}%")), num),
        Cell::Text(
            r.share_of_total
                .map_or_else(dash, |s| format!("{:.0}%", s * 100.0)),
            Tok::Slot(S::Dim),
        ),
        Cell::Text(r.rss.map_or_else(dash, human_bytes), num),
    ]
}

/// The attribution sections: a heading with the sampling status, then the
/// tree as a table. `sort` is CPU for the `cpu` popup, RSS for `mem`.
pub(crate) fn build_sections(model: &FrameModel, sort: SortKey) -> Vec<Section> {
    let tree = tree(model, sort);
    let header = vec!["where".into(), "cpu".into(), "share".into(), "mem".into()];
    vec![
        Section::Heading {
            label: HEADING.into(),
            note: Some(status_note(model, &tree)),
        },
        Section::Table(TableSection {
            header,
            rows: tree.rows.iter().map(render_row).collect(),
            sel: None,
        }),
    ]
}

/// The attribution sections for a popup title, or `None` for other popups.
pub(crate) fn sections_for(model: &FrameModel, title: &str) -> Option<Vec<Section>> {
    sort_for(title).map(|sort| build_sections(model, sort))
}

/// Rebuild the attribution sections in place iff the `cpu`/`mem` popup is
/// still open; the graphs above them are left as they are. Mirrors
/// `status_modal::refresh_open`: called from a drain that already happens (a
/// fresh process publication), never from a timer. Returns `true` when it
/// repainted.
pub(crate) fn refresh_open(
    slot: &mut Option<DetailOverlay>,
    model: &FrameModel,
    ctx: &super::StatusCtx,
) -> bool {
    let Some(ov) = slot.as_mut() else {
        return false;
    };
    let Some(fresh) = sections_for(model, &ov.title) else {
        return false;
    };
    let DetailContent::Sections(SectionsDetail { sections }) = &mut ov.content else {
        return false;
    };
    let start = sections
        .iter()
        .position(|s| matches!(s, Section::Heading { label, .. } if label == HEADING))
        .unwrap_or(sections.len());
    sections.truncate(start);
    sections.extend(fresh);
    let content: usize = sections.iter().map(Section::height).sum();
    ov.rows = content.min(ctx.screen.rows.saturating_sub(3)).max(1);
    ov.scroll = ov.scroll.min(ov.scroll_max());
    true
}

#[cfg(test)]
mod tests {
    use super::{HEADING, MEM_TITLE, TITLE, build_sections, is_open, refresh_open};
    use crate::chrome::{BarItemId, FrameModel};
    use crate::compositor::Rect;
    use crate::detail::{
        Cell, DetailContent, Placement, Section, StatusCtx, keyval, open_detail_for,
    };
    use crate::seg::Tok;
    use crate::telemetry::TelemetryHistory;
    use thegn_core::cpu_attrib::{PaneOwner, SortKey};

    fn model() -> FrameModel {
        let owner = |id: u32, ws: &str, wt: &str| PaneOwner {
            pane_id: id,
            workspace: ws.into(),
            worktree: wt.into(),
            worktree_path: Some(format!("/wt/{}", wt.replace('/', "-"))),
            tab: "1".into(),
            program: Some("claude".into()),
        };
        let totals = |cpu: f64, rss: u64| thegn_metrics::ProcOwnerTotals {
            count: 1,
            cpu_pct: Some(cpu),
            rss_bytes: Some(rss),
        };
        FrameModel {
            stats: thegn_metrics::StatsSnapshot {
                cpu_pct: Some(50),
                cpu_cores: vec![50; 4],
                mem_gib: Some((1.0, 4.0)),
                ..Default::default()
            },
            procs: thegn_metrics::ProcSnapshot {
                procs: vec![thegn_metrics::ProcSample {
                    pid: 11,
                    ppid: Some(1),
                    start_time: 0,
                    start_ticks: None,
                    name: "node".into(),
                    cpu_pct: 30.0,
                    rss_bytes: 1 << 20,
                    run_secs: 0,
                    owner: thegn_metrics::ProcOwner::Pane(1),
                }],
                total: 120,
                primed: true,
                enabled: true,
                summary: Some(thegn_metrics::ProcSummary {
                    pane_totals: vec![(1, totals(40.0, 2 << 20)), (2, totals(10.0, 8 << 20))],
                    owners: thegn_metrics::ProcOwnerSummary {
                        ui: totals(3.0, 1 << 20),
                        daemon: totals(1.0, 1 << 20),
                        panes: totals(50.0, 10 << 20),
                        other: totals(20.0, 1 << 20),
                    },
                    ..Default::default()
                }),
            },
            pane_owners: vec![owner(1, "alpha", "alpha/main"), owner(2, "beta", "beta")].into(),
            ..Default::default()
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
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn build_sections_renders_the_tree_with_theme_tokens_only() {
        let secs = build_sections(&model(), SortKey::Cpu);
        let lines = texts(&secs);
        assert_eq!(
            lines[0],
            "by workspace: sampling while open · 120 processes"
        );
        // Heaviest workspace first by CPU; pane label carries the program.
        assert!(lines[1].starts_with("alpha|40%|20%|"), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("1 · claude|40%")));
        assert!(lines.iter().any(|l| l.starts_with("node (11)|30%")));
        assert!(lines.iter().any(|l| l.starts_with("thegn|4%")));
        // 50% of 4 cores = 200; 200 - (50 + 4) = 146.
        assert!(
            lines.iter().any(|l| l.starts_with("outside thegn|146%")),
            "{lines:?}"
        );
        for s in &secs {
            if let Section::Table(t) = s {
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
        }

        // Memory sorts by RSS: beta (8M) above alpha (2M).
        let mem = texts(&build_sections(&model(), SortKey::Mem));
        let beta = mem.iter().position(|l| l.starts_with("beta|")).unwrap();
        let alpha = mem.iter().position(|l| l.starts_with("alpha|")).unwrap();
        assert!(beta < alpha);
    }

    #[test]
    fn slice_scopes_render_under_their_worktree_and_thegn() {
        let mut m = model();
        let scope = |unit: &str, cpu: f32| thegn_metrics::ScopeTotals {
            unit: unit.into(),
            cpu_pct: Some(cpu),
            rss: Some(1 << 20),
        };
        if let Some(summary) = m.procs.summary.as_mut() {
            summary.scopes = vec![
                scope("thegn-gate-alpha-main-42-0", 20.0),
                scope("thegn-agent-elsewhere-42-1", 7.0),
                scope(thegn_metrics::UNNAMED_SCOPES, 2.0),
            ];
        }
        let lines = texts(&build_sections(&m, SortKey::Cpu));
        let main = lines
            .iter()
            .position(|l| l.starts_with("alpha/main|60%"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(lines[main + 1].starts_with("gate|20%"), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("agent · elsewhere|7%")));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("thegn jobs (unnamed)|2%")),
            "{lines:?}"
        );
        // The thegn row stays UI + daemon.
        assert!(lines.iter().any(|l| l.starts_with("thegn|4%")));
    }

    #[test]
    fn status_note_covers_unsampled_partial_and_warming() {
        let empty = texts(&build_sections(&FrameModel::default(), SortKey::Cpu));
        assert_eq!(
            empty[0],
            "by workspace: open the breakdown to start sampling"
        );
        assert!(empty.iter().any(|l| l == "thegn|—|—|—"), "{empty:?}");

        let mut partial = model();
        partial.procs.summary = None;
        partial.procs.primed = false;
        let lines = texts(&build_sections(&partial, SortKey::Cpu));
        assert_eq!(
            lines[0],
            "by workspace: per-pane totals unavailable on this platform · cpu warming up"
        );
        assert!(lines.iter().any(|l| l.starts_with("alpha|—|—|—")));

        let mut clamped = model();
        clamped.stats.cpu_pct = Some(1);
        let lines = texts(&build_sections(&clamped, SortKey::Cpu));
        assert!(lines[0].ends_with(" · outside clamped"), "{lines:?}");
    }

    #[test]
    fn cpu_and_mem_popups_carry_the_tree_and_refresh_in_place() {
        let hist = TelemetryHistory::default();
        let ctx = StatusCtx::new_for_test(&hist);
        let item = Rect {
            x: 80,
            y: 0,
            cols: 8,
            rows: 1,
        };
        let mut slot = None;
        assert!(!is_open(&slot));
        for (widget, title) in [("cpu", TITLE), ("mem", MEM_TITLE)] {
            let ov = open_detail_for(&BarItemId::Widget(widget.into()), item, &model(), &ctx)
                .expect("popup");
            assert_eq!(ov.title, title);
            assert!(matches!(ov.placement, Placement::NearBelow(_)));
            let DetailContent::Sections(detail) = &ov.content else {
                panic!("{widget} should be sectioned");
            };
            assert!(matches!(detail.sections[0], Section::Graph(_)));
            assert!(
                texts(&detail.sections)
                    .iter()
                    .any(|l| l.starts_with("alpha|"))
            );
            slot = Some(ov);
            assert!(is_open(&slot));
        }

        // A refresh replaces only the tree: the graph stays, the new pane shows.
        let mut fresh = model();
        fresh.pane_owners = Vec::<PaneOwner>::new().into();
        assert!(refresh_open(&mut slot, &fresh, &ctx));
        let Some(ov) = &slot else { unreachable!() };
        let DetailContent::Sections(detail) = &ov.content else {
            panic!("still sectioned");
        };
        let headings = detail
            .sections
            .iter()
            .filter(|s| matches!(s, Section::Heading { label, .. } if label == HEADING))
            .count();
        assert_eq!(headings, 1, "the tree is replaced, not appended");
        let lines = texts(&detail.sections);
        assert!(lines.iter().any(|l| l.starts_with("unattributed panes|")));
        assert!(!lines.iter().any(|l| l.starts_with("alpha|")));

        // Any other popup is left alone and closes the gate.
        let mut other = Some(keyval("Other", Vec::new(), 40, Placement::Center));
        assert!(!is_open(&other));
        assert!(!refresh_open(&mut other, &fresh, &ctx));
        let mut none = None;
        assert!(!refresh_open(&mut none, &fresh, &ctx));
    }
}
