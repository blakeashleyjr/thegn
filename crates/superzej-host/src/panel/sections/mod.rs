//! Per-section renderers for the accordion panel. Each section contributes a
//! one-line summary (the closed row's right side) and three distinct bodies
//! keyed off the panel width: a compact Normal view, a deeper Half view, and
//! a Full view that owns the whole band (the former overlay layouts).

use superzej_core::theme::Hue;
use superzej_core::viz;

use crate::chrome::S;
use crate::seg::{self, Line, Seg, Tok, seg, sp};

use super::{ChangeRow, PanelData, PanelHit, PanelUi, Section, Stage};

mod changes;
mod git;
mod keys;
mod misc;
mod telemetry;

// Token shorthands (the mockup's class vocabulary), shared by the builders.
fn t() -> Tok {
    Tok::Slot(S::Text)
}
fn d() -> Tok {
    Tok::Slot(S::Dim)
}
fn f() -> Tok {
    Tok::Slot(S::Faint)
}
fn g() -> Tok {
    Tok::Slot(S::Ghost)
}
fn g2() -> Tok {
    Tok::Slot(S::Ghost2)
}
fn g3() -> Tok {
    Tok::Slot(S::Ghost3)
}
fn ac() -> Tok {
    Tok::Slot(S::Accent)
}
fn hue(h: Hue) -> Tok {
    Tok::Hue(h)
}

/// One rendered row: its line spec, an optional row background (selection /
/// open-section tint), and an optional hit target.
#[derive(Debug, Clone)]
pub struct PanelRow {
    pub line: Line,
    pub bg: Option<Tok>,
    pub hit: Option<PanelHit>,
}

impl PanelRow {
    pub fn plain(line: Line) -> PanelRow {
        PanelRow {
            line,
            bg: None,
            hit: None,
        }
    }
    pub fn blank() -> PanelRow {
        PanelRow::plain(Line::Blank)
    }
    pub fn with_hit(mut self, h: PanelHit) -> PanelRow {
        self.hit = Some(h);
        self
    }
    pub fn with_bg(mut self, bg: Tok) -> PanelRow {
        self.bg = Some(bg);
        self
    }
}

/// Everything a section body builder needs: the data model, the interactive
/// state (which carries the Normal/Half/Full view on `ui.width` plus the
/// fetched docs), and the real geometry of the space the rows paint into.
pub struct SectionCtx<'a> {
    pub model: &'a crate::chrome::FrameModel,
    pub ui: &'a PanelUi,
    /// Usable content columns (the panel width minus the section indent).
    pub cols: usize,
    /// Body rows available. Normal/Half: a post-skeleton estimate (the budget
    /// still truncates overflow to a "+N more" row). Full: exact.
    pub rows: usize,
}

impl SectionCtx<'_> {
    /// Deep content — anything past the resting width earns richer bodies.
    pub fn deep(&self) -> bool {
        self.ui.width.is_expanded()
    }

    /// Whether this body owns the whole band (the former overlay layouts).
    pub fn full(&self) -> bool {
        self.ui.width == crate::layout::PanelWidth::Full
    }
}

/// A `(bar, track)` pair as segs.
fn bar_segs(frac: f32, w: usize, fg: Tok) -> Vec<Seg> {
    let (bar, track) = viz::bar_track(frac, w);
    vec![seg(fg, bar), seg(g3(), track)]
}

/// The mockup's tiny add/delete split bar (`w` cells, green then red).
fn split_bar(added: u32, deleted: u32, w: usize) -> Vec<Seg> {
    let total = (added + deleted).max(1) as f32;
    let green = ((added as f32 / total) * w as f32).round() as usize;
    let green = green.min(w);
    vec![
        seg(hue(Hue::Green), "█".repeat(green)),
        seg(hue(Hue::Red), "█".repeat(w - green)),
    ]
}

/// `+N` / `−N` diffstat segs, right-padded like the mockup.
fn diffstat(added: u32, deleted: u32) -> Vec<Seg> {
    let plus = format!("{:>4}", format!("+{added}"));
    let minus = if deleted > 0 {
        format!("{:>4}", format!("−{deleted}"))
    } else {
        "    ".into()
    };
    vec![seg(hue(Hue::Green), plus), seg(hue(Hue::Red), minus)]
}

