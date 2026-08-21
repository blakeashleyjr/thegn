//! The keys section: the panel's own vocabulary (Normal), the grouped
//! cheatsheet in one chip column (Half), and the two-column balanced
//! cheatsheet (Full — the former help overlay). Groups come from the
//! effective keymap, cached on the panel docs and refreshed on config reload.

use crate::keyhint::{HintGroup, HintRow};
use crate::seg::{Line, Seg, Tok, seg, sp};

use super::{PanelRow, SectionCtx, d, f, g2, hint_row, two_col};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() {
        full(ctx)
    } else if ctx.deep() {
        half(ctx)
    } else {
        normal(ctx)
    }
}

/// The hand-built "Panel" group — the accordion's own keys.
fn panel_group(ctx: &SectionCtx) -> HintGroup {
    let row = |chord: &str, label: &str| HintRow {
        chord: chord.into(),
        label: label.into(),
    };
    // Digits index the ACTIVE TAB's sections (max 10, `0` = the 10th) — the
    // old `1-26` claim counted every tab's sections, most of which no digit
    // could ever reach.
    let n = ctx.ui.tab_sections().len();
    let jump = match n {
        0..=9 => format!("1-{n}"),
        _ => "1-9,0".to_string(),
    };
    HintGroup {
        title: "Panel".into(),
        rows: vec![
            row(&jump, "jump to section (this tab)"),
            row("e", "cycle width"),
            row("j/k", "walk rows"),
            row("J/K", "hop sections"),
            row("↵", "select row"),
            row("esc", "back"),
        ],
    }
}

/// Every group the wider views render: the cached cheatsheet + Panel.
fn all_groups(ctx: &SectionCtx) -> Vec<HintGroup> {
    let mut groups = ctx.ui.docs.cfg_keys.clone();
    groups.push(panel_group(ctx));
    groups
}

/// Normal: just the Panel group, plain chords.
fn normal(ctx: &SectionCtx) -> Vec<PanelRow> {
    let group = panel_group(ctx);
    let mut rows: Vec<PanelRow> = vec![PanelRow::plain(Line::segs(vec![
        seg(g2(), group.title.to_uppercase()).bold(),
    ]))];
    for r in &group.rows {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(f(), format!("{:<6}", r.chord)),
            seg(d(), r.label.clone()),
        ])));
    }
    rows.push(PanelRow::blank());
    rows.push(hint_row(&[("e", "full cheatsheet")]));
    rows
}

/// One group as chip lines: bold title, chip rows, trailing blank.
fn group_lines(group: &HintGroup) -> Vec<Line> {
    let mut out = vec![Line::segs(vec![
        seg(g2(), group.title.to_uppercase()).bold(),
    ])];
    for row in &group.rows {
        out.push(Line::segs(vec![
            sp(1),
            Seg::chip(
                Tok::Slot(crate::chrome::S::Raise),
                format!(" {} ", row.chord),
            ),
            seg(d(), format!("  {}", row.label)),
        ]));
    }
    out.push(Line::Blank);
    out
}

/// Half: every group in one chip column, scrolled by `ui.scroll` — the
/// unscrolled column made every group past the fold unreachable at this
/// width (`j` left the section instead of scrolling).
fn half(ctx: &SectionCtx) -> Vec<PanelRow> {
    let rows: Vec<PanelRow> = all_groups(ctx)
        .iter()
        .flat_map(group_lines)
        .map(PanelRow::plain)
        .collect();
    let max = rows.len().saturating_sub(ctx.rows.max(1));
    rows.into_iter().skip(ctx.ui.scroll.min(max)).collect()
}

/// Full: two balanced columns (greedy fill by line count), scrolled by
/// `ui.scroll` when taller than the body.
fn full(ctx: &SectionCtx) -> Vec<PanelRow> {
    let groups = all_groups(ctx);
    let total: usize = groups.iter().map(|g| g.rows.len() + 2).sum();
    let mut left: Vec<Line> = Vec::new();
    let mut right: Vec<Line> = Vec::new();
    for g in &groups {
        let lines = group_lines(g);
        if left.len() + lines.len() <= total.div_ceil(2) || left.is_empty() {
            left.extend(lines);
        } else {
            right.extend(lines);
        }
    }
    let to_segs = |lines: Vec<Line>| -> Vec<Vec<Seg>> {
        lines
            .into_iter()
            .map(|l| match l {
                Line::Segs(v) => v,
                _ => Vec::new(),
            })
            .collect()
    };
    let col_w = ctx.cols.saturating_sub(2) / 2;
    let lines = two_col(&to_segs(left), &to_segs(right), col_w, 2);
    let body = ctx.rows.saturating_sub(2); // blank + footer
    let scroll = ctx.ui.scroll.min(lines.len().saturating_sub(1));
    let mut rows: Vec<PanelRow> = lines
        .into_iter()
        .skip(scroll)
        .take(body)
        .map(PanelRow::plain)
        .collect();
    rows.push(PanelRow::blank());
    rows.push(hint_row(&[("j/k", "scroll"), ("esc", "back")]));
    rows
}
