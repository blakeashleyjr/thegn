//! The `cpu`/`mem` popups' attribution tree (THE-2025): who is using the
//! machine, workspace → worktree → pane → process, then thegn itself and
//! everything outside it. The tree is built by the pure
//! [`thegn_core::cpu_attrib::build`]; this module maps the frame model into its
//! inputs and renders the rows as a [`TableSection`].
//!
//! The tree table carries a row cursor: Enter on a workspace, worktree or
//! pane row focuses that worktree; Enter on a process row opens the monitor's
//! process tab (kill/renice with confirmation live there); thegn's own rows
//! and "outside thegn" fall through to the popup's plain Enter. `M` still
//! expands into the monitor.
//!
//! Inert by itself: it reads the last published process snapshot and pane
//! index. The loop refreshes it in place (like the status modal) only while the
//! popup is open, which is also the only time the sampler runs.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use thegn_core::cpu_attrib::{
    self, AttribInputs, AttribRow, Owner, PaneOwner, PaneUsage, RowKind, ScopeUsage, SelfUsage,
    SortKey, TopProc, Usage,
};
use thegn_core::theme::Hue;

use super::status_modal::human_bytes;
use super::{
    Cell, DetailAction, DetailContent, DetailOverlay, RowActs, Section, SectionsCursor,
    SectionsDetail, TableSection, trunc,
};
use crate::chrome::{FrameModel, S};
use crate::compositor::Rect;
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
pub(crate) fn is_open(slot: Option<&DetailOverlay>) -> bool {
    slot.is_some_and(|ov| sort_for(&ov.title).is_some())
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
    cpu_attrib::build_compact(
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
        cpu_attrib::TOP_WORKTREES,
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

/// Samples kept per row.
const SERIES_CAP: usize = 16;
/// Rows tracked at once.
const MAX_SERIES: usize = 64;
/// A series idle longer than this restarts (a reopened popup starts fresh).
const STALE: Duration = Duration::from_secs(10);
/// Width of the trend cell.
const TREND_CELLS: usize = 8;

/// One row's newest-sample instant and recent CPU samples.
type Series = (Instant, VecDeque<f32>);

/// Per-row CPU history of the open popup.
#[derive(Default)]
struct History {
    series: HashMap<String, Series>,
}

thread_local! {
    static HISTORY: RefCell<History> = RefCell::new(History::default());
}

/// Fold one refresh's per-row CPU readings into `h`: a `None` is skipped, a
/// series is capped at [`SERIES_CAP`], a series whose newest sample is older
/// than [`STALE`] restarts, ids absent this round are dropped, and at most
/// [`MAX_SERIES`] series are kept.
fn record(h: &mut History, ids_vals: &[(String, Option<f32>)], now: Instant) {
    h.series
        .retain(|id, _| ids_vals.iter().any(|(i, _)| i == id));
    for (id, v) in ids_vals {
        if !h.series.contains_key(id) && h.series.len() >= MAX_SERIES {
            continue;
        }
        let entry = h
            .series
            .entry(id.clone())
            .or_insert_with(|| (now, VecDeque::new()));
        if now.saturating_duration_since(entry.0) > STALE {
            entry.1.clear();
        }
        let Some(v) = v else { continue };
        entry.0 = now;
        entry.1.push_back(*v);
        while entry.1.len() > SERIES_CAP {
            entry.1.pop_front();
        }
    }
}

/// Stable per-row ids: the label path from the root, so equal labels under
/// different parents stay distinct.
fn row_ids(rows: &[AttribRow]) -> Vec<String> {
    let mut path: Vec<&str> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    rows.iter()
        .map(|r| {
            path.truncate(usize::from(r.depth));
            path.push(&r.label);
            let id = path.join("\u{1f}");
            let n = seen.entry(id.clone()).or_insert(0);
            let out = if *n == 0 {
                id
            } else {
                format!("{id}\u{1f}#{n}")
            };
            *n += 1;
            out
        })
        .collect()
}

/// A sparkline of `vals` through the caps glyph table (ASCII fallback),
/// scaled against 100% or the series peak. Empty under two samples.
fn trend_text(vals: &VecDeque<f32>) -> String {
    if vals.len() < 2 {
        return String::new();
    }
    let peak = vals.iter().copied().fold(100.0_f32, f32::max);
    let spark = crate::caps::raster_table().spark;
    let top = spark.len() - 1;
    vals.iter()
        .rev()
        .take(TREND_CELLS)
        .rev()
        .map(|v| spark[((v / peak).clamp(0.0, 1.0) * top as f32).round() as usize])
        .collect()
}

fn dash() -> String {
    "—".to_string()
}

fn render_row(r: &AttribRow, trend: String) -> Vec<Cell> {
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
        Cell::Text(trend, Tok::Slot(S::Ghost2)),
    ]
}

/// The worktree directory of the `workspace`/`worktree` pair, from the pane
/// index (the tree rows carry labels, not paths).
fn worktree_path(owners: &[PaneOwner], workspace: &str, worktree: &str) -> Option<String> {
    owners
        .iter()
        .find(|o| o.workspace == workspace && o.worktree == worktree)
        .and_then(|o| o.worktree_path.clone())
}

/// One [`RowActs`] per tree row. Workspace rows focus their first (heaviest)
/// worktree; worktree and pane rows focus their worktree; process rows open
/// the monitor's process tab; everything else has no Enter of its own. The id
/// is kind + enclosing worktree path + label (a process label carries its
/// pid), so a refresh keeps the cursor on the same row.
fn row_acts(owners: &[PaneOwner], rows: &[AttribRow]) -> Vec<RowActs> {
    let mut workspace: &str = "";
    let mut wt_path: Option<String> = None;
    let mut out = Vec::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        let path = match r.kind {
            RowKind::Workspace => {
                workspace = &r.label;
                wt_path = None;
                rows[i + 1..]
                    .iter()
                    .take_while(|n| n.depth > 0)
                    .find(|n| n.kind == RowKind::Worktree)
                    .and_then(|n| worktree_path(owners, &r.label, &n.label))
            }
            RowKind::Worktree => {
                wt_path = worktree_path(owners, workspace, &r.label);
                wt_path.clone()
            }
            RowKind::Pane | RowKind::Proc | RowKind::Job | RowKind::More => wt_path.clone(),
            RowKind::Thegn | RowKind::Outside => {
                workspace = "";
                wt_path = None;
                None
            }
        };
        let enter = match r.kind {
            RowKind::Workspace | RowKind::Worktree | RowKind::Pane => {
                path.clone().map(DetailAction::FocusWorktree)
            }
            RowKind::Proc => Some(DetailAction::OpenMonitor {
                tab: crate::monitor::MonitorTab::Procs,
                filter: r.target.map(|pid| format!("pid:{pid}")),
            }),
            RowKind::Thegn | RowKind::Job | RowKind::Outside | RowKind::More => None,
        };
        // `p`: show this pane's / worktree's processes in the monitor. A
        // worktree label with whitespace cannot be written as a `wt=` term.
        let procs = |filter: String| {
            (
                'p',
                DetailAction::OpenMonitor {
                    tab: crate::monitor::MonitorTab::Procs,
                    filter: Some(filter),
                },
            )
        };
        let keys = match (r.kind, r.target) {
            (RowKind::Pane, Some(id)) => vec![procs(format!("pane:{id}"))],
            (RowKind::Worktree, _) if !r.label.chars().any(char::is_whitespace) => {
                vec![procs(format!("wt={}", r.label))]
            }
            _ => Vec::new(),
        };
        out.push(RowActs {
            id: format!("{:?}|{}|{}", r.kind, path.unwrap_or_default(), r.label),
            enter,
            keys,
        });
    }
    out
}

