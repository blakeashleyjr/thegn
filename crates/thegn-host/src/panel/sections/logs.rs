//! Logs section — live tail of the thegn log stream.
//!
//! One rule above all: the renderer and every Logs action key (`y`/`Y`/`E`,
//! tail-follow) consume the SAME list, [`visible_lines`] — the worktree scope
//! (`g`), the minimum level (`l`), and the `/` text filter applied to the
//! structured stream. They previously read two different sources (the
//! structured stream vs the file tail), so filtering visibly did nothing and
//! `y` copied a line the user wasn't looking at.

use thegn_core::log::parser::{LogLevel as ParserLogLevel, ParsedLog};
use thegn_core::theme::Hue;

use crate::panel::{PanelHit, PanelUi, Section};
use crate::seg::{Line, Seg, seg};

use super::{PanelRow, SectionCtx, d, g, g2, g3, hint_row, hue, list_detail, rule, t, wrap_text};

/// Severity rank for the structured-stream level (higher = more severe).
fn rank(l: &ParserLogLevel) -> u8 {
    match l {
        ParserLogLevel::Trace => 0,
        ParserLogLevel::Debug => 1,
        ParserLogLevel::Info => 2,
        ParserLogLevel::Warn => 3,
        ParserLogLevel::Error => 4,
        ParserLogLevel::Fatal => 5,
    }
}

/// The UI's minimum level (a `log_view::LogLevel`, "this and more severe")
/// translated into the structured stream's rank space.
fn min_rank(min: thegn_core::log_view::LogLevel) -> u8 {
    use thegn_core::log_view::LogLevel as V;
    match min {
        V::Error => 4,
        V::Warn => 3,
        V::Info => 2,
        V::Debug => 1,
        V::Trace => 0,
    }
}

/// The one filter chain: worktree scope + level + text, over the structured
/// stream. Both the renderer and the loop's Logs keys iterate this, so what
/// you see is exactly what `y`/`Y`/`E` operate on.
pub(crate) fn visible_lines<'a>(
    panel: &'a crate::panel::PanelData,
    ui: &PanelUi,
) -> impl Iterator<Item = &'a ParsedLog> + 'a {
    visible_with_seq(panel, ui).map(|(_, log)| log)
}

pub(crate) fn visible_with_seq<'a>(
    panel: &'a crate::panel::PanelData,
    ui: &PanelUi,
) -> impl Iterator<Item = (u64, &'a ParsedLog)> + 'a {
    let all = crate::panel::scope::system_all();
    let active = crate::panel::scope::active_wt_tag();
    let min = ui.logs_level.map(min_rank);
    let filter = ui.logs_filter.to_lowercase();
    panel
        .log_lines_structured
        .iter()
        .enumerate()
        .filter_map(move |(i, l)| {
            let visible =
                (all || match l.worktree.as_deref() {
                    None => true,
                    Some(w) => w == active,
                }) && min.is_none_or(|m| rank(&l.level) >= m)
                    && (filter.is_empty()
                        || l.message.to_lowercase().contains(&filter)
                        || l.original.to_lowercase().contains(&filter));
            visible.then_some((panel.log_first_seq + i as u64, l))
        })
}

pub(crate) fn cursor_seq(panel: &crate::panel::PanelData, ui: &PanelUi) -> Option<u64> {
    visible_with_seq(panel, ui)
        .nth(ui.logs_cursor)
        .map(|(seq, _)| seq)
}

pub(crate) fn repin(panel: &crate::panel::PanelData, ui: &mut PanelUi, anchor: Option<u64>) {
    let visible: Vec<u64> = visible_with_seq(panel, ui).map(|(seq, _)| seq).collect();
    let cursor = if ui.logs_tail {
        visible.len().saturating_sub(1)
    } else if let Some(anchor) = anchor {
        visible
            .iter()
            .position(|seq| *seq >= anchor)
            .unwrap_or_else(|| visible.len().saturating_sub(1))
    } else {
        ui.logs_cursor.min(visible.len().saturating_sub(1))
    };
    ui.logs_cursor = cursor;
    if ui.open == Section::Logs {
        ui.cursor = cursor;
    }
}

fn short_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| timestamp.to_string())
}

