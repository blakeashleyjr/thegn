//! Logs section — live tail of the szhost log file.
//!
//! Three view widths:
//!   Normal (narrow): glyph + target (10 cols) + message.
//!   Half   (medium): timestamp + glyph + target (16 cols) + message; filter bar.
//!   Full   (wide):   left list + right detail panel side-by-side.

use superzej_core::log_view::{LogLevel, LogLine};
use superzej_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, ac, d, g, g2, g3, hint_row, hue, rule, t, two_col,
};

// ---- helpers ----------------------------------------------------------------

fn level_hue(l: LogLevel) -> crate::seg::Tok {
    match l {
        LogLevel::Error => hue(Hue::Red),
        LogLevel::Warn => hue(Hue::Amber),
        LogLevel::Info => g(),
        LogLevel::Debug => g2(),
        LogLevel::Trace => g2(),
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
            ctx.ui.logs_level.map_or(true, |lvl| l.level <= lvl)
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

fn level_label(level: Option<LogLevel>) -> &'static str {
    match level {
        None => "all",
        Some(LogLevel::Error) => "ERROR+",
        Some(LogLevel::Warn) => "WARN+",
        Some(LogLevel::Info) => "INFO+",
        Some(LogLevel::Debug) => "DEBUG+",
        Some(LogLevel::Trace) => "TRACE",
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

// ---- public entry -----------------------------------------------------------

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.model.panel.log_lines.is_empty() {
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

// ---- Normal view (narrow) ---------------------------------------------------

fn normal_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let items = visible_lines(ctx);
    if items.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(g2(), "no matching lines")])),
            hint_row(&[("l", "level"), ("/ ", "filter")]),
        ];
    }

    let cursor = ctx.ui.logs_cursor;
    let mut rows = Vec::new();

    if ctx.ui.logs_filter_editing {
        rows.push(filter_bar(ctx));
    }

    let body_rows = ctx.rows.saturating_sub(1 + rows.len());

    for (i, line) in items
        .iter()
        .enumerate()
        .rev()
        .take(body_rows)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let tgt = truncate(&line.target, 10);
        // message fill: cols − glyph(1) − sp(1) − target(10) − sp(1) − sp for overflow
        let msg_w = ctx.cols.saturating_sub(1 + 1 + 10 + 1).max(4);
        let msg = truncate(&line.message, msg_w);

        let row = PanelRow::plain(Line::segs(vec![
            seg(level_hue(line.level), line.level.glyph()),
            seg(g3(), " "),
            seg(g2(), tgt),
            seg(g3(), " "),
            seg(
                if line.level <= LogLevel::Warn {
                    t()
                } else {
                    g2()
                },
                msg,
            ),
        ]))
        .with_hit(PanelHit::Row(Section::Logs, i));

        let row = if i == cursor {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);
    }

    rows.push(hint_row(&[
        ("j/k", "row"),
        ("/ ", "filter"),
        ("l", "level"),
        ("y", "copy"),
        ("e", "export"),
    ]));
    rows
}

// ---- Half view (medium) -----------------------------------------------------

fn half_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let items = visible_lines(ctx);
    if items.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(g2(), "no matching lines")])),
            hint_row(&[("l", "level"), ("/ ", "filter")]),
        ];
    }

    let cursor = ctx.ui.logs_cursor;
    let total = items.len();
    let mut rows = Vec::new();

    if ctx.ui.logs_filter_editing {
        rows.push(filter_bar(ctx));
    }

    let body_rows = ctx.rows.saturating_sub(1 + rows.len());

    for (i, line) in items
        .iter()
        .enumerate()
        .rev()
        .take(body_rows)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let tgt = truncate(&line.target, 16);
        let ts = truncate(&line.timestamp, 19);
        let msg_w = ctx
            .cols
            .saturating_sub(ts.len() + 1 + 1 + 1 + tgt.len() + 1)
            .max(4);
        let msg = truncate(&line.message, msg_w);

        let row = PanelRow::plain(Line::segs(vec![
            seg(g2(), ts),
            seg(g3(), " "),
            seg(level_hue(line.level), line.level.glyph()),
            seg(g3(), " "),
            seg(g2(), tgt),
            seg(g3(), " "),
            seg(
                if line.level <= LogLevel::Warn {
                    t()
                } else {
                    g2()
                },
                msg,
            ),
        ]))
        .with_hit(PanelHit::Row(Section::Logs, i));

        let row = if i == cursor {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);
    }

    rows.push(hint_row(&[
        ("j/k", "row"),
        ("/ ", "filter"),
        ("l", "level"),
        ("y", "copy"),
        ("e", "export"),
        (&format!("{}/{}", items.len().min(body_rows), total), ""),
    ]));
    rows
}

