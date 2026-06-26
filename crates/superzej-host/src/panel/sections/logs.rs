//! Logs section — live tail of the szhost log file.
//!
//! Three view widths:
//!   Normal (narrow): glyph + target (10 cols) + message.
//!   Half   (medium): timestamp + glyph + target (16 cols) + message; filter bar.
//!   Full   (wide):   left list + right detail panel side-by-side.
#![allow(dead_code)] // WIP view helpers, written ahead of wiring (tasks.md §AW 721/723/724)

use superzej_core::log::parser::{LogLevel as ParserLogLevel, ParsedLog};
use superzej_core::log_view::{LogLevel as ViewLogLevel, LogLine};
use superzej_core::theme::Hue;

use crate::seg::{Line, seg};

use super::{PanelRow, SectionCtx, ac, d, g, g2, g3, hint_row, hue, rule, t};

// ---- helpers ----------------------------------------------------------------

fn level_hue(l: ViewLogLevel) -> crate::seg::Tok {
    match l {
        ViewLogLevel::Error => hue(Hue::Red),
        ViewLogLevel::Warn => hue(Hue::Amber),
        ViewLogLevel::Info => g(),
        ViewLogLevel::Debug => g2(),
        ViewLogLevel::Trace => g2(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Lines visible under the current level gate and text filter.
fn visible_lines<'a>(ctx: &'a SectionCtx) -> Vec<&'a LogLine> {
    let filter = ctx.ui.logs_filter.to_lowercase();
    ctx.model
        .panel
        .log_lines
        .iter()
        .filter(|l| {
            ctx.ui.logs_level.is_none_or(|lvl| l.level <= lvl)
                && (filter.is_empty() || l.raw.to_lowercase().contains(&filter))
        })
        .collect()
}

fn filter_bar(ctx: &SectionCtx) -> PanelRow {
    PanelRow::plain(Line::segs(vec![
        seg(ac(), "❯ "),
        seg(t(), ctx.ui.logs_filter.clone()),
        seg(ac(), "▏"),
    ]))
}

fn level_label(level: Option<ViewLogLevel>) -> &'static str {
    match level {
        None => "all",
        Some(ViewLogLevel::Error) => "ERROR+",
        Some(ViewLogLevel::Warn) => "WARN+",
        Some(ViewLogLevel::Info) => "INFO+",
        Some(ViewLogLevel::Debug) => "DEBUG+",
        Some(ViewLogLevel::Trace) => "TRACE",
    }
}

// ---- empty state ------------------------------------------------------------

fn empty_view() -> Vec<PanelRow> {
    vec![
        PanelRow::plain(Line::segs(vec![seg(g2(), "no log data")])),
        PanelRow::plain(Line::segs(vec![
            seg(g3(), "set "),
            seg(d(), "SUPERZEJ_LOG=debug"),
            seg(g3(), " to enable"),
        ])),
    ]
}

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.model.panel.log_lines_structured.is_empty() {
        if ctx.full() {
            return vec![
                PanelRow::plain(Line::segs(vec![seg(g2(), "no log data")])),
                PanelRow::plain(Line::segs(vec![seg(
                    g3(),
                    "Logs will appear here once the sz-log provider fetches them.",
                )])),
                PanelRow::plain(Line::segs(vec![seg(g3(), "(Full view)")])),
            ];
        }
        return vec![
            PanelRow::plain(Line::segs(vec![seg(g2(), "no log data")])),
            PanelRow::plain(Line::segs(vec![seg(
                g3(),
                "Logs will appear here once the sz-log provider fetches them.",
            )])),
        ];
    }

    let items: Vec<&ParsedLog> = ctx.model.panel.log_lines_structured.iter().collect();

    let mut rows = Vec::new();

    let total = items.len();
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "LOGS (Structured)"),
        seg(g2(), format!(" · {total} lines")),
    ])));
    rows.push(rule());

    let body_rows = ctx.rows.saturating_sub(1 + rows.len());
    let mut visible = items.iter().rev().take(body_rows).collect::<Vec<_>>();
    visible.reverse();

    for log in visible.iter() {
        let ts = log.timestamp.clone();
        let lvl = format!("{:?}", log.level);
        let msg = &log.message;

        let text = vec![
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
            seg(t(), msg.clone()),
        ];

        rows.push(PanelRow::plain(Line::segs(text)));
    }

    rows.push(hint_row(&[
        ("j/k", "row"),
        ("/ ", "filter"),
        ("l", "level"),
        ("a", "tail"),
    ]));

    rows
}

// ---- Normal view (narrow) ---------------------------------------------------