fn display_message(log: &ParsedLog) -> String {
    match log.target.as_deref() {
        Some(target) => log
            .message
            .strip_prefix(target)
            .map(str::trim_start)
            .unwrap_or(&log.message)
            .to_string(),
        None => log.message.clone(),
    }
}

pub(crate) fn copy_status(lines: usize, result: Result<(), crate::clipboard::CopyError>) -> String {
    match result {
        Ok(()) if lines == 1 => "Copying log line to clipboard…".to_string(),
        Ok(()) => format!("Copying {lines} log lines to clipboard…"),
        Err(error) => format!("Clipboard copy failed: {error} (sent via OSC 52)"),
    }
}

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.model.panel.log_lines_structured.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(g2(), "no log data")])),
            PanelRow::plain(Line::segs(vec![seg(
                g3(),
                "Set THEGN_LOG (or [log] file = true) and lines stream in here.",
            )])),
        ];
    }

    let all = crate::panel::scope::system_all();
    let items: Vec<&ParsedLog> = visible_lines(&ctx.model.panel, ctx.ui).collect();

    if ctx.full() {
        return full_view(ctx, &items, all);
    }

    let mut rows = Vec::new();

    let total = items.len();
    let scope = if all {
        " · all worktrees"
    } else {
        " · this repo"
    };
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "LOGS"),
        seg(g2(), format!(" · {total} lines")),
        seg(g2(), scope.to_string()),
    ])));
    // The `/` filter bar: visible while typing AND while a filter is active,
    // so a narrowed list can never masquerade as the full stream.
    if ctx.ui.logs_filter_editing || !ctx.ui.logs_filter.is_empty() {
        let mut segs = vec![
            seg(g2(), "/ ".to_string()),
            seg(t(), ctx.ui.logs_filter.clone()),
        ];
        if ctx.ui.logs_filter_editing {
            segs.push(seg(d(), "▌".to_string()));
        } else {
            segs.push(seg(g3(), "  (Esc clears)".to_string()));
        }
        rows.push(PanelRow::plain(Line::segs(segs)));
    }
    if let Some(lvl) = ctx.ui.logs_level {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            format!("level ≥ {lvl:?}"),
        )])));
    }
    rows.push(rule());

    // Every visible line is a cursor target; the frame's cursor-following
    // window keeps the highlighted one on screen (tail mode parks the cursor
    // on the newest line, so the tail stays in view).
    for (i, log) in items.iter().enumerate() {
        let ts = short_time(&log.timestamp);
        let lvl = format!("{:?}", log.level);
        let msg = display_message(log);

        let mut text = vec![
            seg(g2(), format!("{ts} ")),
            seg(
                match log.level {
                    ParserLogLevel::Error | ParserLogLevel::Fatal => hue(Hue::Red),
                    ParserLogLevel::Warn => hue(Hue::Amber),
                    ParserLogLevel::Info => g(),
                    ParserLogLevel::Debug | ParserLogLevel::Trace => g2(),
                },
                format!("{lvl:<5} "),
            ),
        ];
        if ctx.deep() {
            let target = log.target.as_deref().unwrap_or("-");
            let worktree = log.worktree.as_deref().unwrap_or("global");
            text.push(seg(g2(), format!("{target} [{worktree}] ")));
        }
        text.push(seg(t(), msg));

        rows.push(PanelRow::plain(Line::segs(text)).with_hit(PanelHit::Row(Section::Logs, i)));
    }
    if total == 0 {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g3(),
            "no matching lines — Esc clears the filter, l resets the level",
        )])));
    }

    rows.push(hint_row(&[
        ("/", "filter"),
        ("l", "level"),
        ("y", "copy"),
        ("a", "tail"),
        ("g", if all { "this repo" } else { "all" }),
        ("w", "export"),
    ]));

    rows
}

