//! The commits section: the structured commit list at every width (the Full
//! git frame renders the graph beside it). Rows carry the lazygit marks —
//! copied (cherry-pick clipboard), rebase base, diff mark — and the focused
//! filter line when `/` is live.

use superzej_core::theme::Hue;
use superzej_core::util::age;

use crate::panel::gitui::{GitFlow, GitView};
use crate::seg::{Line, Seg, seg, sp};

use super::{PanelHit, PanelRow, Section, SectionCtx, ac, d, filter_row, g, g2, hue, t};

/// Stable per-author hue (fnv1a over the name, mod the ramp).
pub fn author_hue(name: &str) -> Hue {
    const RAMP: [Hue; 8] = [
        Hue::Blue,
        Hue::Green,
        Hue::Amber,
        Hue::Purple,
        Hue::Orange,
        Hue::Red,
        Hue::Magenta,
        Hue::Teal,
    ];
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    RAMP[(h % RAMP.len() as u64) as usize]
}

/// Initials for the author column ("Blake Ashley" → "BA").
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (data, ui) = (&ctx.model.panel, ctx.ui);
    let mut rows: Vec<PanelRow> = Vec::new();
    if let Some(fr) = filter_row(ui, GitView::Commits, data.commits.len()) {
        rows.push(fr);
    }
    if data.commits.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g(), "no commits")])));
        return rows;
    }
    let indices = super::filtered_indices(ui, GitView::Commits, data.commits.len(), |i| {
        format!("{} {}", data.commits[i].short, data.commits[i].subject)
    });
    let sel = (ui.git.focus == GitView::Commits).then(|| ui.git.selection());
    for (display, &i) in indices.iter().enumerate() {
        let c = &data.commits[i];
        let mut l: Vec<Seg> = Vec::new();
        // Mark gutter: copied ❐ · base ▶ · diff-mark ◈ (one cell).
        let mark = if ui.git.clipboard.iter().any(|s| s == &c.sha) {
            seg(hue(Hue::Teal), "❐")
        } else if ui.git.mark_base.as_deref() == Some(c.sha.as_str()) {
            seg(hue(Hue::Magenta), "▶")
        } else if matches!(&ui.git.flow, GitFlow::Diffing(m) if m == &c.sha) {
            seg(hue(Hue::Blue), "◈")
        } else {
            sp(1)
        };
        l.push(mark);
        l.push(seg(ac(), c.short.clone()));
        l.push(sp(1));
        // Range-selection tint rides the subject (the cursor row itself is
        // tinted by the frame).
        let in_range = sel
            .as_ref()
            .is_some_and(|r| r.contains(&display) && ui.git.sel_anchor.is_some());
        let subject = if in_range {
            seg(t(), c.subject.clone()).bold()
        } else {
            seg(d(), c.subject.clone())
        };
        l.push(subject);
        if !c.refs.is_empty() && ctx.deep() {
            l.push(sp(1));
            l.push(seg(hue(Hue::Amber), format!("({})", c.refs)));
        }
        let r = vec![
            seg(hue(author_hue(&c.author)), initials(&c.author)),
            sp(1),
            seg(g2(), age(c.date)),
        ];
        rows.push(
            PanelRow::plain(Line::split(l, r)).with_hit(PanelHit::Row(Section::Commits, display)),
        );
    }
    if ctx.full() {
        rows.push(super::rule());
        rows.push(super::context_hint_row(GitView::Commits));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_hue_is_stable_and_initials_trim() {
        assert_eq!(author_hue("Blake"), author_hue("Blake"));
        assert_eq!(initials("Blake Ashley"), "BA");
        assert_eq!(initials("solo"), "S");
        assert_eq!(initials("a b c"), "AB");
        assert_eq!(initials(""), "");
    }
}