/// The attribution sections (a heading with the sampling status, then the
/// tree as a table) and the tree table's row actions. `sort` is CPU for the
/// `cpu` popup, RSS for `mem`.
pub(crate) fn build_sections(model: &FrameModel, sort: SortKey) -> (Vec<Section>, Vec<RowActs>) {
    let tree = tree(model, sort);
    let header = vec![
        "where".into(),
        "cpu".into(),
        "share".into(),
        "mem".into(),
        "trend".into(),
    ];
    let ids = row_ids(&tree.rows);
    let trends: Vec<String> = HISTORY.with(|h| {
        let h = h.borrow();
        ids.iter()
            .map(|id| {
                h.series
                    .get(id)
                    .map(|s| trend_text(&s.1))
                    .unwrap_or_default()
            })
            .collect()
    });
    let acts = row_acts(&model.pane_owners, &tree.rows);
    let secs = vec![
        Section::Heading {
            label: HEADING.into(),
            note: Some(status_note(model, &tree)),
        },
        Section::Table(TableSection {
            header,
            rows: tree
                .rows
                .iter()
                .zip(trends)
                .map(|(r, t)| render_row(r, t))
                .collect(),
            sel: None,
        }),
    ];
    (secs, acts)
}

/// (Re)build the attribution sections of a `cpu`/`mem` popup in place:
/// everything from the [`HEADING`] down is replaced (appended when absent),
/// the graphs above are left alone, and the tree table gets the row cursor,
/// kept on the same row across rebuilds. `false` for any other popup.
pub(crate) fn install(ov: &mut DetailOverlay, model: &FrameModel, screen: Rect) -> bool {
    let Some(sort) = sort_for(&ov.title) else {
        return false;
    };
    let DetailContent::Sections(detail) = &mut ov.content else {
        return false;
    };
    let mut sections = std::mem::take(&mut detail.sections);
    let start = sections
        .iter()
        .position(|s| matches!(s, Section::Heading { label, .. } if label == HEADING))
        .unwrap_or(sections.len());
    sections.truncate(start);
    // Popup open only: the history advances here and nowhere else.
    let t = tree(model, sort);
    let ids: Vec<(String, Option<f32>)> = row_ids(&t.rows)
        .into_iter()
        .zip(&t.rows)
        .map(|(id, r)| (id, r.cpu_pct))
        .collect();
    HISTORY.with(|h| record(&mut h.borrow_mut(), &ids, Instant::now()));
    let (fresh, rows) = build_sections(model, sort);
    sections.extend(fresh);
    let content: usize = sections.iter().map(Section::height).sum();
    ov.rows = content.min(screen.rows.saturating_sub(3)).max(1);
    // No row with an Enter (thegn's own rows only): stay scroll-only.
    let actionable = rows.iter().any(|r| r.enter.is_some());
    ov.set_sections(SectionsDetail {
        sections,
        // The heading sits at `start`; the tree table right under it.
        cursor: actionable.then_some(SectionsCursor {
            table: start + 1,
            rows,
        }),
    });
    true
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
    install(ov, model, ctx.screen)
}