/// Compact seconds ("41s", "2m41s", "1h12m").
fn fmt_secs(s: i64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// A dim per-section hint row from (key, label) pairs.
fn hint_row(pairs: &[(&str, &str)]) -> PanelRow {
    let mut segs: Vec<Seg> = Vec::new();
    for (i, (k, label)) in pairs.iter().enumerate() {
        if i > 0 {
            segs.push(seg(g2(), " · "));
        }
        segs.push(seg(g2(), format!("{k} {label}")));
    }
    PanelRow::plain(Line::segs(segs))
}

/// A dotted rule row (the wide views' section seams).
fn rule() -> PanelRow {
    PanelRow::plain(Line::Fill {
        ch: '╌', fg: g3()
    })
}

/// The loading line for a body whose document fetch is still out.
fn spinner_row(tick: u64, what: &str) -> PanelRow {
    PanelRow::plain(Line::segs(vec![
        seg(ac(), viz::spin(tick).to_string()),
        seg(d(), format!(" loading {what}…")),
    ]))
}

/// Compose two columns of seg rows into single lines: the left column is
/// clipped/padded to `left_w`, then a `gap`, then the right column. Rows
/// beyond the shorter column render the other column alone.
fn two_col(left: &[Vec<Seg>], right: &[Vec<Seg>], left_w: usize, gap: usize) -> Vec<Line> {
    let rows = left.len().max(right.len());
    let empty: Vec<Seg> = Vec::new();
    (0..rows)
        .map(|i| {
            let mut segs = seg::cut(left.get(i).unwrap_or(&empty), left_w);
            let used = seg::seg_width(&segs);
            segs.push(sp(left_w.saturating_sub(used) + gap));
            segs.extend(right.get(i).unwrap_or(&empty).iter().cloned());
            Line::Segs(segs)
        })
        .collect()
}

// ---- summaries (the closed row's right side) -------------------------------

pub fn summary(section: Section, model: &crate::chrome::FrameModel) -> Vec<Seg> {
    let data = &model.panel;
    match section {
        Section::Changes => {
            let (a, del): (u32, u32) = data
                .changes
                .iter()
                .fold((0, 0), |(a, d), c| (a + c.added, d + c.deleted));
            if data.changes.is_empty() {
                vec![seg(g(), "clean")]
            } else {
                vec![
                    seg(hue(Hue::Green), format!("+{a}")),
                    seg(g(), " "),
                    seg(hue(Hue::Red), format!("−{del}")),
                ]
            }
        }
        Section::Git => match &data.pr {
            Some(pr) => {
                let (mut pass, mut fail, mut pend) = (0u32, 0u32, 0u32);
                for c in &data.checks {
                    match c.state {
                        super::CheckState::Pass => pass += 1,
                        super::CheckState::Fail => fail += 1,
                        super::CheckState::Pending => pend += 1,
                    }
                }
                let mut v = vec![
                    seg(g(), format!("#{} ", pr.number)),
                    seg(
                        pr_state_hue(&pr.state, pr.is_draft),
                        pr.state.to_lowercase(),
                    ),
                ];
                if pass + fail + pend > 0 {
                    v.push(seg(g(), " · "));
                    v.push(seg(hue(Hue::Green), format!("✓{pass}")));
                    if fail > 0 {
                        v.push(seg(g(), " "));
                        v.push(seg(hue(Hue::Red), format!("✗{fail}")));
                    }
                    if pend > 0 {
                        v.push(seg(g(), " "));
                        v.push(seg(hue(Hue::Amber), format!("…{pend}")));
                    }
                }
                let unresolved = data.threads.iter().filter(|t| !t.resolved).count();
                if unresolved > 0 {
                    v.push(seg(g(), " "));
                    v.push(seg(hue(Hue::Amber), format!("⊘{unresolved}")));
                }
                v
            }
            None => vec![seg(g(), "no pr")],
        },
        Section::Files => {
            let loc = model.loc.map(compact_count);
            match (data.file_count, loc) {
                (Some(n), Some(loc)) => vec![seg(g(), format!("{n} · {loc} loc"))],
                (None, Some(loc)) => vec![seg(g(), format!("{loc} loc"))],
                _ => vec![seg(g(), format!("{} changed", data.changes.len()))],
            }
        }
        Section::Tests => match &data.tests {
            Some(t) if t.passed + t.failed + t.skipped > 0 => {
                let mut v = vec![seg(hue(Hue::Green), format!("✓ {}", t.passed))];
                if t.failed > 0 {
                    v.push(seg(g(), " · "));
                    v.push(seg(hue(Hue::Red), format!("✗ {}", t.failed)));
                }
                if t.skipped > 0 {
                    v.push(seg(g(), format!(" · {} skip", t.skipped)));
                }
                v
            }
            _ => vec![seg(g(), "not run")],
        },
        Section::Debug => vec![seg(g2(), "○ idle")],
        Section::Sandbox => {
            let ours: Vec<_> = model.containers.iter().filter(|c| c.ours).collect();
            match ours.first() {
                Some(c) => vec![
                    seg(hue(Hue::Green), "●"),
                    seg(g(), format!(" {}", c.backend)),
                ],
                None => vec![seg(g2(), "○ off")],
            }
        }
        Section::Db => vec![seg(g2(), "—")],
        Section::Telemetry => {
            let s = &model.stats;
            match (s.cpu_pct, s.mem_gib) {
                (Some(c), Some((u, t))) if t > 0.0 => vec![seg(
                    g(),
                    format!("cpu {c}% · mem {}%", ((u / t) * 100.0).round() as u32),
                )],
                (Some(c), _) => vec![seg(g(), format!("cpu {c}%"))],
                _ => vec![seg(g2(), "—")],
            }
        }
        Section::Keys => vec![seg(g2(), "?")],
    }
}

fn pr_state_hue(state: &str, draft: bool) -> Tok {
    if draft {
        return hue(Hue::Amber);
    }
    match state {
        "OPEN" => hue(Hue::Green),
        "MERGED" | "CLOSED" => hue(Hue::Purple),
        _ => hue(Hue::Amber),
    }
}

fn compact_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// ---- content dispatch -------------------------------------------------------

/// A section's content rows for the current view.
pub fn content(section: Section, ctx: &SectionCtx) -> Vec<PanelRow> {
    match section {
        Section::Changes => changes::content(ctx),
        Section::Git => git::content(ctx),
        Section::Files => misc::files(ctx),
        Section::Tests => misc::tests(ctx),
        Section::Debug => misc::debug(),
        Section::Sandbox => misc::sandbox(ctx),
        Section::Db => misc::db(),
        Section::Telemetry => telemetry::content(ctx),
        Section::Keys => keys::content(ctx),
    }
}

/// The review threads the git section displays for a view — render and the
/// loop's Enter handler share this filter so they can never drift.
pub fn visible_threads(
    data: &PanelData,
    deep: bool,
) -> impl Iterator<Item = &superzej_core::github::ReviewThreadRow> {
    let cap = if deep { 4 } else { 2 };
    data.threads
        .iter()
        .filter(move |t| !t.resolved || deep)
        .take(cap)
}

#[cfg(test)]
mod spec {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::layout::PanelWidth;
    use crate::panel::docs::{DiffDoc, GitDocs, PanelDocs};

    fn model() -> FrameModel {
        let mut m = FrameModel::default();
        m.panel.branch = "feat/views".into();
        m.panel.changes = vec![ChangeRow {
            status: "M".into(),
            stage: Stage::Unstaged,
            dir: "src/".into(),
            name: "lib.rs".into(),
            path: "src/lib.rs".into(),
            added: 10,
            deleted: 2,
        }];
        m.panel.log = vec![superzej_svc::git::LogRow {
            graph: "*".into(),
            sha: "abc1234".into(),
            subject: "feat: land".into(),
            refs: "HEAD -> main".into(),
        }];
        m.panel.tests = Some(crate::panel::TestsLite {
            passed: 18,
            failed: 1,
            skipped: 2,
            error: None,
            failures: vec![("flaky::case".into(), "src/lib.rs:42".into())],
            history: vec![
                crate::testkit::model::TestRunRec {
                    at: 200,
                    passed: 18,
                    failed: 1,
                    skipped: 2,
                    duration_ms: 1200,
                    branch: "feat/views".into(),
                },
                crate::testkit::model::TestRunRec {
                    at: 100,
                    passed: 19,
                    failed: 0,
                    skipped: 2,
                    duration_ms: 1100,
                    branch: "main".into(),
                },
            ],
        });
        m.containers = vec![
            superzej_core::sandbox::ContainerInfo {
                name: "sz-feat-views".into(),
                image: "rust:1".into(),
                status: "Up 3m".into(),
                ours: true,
                backend: "podman".into(),
                cpu: "4%".into(),
                mem: "120MiB".into(),
                net: "1.2kB".into(),
                containment: "rootless".into(),
                mounts: "/work → /repo".into(),
            },
            superzej_core::sandbox::ContainerInfo {
                name: "other-svc".into(),
                image: "postgres:16".into(),
                status: "Up 1h".into(),
                ours: false,
                backend: "docker".into(),
                cpu: "1%".into(),
                mem: "60MiB".into(),
                net: "0.4kB".into(),
                containment: String::new(),
                mounts: String::new(),
            },
        ];
        m.stats.cpu_pct = Some(42);
        m.stats.cpu_cores = vec![10, 90];
        m.stats.mem_gib = Some((8.0, 16.0));
        m.stats.net_bps = Some((2048, 1024));
        m
    }

    fn docs() -> PanelDocs {
        let mut docs = PanelDocs {
            git: Some(GitDocs {
                heat: vec![[0, 1, 2, 3, 4, 0, 1]; 36],
                weekly: vec![3; 36],
                log: vec![superzej_svc::git::LogRow {
                    graph: "*".into(),
                    sha: "abc1234".into(),
                    subject: "feat: land".into(),
                    refs: "HEAD -> main".into(),
                }],
                total: 108,
                head_branch: "main".into(),
            }),
            diff: Some(DiffDoc {
                path: "src/lib.rs".into(),
                file: superzej_core::diff_sbs::parse_unified(
                    "@@ -1,2 +1,2 @@ fn demo()\n ctx\n-old\n+new\n@@ -10,1 +10,2 @@\n keep\n+added\n",
                ),
            }),
            cfg_keys: crate::keyhint::cheatsheet_groups(&superzej_core::config::Config::default()),
            ..Default::default()
        };
        docs.telemetry.push(&model().stats);
        docs
    }

    fn ui(width: PanelWidth, open: Section) -> PanelUi {
        PanelUi {
            open,
            width,
            docs: docs(),
            ..Default::default()
        }
    }

    fn text(rows: &[PanelRow]) -> String {
        let segs = |v: &[Seg]| v.iter().map(|s| s.text.clone()).collect::<String>();
        rows.iter()
            .map(|r| match &r.line {
                Line::Blank => String::new(),
                Line::Fill { ch, .. } => ch.to_string(),
                Line::Segs(v) => segs(v),
                Line::Split { l, r } => format!("{} {}", segs(l), segs(r)),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(section: Section, width: PanelWidth) -> String {
        let m = model();
        let u = ui(width, section);
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
        text(&content(section, &ctx))
    }

    #[test]
    fn every_section_renders_three_distinct_views() {
        for section in crate::panel::SECTION_ORDER {
            let n = render(section, PanelWidth::Normal);
            let h = render(section, PanelWidth::Half);
            let f = render(section, PanelWidth::Full);
            assert!(!n.is_empty(), "{section:?} normal");
            assert!(!h.is_empty(), "{section:?} half");
            assert!(!f.is_empty(), "{section:?} full");
            // Debug/Db are placeholder sections — distinctness is waived.
            if matches!(section, Section::Debug | Section::Db) {
                continue;
            }
            assert_ne!(n, f, "{section:?}: normal vs full");
            assert_ne!(h, f, "{section:?}: half vs full");
        }
    }

    #[test]
    fn full_views_carry_the_overlay_signatures() {
        let f = render(Section::Telemetry, PanelWidth::Full);
        assert!(
            f.contains("CPU") && f.contains("MEM") && f.contains("NET"),
            "{f}"
        );
        assert!(f.contains("c0") && f.contains("c1"), "core sparkrow: {f}");
        let f = render(Section::Git, PanelWidth::Full);
        assert!(
            f.contains("COMMITS") && f.contains("VELOCITY") && f.contains("LOG"),
            "{f}"
        );
        assert!(f.contains("abc1234"), "{f}");
        let f = render(Section::Changes, PanelWidth::Full);
        assert!(f.contains("src/lib.rs") && f.contains("hunk 1/2"), "{f}");
        assert!(f.contains("old") && f.contains("new"), "both sides: {f}");
        let f = render(Section::Keys, PanelWidth::Full);
        assert!(f.contains("PANEL"), "{f}");
        assert!(
            f.to_uppercase().contains("NAVIGATION"),
            "cheatsheet group: {f}"
        );
    }

    #[test]
    fn full_bodies_load_gracefully_without_docs() {
        let m = model();
        let mut u = ui(PanelWidth::Full, Section::Git);
        u.docs.git = None;
        u.docs.diff = None;
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 150,
            rows: 38,
        };
        assert!(text(&content(Section::Git, &ctx)).contains("loading"));
        let mut u = ui(PanelWidth::Full, Section::Changes);
        u.docs.diff = None;
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 150,
            rows: 38,
        };
        assert!(text(&content(Section::Changes, &ctx)).contains("loading"));
    }

    #[test]
    fn two_col_pads_clips_and_zips() {
        let left = vec![
            vec![seg(t(), "left")],
            vec![seg(t(), "a-very-long-left-row")],
        ];
        let right = vec![vec![seg(t(), "right")]];
        let lines = two_col(&left, &right, 8, 2);
        assert_eq!(lines.len(), 2);
        let segs = |l: &Line| match l {
            Line::Segs(v) => v.iter().map(|s| s.text.clone()).collect::<String>(),
            _ => String::new(),
        };
        assert_eq!(segs(&lines[0]), "left      right");
        // The long left row clips with an ellipsis and the right stays put.
        assert!(segs(&lines[1]).starts_with("a-very-"));
        assert!(segs(&lines[1]).contains('…'));
    }
}
