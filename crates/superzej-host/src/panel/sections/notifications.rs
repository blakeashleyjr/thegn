//! Notifications section — program-wide inbox for all event kinds.
//!
//! Three view widths:
//!   Normal (39 cols): compact one-liners, kind glyph + source ref + message + age.
//!   Half   (75 cols): two-line rows with worktree basename and kind label.
//!   Full   (150 cols): left list + right detail panel side-by-side.

use superzej_core::notification::{Notification, NotificationKind};
use superzej_core::theme::Hue;

use crate::seg::{Line, Seg, seg, sp};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, ac, d, g, g2, g3, hint_row, hue, rule, t, two_col,
};

// ---- helpers -----------------------------------------------------------------

fn kind_hue(k: NotificationKind) -> crate::seg::Tok {
    match k {
        NotificationKind::AgentDone
        | NotificationKind::WorktreeCreated
        | NotificationKind::BlockerResolved => hue(Hue::Green),
        NotificationKind::AgentFailed
        | NotificationKind::TestFailed
        | NotificationKind::Overdue
        | NotificationKind::LogError
        | NotificationKind::ProcessFailed => hue(Hue::Red),
        NotificationKind::PrStateChanged
        | NotificationKind::StatusChanged
        | NotificationKind::PrLinked => hue(Hue::Amber),
        NotificationKind::Assigned | NotificationKind::Mentioned => hue(Hue::Blue),
        NotificationKind::ProcessExited => hue(Hue::Green),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

fn time_ago(now_ms: i64, created_at_ms: i64) -> String {
    let secs = now_ms.saturating_sub(created_at_ms).max(0) / 1_000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Notifications visible under the current show_read toggle and text filter.
fn visible<'a>(ctx: &'a SectionCtx) -> Vec<&'a Notification> {
    let filter = ctx.ui.notifications_filter.to_lowercase();
    ctx.model
        .panel
        .notifications
        .iter()
        .filter(|n| {
            (ctx.ui.notifications_show_read || !n.read)
                && (filter.is_empty()
                    || n.message.to_lowercase().contains(&filter)
                    || n.source_ref.to_lowercase().contains(&filter)
                    || n.worktree_path.to_lowercase().contains(&filter))
        })
        .collect()
}

fn filter_bar(ctx: &SectionCtx) -> PanelRow {
    PanelRow::plain(Line::segs(vec![
        seg(ac(), "❯ "),
        seg(t(), ctx.ui.notifications_filter.clone()),
        seg(ac(), "▏"),
    ]))
}

// ---- main entry point -------------------------------------------------------

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() {
        full_view(ctx)
    } else if ctx.deep() {
        half_view(ctx)
    } else {
        normal_view(ctx)
    }
}

// ---- Normal view (39 cols) --------------------------------------------------

fn normal_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let now = now_ms();
    let items = visible(ctx);
    let cursor = ctx.ui.notifications_cursor;
    let mut rows = Vec::new();

    if ctx.ui.notifications_filter_editing {
        rows.push(filter_bar(ctx));
    }

    if items.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g2(), "inbox zero")])));
        rows.push(hint_row(&[("/ ", "search"), ("A", "show read")]));
        return rows;
    }

    for (i, n) in items.iter().enumerate() {
        let ago = time_ago(now, n.created_at_ms);
        // Budget: 1 glyph + 1 sp + 12 ref + 1 sp + message + 1 sp + age
        let msg_budget = ctx
            .cols
            .saturating_sub(1 + 1 + 12 + 1 + 1 + ago.len().max(2));
        let msg = truncate(&n.message, msg_budget.max(4));
        let src = truncate(&n.source_ref, 12);

        let (glyph_tok, src_tok, msg_tok) = if n.read {
            (g2(), g2(), g2())
        } else {
            (kind_hue(n.kind), d(), t())
        };

        let row = PanelRow::plain(Line::segs(vec![
            seg(glyph_tok, n.kind.glyph()),
            seg(g3(), " "),
            seg(src_tok, src),
            seg(g3(), " "),
            seg(msg_tok, msg),
            seg(g3(), " "),
            seg(g2(), ago),
        ]))
        .with_hit(PanelHit::Row(Section::Notifications, i));

        let row = if i == cursor {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);

        if rows.len() >= ctx.rows.saturating_sub(1) {
            break;
        }
    }

    rows.push(hint_row(&[
        ("↵", "read"),
        ("/ ", "search"),
        ("r", "read"),
        ("R", "all"),
        ("d", "del"),
        ("A", "show read"),
    ]));
    rows
}

// ---- Half view (75 cols) ----------------------------------------------------

