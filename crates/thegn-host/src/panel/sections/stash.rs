//! The stash section: structured stash entries.

use thegn_core::util::age;

use crate::panel::gitui::DocLoad;
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
        let message = match &data.stashes_load {
            DocLoad::Pending => "loading stashes…".to_string(),
            DocLoad::Ready => "no stashes".to_string(),
            DocLoad::Failed(error) => format!("stashes unavailable: {error}"),
        };
        rows.push(PanelRow::plain(Line::segs(vec![seg(g(), message)])));
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

#[cfg(test)]
mod tests {
    use super::content;
    use crate::chrome::FrameModel;
    use crate::layout::PanelWidth;
    use crate::panel::gitui::DocLoad;
    use crate::panel::sections::{Section, SectionCtx};

    #[test]
    fn stash_pending_ready_failed_render_distinctly() {
        let render = |load| {
            let model = FrameModel {
                panel: crate::panel::PanelData {
                    stashes_load: load,
                    ..Default::default()
                },
                ..Default::default()
            };
            let ui = crate::panel::PanelUi {
                open: Section::Stash,
                width: PanelWidth::Normal,
                ..Default::default()
            };
            let ctx = SectionCtx {
                model: &model,
                ui: &ui,
                cols: 39,
                rows: 10,
            };
            content(&ctx)
                .iter()
                .flat_map(|row| match &row.line {
                    crate::seg::Line::Segs(segs) => {
                        segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                })
                .collect::<String>()
        };
        let pending = render(DocLoad::Pending);
        let ready = render(DocLoad::Ready);
        let failed = render(DocLoad::Failed("git unavailable".into()));
        assert_ne!(pending, ready);
        assert_ne!(pending, failed);
        assert_ne!(ready, failed);
        assert!(pending.contains("loading stashes"), "{pending}");
        assert!(ready.contains("no stashes"), "{ready}");
        assert!(failed.contains("git unavailable"), "{failed}");
    }
}
