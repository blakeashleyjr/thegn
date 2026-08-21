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

use super::{PanelRow, SectionCtx, d, g, g2, g3, hint_row, hue, rule, t, two_col, wrap_text};

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
    let all = crate::panel::scope::system_all();
    let active = crate::panel::scope::active_wt_tag();
    let min = ui.logs_level.map(min_rank);
    let filter = ui.logs_filter.to_lowercase();
    panel.log_lines_structured.iter().filter(move |l| {
        (all || match l.worktree.as_deref() {
            None => true,
            Some(w) => w == active,
        }) && min.is_none_or(|m| rank(&l.level) >= m)
            && (filter.is_empty()
                || l.message.to_lowercase().contains(&filter)
                || l.original.to_lowercase().contains(&filter))
    })
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
        ("E", "export"),
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
    let list_rows: Vec<Vec<Seg>> = items
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let sel = if i == cursor { "▶ " } else { "  " };
            let lvl = format!("{:?}", l.level);
            vec![
                seg(if i == cursor { t() } else { g() }, sel),
                seg(level_tok(&l.level), format!("{lvl:<5} ")),
                seg(if i == cursor { t() } else { d() }, l.message.clone()),
            ]
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

    let combined = two_col(&list_rows, &detail, list_w, 2);
    let n = items.len();
    rows.extend(combined.into_iter().enumerate().map(|(i, line)| {
        let row = PanelRow::plain(line);
        // Only the list rows are cursor targets, indexed by the VISIBLE item
        // index (never a row offset — the audited notifications bug).
        if i < n {
            row.with_hit(PanelHit::Row(Section::Logs, i))
        } else {
            row
        }
    }));
    rows.push(rule());
    rows.push(hint_row(&[
        ("/", "filter"),
        ("l", "level"),
        ("y", "copy"),
        ("Y", "copy all"),
        ("a", "tail"),
        ("E", "export"),
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
