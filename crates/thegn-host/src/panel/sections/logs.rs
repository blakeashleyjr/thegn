//! Logs section — live tail of the thegn log file.
//!
//! Three view widths:
//!   Normal (narrow): glyph + target (10 cols) + message.
//!   Half   (medium): timestamp + glyph + target (16 cols) + message; filter bar.
//!   Full   (wide):   left list + right detail panel side-by-side.

use thegn_core::log::parser::{LogLevel as ParserLogLevel, ParsedLog};
use thegn_core::theme::Hue;

use crate::seg::{Line, seg};

use super::{PanelRow, SectionCtx, d, g, g2, g3, hint_row, hue, rule, t};

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

    // Scope to the active worktree by default: keep its own lines (`wt=<slug>`)
    // plus host-global lines (untagged) and hide ones tagged with a *different*
    // worktree — so a sibling worktree's / another host's log noise doesn't bleed
    // in. The System-tab `g` toggle shows every worktree's lines.
    let all = crate::panel::scope::system_all();
    let active = crate::panel::scope::active_wt_tag();
    let items: Vec<&ParsedLog> = ctx
        .model
        .panel
        .log_lines_structured
        .iter()
        .filter(|l| {
            all || match l.worktree.as_deref() {
                None => true,
                Some(w) => w == active,
            }
        })
        .collect();

    let mut rows = Vec::new();

    let total = items.len();
    let scope = if all {
        " · all worktrees"
    } else {
        " · this repo"
    };
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "LOGS (Structured)"),
        seg(g2(), format!(" · {total} lines")),
        seg(g2(), scope.to_string()),
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
        ("g", if all { "this repo" } else { "all" }),
    ]));

    rows
}

// ---- Normal view (narrow) ---------------------------------------------------
