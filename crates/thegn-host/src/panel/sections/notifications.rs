//! Notifications section — program-wide inbox for all event kinds.
//!
//! Three view widths:
//!   Normal (39 cols): compact one-liners, kind glyph + source ref + message + age.
//!   Half   (75 cols): two-line rows with worktree basename and kind label.
//!   Full   (150 cols): left list + right detail panel side-by-side.

use thegn_core::notification::{Notification, NotificationKind};
use thegn_core::pipeline_report::display_cell;

use crate::seg::{Line, Seg, seg, sp};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, ac, d, f, g, g3, hint_row, hue, list_detail, rule, t,
    wrap_text,
};

// ---- helpers -----------------------------------------------------------------

/// The kind's marker glyph + color from the single core vocabulary
/// ([`NotificationKind::hued_glyph`]), so the panel inbox and the unified
/// overlay never diverge and both degrade to ASCII with the chrome.
fn kind_gh(k: NotificationKind) -> (&'static str, crate::seg::Tok) {
    let (glyph, h) = k.hued_glyph(crate::caps::active_glyphs());
    (glyph, hue(h))
}

fn kind_hue(k: NotificationKind) -> crate::seg::Tok {
    kind_gh(k).1
}

fn truncate(s: &str, max: usize) -> String {
    let s = display_cell(s);
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Indices (into `panel.notifications`) visible under the current show-read
/// toggle and `/` text filter — the ONE predicate both the renderer and the
/// loop's `x`/`d` dispatch resolve the cursor against. The dispatch used to
/// re-implement this without the `worktree_path` term, so filtering by a
/// worktree name made `d` (irreversibly) delete a different notification than
/// the highlighted one.
pub(crate) fn visible_indices(
    panel: &crate::panel::PanelData,
    ui: &crate::panel::PanelUi,
) -> Vec<usize> {
    let filter = ui.notifications_filter.to_lowercase();
    panel
        .notifications
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            (ui.notifications_show_read || !n.read)
                && (filter.is_empty()
                    || n.message.to_lowercase().contains(&filter)
                    || n.source_ref.to_lowercase().contains(&filter)
                    || n.worktree_path.to_lowercase().contains(&filter))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Notifications visible under the current show_read toggle and text filter.
fn visible<'a>(ctx: &'a SectionCtx) -> Vec<&'a Notification> {
    visible_indices(&ctx.model.panel, ctx.ui)
        .into_iter()
        .filter_map(|i| ctx.model.panel.notifications.get(i))
        .collect()
}

fn filter_bar(ctx: &SectionCtx) -> PanelRow {
    PanelRow::plain(Line::segs(vec![
        seg(ac(), "❯ "),
        seg(t(), ctx.ui.notifications_filter.clone()),
        crate::seg::caret(),
    ]))
}

fn committed_filter_bar(ctx: &SectionCtx) -> PanelRow {
    PanelRow::plain(Line::segs(vec![seg(
        f(),
        format!("/ {}  (/ then Esc clears)", ctx.ui.notifications_filter),
    )]))
}

fn filter_row(ctx: &SectionCtx) -> PanelRow {
    if ctx.ui.notifications_filter_editing {
        filter_bar(ctx)
    } else {
        committed_filter_bar(ctx)
    }
}

