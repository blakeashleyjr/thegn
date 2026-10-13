//! The Help section: the docked twin of the F1 overlay. Renders the page on
//! `PanelUi::help` through the same `help::render` engine at the panel's
//! width. A one-entry memo avoids flowing the same page twice in one frame.

use std::cell::RefCell;

use crate::seg::{Line, seg};

use super::{PanelRow, SectionCtx, d, hint_row};

thread_local! {
    static PAGE_MEMO: RefCell<Option<(usize, String, usize, Vec<Line>)>> = const { RefCell::new(None) };
}

fn rendered_lines(ctx: &SectionCtx) -> Vec<Line> {
    let Some(reg) = ctx.ui.help.reg.as_ref() else {
        return Vec::new();
    };
    let page_id = if ctx.ui.help.page.is_empty() {
        "index"
    } else {
        ctx.ui.help.page.as_str()
    };
    let Some(page) = reg.page(page_id).or_else(|| reg.page("index")) else {
        return Vec::new();
    };
    let width = ctx.cols.saturating_sub(1).max(8);
    let registry = std::sync::Arc::as_ptr(reg) as usize;
    let id = page.meta.id.clone();
    if let Some(lines) = PAGE_MEMO.with(|memo| {
        memo.borrow()
            .as_ref()
            .filter(|(ptr, cached_id, cached_width, _)| {
                *ptr == registry && *cached_id == id && *cached_width == width
            })
            .map(|(_, _, _, lines)| lines.clone())
    }) {
        return lines;
    }
    let blocks = crate::help::render::page_blocks(reg, &id);
    let lines = crate::help::render::render_page(&blocks, width, None).lines;
    PAGE_MEMO.with(|memo| *memo.borrow_mut() = Some((registry, id, width, lines.clone())));
    lines
}

pub(super) fn line_count(ctx: &SectionCtx) -> usize {
    if ctx.ui.help.reg.is_none() {
        return 1;
    }
    rendered_lines(ctx).len()
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.ui.help.reg.is_none() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            d(),
            "help registry not loaded",
        )]))];
    }
    let lines = rendered_lines(ctx);
    if lines.is_empty() {
        return vec![PanelRow::plain(Line::segs(vec![seg(d(), "no help pages")]))];
    }
    let body = ctx.rows.saturating_sub(2); // blank + pinned footer
    let scroll = ctx.ui.scroll.min(lines.len().saturating_sub(1));
    let mut rows: Vec<PanelRow> = lines
        .into_iter()
        .skip(scroll)
        .take(body)
        .map(PanelRow::plain)
        .collect();
    rows.push(PanelRow::blank());
    rows.push(hint_row(&[
        ("j/k", "scroll"),
        (ctx.model.chord("help"), "full help"),
        ("esc", "back"),
    ]));
    rows
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
                _ => String::new(),
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
            assert_eq!(
                extent.max,
                line_count(&ctx(&model, &ui, 39)).saturating_sub(10)
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
}
