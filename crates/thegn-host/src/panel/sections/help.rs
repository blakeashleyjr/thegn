//! The Help section: the docked twin of the F1 overlay. Renders the page on
//! `PanelUi::help` through the same `help::render` engine at the panel's
//! width. A one-entry memo avoids flowing the same page twice in one frame.

use std::cell::RefCell;
use std::sync::Arc;

use crate::seg::{Line, seg};

use super::{PanelRow, SectionCtx, d, hint_row};

type PageMemo = (
    Arc<thegn_core::help::HelpRegistry>,
    String,
    usize,
    Vec<Line>,
    Vec<(usize, String)>,
);

thread_local! {
    static PAGE_MEMO: RefCell<Option<PageMemo>> = const { RefCell::new(None) };
}

fn with_lines<R>(ctx: &SectionCtx, f: impl FnOnce(&[Line]) -> R) -> R {
    let Some(reg) = ctx.ui.help.reg.as_ref() else {
        return f(&[]);
    };
    let page_id = if ctx.ui.help.page.is_empty() {
        "index"
    } else {
        ctx.ui.help.page.as_str()
    };
    let Some(page) = reg.page(page_id).or_else(|| reg.page("index")) else {
        return f(&[]);
    };
    let width = ctx.cols.saturating_sub(1).max(8);
    let id = page.meta.id.clone();
    let cached = PAGE_MEMO.with(|memo| {
        let memo = memo.borrow();
        memo.as_ref()
            .filter(|(cached_reg, cached_id, cached_width, _, _)| {
                Arc::ptr_eq(cached_reg, reg) && *cached_id == id && *cached_width == width
            })
            .is_some()
    });
    if !cached {
        let blocks = crate::help::render::page_blocks(reg, &id);
        let rendered = crate::help::render::render_page(&blocks, width, None);
        PAGE_MEMO.with(|memo| {
            *memo.borrow_mut() = Some((
                Arc::clone(reg),
                id,
                width,
                rendered.lines,
                rendered.headings,
            ))
        });
    }
    PAGE_MEMO.with(|memo| {
        let memo = memo.borrow();
        f(&memo.as_ref().expect("page memo was just populated").3)
    })
}

pub(crate) fn anchor_at(ui: &crate::panel::PanelUi) -> Option<crate::help::HelpAnchor> {
    let page_id = if ui.help.page.is_empty() {
        "index"
    } else {
        &ui.help.page
    };
    PAGE_MEMO.with(|memo| {
        let memo = memo.borrow();
        let (cached_reg, cached_id, _, _, headings) = memo.as_ref()?;
        let reg = ui.help.reg.as_ref()?;
        (Arc::ptr_eq(cached_reg, reg) && cached_id == page_id)
            .then(|| crate::help::anchor_at(headings, ui.scroll))?
    })
}

pub(crate) fn scroll_for_anchor(
    reg: &thegn_core::help::HelpRegistry,
    page_id: &str,
    cols: usize,
    anchor: Option<crate::help::HelpAnchor>,
) -> usize {
    let Some(anchor) = anchor else { return 0 };
    let Some(page) = reg.page(page_id).or_else(|| reg.page("index")) else {
        return 0;
    };
    let width = cols.saturating_sub(1).max(8);
    let blocks = crate::help::render::page_blocks(reg, &page.meta.id);
    let rendered = crate::help::render::render_page(&blocks, width, None);
    crate::help::scroll_for(&rendered.headings, anchor, rendered.lines.len())
}