fn empty_label(ctx: &SectionCtx) -> String {
    if ctx.ui.notifications_filter.is_empty() {
        "inbox zero".into()
    } else {
        format!("no matches for /{}/", ctx.ui.notifications_filter)
    }
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
    let items = visible(ctx);
    let cursor = ctx.ui.notifications_cursor;
    let mut rows = Vec::new();

    if ctx.ui.notifications_filter_editing || !ctx.ui.notifications_filter.is_empty() {
        rows.push(filter_row(ctx));
    }

    if items.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            f(),
            empty_label(ctx),
        )])));
        rows.push(hint_row(&[("/ ", "search"), ("A", "show read")]));
        return rows;
    }

    for (i, n) in items.iter().enumerate() {
        let ago = thegn_core::util::age(n.created_at_ms);
        // Budget: 1 glyph + 1 sp + 12 ref + 1 sp + message + 1 sp + age
        let msg_budget = ctx
            .cols
            .saturating_sub(1 + 1 + 12 + 1 + 1 + ago.len().max(2));
        let msg = truncate(&n.message, msg_budget.max(4));
        let src = truncate(&n.source_ref, 12);

        // Read items dim to `faint` (still legible) so they read as handled
        // without becoming the grey-on-grey that `ghost*` text would.
        let (glyph_tok, src_tok, msg_tok) = if n.read {
            (f(), f(), f())
        } else {
            (kind_hue(n.kind), d(), t())
        };

        let row = PanelRow::plain(Line::segs(vec![
            seg(glyph_tok, kind_gh(n.kind).0),
            seg(g3(), " "),
            seg(src_tok, src),
            seg(g3(), " "),
            seg(msg_tok, msg),
            seg(g3(), " "),
            seg(f(), ago),
        ]))
        .with_hit(PanelHit::Row(Section::Notifications, i));

        let row = if i == cursor {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);
        // No in-section truncation: the frame's cursor-following window keeps
        // the highlighted row on screen, so every row stays reachable.
    }

    rows.push(hint_row(&[
        ("↵", "expand"),
        ("/ ", "search"),
        ("x", "read"),
        ("d", "delete"),
        ("a", "clear"),
        (
            "g",
            if crate::panel::scope::system_all() {
                "this repo"
            } else {
                "all worktrees"
            },
        ),
    ]));
    rows
}

// ---- Half view (75 cols) ----------------------------------------------------

fn half_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let items = visible(ctx);
    let cursor = ctx.ui.notifications_cursor;
    let mut rows = Vec::new();

    if ctx.ui.notifications_filter_editing || !ctx.ui.notifications_filter.is_empty() {
        rows.push(filter_row(ctx));
    }

    if items.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            f(),
            empty_label(ctx),
        )])));
        rows.push(hint_row(&[("/ ", "search"), ("A", "show read")]));
        return rows;
    }

    for (i, n) in items.iter().enumerate() {
        let ago = thegn_core::util::age(n.created_at_ms);
        let src = truncate(&n.source_ref, 14);
        let msg_budget = ctx
            .cols
            .saturating_sub(kind_gh(n.kind).0.len() + 1 + src.len() + 3 + ago.len() + 1);
        let msg = truncate(&n.message, msg_budget.max(4));
        let wt_base = n
            .worktree_path
            .rsplit('/')
            .next()
            .unwrap_or(&n.worktree_path);
        let wt_base = display_cell(wt_base);

        let (glyph_tok, src_tok, msg_tok) = if n.read {
            (f(), f(), f())
        } else {
            (kind_hue(n.kind), d(), t())
        };

        let line1 = Line::segs(vec![
            seg(glyph_tok, kind_gh(n.kind).0),
            seg(g3(), " "),
            seg(src_tok, src),
            seg(g(), " · "),
            seg(msg_tok, msg),
            seg(g3(), " "),
            seg(f(), ago),
        ]);

        // The cursor's secondary row wraps the complete message; other rows
        // retain the compact worktree and kind metadata.
        let secondary = if i == cursor {
            wrap_text(&n.message, ctx.cols)
                .into_iter()
                .map(|part| Line::segs(vec![sp(2), seg(t(), part)]))
                .collect::<Vec<_>>()
        } else {
            vec![Line::segs(vec![
                sp(2),
                seg(f(), wt_base),
                seg(g(), "  ·  "),
                seg(f(), n.kind.label()),
            ])]
        };

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
        rows.extend(secondary.into_iter().map(|line| PanelRow {
            line,
            bg: None,
            hit: None,
        }));
        // No in-section truncation — see `normal_view`.
    }

    rows.push(hint_row(&[
        ("↵", "expand"),
        ("/ ", "search"),
        ("x", "read"),
        ("d", "delete"),
        ("a", "clear"),
        (
            "g",
            if crate::panel::scope::system_all() {
                "this repo"
            } else {
                "all worktrees"
            },
        ),
    ]));
    rows
}