#[cfg(test)]
fn reset_history() {
    HISTORY.with(|h| h.borrow_mut().series.clear());
}

#[cfg(test)]
mod tests {
    use super::{
        HEADING, History, MAX_SERIES, MEM_TITLE, SERIES_CAP, STALE, TITLE, build_sections, is_open,
        record, refresh_open, reset_history, row_acts, row_ids,
    };
    use crate::chrome::{BarItemId, FrameModel};
    use crate::compositor::Rect;
    use crate::detail::{
        Cell, DetailAction, DetailContent, DetailOutcome, Placement, Section, StatusCtx, keyval,
        open_detail_for,
    };
    use crate::seg::Tok;
    use crate::telemetry::TelemetryHistory;
    use std::time::{Duration, Instant};
    use termwiz::input::{KeyCode, Modifiers};
    use thegn_core::cpu_attrib::{AttribRow, PaneOwner, RowKind, SortKey};

    #[test]
    fn row_ids_are_unique_for_equal_sibling_labels() {
        let r = |l: &str| AttribRow {
            depth: 0,
            kind: RowKind::Workspace,
            label: l.into(),
            cpu_pct: None,
            share_of_total: None,
            rss: None,
            target: None,
        };
        let ids = row_ids(&[r("x"), r("x")]);
        assert_ne!(ids[0], ids[1]);
    }

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
        let secs = build_sections(&model(), SortKey::Cpu).0;
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
        let mem = texts(&build_sections(&model(), SortKey::Mem).0);
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
        let lines = texts(&build_sections(&m, SortKey::Cpu).0);
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
    fn p_is_withheld_on_a_worktree_label_with_whitespace() {
        let row = |kind: RowKind, label: &str, target: Option<u32>| AttribRow {
            depth: 1,
            kind,
            label: label.into(),
            cpu_pct: None,
            share_of_total: None,
            rss: None,
            target,
        };
        let acts = row_acts(
            &[],
            &[
                row(RowKind::Worktree, "my tree", None),
                row(RowKind::Worktree, "tree", None),
                row(RowKind::Pane, "1 · sh", None),
            ],
        );
        assert!(acts[0].keys.is_empty());
        assert_eq!(acts[1].keys.len(), 1);
        assert!(acts[2].keys.is_empty(), "a pane row without an id");
    }

