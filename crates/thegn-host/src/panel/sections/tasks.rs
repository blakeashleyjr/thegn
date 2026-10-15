//! Tasks section — named task registry with run/stop/re-run lifecycle.
//!
//! Shows configured `[[tasks]]` entries and auto-discovered tasks (Justfile,
//! Makefile, Cargo, package.json scripts, etc.). Three view widths:
//!   Normal (39 cols): kind glyph + name + status + duration
//!   Half   (75 cols): + command preview + last-run timestamp
//!   Full   (150 cols): left list + right output/detail

use thegn_core::config::TaskKind;
use thegn_core::pipeline_report::{display_cell, display_clip};

use crate::seg::{Line, Seg, seg, sp};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, g, g2, g3, hint_row, hue, list_detail, rule, t,
};
use thegn_core::theme::Hue;

// ---- helpers -----------------------------------------------------------------

fn kind_glyph(kind: &TaskKind) -> &'static str {
    match kind {
        TaskKind::Custom => "◎",
        TaskKind::Test => "◉",
        TaskKind::Build => "⬡",
        TaskKind::Lint => "◇",
        TaskKind::Run => "▶",
    }
}

fn kind_hue(kind: &TaskKind) -> crate::seg::Tok {
    match kind {
        TaskKind::Custom => g(),
        TaskKind::Test => hue(Hue::Blue),
        TaskKind::Build => hue(Hue::Amber),
        TaskKind::Lint => hue(Hue::Purple),
        TaskKind::Run => hue(Hue::Green),
    }
}

fn failed_mark() -> Seg {
    seg(hue(Hue::Red), "✗")
}

fn status_segs(run: Option<&crate::panel::TaskRunRecord>) -> Vec<Seg> {
    match run {
        None => vec![seg(g2(), "not run")],
        Some(r) if r.running => vec![seg(hue(Hue::Amber), "…"), seg(g2(), " running")],
        Some(r) if r.end == crate::task::RunEnd::Stopped => vec![seg(g3(), "stopped")],
        Some(r) if matches!(r.end, crate::task::RunEnd::TimedOut { .. }) => vec![
            failed_mark(),
            seg(g2(), format!(" timed out {}", fmt_ms(r.duration_ms))),
        ],
        Some(r) if r.exit_code == 0 => vec![
            seg(hue(Hue::Green), "✓"),
            seg(g2(), format!(" {:.1}s", r.duration_ms as f64 / 1000.0)),
        ],
        Some(r) => vec![failed_mark(), seg(g2(), format!(" exit {}", r.exit_code))],
    }
}

fn fmt_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1_000)
    }
}

// ---- main entry point -------------------------------------------------------

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.model.panel.task_specs.is_empty() {
        return empty_view();
    }
    if ctx.full() {
        full_view(ctx)
    } else if ctx.deep() {
        half_view(ctx)
    } else {
        normal_view(ctx)
    }
}

fn empty_view() -> Vec<PanelRow> {
    vec![
        PanelRow::plain(Line::segs(vec![seg(g2(), "no tasks configured")])),
        PanelRow::plain(Line::segs(vec![seg(g3(), "add [[tasks]] to config")])),
        hint_row(&[("↵", "run"), ("r", "re-run"), ("s", "stop")]),
    ]
}

// ---- Normal view (39 cols) --------------------------------------------------

fn normal_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let specs = &ctx.model.panel.task_specs;
    let runs = &ctx.model.panel.task_last_runs;
    let cursor = ctx.ui.tasks_cursor;
    let mut rows = Vec::new();

    for (i, task) in specs.iter().enumerate() {
        let run = runs.get(&task.name);
        let glyph = kind_glyph(&task.kind);
        let name_w = ctx.cols.saturating_sub(glyph.len() + 1 + 8);
        let name = display_clip(&task.name, name_w);

        let mut segs = vec![
            seg(kind_hue(&task.kind), glyph),
            seg(g3(), " "),
            seg(if i == cursor { t() } else { d() }, name),
            seg(g3(), " "),
        ];
        segs.extend(status_segs(run));

        let row = PanelRow::plain(Line::segs(segs)).with_hit(PanelHit::Row(Section::Jobs, i));
        let row = if i == cursor {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);
    }

    rows.push(hint_row(&[
        ("↵", "run"),
        ("r", "re-run"),
        ("s", "stop"),
        ("o", "output"),
    ]));
    rows
}

// ---- Half view (75 cols) ----------------------------------------------------

