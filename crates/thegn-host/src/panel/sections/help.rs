//! The Help section: the docked twin of the F1 overlay. Renders the page on
//! `PanelUi::help` through the same `help::render` engine at the panel's
//! width — Normal/Half/Full differ only in the columns they're given. Pure
//! recompute from the embedded registry on every draw; nothing to fetch.

use crate::seg::{Line, seg};

use super::{PanelRow, SectionCtx, d, g2, hint_row};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let Some(reg) = ctx.ui.help.reg.as_ref() else {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            d(),
            "help registry not loaded",
        )]))];
    };
    let page_id = if ctx.ui.help.page.is_empty() {
        "index"
    } else {
        ctx.ui.help.page.as_str()
    };
    let Some(page) = reg.page(page_id).or_else(|| reg.page("index")) else {
        return vec![PanelRow::plain(Line::segs(vec![seg(d(), "no help pages")]))];
    };

    let width = ctx.cols.saturating_sub(1).max(8);
    let rendered = crate::help::render::render_page(&page.blocks, width, None);

    if ctx.full() {
        // Full view: the whole page, scrolled by the shared `ui.scroll`.
        let body = ctx.rows.saturating_sub(2); // blank + footer
        let scroll = ctx.ui.scroll.min(rendered.lines.len().saturating_sub(1));
        let mut rows: Vec<PanelRow> = rendered
            .lines
            .into_iter()
            .skip(scroll)
            .take(body)
            .map(PanelRow::plain)
            .collect();
        rows.push(PanelRow::blank());
        rows.push(hint_row(&[
            ("j/k", "scroll"),
            ("F1", "full help"),
            ("esc", "back"),
        ]));
        return rows;
    }

    // Normal/Half: the page head + a pointer to the overlay (the budget
    // truncates overflow to a "+N more" row anyway).
    let mut rows: Vec<PanelRow> = vec![PanelRow::plain(Line::segs(vec![
        seg(g2(), page.meta.title.to_uppercase()).bold(),
    ]))];
    rows.extend(rendered.lines.into_iter().skip(2).map(PanelRow::plain));
    rows.push(PanelRow::blank());
    rows.push(hint_row(&[("e", "read full-width"), ("F1", "full help")]));
    rows
}