fn half_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let now = now_ms();
    let items = visible(ctx);
    let cursor = ctx.ui.notifications_cursor;
    let mut rows = Vec::new();

    if ctx.ui.notifications_filter_editing {
        rows.push(filter_bar(ctx));
    }

    if items.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g2(), "inbox zero")])));
        rows.push(hint_row(&[("/ ", "search"), ("A", "show read")]));
        return rows;
    }

    for (i, n) in items.iter().enumerate() {
        let ago = time_ago(now, n.created_at_ms);
        let src = truncate(&n.source_ref, 14);
        let msg_budget = ctx
            .cols
            .saturating_sub(n.kind.glyph().len() + 1 + src.len() + 3 + ago.len() + 1);
        let msg = truncate(&n.message, msg_budget.max(4));
        let wt_base = n
            .worktree_path
            .rsplit('/')
            .next()
            .unwrap_or(&n.worktree_path);

        let (glyph_tok, src_tok, msg_tok) = if n.read {
            (g2(), g2(), g2())
        } else {
            (kind_hue(n.kind), d(), t())
        };

        let line1 = Line::segs(vec![
            seg(glyph_tok, n.kind.glyph()),
            seg(g3(), " "),
            seg(src_tok, src),
            seg(g3(), " · "),
            seg(msg_tok, msg),
            seg(g3(), " "),
            seg(g2(), ago),
        ]);

        let line2 = Line::segs(vec![
            sp(2),
            seg(g3(), wt_base),
            seg(g3(), "  ·  "),
            seg(g3(), n.kind.label()),
        ]);

        let bg = if i == cursor {
            Some(crate::seg::Tok::SelAccent)
        } else {
            None
        };

        rows.push(PanelRow {
            line: line1,
            bg,
            hit: Some(PanelHit::Row(Section::Notifications, i)),
        });
        rows.push(PanelRow {
            line: line2,
            bg: None,
            hit: None,
        });

        if rows.len() + 2 > ctx.rows.saturating_sub(2) {
            break;
        }
    }

    rows.push(hint_row(&[
        ("↵", "read"),
        ("/ ", "search"),
        ("r", "read"),
        ("R", "all"),
        ("d", "del"),
        ("A", "show read"),
    ]));
    rows
}

// ---- Full view (150 cols) ---------------------------------------------------

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let now = now_ms();
    let items = visible(ctx);
    let cols = ctx.cols;
    let mut rows = Vec::new();

    // Header bar
    let unread = ctx
        .model
        .panel
        .notifications
        .iter()
        .filter(|n| !n.read)
        .count();
    let total = ctx.model.panel.notifications.len();
    let show_tag = if ctx.ui.notifications_show_read {
        " [all]"
    } else {
        ""
    };
    let filt_tag = if ctx.ui.notifications_filter.is_empty() {
        String::new()
    } else {
        format!(" /{}/ ", ctx.ui.notifications_filter)
    };
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "NOTIFICATIONS"),
        seg(
            g2(),
            format!("  ⚑ {unread} unread / {total}{show_tag}{filt_tag}"),
        ),
    ])));
    rows.push(rule());

    if ctx.ui.notifications_filter_editing {
        rows.push(filter_bar(ctx));
    }

    if items.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g2(), "inbox zero")])));
        rows.push(hint_row(&[("/ ", "search"), ("A", "show read")]));
        return rows;
    }

    let cursor = ctx.ui.notifications_cursor;
    let list_w = 45_usize.min(cols / 2);

    // Left column: compact list
    let list_rows: Vec<Vec<Seg>> = items
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let ago = time_ago(now, n.created_at_ms);
            let sel_mark = if i == cursor { "▶ " } else { "  " };
            let src_w = list_w.saturating_sub(4 + ago.len() + 1).max(4);
            let src = truncate(&n.source_ref, src_w);
            let (glyph_tok, src_tok) = if n.read {
                (g2(), g2())
            } else {
                (kind_hue(n.kind), d())
            };
            vec![
                seg(if i == cursor { t() } else { g() }, sel_mark),
                seg(glyph_tok, n.kind.glyph()),
                seg(g3(), " "),
                seg(src_tok, src),
                seg(g3(), " "),
                seg(g2(), ago),
            ]
        })
        .collect();

    // Right column: detail for cursor item
    let detail_rows: Vec<Vec<Seg>> = if let Some(n) = items.get(cursor) {
        notification_detail_segs(n, now, cols.saturating_sub(list_w + 2))
    } else {
        vec![vec![seg(g2(), "select a notification")]]
    };

    let combined = two_col(&list_rows, &detail_rows, list_w, 2);
    let header_offset = rows.len();
    rows.extend(combined.into_iter().enumerate().map(|(i, l)| {
        PanelRow::plain(l).with_hit(PanelHit::Row(Section::Notifications, header_offset + i))
    }));

    rows.push(rule());
    rows.push(hint_row(&[
        ("↵", "read"),
        ("/ ", "search"),
        ("r", "mark read"),
        ("R", "all read"),
        ("d", "dismiss"),
        ("A", "show all"),
    ]));
    rows
}

fn notification_detail_segs(n: &Notification, now: i64, w: usize) -> Vec<Vec<Seg>> {
    let mut out: Vec<Vec<Seg>> = Vec::new();

    // Kind glyph + source reference (title row)
    out.push(vec![
        seg(kind_hue(n.kind), n.kind.glyph()),
        seg(g(), "  "),
        seg(t(), truncate(&n.source_ref, w.saturating_sub(4))).bold(),
    ]);

    // Kind label + read state
    let read_tag = if n.read { "  read" } else { "  unread" };
    out.push(vec![seg(g2(), n.kind.label()), seg(g3(), read_tag)]);

    // Message
    out.push(vec![seg(t(), truncate(&n.message, w))]);

    // Worktree path
    if !n.worktree_path.is_empty() {
        out.push(vec![
            seg(g2(), "worktree  "),
            seg(g(), truncate(&n.worktree_path, w.saturating_sub(10))),
        ]);
    }

    // Age
    let ago = time_ago(now, n.created_at_ms);
    out.push(vec![seg(g2(), format!("{ago} ago"))]);

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_ago_formats_correctly() {
        let base = 1_700_000_000_000_i64;
        assert_eq!(time_ago(base + 30_000, base), "30s");
        assert_eq!(time_ago(base + 90_000, base), "1m");
        assert_eq!(time_ago(base + 3_700_000, base), "1h");
        assert_eq!(time_ago(base + 90_000_000, base), "1d");
        // future timestamps clamp to zero
        assert_eq!(time_ago(base, base + 5_000), "0s");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("ab", 1), "…");
    }
}