fn half_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let specs = &ctx.model.panel.task_specs;
    let runs = &ctx.model.panel.task_last_runs;
    let cursor = ctx.ui.tasks_cursor;
    let mut rows = Vec::new();

    for (i, task) in specs.iter().enumerate() {
        let run = runs.get(&task.name);
        let glyph = kind_glyph(&task.kind);

        let status = status_segs(run);
        let dur = run
            .as_ref()
            .map(|r| fmt_ms(r.duration_ms))
            .unwrap_or_default();
        let cmd_w = ctx
            .cols
            .saturating_sub(glyph.len() + 1 + task.name.len() + 3 + dur.len() + 2);
        let cmd = display_clip(&task.command, cmd_w);

        let line1 = Line::segs(vec![
            seg(kind_hue(&task.kind), glyph),
            seg(g3(), " "),
            seg(
                if i == cursor { t() } else { d() },
                display_cell(&task.name),
            ),
            seg(g3(), " · "),
            seg(g2(), cmd),
        ]);

        let mut line2_segs = vec![sp(2)];
        line2_segs.extend(status);
        if !dur.is_empty() {
            line2_segs.push(seg(g3(), "  "));
            line2_segs.push(seg(g2(), dur));
        }

        let bg = if i == cursor {
            Some(crate::seg::Tok::SelAccent)
        } else {
            None
        };
        rows.push(PanelRow {
            line: line1,
            bg,
            hit: Some(PanelHit::Row(Section::Jobs, i)),
        });
        rows.push(PanelRow {
            line: Line::segs(line2_segs),
            bg: None,
            hit: None,
        });
    }

    rows.push(hint_row(&[
        ("↵", "run"),
        ("r", "re-run"),
        ("s", "stop"),
        ("o", "output"),
    ]));
    rows
}

// ---- Full view (150 cols) ---------------------------------------------------

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let specs = &ctx.model.panel.task_specs;
    let runs = &ctx.model.panel.task_last_runs;
    let cols = ctx.cols;
    let cursor = ctx.ui.tasks_cursor;
    let mut rows = Vec::new();

    let running = runs.values().filter(|r| r.running).count();
    let failed = runs
        .values()
        .filter(|r| !r.running && r.end != crate::task::RunEnd::Stopped && r.exit_code != 0)
        .count();
    let passed = runs
        .values()
        .filter(|r| !r.running && r.exit_code == 0)
        .count();

    // Header
    let mut header_segs = vec![seg(d(), "TASKS")];
    if running > 0 {
        header_segs.push(seg(g2(), format!("  … {running} running")));
    }
    if failed > 0 {
        header_segs.push(seg(g2(), "  "));
        header_segs.push(seg(hue(Hue::Red), format!("✗ {failed}")));
    }
    if passed > 0 {
        header_segs.push(seg(g2(), "  "));
        header_segs.push(seg(hue(Hue::Green), format!("✓ {passed}")));
    }
    rows.push(PanelRow::plain(Line::segs(header_segs)));
    rows.push(rule());

    if specs.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "no tasks configured",
        )])));
        return rows;
    }

    let list_w = 40_usize.min(cols / 2);

    let list_rows: Vec<(Vec<Seg>, Option<usize>)> = specs
        .iter()
        .enumerate()
        .map(|(i, task)| {
            let run = runs.get(&task.name);
            let sel = if i == cursor { "▶ " } else { "  " };
            let name_w = list_w.saturating_sub(sel.len() + 2 + 8);
            let name = display_clip(&task.name, name_w);
            let mut s = vec![
                seg(if i == cursor { t() } else { g() }, sel),
                seg(kind_hue(&task.kind), kind_glyph(&task.kind)),
                seg(g3(), " "),
                seg(if i == cursor { t() } else { d() }, name),
                seg(g3(), " "),
            ];
            s.extend(status_segs(run));
            (s, Some(i))
        })
        .collect();

    let detail_rows = if let Some(task) = specs.get(cursor) {
        let run = runs.get(&task.name);
        task_detail_segs(task, run, cols.saturating_sub(list_w + 2))
    } else {
        vec![vec![seg(g2(), "select a task")]]
    };

    rows.extend(list_detail(
        Section::Jobs,
        &list_rows,
        &detail_rows,
        cursor,
        list_w,
        ctx.rows.saturating_sub(rows.len() + 2),
    ));

    rows.push(rule());
    rows.push(hint_row(&[
        ("↵", "run"),
        ("r", "re-run"),
        ("s", "stop"),
        ("o", "output"),
        ("j/k", "select"),
    ]));
    rows
}