/// Full: list + detail (the notifications recipe) — the line list on the
/// left, the cursor line's full fields on the right, so long messages are
/// finally readable without copying them out.
fn full_view(ctx: &SectionCtx, items: &[&ParsedLog], all: bool) -> Vec<PanelRow> {
    let cols = ctx.cols;
    let mut rows: Vec<PanelRow> = Vec::new();
    let scope = if all {
        " · all worktrees"
    } else {
        " · this repo"
    };
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "LOGS"),
        seg(g2(), format!(" · {} lines{scope}", items.len())),
    ])));
    if ctx.ui.logs_filter_editing || !ctx.ui.logs_filter.is_empty() {
        let mut segs = vec![
            seg(g2(), "/ ".to_string()),
            seg(t(), ctx.ui.logs_filter.clone()),
        ];
        if ctx.ui.logs_filter_editing {
            segs.push(seg(d(), "▌".to_string()));
        } else {
            segs.push(seg(g3(), "  (Esc clears)".to_string()));
        }
        rows.push(PanelRow::plain(Line::segs(segs)));
    }
    if let Some(lvl) = ctx.ui.logs_level {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            format!("level ≥ {lvl:?}"),
        )])));
    }
    rows.push(rule());
    if items.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g3(),
            "no matching lines — Esc clears the filter, l resets the level",
        )])));
        return rows;
    }

    let cursor = ctx.ui.logs_cursor.min(items.len().saturating_sub(1));
    let list_w = 45_usize.min(cols / 2);
    let list_rows: Vec<(Vec<Seg>, Option<usize>)> = items
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let sel = if i == cursor { "▶ " } else { "  " };
            let lvl = format!("{:?}", l.level);
            (
                vec![
                    seg(if i == cursor { t() } else { g() }, sel),
                    seg(level_tok(&l.level), format!("{lvl:<5} ")),
                    seg(if i == cursor { t() } else { d() }, l.message.clone()),
                ],
                Some(i),
            )
        })
        .collect();

    let detail_w = cols.saturating_sub(list_w + 2);
    let l = items[cursor];
    let mut detail: Vec<Vec<Seg>> = vec![
        vec![
            seg(level_tok(&l.level), format!("{:?}", l.level)).bold(),
            seg(g(), "  "),
            seg(g2(), l.timestamp.clone()),
        ],
        vec![seg(
            g2(),
            match l.worktree.as_deref() {
                Some(w) => format!("worktree  {w}"),
                None => "host-global".to_string(),
            },
        )],
        Vec::new(),
    ];
    for chunk in wrap_text(&l.message, detail_w) {
        detail.push(vec![seg(t(), chunk)]);
    }
    if l.original != l.message {
        detail.push(Vec::new());
        detail.push(vec![seg(g2(), "raw".to_string())]);
        for chunk in wrap_text(&l.original, detail_w) {
            detail.push(vec![seg(g(), chunk)]);
        }
    }

    rows.extend(list_detail(
        Section::Logs,
        &list_rows,
        &detail,
        cursor,
        list_w,
        ctx.rows.saturating_sub(rows.len() + 2),
    ));
    rows.push(rule());
    rows.push(hint_row(&[
        ("/", "filter"),
        ("l", "level"),
        ("y", "copy"),
        ("Y", "copy all"),
        ("a", "tail"),
        ("w", "export"),
    ]));
    rows
}