pub(super) fn line_count(ctx: &SectionCtx) -> usize {
    if ctx.ui.help.reg.is_none() {
        return 1;
    }
    with_lines(ctx, <[Line]>::len)
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.ui.help.reg.is_none() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            d(),
            "help registry not loaded",
        )]))];
    }
    with_lines(ctx, |lines| {
        if lines.is_empty() {
            return vec![PanelRow::plain(Line::segs(vec![seg(d(), "no help pages")]))];
        }
        let body = ctx.rows.saturating_sub(2); // blank + pinned footer
        let scroll = ctx.ui.scroll.min(lines.len().saturating_sub(1));
        let mut rows: Vec<PanelRow> = lines
            .iter()
            .skip(scroll)
            .take(body)
            .cloned()
            .map(PanelRow::plain)
            .collect();
        rows.push(PanelRow::blank());
        rows.push(hint_row(&[
            ("j/k", "scroll"),
            ("↵", "full help"),
            (ctx.model.chord("help"), "full help"),
            ("esc", "back"),
        ]));
        rows
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::layout::PanelWidth;
    use crate::panel::{HelpPanelState, PanelUi, Section};
    use std::sync::Arc;
    use thegn_core::config::Config;

    fn fixture(width: PanelWidth, scroll: usize) -> (FrameModel, PanelUi) {
        let model = FrameModel::default();
        let ui = PanelUi {
            open: Section::Help,
            tab: crate::panel::PanelTab::Help,
            width,
            scroll,
            help: HelpPanelState {
                page: String::new(),
                reg: Some(Arc::new(crate::help::pages::registry_logged(
                    &Config::default(),
                ))),
            },
            ..Default::default()
        };
        (model, ui)
    }

    fn ctx<'a>(model: &'a FrameModel, ui: &'a PanelUi, width: usize) -> SectionCtx<'a> {
        SectionCtx {
            model,
            ui,
            cols: width,
            rows: 12,
        }
    }

    fn text(rows: &[PanelRow]) -> String {
        rows.iter()
            .map(|row| match &row.line {
                Line::Blank => String::new(),
                Line::Segs(segs) => segs
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => l
                    .iter()
                    .chain(r)
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                Line::Fill { ch, .. } => ch.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn normal_help_scrolls_and_starts_with_rendered_heading() {
        let (model, ui) = fixture(PanelWidth::Normal, 0);
        let first = content(&ctx(&model, &ui, 39));
        assert!(
            text(&first)
                .lines()
                .next()
                .is_some_and(|line| line.starts_with("thegn"))
        );

        let (model, ui) = fixture(PanelWidth::Normal, 5);
        let scrolled = content(&ctx(&model, &ui, 39));
        assert_ne!(text(&first).lines().next(), text(&scrolled).lines().next());
    }

    #[test]
    fn switching_from_scrolled_keys_renders_help_at_the_top() {
        let (model, mut ui) = fixture(PanelWidth::Full, 7);
        ui.open = Section::Keys;
        ui.tab = crate::panel::PanelTab::System;
        ui.switch_tab(crate::panel::PanelTab::Help);
        let rendered = content(&ctx(&model, &ui, 80));
        assert_eq!(ui.scroll, 0);
        assert!(
            text(&rendered)
                .lines()
                .next()
                .is_some_and(|line| line.starts_with("thegn"))
        );
    }

    #[test]
    fn help_chord_is_shared_by_footer_and_summary_at_every_width() {
        let mut model = FrameModel::default();
        model.chord_hints.insert("help".into(), "F2".into());
        for width in [PanelWidth::Normal, PanelWidth::Full] {
            let (_, base) = fixture(width, 0);
            let ui = PanelUi { ..base };
            let rows = content(&ctx(&model, &ui, 80));
            let footer = text(&rows[rows.len() - 1..]);
            assert!(footer.contains("F2"), "{footer}");
            assert!(!footer.contains("F1"), "{footer}");
        }
        assert_eq!(super::super::summary(Section::Help, &model)[0].text, "F2");
    }

    #[test]
    fn line_count_matches_rendered_index_page() {
        let (model, ui) = fixture(PanelWidth::Normal, 0);
        let context = ctx(&model, &ui, 39);
        let registry = ui.help.reg.as_ref().expect("fixture registry");
        let blocks = crate::help::render::page_blocks(registry, "index");
        let expected = crate::help::render::render_page(&blocks, 38, None)
            .lines
            .len();
        assert_eq!(line_count(&context), expected);
    }

    #[test]
    fn scroll_extent_covers_help_at_each_width() {
        for width in [PanelWidth::Normal, PanelWidth::Half, PanelWidth::Full] {
            let (model, ui) = fixture(width, 0);
            let extent = super::super::scroll_extent(Section::Help, &model, &ui, 39, 12)
                .expect("Help is scrollable at every width");
            assert!(extent.owns_nav);
            let content_cols = if width == PanelWidth::Full { 37 } else { 36 };
            let viewport = super::super::scroll_viewport(&model, &ui, 39, 12);
            assert_eq!(
                extent.max,
                line_count(&ctx(&model, &ui, content_cols))
                    .saturating_sub(viewport.saturating_sub(2))
            );

            let tiny_extent = super::super::scroll_extent(Section::Help, &model, &ui, 39, 1)
                .expect("Help remains scrollable in a tiny frame");
            let tiny_viewport = super::super::scroll_viewport(&model, &ui, 39, 1);
            assert_eq!(
                tiny_extent.max,
                line_count(&ctx(&model, &ui, content_cols))
                    .saturating_sub(tiny_viewport.saturating_sub(2))
            );
        }
    }

    #[test]
    fn max_scroll_displays_the_last_help_line_in_normal_and_full_frames() {
        for width in [PanelWidth::Normal, PanelWidth::Full] {
            let (model, mut ui) = fixture(width, 0);
            let extent = super::super::scroll_extent(Section::Help, &model, &ui, 80, 40)
                .expect("Help is scrollable");
            ui.scroll = extent.max;
            let content_cols = if width == PanelWidth::Full { 78 } else { 77 };
            let registry = ui.help.reg.as_ref().expect("fixture registry");
            let blocks = crate::help::render::page_blocks(registry, "index");
            let lines = crate::help::render::render_page(&blocks, content_cols - 1, None).lines;
            let last_line = lines.last().expect("index page has lines");
            let last_text = text(&[PanelRow::plain(last_line.clone())]);
            assert!(!last_text.is_empty(), "last page line must contain text");
            let frame = crate::panel::frame::build_panel(&model, &ui, 80, 40, true);
            let rendered = text(&frame.rows);
            assert!(
                rendered.contains(&last_text),
                "last Help page line was absent at {width:?}"
            );
        }
    }

    #[test]
    fn scroll_extent_only_includes_the_planned_sections_and_widths() {
        let (model, base) = fixture(PanelWidth::Full, 0);
        let mut stash = PanelUi {
            open: Section::Stash,
            ..base
        };
        stash.git.stash_diff = Some((0, "one\ntwo\nthree".into()));
        let extent = super::super::scroll_extent(Section::Stash, &model, &stash, 80, 12)
            .expect("Full Stash diff scrolls");
        assert!(!extent.owns_nav);
        assert_eq!(extent.max, 2);
        let stash_half = PanelUi {
            width: PanelWidth::Half,
            ..stash
        };
        assert!(super::super::scroll_extent(Section::Stash, &model, &stash_half, 80, 12).is_none());
        let keys_normal = PanelUi {
            open: Section::Keys,
            width: PanelWidth::Normal,
            ..Default::default()
        };
        assert!(super::super::scroll_extent(Section::Keys, &model, &keys_normal, 80, 12).is_none());
        for width in [PanelWidth::Half, PanelWidth::Full] {
            let keys = PanelUi {
                open: Section::Keys,
                width,
                ..Default::default()
            };
            let extent = super::super::scroll_extent(Section::Keys, &model, &keys, 80, 12)
                .expect("expanded Keys body scrolls");
            assert!(extent.owns_nav);
        }
        let changes = PanelUi {
            open: Section::Changes,
            width: PanelWidth::Full,
            ..Default::default()
        };
        assert!(super::super::scroll_extent(Section::Changes, &model, &changes, 80, 12).is_none());
        let pr = PanelUi {
            open: Section::Pr,
            width: PanelWidth::Full,
            ..Default::default()
        };
        assert!(super::super::scroll_extent(Section::Pr, &model, &pr, 80, 12).is_none());
    }

    #[test]
    fn scroll_for_anchor_round_trips_at_two_panel_widths() {
        let reg = crate::help::pages::build_registry(&Config::default()).0;
        let page = "panel";
        for cols in [28, 64] {
            let width = cols.saturating_sub(1).max(8);
            let rendered = crate::help::render::render_page(
                &crate::help::render::page_blocks(&reg, page),
                width,
                None,
            );
            let heading =
                crate::help::heading_for_section(&rendered.headings, "merge", "merge").unwrap();
            let scroll = rendered.headings[heading].0 + 1;
            let anchor = crate::help::anchor_at(&rendered.headings, scroll).unwrap();
            let resolved = scroll_for_anchor(&reg, page, cols, Some(anchor));
            let rerendered = crate::help::render::render_page(
                &crate::help::render::page_blocks(&reg, page),
                width,
                None,
            );
            assert_eq!(
                crate::help::anchor_at(&rerendered.headings, resolved),
                Some(anchor)
            );
        }
    }
}