fn task_detail_segs(
    task: &thegn_core::config::Task,
    run: Option<&crate::panel::TaskRunRecord>,
    w: usize,
) -> Vec<Vec<Seg>> {
    let mut out: Vec<Vec<Seg>> = Vec::new();

    // Kind + name
    out.push(vec![
        seg(kind_hue(&task.kind), kind_glyph(&task.kind)),
        seg(g(), "  "),
        seg(t(), display_cell(&task.name)).bold(),
    ]);

    // Command
    let cmd_display = if task.args.is_empty() {
        task.command.clone()
    } else {
        format!("{} {}", task.command, task.args.join(" "))
    };
    out.push(vec![
        seg(g2(), "cmd  "),
        seg(g(), display_clip(&cmd_display, w.saturating_sub(5))),
    ]);

    // Cwd
    if let Some(cwd) = &task.cwd {
        out.push(vec![
            seg(g2(), "cwd  "),
            seg(g(), display_clip(cwd, w.saturating_sub(5))),
        ]);
    }

    // Last run status
    match run {
        None => out.push(vec![seg(g3(), "never run")]),
        Some(r) if r.running => out.push(vec![seg(hue(Hue::Amber), "● running")]),
        Some(r) if r.end == crate::task::RunEnd::Stopped => {
            out.push(vec![seg(g3(), "stopped")]);
        }
        Some(r) if matches!(r.end, crate::task::RunEnd::TimedOut { .. }) => {
            let secs = match r.end {
                crate::task::RunEnd::TimedOut { secs } => secs,
                _ => unreachable!(),
            };
            out.push(vec![
                failed_mark(),
                seg(hue(Hue::Red), format!(" timed out after {secs}s")),
            ]);
        }
        Some(r) if r.exit_code == 0 => out.push(vec![
            seg(hue(Hue::Green), "✓ passed  "),
            seg(g(), fmt_ms(r.duration_ms)),
        ]),
        Some(r) => out.push(vec![
            failed_mark(),
            seg(hue(Hue::Red), format!(" exit {}  ", r.exit_code)),
            seg(g(), fmt_ms(r.duration_ms)),
        ]),
    }

    // Output tail
    if let Some(r) = run
        && !r.output_tail.is_empty()
    {
        out.push(vec![seg(g3(), "─── output ───────────────────────────")]);
        let lines: Vec<&str> = r.output_tail.lines().collect();
        for line in lines.iter().skip(lines.len().saturating_sub(8)) {
            out.push(vec![seg(g2(), display_clip(line, w))]);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::panel::PanelUi;

    fn assert_safe_rows(rows: &[PanelRow], cols: usize) {
        fn check(segs: &[Seg], cols: usize) {
            for segment in segs {
                assert!(
                    segment.text.chars().all(|c| {
                        !c.is_control() && !thegn_core::pipeline_report::is_format_control(c)
                    }),
                    "{:?}",
                    segment.text
                );
                if segment.text.contains('…') {
                    assert!(
                        unicode_width::UnicodeWidthStr::width(segment.text.as_str()) <= cols,
                        "clipped segment exceeds {cols} columns: {:?}",
                        segment.text
                    );
                }
            }
        }

        for row in rows {
            match &row.line {
                Line::Segs(segs) => check(segs, cols),
                Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
                    check(l, cols);
                    check(r, cols);
                }
                Line::Blank | Line::Fill { .. } => {}
            }
        }
    }

    #[test]
    fn kind_glyphs_and_hues_are_defined_for_all_variants() {
        for kind in [
            TaskKind::Custom,
            TaskKind::Test,
            TaskKind::Build,
            TaskKind::Lint,
            TaskKind::Run,
        ] {
            assert!(!kind_glyph(&kind).is_empty(), "{kind:?} glyph empty");
        }
    }

    #[test]
    fn status_segs_for_not_run_pass_fail_running() {
        assert!(!status_segs(None).is_empty());
        let running = crate::panel::TaskRunRecord {
            running: true,
            ..Default::default()
        };
        assert!(!status_segs(Some(&running)).is_empty());
        let passed = crate::panel::TaskRunRecord {
            exit_code: 0,
            duration_ms: 1234,
            ..Default::default()
        };
        let s = status_segs(Some(&passed));
        let text: String = s.iter().map(|sg| sg.text.clone()).collect();
        assert!(text.contains('✓'), "{text}");
        let failed = crate::panel::TaskRunRecord {
            exit_code: 1,
            ..Default::default()
        };
        let s = status_segs(Some(&failed));
        let text: String = s.iter().map(|sg| sg.text.clone()).collect();
        assert!(text.contains('✗'), "{text}");
    }

    #[test]
    fn status_segs_label_stopped_and_timed_out() {
        let stopped = crate::panel::TaskRunRecord {
            end: crate::task::RunEnd::Stopped,
            exit_code: -1,
            ..Default::default()
        };
        let text: String = status_segs(Some(&stopped))
            .iter()
            .map(|sg| sg.text.clone())
            .collect();
        assert!(text.contains("stopped"), "{text}");
        assert!(!text.contains('\u{2717}'), "{text}");

        let timed_out = crate::panel::TaskRunRecord {
            end: crate::task::RunEnd::TimedOut { secs: 1 },
            exit_code: 124,
            duration_ms: 1_000,
            ..Default::default()
        };
        let text: String = status_segs(Some(&timed_out))
            .iter()
            .map(|sg| sg.text.clone())
            .collect();
        assert!(text.contains("timed out"), "{text}");
    }

    #[test]
    fn full_header_does_not_count_stopped_as_failed() {
        let tasks = ["stopped", "failed"]
            .into_iter()
            .map(|name| thegn_core::config::Task {
                name: name.into(),
                command: "true".into(),
                args: Vec::new(),
                cwd: None,
                env: Default::default(),
                kind: TaskKind::Build,
                matcher: None,
                scope: None,
            })
            .collect();
        let mut task_last_runs = std::collections::HashMap::new();
        task_last_runs.insert(
            "stopped".into(),
            crate::panel::TaskRunRecord {
                name: "stopped".into(),
                exit_code: -1,
                end: crate::task::RunEnd::Stopped,
                ..Default::default()
            },
        );
        task_last_runs.insert(
            "failed".into(),
            crate::panel::TaskRunRecord {
                name: "failed".into(),
                exit_code: 1,
                ..Default::default()
            },
        );
        let model = FrameModel {
            panel: crate::panel::PanelData {
                task_specs: tasks,
                task_last_runs,
                ..Default::default()
            },
            ..Default::default()
        };
        let ui = PanelUi {
            width: crate::layout::PanelWidth::Full,
            ..Default::default()
        };
        let rows = full_view(&SectionCtx {
            model: &model,
            ui: &ui,
            cols: 150,
            rows: 20,
        });
        let header = format!("{:?}", rows.first().unwrap());
        assert!(header.contains("\u{2717} 1"), "{header}");
    }

    #[test]
    fn task_detail_shows_last_eight_output_lines() {
        let output_tail = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let run = crate::panel::TaskRunRecord {
            output_tail,
            ..Default::default()
        };
        let task = thegn_core::config::Task {
            name: "build".into(),
            command: "make".into(),
            args: Vec::new(),
            cwd: None,
            env: Default::default(),
            kind: TaskKind::Build,
            matcher: None,
            scope: None,
        };
        let lines = task_detail_segs(&task, Some(&run), 100)
            .into_iter()
            .flatten()
            .map(|segment| segment.text)
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line == "line 13"), "{lines:?}");
        assert!(lines.iter().any(|line| line == "line 20"), "{lines:?}");
        assert!(!lines.iter().any(|line| line == "line 1"), "{lines:?}");
    }

    #[test]
    fn fmt_ms_formats_correctly() {
        assert_eq!(fmt_ms(500), "500ms");
        assert_eq!(fmt_ms(1_500), "1.5s");
        assert_eq!(fmt_ms(90_000), "1m30s");
    }

    #[test]
    fn hostile_task_detail_fields_render_without_terminal_controls() {
        let task = thegn_core::config::Task {
            name: "\u{1b}]0;pwn\u{7}日本語".repeat(12),
            command: "echo \u{202e}日本語".into(),
            args: Vec::new(),
            cwd: None,
            env: Default::default(),
            kind: TaskKind::Build,
            matcher: None,
            scope: None,
        };
        let lines = task_detail_segs(&task, None, 40);
        assert!(
            lines.iter().flatten().all(|segment| segment
                .text
                .chars()
                .all(|c| !c.is_control() && !thegn_core::pipeline_report::is_format_control(c))),
            "{lines:?}"
        );
    }

    #[test]
    fn hostile_task_rows_render_at_all_panel_widths() {
        let task = thegn_core::config::Task {
            name: "\u{1b}]0;pwn\u{7}日本語".repeat(3),
            command: "echo \u{202e}日本語".repeat(12),
            args: Vec::new(),
            cwd: None,
            env: Default::default(),
            kind: TaskKind::Build,
            matcher: None,
            scope: None,
        };
        let model = FrameModel {
            panel: crate::panel::PanelData {
                task_specs: vec![task],
                ..Default::default()
            },
            ..Default::default()
        };

        for (width, cols) in [
            (crate::layout::PanelWidth::Normal, 39),
            (crate::layout::PanelWidth::Half, 75),
            (crate::layout::PanelWidth::Full, 150),
        ] {
            let ui = PanelUi {
                width,
                tasks_cursor: 0,
                ..Default::default()
            };
            let rows = content(&SectionCtx {
                model: &model,
                ui: &ui,
                cols,
                rows: 20,
            });
            assert_safe_rows(&rows, cols);
            let text = format!("{rows:?}");
            assert!(
                text.contains("�"),
                "hostile task row was not rendered: {text}"
            );
            assert!(text.contains("pwn"), "task name was not rendered: {text}");
            if width == crate::layout::PanelWidth::Half {
                assert!(
                    text.contains("echo"),
                    "task command was not rendered: {text}"
                );
            }
        }
    }
}