    #[test]
    fn worktree_rows_focus_their_worktree_and_process_rows_open_the_monitor() {
        let (secs, acts) = build_sections(&model(), SortKey::Cpu);
        let lines = texts(&secs);
        // Row i of the table is line i + 1 (line 0 is the heading).
        assert_eq!(acts.len() + 1, lines.len());
        let enter_of = |prefix: &str| {
            let i = lines
                .iter()
                .position(|l| l.starts_with(prefix))
                .unwrap_or_else(|| panic!("{prefix} in {lines:?}"));
            acts[i - 1].enter.clone()
        };
        let focus = |p: &str| Some(DetailAction::FocusWorktree(p.into()));
        assert_eq!(enter_of("alpha|"), focus("/wt/alpha-main"));
        assert_eq!(enter_of("alpha/main|"), focus("/wt/alpha-main"));
        assert_eq!(enter_of("1 · claude|"), focus("/wt/alpha-main"));
        assert_eq!(enter_of("beta|"), focus("/wt/beta"));
        assert_eq!(
            enter_of("node (11)|"),
            Some(DetailAction::OpenMonitor {
                tab: crate::monitor::MonitorTab::Procs,
                filter: Some("pid:11".into()),
            })
        );
        let keys_of = |prefix: &str| {
            let i = lines
                .iter()
                .position(|l| l.starts_with(prefix))
                .unwrap_or_else(|| panic!("{prefix} in {lines:?}"));
            acts[i - 1].keys.clone()
        };
        let procs_of = |f: &str| {
            vec![(
                'p',
                DetailAction::OpenMonitor {
                    tab: crate::monitor::MonitorTab::Procs,
                    filter: Some(f.into()),
                },
            )]
        };
        assert_eq!(keys_of("1 · claude|"), procs_of("pane:1"));
        assert_eq!(keys_of("alpha/main|"), procs_of("wt=alpha/main"));
        assert!(keys_of("alpha|").is_empty());
        assert!(keys_of("node (11)|").is_empty());
        assert_eq!(enter_of("thegn|"), None);
        assert_eq!(enter_of("outside thegn|"), None);
        // Identities are distinct, so a refresh can find the same row again.
        let ids: std::collections::BTreeSet<_> = acts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids.len(), acts.len());
    }

    #[test]
    fn the_popup_cursor_fires_focus_and_survives_a_refresh() {
        let hist = TelemetryHistory::default();
        let ctx = StatusCtx::new_for_test(&hist);
        let item = Rect {
            x: 80,
            y: 0,
            cols: 8,
            rows: 1,
        };
        let mut slot = open_detail_for(&BarItemId::Widget("cpu".into()), item, &model(), &ctx);
        let ov = slot.as_mut().expect("popup");
        // Cursor starts on the heaviest workspace (alpha).
        assert_eq!(
            ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
            DetailOutcome::Act(DetailAction::FocusWorktree("/wt/alpha-main".into()))
        );
        ov.handle_key(&KeyCode::Char('j'), Modifiers::NONE);
        let before = ov.sel;
        assert!(refresh_open(&mut slot, &model(), &ctx));
        let ov = slot.as_mut().expect("still open");
        assert_eq!(ov.sel, before, "same row after a rebuild");
        assert_eq!(
            ov.handle_key(&KeyCode::Char('M'), Modifiers::NONE),
            DetailOutcome::Act(DetailAction::OpenMonitor {
                tab: crate::monitor::MonitorTab::Cpu,
                filter: None,
            })
        );
    }

    #[test]
    fn monitor_popovers_anchor_to_their_chip_at_small_and_large_sizes() {
        let hist = TelemetryHistory::default();
        for screen in [(60usize, 20usize), (200, 60)] {
            let ctx = StatusCtx::new_for_test_on(&hist, Rect::full(screen.0, screen.1));
            let item = Rect {
                x: 30,
                y: 0,
                cols: 8,
                rows: 1,
            };
            let mut m = model();
            m.stats.disks = vec![thegn_metrics::DiskInfo {
                name: "sda".into(),
                mount: "/".into(),
                free_pct: 20,
                read_bps: 0,
                write_bps: 0,
                kind: thegn_metrics::DiskKind::Ssd,
                extra_mounts: 0,
            }]
            .into();
            for widget in ["cpu", "mem", "disk"] {
                let ov = open_detail_for(&BarItemId::Widget(widget.into()), item, &m, &ctx)
                    .unwrap_or_else(|| panic!("{widget} opens a popup"));
                assert!(
                    matches!(ov.placement, Placement::NearBelow(_)),
                    "{widget} at {screen:?}"
                );
                assert!(ov.rows <= screen.1, "{widget} fits {screen:?}: {}", ov.rows);
            }
        }
    }

    #[test]
    fn status_note_covers_unsampled_partial_and_warming() {
        let empty = texts(&build_sections(&FrameModel::default(), SortKey::Cpu).0);
        assert_eq!(
            empty[0],
            "by workspace: open the breakdown to start sampling"
        );
        assert!(empty.iter().any(|l| l == "thegn|—|—|—|"), "{empty:?}");

        let mut partial = model();
        partial.procs.summary = None;
        partial.procs.primed = false;
        let lines = texts(&build_sections(&partial, SortKey::Cpu).0);
        assert_eq!(
            lines[0],
            "by workspace: per-pane totals unavailable on this platform · cpu warming up"
        );
        assert!(lines.iter().any(|l| l.starts_with("alpha|—|—|—")));

        let mut clamped = model();
        clamped.stats.cpu_pct = Some(1);
        let lines = texts(&build_sections(&clamped, SortKey::Cpu).0);
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
        assert!(!is_open(slot.as_ref()));
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
            assert!(is_open(slot.as_ref()));
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
        assert!(!is_open(other.as_ref()));
        assert!(!refresh_open(&mut other, &fresh, &ctx));
        let mut none = None;
        assert!(!refresh_open(&mut none, &fresh, &ctx));
    }

    fn one(id: &str, v: Option<f32>) -> Vec<(String, Option<f32>)> {
        vec![(id.to_string(), v)]
    }

    #[test]
    fn record_caps_each_series_at_sixteen() {
        let mut h = History::default();
        let t = Instant::now();
        for i in 0..40 {
            record(&mut h, &one("a", Some(i as f32)), t);
        }
        let s = &h.series["a"].1;
        assert_eq!(s.len(), SERIES_CAP);
        assert_eq!(s.back().copied(), Some(39.0));
    }

    #[test]
    fn record_resets_a_stale_series_and_skips_none() {
        let mut h = History::default();
        let t = Instant::now();
        record(&mut h, &one("a", Some(1.0)), t);
        record(&mut h, &one("a", Some(2.0)), t);
        record(&mut h, &one("a", None), t);
        assert_eq!(h.series["a"].1.len(), 2, "None is skipped");
        record(
            &mut h,
            &one("a", Some(3.0)),
            t + STALE + Duration::from_secs(1),
        );
        assert_eq!(h.series["a"].1.len(), 1, "stale series restarts");
    }

    #[test]
    fn record_drops_absent_ids_and_caps_series_count() {
        let mut h = History::default();
        let t = Instant::now();
        record(&mut h, &one("a", Some(1.0)), t);
        record(&mut h, &one("b", Some(1.0)), t);
        assert!(!h.series.contains_key("a"));
        let many: Vec<_> = (0..MAX_SERIES + 10)
            .map(|i| (format!("r{i}"), Some(1.0)))
            .collect();
        record(&mut h, &many, t);
        assert_eq!(h.series.len(), MAX_SERIES);
    }

    #[test]
    fn trend_cell_fills_after_two_installs_and_is_empty_for_fresh_rows() {
        reset_history();
        let hist = TelemetryHistory::default();
        let ctx = StatusCtx::new_for_test(&hist);
        let item = Rect {
            x: 80,
            y: 0,
            cols: 8,
            rows: 1,
        };
        let mut slot = open_detail_for(&BarItemId::Widget("cpu".into()), item, &model(), &ctx);
        let trend_of = |slot: Option<&crate::detail::DetailOverlay>, label: &str| {
            let Some(ov) = slot else { unreachable!() };
            let DetailContent::Sections(d) = &ov.content else {
                panic!("sectioned")
            };
            texts(&d.sections)
                .into_iter()
                .find(|l| l.starts_with(label))
                .map(|l| l.rsplit('|').next().unwrap_or("").to_string())
                .expect("row")
        };
        assert_eq!(
            trend_of(slot.as_ref(), "alpha|"),
            "",
            "one sample: no trend"
        );
        assert!(refresh_open(&mut slot, &model(), &ctx));
        assert!(!trend_of(slot.as_ref(), "alpha|").is_empty());
        assert!(!trend_of(slot.as_ref(), "alpha|").is_ascii());
        crate::caps::test_override::with_unicode(thegn_core::termcaps::UnicodeLevel::Ascii, || {
            assert!(refresh_open(&mut slot, &model(), &ctx));
            let t = trend_of(slot.as_ref(), "alpha|");
            assert!(!t.is_empty() && t.is_ascii(), "{t:?}");
        });
        // A row that appears only now has a single sample.
        let mut fresh = model();
        fresh.pane_owners = Vec::<PaneOwner>::new().into();
        assert!(refresh_open(&mut slot, &fresh, &ctx));
        assert_eq!(trend_of(slot.as_ref(), "unattributed panes|"), "");
        assert_eq!(row_ids(&[]).len(), 0);
        reset_history();
    }
}