// ---- Full view (150 cols) ---------------------------------------------------

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let items = visible(ctx);
    let cols = ctx.cols;
    let mut rows = Vec::new();

    // Header bar
    let counts = ctx.model.panel.notification_counts();
    let mut count_parts = Vec::new();
    if counts.alert > 0 {
        count_parts.push(format!("⚑ {} alert", counts.alert));
    }
    if counts.alert + counts.notice > 0 {
        count_parts.push(format!("{} unread", counts.alert + counts.notice));
    }
    if counts.info > 0 {
        count_parts.push(format!("{} low", counts.info));
    }
    count_parts.push(format!("{} shown", ctx.model.panel.notifications.len()));
    if ctx.ui.notifications_show_read && ctx.model.panel.notifications_hidden_read > 0 {
        count_parts.push(format!(
            "+{} older read",
            ctx.model.panel.notifications_hidden_read
        ));
    }
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
    let mut header = format!("  {}", count_parts.join(" · "));
    if !show_tag.is_empty() {
        header.push_str(show_tag);
    }
    if !filt_tag.is_empty() {
        header.push_str(&filt_tag);
    }
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "NOTIFICATIONS"),
        seg(f(), header),
    ])));
    rows.push(rule());

    if ctx.ui.notifications_filter_editing || !ctx.ui.notifications_filter.is_empty() {
        rows.push(filter_row(ctx));
    }

    if items.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            f(),
            empty_label(ctx),
        )])));
        rows.push(hint_row(&[("/ ", "search"), ("A", "show read")]));
        return rows;
    }

    let cursor = ctx.ui.notifications_cursor;
    let list_w = 45_usize.min(cols / 2);

    // Left column: compact list
    let list_rows: Vec<(Vec<Seg>, Option<usize>)> = items
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let ago = thegn_core::util::age(n.created_at_ms);
            let sel_mark = if i == cursor { "▶ " } else { "  " };
            let src_w = list_w.saturating_sub(4 + ago.len() + 1).max(4);
            let src = truncate(&n.source_ref, src_w);
            let (glyph_tok, src_tok) = if n.read {
                (f(), f())
            } else {
                (kind_hue(n.kind), d())
            };
            (
                vec![
                    seg(if i == cursor { t() } else { g() }, sel_mark),
                    seg(glyph_tok, kind_gh(n.kind).0),
                    seg(g3(), " "),
                    seg(src_tok, src),
                    seg(g3(), " "),
                    seg(f(), ago),
                ],
                Some(i),
            )
        })
        .collect();

    // Right column: detail for cursor item
    let detail_rows: Vec<Vec<Seg>> = if let Some(n) = items.get(cursor) {
        notification_detail_segs(n, cols.saturating_sub(list_w + 2))
    } else {
        vec![vec![seg(f(), "select a notification")]]
    };

    rows.extend(list_detail(
        Section::Notifications,
        &list_rows,
        &detail_rows,
        cursor,
        list_w,
        ctx.rows.saturating_sub(rows.len() + 2),
    ));

    rows.push(rule());
    rows.push(hint_row(&[
        ("↵", "expand"),
        ("/ ", "search"),
        ("x", "read"),
        ("d", "delete"),
        ("a", "clear all"),
        ("A", "show all"),
    ]));
    rows
}