fn level_tok(l: &ParserLogLevel) -> crate::seg::Tok {
    match l {
        ParserLogLevel::Error | ParserLogLevel::Fatal => hue(Hue::Red),
        ParserLogLevel::Warn => hue(Hue::Amber),
        ParserLogLevel::Info => g(),
        ParserLogLevel::Debug | ParserLogLevel::Trace => g2(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with_logs(logs: Vec<ParsedLog>, first_seq: u64) -> crate::chrome::FrameModel {
        crate::chrome::FrameModel {
            panel: crate::panel::PanelData {
                log_lines_structured: logs,
                log_first_seq: first_seq,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn rows_text(rows: &[PanelRow]) -> String {
        let segs = |parts: &[Seg]| {
            parts
                .iter()
                .map(|part| part.text.clone())
                .collect::<String>()
        };
        rows.iter()
            .map(|row| match &row.line {
                Line::Blank => String::new(),
                Line::Fill { ch, .. } => ch.to_string(),
                Line::Segs(parts) => segs(parts),
                Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
                    format!("{} {}", segs(l), segs(r))
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn cursor_keeps_its_line_when_the_ring_drops() {
        let logs = (0..crate::loop_policy::LOG_RETAIN)
            .map(|i| thegn_core::log::parser::parse_log(&format!("line-{i}")))
            .collect();
        let mut model = model_with_logs(logs, 0);
        let mut ui = PanelUi {
            open: Section::Logs,
            logs_tail: false,
            logs_cursor: 500,
            ..Default::default()
        };
        let anchor = cursor_seq(&model.panel, &ui);
        assert_eq!(anchor, Some(500));
        let slice = (0..10)
            .map(|i| thegn_core::log::parser::parse_log(&format!("incoming-{i}")))
            .collect();
        assert!(!crate::loop_policy::apply_log_slice(
            &mut model.panel.log_lines_structured,
            &mut model.panel.log_first_seq,
            slice,
            false,
        ));
        repin(&model.panel, &mut ui, anchor);
        let selected = visible_lines(&model.panel, &ui)
            .nth(ui.logs_cursor)
            .map(|log| log.message.as_str());
        assert_eq!(selected, Some("line-500"));
        assert_eq!(model.panel.log_first_seq, 10);
    }

    #[test]
    fn tail_follows_arrivals() {
        let logs = vec![thegn_core::log::parser::parse_log("first")];
        let mut model = model_with_logs(logs, 0);
        let mut ui = PanelUi {
            open: Section::Logs,
            logs_tail: true,
            ..Default::default()
        };
        assert!(crate::loop_policy::apply_log_slice(
            &mut model.panel.log_lines_structured,
            &mut model.panel.log_first_seq,
            vec![thegn_core::log::parser::parse_log("newest")],
            true,
        ));
        repin(&model.panel, &mut ui, Some(0));
        assert_eq!(ui.logs_cursor, 1);
        assert_eq!(ui.cursor, 1);
    }

    #[test]
    fn short_time_formats_hh_mm_ss() {
        assert_eq!(short_time("2026-06-05T12:34:56-07:00"), "12:34:56");
        assert_eq!(short_time(""), "");
    }

    #[test]
    fn half_rows_show_target_normal_rows_do_not() {
        let log =
            thegn_core::log::parser::parse_log("2026-06-05T12:00:00  INFO  thegn::db  connected");
        let model = model_with_logs(vec![log], 0);
        let render_width = |width| {
            let ui = PanelUi {
                open: Section::Logs,
                width,
                ..Default::default()
            };
            let ctx = SectionCtx {
                model: &model,
                ui: &ui,
                cols: 80,
                rows: 20,
            };
            rows_text(&content(&ctx))
        };
        let normal = render_width(crate::layout::PanelWidth::Normal);
        let half = render_width(crate::layout::PanelWidth::Half);
        assert!(normal.contains("12:00:00"));
        assert!(normal.contains("connected"));
        assert!(!normal.contains("thegn::db"));
        assert!(half.contains("thegn::db [global]"));
    }

    #[test]
    fn copy_status_reports_failure() {
        let status = copy_status(1, Err(crate::clipboard::CopyError::NotStarted));
        assert!(status.contains("failed"));
    }

    #[test]
    fn logs_summary_counts_the_structured_stream() {
        let logs = vec![
            thegn_core::log::parser::parse_log("one"),
            thegn_core::log::parser::parse_log("two"),
            thegn_core::log::parser::parse_log("2026-06-05T12:00:00 ERROR thegn::db failure"),
        ];
        let model = model_with_logs(logs, 0);
        let rendered = super::super::summary(Section::Logs, &model);
        let summary = rendered
            .iter()
            .map(|part| part.text.clone())
            .collect::<String>();
        assert!(summary.contains('1'), "summary: {summary}");
        assert!(summary.contains('3'), "summary: {summary}");
        assert!(!summary.contains("off"), "summary: {summary}");
    }
}