// ---- Full view (wide) -------------------------------------------------------

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let items = visible_lines(ctx);
    let cols = ctx.cols;
    let mut rows = Vec::new();

    // Header
    let total = ctx.model.panel.log_lines.len();
    let lvl = level_label(ctx.ui.logs_level);
    let filt = if ctx.ui.logs_filter.is_empty() {
        "no filter".to_string()
    } else {
        format!("/{}", ctx.ui.logs_filter)
    };
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "LOGS"),
        seg(g2(), format!("  szhost.log · {total} lines")),
        seg(g3(), format!(" · {lvl} · {filt}")),
    ])));
    rows.push(rule());

    if items.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "no matching lines",
        )])));
        rows.push(hint_row(&[("l", "level"), ("/ ", "filter")]));
        return rows;
    }

    let cursor = ctx.ui.logs_cursor;
    let list_w = 55_usize.min(cols / 2);

    let list_rows: Vec<Vec<Seg>> = items
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let sel = if i == cursor { "▶ " } else { "  " };
            let tgt_w = list_w.saturating_sub(4 + 2).max(4);
            let tgt = truncate(&line.target, tgt_w);
            let msg_w = list_w.saturating_sub(2 + 1 + 1 + 1 + tgt.len() + 1).max(4);
            let msg = truncate(&line.message, msg_w);
            vec![
                seg(if i == cursor { t() } else { g() }, sel),
                seg(level_hue(line.level), line.level.glyph()),
                seg(g3(), " "),
                seg(g2(), tgt),
                seg(g3(), " "),
                seg(g2(), msg),
            ]
        })
        .collect();

    let detail_rows: Vec<Vec<Seg>> = if let Some(line) = items.get(cursor) {
        log_detail_segs(line, cols.saturating_sub(list_w + 2))
    } else {
        vec![vec![seg(g2(), "select a line")]]
    };

    let header_offset = rows.len();
    let combined = two_col(&list_rows, &detail_rows, list_w, 2);
    rows.extend(combined.into_iter().enumerate().map(|(i, l)| {
        PanelRow::plain(l).with_hit(PanelHit::Row(Section::Logs, header_offset + i))
    }));

    rows.push(rule());
    rows.push(hint_row(&[
        ("j/k", "row"),
        ("/ ", "filter"),
        ("l", "level"),
        ("a", "tail"),
        ("y", "copy"),
        ("e", "export"),
    ]));
    rows
}

fn log_detail_segs(line: &LogLine, w: usize) -> Vec<Vec<Seg>> {
    let mut out: Vec<Vec<Seg>> = Vec::new();

    // Level glyph + target
    out.push(vec![
        seg(level_hue(line.level), line.level.glyph()),
        seg(g(), "  "),
        seg(t(), truncate(&line.target, w.saturating_sub(4))),
    ]);

    // Kind label
    out.push(vec![
        seg(level_hue(line.level), line.level.label()),
        seg(g3(), "  "),
        seg(g2(), line.timestamp.clone()),
    ]);

    // Separator
    out.push(vec![seg(g3(), "─".repeat(w.min(40)))]);

    // Message (may be long — wrap at w)
    let msg = &line.message;
    if msg.chars().count() <= w {
        out.push(vec![seg(t(), msg.clone())]);
    } else {
        // Simple word-wrap at w chars
        let mut remaining = msg.as_str();
        while !remaining.is_empty() {
            let take = remaining
                .char_indices()
                .take_while(|(i, _)| *i < w)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(remaining.len());
            out.push(vec![seg(t(), remaining[..take].to_string())]);
            remaining = &remaining[take..];
        }
    }

    out
}