fn notification_detail_segs(n: &Notification, w: usize) -> Vec<Vec<Seg>> {
    let mut out: Vec<Vec<Seg>> = Vec::new();

    // Kind glyph + source reference (title row)
    out.push(vec![
        seg(kind_hue(n.kind), kind_gh(n.kind).0),
        seg(g(), "  "),
        seg(t(), truncate(&n.source_ref, w.saturating_sub(4))).bold(),
    ]);

    // Kind label + read state
    let read_tag = if n.read { "  read" } else { "  unread" };
    out.push(vec![seg(f(), n.kind.label()), seg(g(), read_tag)]);

    // Message
    for chunk in wrap_text(&n.message, w) {
        out.push(vec![seg(t(), chunk)]);
    }

    // Worktree path
    if !n.worktree_path.is_empty() {
        out.push(vec![
            seg(f(), "worktree  "),
            seg(g(), truncate(&n.worktree_path, w.saturating_sub(10))),
        ]);
    }

    // Age
    let ago = thegn_core::util::age(n.created_at_ms);
    out.push(vec![seg(f(), format!("{ago} ago"))]);

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_text(rows: &[PanelRow]) -> String {
        rows.iter()
            .filter_map(|row| match &row.line {
                Line::Segs(segs) => Some(
                    segs.iter()
                        .map(|part| part.text.as_str())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn test_notification(kind: NotificationKind, read: bool, message: &str) -> Notification {
        Notification {
            id: 1,
            kind,
            source_ref: "THE-1".into(),
            message: message.into(),
            created_at_ms: 1,
            read,
            worktree_path: String::new(),
        }
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("ab", 1), "…");
        assert_eq!(truncate("ok\r\nforged\u{202e}\u{200b}", 40), "ok��forged��");
    }

    #[test]
    fn notification_detail_replaces_hostile_message_and_source_text() {
        let n = Notification {
            id: 1,
            kind: NotificationKind::AgentFailed,
            source_ref: "THE-1\n\x1b]52;c;QQ==\x07".into(),
            message: "agent finished in a\r\nTHE-9 ✓\u{202e}\u{200b}".into(),
            created_at_ms: thegn_core::util::now(),
            read: false,
            worktree_path: String::new(),
        };
        let text = notification_detail_segs(&n, 40)
            .iter()
            .flatten()
            .map(|part| part.text.as_str())
            .collect::<String>();

        for ch in ['\r', '\n', '\x1b', '\x07', '\u{202e}', '\u{200b}'] {
            assert!(!text.contains(ch), "rendered text contains {ch:?}");
        }
        assert!(text.contains('\u{fffd}'));
    }

    #[test]
    fn committed_filter_is_visible_at_normal_and_half() {
        let model = crate::chrome::FrameModel::default();
        for width in [
            crate::layout::PanelWidth::Normal,
            crate::layout::PanelWidth::Half,
        ] {
            let ui = crate::panel::PanelUi {
                open: Section::Notifications,
                width,
                notifications_filter: "zzz".into(),
                ..Default::default()
            };
            let text = rendered_text(&content(&SectionCtx {
                model: &model,
                ui: &ui,
                cols: 75,
                rows: 20,
            }));
            assert!(text.contains("/ zzz  (/ then Esc clears)"));
        }
    }

    #[test]
    fn filtered_empty_state_says_no_matches() {
        let mut model = crate::chrome::FrameModel::default();
        model.panel.notifications = vec![test_notification(
            NotificationKind::Assigned,
            false,
            "different text",
        )];
        let ui = crate::panel::PanelUi {
            open: Section::Notifications,
            notifications_filter: "zzz".into(),
            ..Default::default()
        };
        let text = rendered_text(&content(&SectionCtx {
            model: &model,
            ui: &ui,
            cols: 75,
            rows: 20,
        }));
        assert!(text.contains("no matches for /zzz/"));
        assert!(!text.contains("inbox zero"));
    }

    #[test]
    fn full_header_counts_match_the_chip() {
        let mut model = crate::chrome::FrameModel::default();
        model.panel.notifications = vec![
            test_notification(NotificationKind::AgentFailed, false, "alert one"),
            Notification {
                id: 2,
                ..test_notification(NotificationKind::TestFailed, false, "alert two")
            },
            Notification {
                id: 3,
                ..test_notification(NotificationKind::Assigned, false, "notice")
            },
            Notification {
                id: 4,
                ..test_notification(NotificationKind::WorktreeCreated, false, "info")
            },
        ];
        let ui = crate::panel::PanelUi {
            open: Section::Notifications,
            width: crate::layout::PanelWidth::Full,
            ..Default::default()
        };
        let text = rendered_text(&content(&SectionCtx {
            model: &model,
            ui: &ui,
            cols: 150,
            rows: 20,
        }));
        assert!(text.contains("⚑ 2 alert · 3 unread · 1 low · 4 shown"));
        let chip = crate::panel::sections::summary(Section::Notifications, &model)
            .iter()
            .map(|part| part.text.as_str())
            .collect::<String>();
        assert!(chip.contains("⚑ 2"));
    }
}
