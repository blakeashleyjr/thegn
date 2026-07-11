//! The branches section: local branches with upstream divergence and the
//! per-branch PR badge (joined from `pr_branch_cache`).

use thegn_core::theme::Hue;
use thegn_core::util::age;

use crate::panel::gitui::GitView;
use crate::seg::{Line, Seg, seg, sp};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, filter_row, g, g2, hue, t};

fn pr_state_hue(state: &str, draft: bool) -> Hue {
    if draft {
        return Hue::Amber;
    }
    match state {
        "OPEN" => Hue::Green,
        "MERGED" | "CLOSED" => Hue::Purple,
        _ => Hue::Amber,
    }
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (data, ui) = (&ctx.model.panel, ctx.ui);
    let mut rows: Vec<PanelRow> = Vec::new();
    if let Some(fr) = filter_row(ui, GitView::Branches, data.branches.len()) {
        rows.push(fr);
    }
    if data.branches.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g(), "no branches")])));
        return rows;
    }
    let indices = super::filtered_indices(ui, GitView::Branches, data.branches.len(), |i| {
        data.branches[i].name.clone()
    });
    for (display, &i) in indices.iter().enumerate() {
        let b = &data.branches[i];
        let mut l: Vec<Seg> = Vec::new();
        l.push(if b.is_head {
            seg(hue(Hue::Green), "*").bold()
        } else {
            sp(1)
        });
        l.push(sp(1));
        l.push(if b.is_head {
            seg(t(), b.name.clone()).bold()
        } else {
            seg(d(), b.name.clone())
        });
        if b.ahead > 0 || b.behind > 0 {
            l.push(sp(1));
            if b.ahead > 0 {
                l.push(seg(hue(Hue::Green), format!("↑{}", b.ahead)));
            }
            if b.behind > 0 {
                l.push(seg(hue(Hue::Red), format!("↓{}", b.behind)));
            }
        } else if b.upstream_gone {
            l.push(sp(1));
            l.push(seg(hue(Hue::Red), "✗gone"));
        }
        let mut r: Vec<Seg> = Vec::new();
        if let Some(pr) = &b.pr {
            r.push(seg(
                hue(pr_state_hue(&pr.state, pr.is_draft)),
                format!("⬤ #{}", pr.number),
            ));
            r.push(sp(1));
        }
        r.push(seg(g2(), age(b.date)));
        rows.push(
            PanelRow::plain(Line::split(l, r)).with_hit(PanelHit::Row(Section::Branches, display)),
        );
    }
    if ctx.full() {
        rows.push(super::rule());
        rows.push(super::context_hint_row(GitView::Branches));
    }
    rows
}
