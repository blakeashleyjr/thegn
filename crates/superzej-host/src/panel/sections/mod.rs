//! Per-section renderers for the accordion panel. Each section contributes a
//! one-line summary (the closed row's right side) and three distinct bodies
//! keyed off the panel width: a compact Normal view, a deeper Half view, and
//! a Full view that owns the whole band (the former overlay layouts).

use superzej_core::theme::Hue;
use superzej_core::viz;

use crate::chrome::S;
use crate::seg::{self, Line, Seg, Tok, seg, sp};

use super::{ChangeRow, PanelData, PanelHit, PanelUi, Section, Stage};

mod branches;
mod changes;
pub(crate) mod commits;
mod git;
mod issues;
mod keys;
mod logs;
mod misc;
mod notifications;
mod problems;
mod stash;
mod symbols;
mod tasks;
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

/// `+N` / `−N` diffstat segs. The minus is omitted when there are no deletions
/// so the phantom padding doesn't eat into the path display budget.
fn diffstat(added: u32, deleted: u32) -> Vec<Seg> {
    let plus = seg(hue(Hue::Green), format!("{:>4}", format!("+{added}")));
    if deleted > 0 {
        vec![
            plus,
            seg(hue(Hue::Red), format!("{:>4}", format!("−{deleted}"))),
        ]
    } else {
        vec![plus]
    }
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

/// A hint row built from a git context's key table (the same data that
/// drives dispatch, so the hints can never drift).
pub(super) fn context_hint_row(view: crate::panel::gitui::GitView) -> PanelRow {
    let keys = crate::panel::gitui::context_keys(view);
    let pairs: Vec<(&str, &str)> = keys.iter().take(5).map(|c| (c.chord, c.label)).collect();
    hint_row(&pairs)
}

/// The live filter line for a git list view (`❯ query  n/m`), when `/` is
/// active on it.
pub(super) fn filter_row(
    ui: &PanelUi,
    view: crate::panel::gitui::GitView,
    total: usize,
) -> Option<PanelRow> {
    let f = ui.git.filter.as_ref().filter(|f| f.view == view)?;
    let shown = f.map.len();
    Some(PanelRow::plain(Line::split(
        vec![
            seg(ac(), "❯ "),
            seg(t(), f.query.clone()).bold(),
            if f.editing { seg(ac(), "▏") } else { sp(0) },
        ],
        vec![seg(g2(), format!("{shown}/{total}"))],
    )))
}

/// The display-ordered source indices of a git list under its filter
/// (identity when no filter is live on `view`).
pub(super) fn filtered_indices(
    ui: &PanelUi,
    view: crate::panel::gitui::GitView,
    len: usize,
    label: impl Fn(usize) -> String,
) -> Vec<usize> {
    match &ui.git.filter {
        Some(f) if f.view == view && !f.query.is_empty() => {
            let labels: Vec<String> = (0..len).map(label).collect();
            crate::panel::gitui::fuzzy_filter(&labels, &f.query)
        }
        _ => (0..len).collect(),
    }
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
        Section::Commits => match data.commits.first() {
            Some(c) => vec![
                seg(ac(), c.short.clone()),
                seg(g(), format!(" {}", truncate_summary(&c.subject, 18))),
            ],
            None => vec![seg(g2(), "—")],
        },
        Section::Branches => {
            let n = data.branches.len();
            let prs = data.branches.iter().filter(|b| b.pr.is_some()).count();
            if prs > 0 {
                vec![
                    seg(g(), format!("{n} · ")),
                    seg(hue(Hue::Green), format!("⬤ {prs} pr")),
                ]
            } else if n > 0 {
                vec![seg(g(), n.to_string())]
            } else {
                vec![seg(g2(), "—")]
            }
        }
        Section::Stash => {
            if data.stashes.is_empty() {
                vec![seg(g2(), "—")]
            } else {
                vec![seg(g(), data.stashes.len().to_string())]
            }
        }
        Section::Pr => match &data.pr {
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
        Section::Jobs => {
            let specs = &data.task_specs;
            let runs = &data.task_last_runs;
            if specs.is_empty() {
                vec![seg(g2(), "none")]
            } else {
                let running = runs.values().filter(|r| r.running).count();
                let failed = runs
                    .values()
                    .filter(|r| !r.running && r.exit_code != 0)
                    .count();
                if running > 0 {
                    vec![
                        seg(hue(Hue::Amber), format!("… {running}")),
                        seg(g(), format!("/{}", specs.len())),
                    ]
                } else if failed > 0 {
                    vec![
                        seg(hue(Hue::Red), format!("✗ {failed}")),
                        seg(g(), format!("/{}", specs.len())),
                    ]
                } else {
                    vec![seg(g(), specs.len().to_string())]
                }
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
        Section::Issues => {
            let n = model.panel.tracker_issues.len();
            let open = model
                .panel
                .tracker_issues
                .iter()
                .filter(|i| i.status.is_active())
                .count();
            let linked = model.panel.tracker_links.len();
            if n == 0 {
                vec![seg(g2(), "off")]
            } else if linked > 0 {
                vec![
                    seg(hue(Hue::Amber), format!("◈{linked} ")),
                    seg(g(), format!("{open}/{n}")),
                ]
            } else {
                vec![seg(g(), format!("{open} open"))]
            }
        }
        Section::Notifications => {
            let u = model.panel.unread_notifications;
            if u == 0 {
                vec![seg(g2(), "inbox zero")]
            } else {
                vec![seg(hue(Hue::Red), format!("⚑ {u}"))]
            }
        }
        Section::Logs => {
            let n = model.panel.log_lines.len();
            let errors = model
                .panel
                .log_lines
                .iter()
                .filter(|l| l.level == superzej_core::log_view::LogLevel::Error)
                .count();
            if n == 0 {
                vec![seg(g2(), "off")]
            } else if errors > 0 {
                vec![
                    seg(hue(Hue::Red), format!("✗ {errors}")),
                    seg(g(), format!(" · {n}")),
                ]
            } else {
                vec![seg(g(), format!("{n} lines"))]
            }
        }
        Section::Problems => {
            let n = model.panel.diagnostics.len();
            let errors = model
                .panel
                .diagnostics
                .iter()
                .filter(|d| d.severity == super::Severity::Error)
                .count();
            let warnings = model
                .panel
                .diagnostics
                .iter()
                .filter(|d| d.severity == super::Severity::Warning)
                .count();
            if n == 0 {
                vec![seg(g2(), "clean")]
            } else if errors > 0 {
                vec![
                    seg(hue(Hue::Red), format!("✗ {errors}")),
                    if warnings > 0 {
                        seg(g(), format!("  ⚠ {warnings}"))
                    } else {
                        seg(g(), String::new())
                    },
                ]
            } else {
                vec![seg(hue(Hue::Amber), format!("⚠ {warnings}"))]
            }
        }
        Section::Symbols => {
            let n = model.panel.symbols.len();
            if n == 0 {
                vec![seg(g2(), "—")]
            } else {
                vec![seg(g(), n.to_string())]
            }
        }
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

/// Clip a summary string to `max` chars with an ellipsis.
fn truncate_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
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
        Section::Commits => commits::content(ctx),
        Section::Branches => branches::content(ctx),
        Section::Stash => stash::content(ctx),
        Section::Pr => git::content(ctx),
        Section::Files => misc::files(ctx),
        Section::Problems => problems::content(ctx),
        Section::Jobs => tasks::content(ctx),
        Section::Tests => misc::tests(ctx),
        Section::Symbols => symbols::content(ctx),
        Section::Debug => misc::debug(),
        Section::Sandbox => misc::sandbox(ctx),
        Section::Db => misc::db(),
        Section::Telemetry => telemetry::content(ctx),
        Section::Keys => keys::content(ctx),
        Section::Issues => issues::content(ctx),
        Section::Notifications => notifications::content(ctx),
        Section::Logs => logs::content(ctx),
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
        m.panel.symbols_file = "src/lib.rs".into();
        m.panel.symbols = vec![
            crate::panel::SymbolRow {
                kind: "struct".into(),
                name: "Views".into(),
                file: "src/lib.rs".into(),
                line: 1,
                col: 7,
                depth: 0,
            },
            crate::panel::SymbolRow {
                kind: "fn".into(),
                name: "render".into(),
                file: "src/lib.rs".into(),
                line: 12,
                col: 7,
                depth: 1,
            },
        ];
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
        m.panel.commits = vec![crate::panel::CommitRow {
            sha: "abc1234def".into(),
            short: "abc1234".into(),
            subject: "feat: land the views".into(),
            author: "Blake Ashley".into(),
            date: 1,
            refs: "HEAD -> main".into(),
            parents: vec![],
        }];
        m.panel.branches = vec![crate::panel::BranchRow {
            name: "feat/views".into(),
            is_head: true,
            upstream: Some("origin/feat/views".into()),
            ahead: 2,
            behind: 1,
            upstream_gone: false,
            sha: "abc1234def".into(),
            date: 1,
            subject: "feat: land the views".into(),
            pr: Some(crate::panel::PrBadge {
                number: 7,
                state: "OPEN".into(),
                is_draft: false,
                url: "https://github.com/o/r/pull/7".into(),
            }),
        }];
        m.panel.stashes = vec![crate::panel::StashRow {
            index: 0,
            sha: "abc1234def".into(),
            date: 1,
            message: "WIP on main: tinkering".into(),
        }];
        m.panel.notifications = vec![superzej_core::notification::Notification {
            id: 1,
            kind: superzej_core::notification::NotificationKind::AgentDone,
            source_ref: "linear:ABC-42".into(),
            message: "agent finished in feat/views".into(),
            created_at_ms: 1_700_000_000_000,
            read: false,
            worktree_path: "/repo/feat-views".into(),
        }];
        m.panel.task_specs = vec![superzej_core::config::Task {
            name: "build".into(),
            command: "cargo build".into(),
            args: vec![],
            cwd: None,
            env: Default::default(),
            kind: superzej_core::config::TaskKind::Build,
            matcher: None,
            scope: None,
        }];
        m.panel.task_last_runs = {
            let mut h = std::collections::HashMap::new();
            h.insert(
                "build".into(),
                crate::panel::TaskRunRecord {
                    name: "build".into(),
                    exit_code: 0,
                    duration_ms: 2_340,
                    finished_at: 1_700_000_000_000,
                    output_tail: "Compiling superzej v0.1.0\nFinished dev profile".into(),
                    running: false,
                },
            );
            h
        };
        m.panel.unread_notifications = 1;
        m.panel.log_lines = vec![
            superzej_core::log_view::parse_log_line(
                "2026-06-05T12:00:00  INFO  superzej::db  connection opened",
            )
            .unwrap(),
            superzej_core::log_view::parse_log_line(
                "2026-06-05T12:00:01  ERROR superzej::host  fatal error",
            )
            .unwrap(),
        ];
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
    fn changes_section_shows_entity_impact_and_headers() {
        use superzej_core::semantic::{EntityChange, EntityKind, EntitySummary, Touch};
        let mut m = model(); // includes a change row at src/lib.rs
        m.panel.entities = Some(EntitySummary::new(vec![(
            "src/lib.rs".into(),
            vec![EntityChange {
                kind: EntityKind::Function,
                name: "go".into(),
                added: 3,
                deleted: 1,
                touch: Touch::Modified,
            }],
        )]));
        let mut u = ui(PanelWidth::Normal, Section::Changes);
        u.chg_sel = Some(0); // select the row so its entity header renders
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 39,
            rows: 28,
        };
        let rendered = text(&content(Section::Changes, &ctx));
        assert!(rendered.contains('◈'), "impact line: {rendered}");
        assert!(rendered.contains("fn go"), "entity header: {rendered}");
    }

    #[test]
    fn commits_section_shows_loading_while_cache_is_empty() {
        let mut m = model();
        m.panel.commits.clear();
        m.panel.commits_loading = true;
        let u = ui(PanelWidth::Normal, Section::Commits);
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 39,
            rows: 28,
        };
        let rendered = text(&content(Section::Commits, &ctx));
        assert!(rendered.contains("loading commits…"), "{rendered}");
    }

    #[test]
    fn sandbox_section_prefers_active_worktree_container() {
        let mut m = model();
        m.active_container_name = "superzej-active-worktree".into();
        m.containers = vec![
            superzej_core::sandbox::ContainerInfo {
                name: "superzej-other-worktree".into(),
                image: "rust:1".into(),
                status: "Up 1h".into(),
                ours: true,
                backend: "podman".into(),
                cpu: "9%".into(),
                mem: "300MiB".into(),
                net: "9kB".into(),
                containment: "other-policy".into(),
                mounts: String::new(),
            },
            superzej_core::sandbox::ContainerInfo {
                name: "superzej-active-worktree".into(),
                image: "rust:1".into(),
                status: "Up 2m".into(),
                ours: true,
                backend: "podman".into(),
                cpu: "2%".into(),
                mem: "120MiB".into(),
                net: "2kB".into(),
                containment: "active-policy".into(),
                mounts: String::new(),
            },
        ];
        let u = ui(PanelWidth::Normal, Section::Sandbox);
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 39,
            rows: 28,
        };
        let rendered = text(&content(Section::Sandbox, &ctx));
        assert!(rendered.contains("superzej-active-worktree"), "{rendered}");
        assert!(rendered.contains("active-policy"), "{rendered}");
        assert!(!rendered.contains("superzej-other-worktree"), "{rendered}");
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
            // Debug/Db are dead-code placeholder sections — distinctness is waived.
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
        let f = render(Section::Pr, PanelWidth::Full);
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
        let mut u = ui(PanelWidth::Full, Section::Pr);
        u.docs.git = None;
        u.docs.diff = None;
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 150,
            rows: 38,
        };
        assert!(text(&content(Section::Pr, &ctx)).contains("loading"));
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

    // ── Suite B: sandbox section rendering ────────────────────────────────

    fn sandbox_rows(m: &FrameModel, width: PanelWidth) -> String {
        let u = ui(width, Section::Sandbox);
        let ctx = SectionCtx {
            model: m,
            ui: &u,
            cols: match width {
                PanelWidth::Normal => 39,
                PanelWidth::Half => 75,
                PanelWidth::Full => 150,
            },
            rows: 28,
        };
        text(&content(Section::Sandbox, &ctx))
    }

    fn container_info(name: &str, status: &str) -> superzej_core::sandbox::ContainerInfo {
        superzej_core::sandbox::ContainerInfo {
            name: name.into(),
            image: "alpine:latest".into(),
            status: status.into(),
            ours: true,
            backend: "podman".into(),
            cpu: String::new(),
            mem: String::new(),
            net: String::new(),
            containment: String::new(),
            mounts: String::new(),
        }
    }

    #[test]
    fn b1_healthy_container_renders_green_bullet() {
        let mut m = model();
        m.active_container_name = "superzej-wt-feat".into();
        m.container_health = crate::chrome::ContainerHealth::Healthy;
        m.containers = vec![container_info("superzej-wt-feat", "Up 3 hours")];
        let rendered = sandbox_rows(&m, PanelWidth::Normal);
        assert!(
            rendered.contains("● running"),
            "expected running bullet: {rendered}"
        );
        assert!(!rendered.contains('⚠'), "unexpected warning: {rendered}");
    }

    #[test]
    fn b2_degraded_container_renders_amber_warning() {
        let mut m = model();
        m.active_container_name = "superzej-wt-feat".into();
        m.container_health =
            crate::chrome::ContainerHealth::Degraded("stale mount: /wt/feat".into());
        m.containers = vec![container_info("superzej-wt-feat", "Up 1 hour (degraded)")];
        let rendered = sandbox_rows(&m, PanelWidth::Normal);
        assert!(rendered.contains("⚠ degraded"), "expected ⚠: {rendered}");
        assert!(
            rendered.contains("stale mount: /wt/feat"),
            "expected degradation reason: {rendered}"
        );
    }

    #[test]
    fn b3_no_container_shows_not_sandboxed() {
        let mut m = model();
        m.active_container_name = "superzej-wt-feat".into();
        m.container_health = crate::chrome::ContainerHealth::Unknown;
        m.containers = vec![];
        m.active_sandbox_backend = String::new();
        let rendered = sandbox_rows(&m, PanelWidth::Normal);
        assert!(
            rendered.contains("○ not sandboxed"),
            "expected not-sandboxed: {rendered}"
        );
    }

    #[test]
    fn b4_bwrap_backend_shows_active() {
        let mut m = model();
        m.active_container_name = String::new();
        m.containers = vec![];
        m.active_sandbox_backend = "bwrap".into();
        let rendered = sandbox_rows(&m, PanelWidth::Normal);
        assert!(
            rendered.contains("● active"),
            "expected active bullet: {rendered}"
        );
        assert!(
            rendered.contains("bwrap"),
            "expected backend name: {rendered}"
        );
    }

    #[test]
    fn b5_startup_orphans_notice() {
        let mut m = model();
        m.active_container_name = String::new();
        m.containers = vec![];
        m.active_sandbox_backend = String::new();
        m.startup_orphans_removed = vec!["superzej-old-wt".into(), "superzej-stale".into()];
        let rendered = sandbox_rows(&m, PanelWidth::Normal);
        assert!(rendered.contains('⚠'), "expected warning: {rendered}");
        assert!(
            rendered.contains("removed 2 orphan container(s) at startup"),
            "expected orphan notice: {rendered}"
        );
        assert!(
            rendered.contains("superzej-old-wt"),
            "expected first name: {rendered}"
        );
        assert!(
            rendered.contains("superzej-stale"),
            "expected second name: {rendered}"
        );
    }

    #[test]
    fn b6_audit_log_section_in_deep_view() {
        let mut m = model();
        m.active_container_name = "superzej-wt-feat".into();
        m.containers = vec![container_info("superzej-wt-feat", "Up 5m")];
        m.container_events = vec![
            superzej_core::models::ContainerEvent {
                id: 1,
                worktree: "/wt/feat".into(),
                ts: 1000,
                kind: "exec".into(),
                detail: Some("cargo build".into()),
                exit_code: None,
            },
            superzej_core::models::ContainerEvent {
                id: 2,
                worktree: "/wt/feat".into(),
                ts: 2000,
                kind: "network".into(),
                detail: Some("eth0".into()),
                exit_code: None,
            },
            superzej_core::models::ContainerEvent {
                id: 3,
                worktree: "/wt/feat".into(),
                ts: 3000,
                kind: "die".into(),
                detail: None,
                exit_code: Some(0),
            },
        ];
        // Half width → deep() == true
        let rendered = sandbox_rows(&m, PanelWidth::Half);
        assert!(
            rendered.contains("AUDIT LOG"),
            "expected AUDIT LOG: {rendered}"
        );
        assert!(rendered.contains("exec"), "expected exec event: {rendered}");
        assert!(
            rendered.contains("network"),
            "expected network event: {rendered}"
        );
        assert!(rendered.contains("die"), "expected die event: {rendered}");
    }

    #[test]
    fn b7_mounts_section_in_deep_view() {
        let mut m = model();
        m.active_container_name = "superzej-wt-feat".into();
        let mut ci = container_info("superzej-wt-feat", "Up 5m");
        ci.mounts = "/wt/feat:/wt/feat:z /repo/.git:/repo/.git:z".into();
        m.containers = vec![ci];
        let rendered = sandbox_rows(&m, PanelWidth::Half);
        assert!(
            rendered.contains("MOUNTS"),
            "expected MOUNTS section: {rendered}"
        );
        assert!(
            rendered.contains("/wt/feat"),
            "expected mount path: {rendered}"
        );
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

    #[test]
    fn files_section_renders_inline_file_preview_when_set() {
        let m = model();
        let mut u = ui(PanelWidth::Full, Section::Files);
        u.file_preview = Some(crate::panel::FilePreview {
            path: "src/main.rs".into(),
            lines: (0..50).map(|i| format!("code line {i}")).collect(),
            loading: false,
            error: None,
            scroll: 10,
        });
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 150,
            rows: 38,
        };
        let out = text(&content(Section::Files, &ctx));
        // Header names the file; the close hint is present.
        assert!(out.contains("src/main.rs"), "{out}");
        assert!(out.contains("esc/q close"), "{out}");
        // Scrolled to line 10 (0-based) → the first body line is "11 …".
        assert!(out.contains("code line 10"), "{out}");
        assert!(out.contains("11"), "line numbers shown: {out}");
        // Content above the scroll point is not shown.
        assert!(
            !out.contains("code line 0\n") && !out.contains("code line 9 "),
            "{out}"
        );
    }

    #[test]
    fn files_section_preview_shows_loading_then_error() {
        let m = model();
        let mut u = ui(PanelWidth::Full, Section::Files);
        u.file_preview = Some(crate::panel::FilePreview::loading("big.bin"));
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 80,
            rows: 20,
        };
        assert!(text(&content(Section::Files, &ctx)).contains("loading"));

        u.file_preview = Some(crate::panel::FilePreview {
            path: "big.bin".into(),
            error: Some("binary file".into()),
            ..Default::default()
        });
        let ctx = SectionCtx {
            model: &m,
            ui: &u,
            cols: 80,
            rows: 20,
        };
        assert!(text(&content(Section::Files, &ctx)).contains("binary file"));
    }
}
