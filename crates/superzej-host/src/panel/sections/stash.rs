//! The stash section: structured stash entries.

use superzej_core::util::age;

use crate::panel::gitui::GitView;
use crate::seg::{Line, Seg, seg, sp};

use super::{PanelHit, PanelRow, Section, SectionCtx, ac, d, filter_row, g, g2};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (data, ui) = (&ctx.model.panel, ctx.ui);
    let mut rows: Vec<PanelRow> = Vec::new();
    if let Some(fr) = filter_row(ui, GitView::Stash, data.stashes.len()) {
        rows.push(fr);
    }
    if data.stashes.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g(), "no stashes")])));
        return rows;
    }
    let indices = super::filtered_indices(ui, GitView::Stash, data.stashes.len(), |i| {
        data.stashes[i].message.clone()
    });
    for (display, &i) in indices.iter().enumerate() {
        let s = &data.stashes[i];
        let l: Vec<Seg> = vec![
            seg(ac(), format!("{{{}}}", s.index)),
            sp(1),
            seg(d(), s.message.clone()),
        ];
        let r = vec![seg(g2(), age(s.date))];
        rows.push(
            PanelRow::plain(Line::split(l, r)).with_hit(PanelHit::Row(Section::Stash, display)),
        );
    }
    if ctx.full() {
        rows.push(super::rule());
        rows.push(super::context_hint_row(GitView::Stash));
    }
    rows
}
