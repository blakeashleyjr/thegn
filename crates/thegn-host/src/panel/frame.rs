//! The accordion frame: assemble the header zone and the numbered section
//! rows (the config-resolved order) with the open section's content into a
//! row list with per-row backgrounds and hit targets — the single source of
//! truth for both painting and mouse hit-testing.

use thegn_core::theme::Hue;
use thegn_core::viz;

use crate::chrome::{FrameModel, S};
use crate::seg::{Line, Seg, Tok, seg, sp};

use super::sections::{self, PanelRow, SectionCtx};
use super::{PanelHit, PanelTab, PanelUi, budget};

/// The fully-assembled panel: one entry per visible row, plus the full
/// view's rail hit spans (empty in Normal/Half).
#[derive(Debug, Clone, Default)]
pub struct PanelFrame {
    pub rows: Vec<PanelRow>,
    pub rail: Vec<RailSpan>,
    /// X-spans for the tab bar (always row 0): `(col_range, tab)`.
    /// Used by `chrome::panel_tab_hit` for x-based click resolution.
    /// Empty for the git-family full view which omits the tab bar.
    pub tab_spans: Vec<(std::ops::Range<usize>, PanelTab)>,
}

/// One section's clickable span in the full view's horizontal rail:
/// frame-row index + the x range (panel-relative) its `N label` occupies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailSpan {
    pub row: usize,
    pub cols: std::ops::Range<usize>,
    pub section: crate::panel::Section,
}

fn hue(h: Hue) -> Tok {
    Tok::Hue(h)
}

/// Clip `s` to at most `budget` columns, appending a trailing `…` when it
/// overflows. Returns empty when the budget can't fit even one char + ellipsis.
fn clip_end(s: &str, budget: usize) -> String {
    if s.chars().count() <= budget {
        return s.to_string();
    }
    if budget < 2 {
        return String::new();
    }
    let mut out: String = s.chars().take(budget - 1).collect();
    out.push('…');
    out
}

/// The header zone: branch + divergence, the merge banner when one is in
/// progress (chip, conflicts, resolved bar), a dirty summary otherwise,
/// then the rule. Row count varies (2..=5); the budget gets the real number.
pub(super) fn header_rows(model: &FrameModel, focused: bool, cols: usize) -> Vec<PanelRow> {
    let data = &model.panel;
    let mut rows: Vec<PanelRow> = Vec::new();

    let mut branch_l = vec![
        sp(1),
        seg(
            Tok::Slot(if focused { S::Accent } else { S::Text }),
            data.branch.clone(),
        )
        .bold(),
    ];
    if let Some((ahead, behind)) = data.ahead_behind {
        if ahead > 0 {
            branch_l.push(seg(hue(Hue::Green), format!(" ⇡{ahead}")));
        }
        if behind > 0 {
            branch_l.push(seg(hue(Hue::Red), format!("⇣{behind}")));
        }
    }
    let branch_r: Vec<Seg> = match &data.merge {
        Some(m) => vec![Seg::chip(hue(Hue::Amber), format!(" {} ", m.label)), sp(1)],
        None => match &data.pr {
            Some(pr) => vec![seg(Tok::Slot(S::Ghost), format!("#{} ", pr.number)), sp(1)],
            None => Vec::new(),
        },
    };
    rows.push(PanelRow::plain(Line::split(branch_l, branch_r)));

    match &data.merge {
        Some(m) => {
            // Zero unresolved is a *good* state (the merge is ready to commit),
            // so it reads green, not the alarming red used while conflicts remain.
            let (conf_text, conf_tok) = if m.unresolved == 0 {
                ("no conflicts".to_string(), hue(Hue::Green))
            } else {
                (
                    format!(
                        "{} conflict{}",
                        m.unresolved,
                        if m.unresolved == 1 { "" } else { "s" }
                    ),
                    hue(Hue::Red),
                )
            };
            let mut l = vec![sp(1), seg(Tok::Slot(S::Ghost), "merging ")];
            if !m.onto.is_empty() {
                // Clip the branch name, never the conflict count: the count is
                // the actionable bit, the onto ref is context. Budget = cols −
                // pad(1) − "merging "(8) − " · "(3) − suffix − trailing(1).
                let budget = cols.saturating_sub(13 + conf_text.chars().count());
                let onto = clip_end(&m.onto, budget);
                if !onto.is_empty() {
                    l.push(seg(Tok::Slot(S::Faint), onto));
                    l.push(seg(Tok::Slot(S::Ghost), " · "));
                }
            }
            l.push(seg(conf_tok, conf_text));
            rows.push(PanelRow::plain(Line::segs(l)));
            if let Some(total) = m.total
                && total > 0
            {
                let resolved = total.saturating_sub(m.unresolved);
                let (bar, track) = viz::bar_track(resolved as f32 / total as f32, 16);
                rows.push(PanelRow::plain(Line::split(
                    vec![
                        sp(1),
                        seg(Tok::Slot(S::Ghost), "resolved "),
                        seg(hue(Hue::Green), bar),
                        seg(Tok::Slot(S::Ghost3), track),
                    ],
                    vec![
                        seg(Tok::Slot(S::Faint), resolved.to_string()),
                        seg(Tok::Slot(S::Ghost), format!("/{total}")),
                        sp(1),
                    ],
                )));
            }
        }
        None if !data.changes.is_empty() => {
            let (a, d): (u32, u32) = data
                .changes
                .iter()
                .fold((0, 0), |(a, d), c| (a + c.added, d + c.deleted));
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(1),
                seg(hue(Hue::Green), format!("+{a}")),
                seg(Tok::Slot(S::Ghost), " "),
                seg(hue(Hue::Red), format!("−{d}")),
                seg(
                    Tok::Slot(S::Ghost),
                    format!(
                        " · {} file{}",
                        data.changes.len(),
                        if data.changes.len() == 1 { "" } else { "s" }
                    ),
                ),
            ])));
        }
        None => {}
    }

    rows.push(PanelRow::plain(Line::Fill {
        ch: '─',
        fg: Tok::Slot(S::Ghost3),
    }));
    rows
}

/// Compact header chips for the active git flows (bisect / diff-mode /
/// patch) and the in-flight mutation spinner, so a modal flow stays visible
/// at Normal/Half width too (the Full git frame renders its own richer
/// status). No timers: the spinner advances on the docs tick the stats
/// sampler already drives.
fn flow_chips(ui: &PanelUi, merge_banner: bool) -> Vec<Seg> {
    use crate::panel::gitui::GitFlow;
    let short = |s: &str| s.chars().take(7).collect::<String>();
    let mut chips: Vec<Seg> = Vec::new();
    if let Some(p) = &ui.git.pending {
        chips.push(seg(
            Tok::Slot(S::Accent),
            format!("{} {} ", viz::spin(ui.docs.tick), p.label),
        ));
    }
    match &ui.git.flow {
        GitFlow::Bisect(b) => {
            let label = match &b.culprit {
                Some(c) => format!(" BISECT {} ", short(c)),
                None => " BISECT ".to_string(),
            };
            chips.push(Seg::chip(hue(Hue::Purple), label));
            chips.push(sp(1));
        }
        GitFlow::Diffing(m) => {
            chips.push(Seg::chip(hue(Hue::Blue), format!(" DIFF vs {} ", short(m))));
            chips.push(sp(1));
        }
        GitFlow::Patch(p) => {
            chips.push(Seg::chip(hue(Hue::Teal), format!(" PATCH {} ", p.marked())));
            chips.push(sp(1));
        }
        GitFlow::Rebase(_) | GitFlow::None => {}
        // The header already renders a richer MERGING banner (chip + conflicts +
        // resolved bar) from the hydrated MERGE_HEAD state; emitting the flow chip
        // too painted a second amber MERGING box on the same row. Only show the
        // compact chip as a fallback when that banner isn't present.
        GitFlow::Merge(_) if merge_banner => {}
        GitFlow::Merge(s) => {
            let label = if s.conflict {
                " MERGING ⚑ "
            } else {
                " MERGING "
            };
            chips.push(Seg::chip(hue(Hue::Amber), label));
            chips.push(sp(1));
        }
        GitFlow::CherryPick(s) => {
            let label = if s.conflict {
                " CHERRY-PICK ⚑ "
            } else {
                " CHERRY-PICK "
            };
            chips.push(Seg::chip(hue(Hue::Purple), label));
            chips.push(sp(1));
        }
        GitFlow::Revert(s) => {
            let label = if s.conflict {
                " REVERTING ⚑ "
            } else {
                " REVERTING "
            };
            chips.push(Seg::chip(hue(Hue::Purple), label));
            chips.push(sp(1));
        }
    }
    chips
}

/// Prepend the flow chips to the header's branch row (its right side).
fn with_flow_chips(mut header: Vec<PanelRow>, ui: &PanelUi, merge_banner: bool) -> Vec<PanelRow> {
    let chips = flow_chips(ui, merge_banner);
    if chips.is_empty() {
        return header;
    }
    if let Some(first) = header.first_mut()
        && let Line::Split { r, .. } = &mut first.line
    {
        let mut new_r = chips;
        new_r.append(r);
        *r = new_r;
    }
    header
}

/// Indent a content line by 2 cells (tight section-content inset).
fn indent(line: Line) -> Line {
    const PAD: usize = 2;
    match line {
        Line::Blank => Line::Blank,
        Line::Fill { ch, fg } => Line::Fill { ch, fg },
        Line::Segs(mut v) => {
            v.insert(0, sp(PAD));
            Line::Segs(v)
        }
        Line::Split { mut l, mut r } => {
            l.insert(0, sp(PAD));
            if !r.is_empty() {
                r.push(sp(1));
            }
            Line::Split { l, r }
        }
    }
}

/// One-row tab bar: `git  work  system` with the active tab accented.
fn tab_bar_row(ui: &PanelUi, focused: bool) -> (PanelRow, Vec<(std::ops::Range<usize>, PanelTab)>) {
    use crate::seg::{seg, sp};
    let tabs = [PanelTab::Git, PanelTab::Work, PanelTab::System];
    let mut segs = vec![sp(1)];
    let mut spans: Vec<(std::ops::Range<usize>, PanelTab)> = Vec::new();
    let mut col = 1usize; // column after the leading sp(1)
    for (i, &tab) in tabs.iter().enumerate() {
        if i > 0 {
            segs.push(seg(Tok::Slot(S::Ghost3), "  "));
            col += 2;
        }
        let on = tab == ui.tab;
        let tok = if on && focused {
            Tok::Slot(S::Accent)
        } else if on {
            Tok::Slot(S::Text)
        } else {
            Tok::Slot(S::Ghost2)
        };
        let label = tab.label();
        let mut s = seg(tok, label);
        if on {
            s = s.bold();
        }
        segs.push(s);
        spans.push((col..col + label.len(), tab));
        col += label.len();
    }
    (PanelRow::plain(crate::seg::Line::Segs(segs)), spans)
}

/// The content geometry handed to section builders: the panel width minus
/// the 2-cell indent + 1-cell right pad, and a post-skeleton row estimate.
fn section_ctx<'a>(
    model: &'a FrameModel,
    ui: &'a PanelUi,
    cols: usize,
    rows: usize,
    header_len: usize,
) -> SectionCtx<'a> {
    SectionCtx {
        model,
        ui,
        cols: cols.saturating_sub(3),
        rows: rows.saturating_sub(header_len + ui.visible_section_count() + 2),
    }
}

/// Assemble the whole panel for a `cols` × `rows` rect. `focused` colors the
/// header branch row with the accent. The Full width renders the slim-rail
/// layout; Normal/Half keep the vertical accordion skeleton.
pub fn build_panel(
    model: &FrameModel,
    ui: &PanelUi,
    cols: usize,
    rows: usize,
    focused: bool,
) -> PanelFrame {
    if ui.width == crate::layout::PanelWidth::Full {
        if ui.open.is_git_family() {
            return super::gitfull::build_git_full(model, ui, cols, rows, focused);
        }
        return build_full(model, ui, cols, rows, focused);
    }

    // A drill detail view (commit files, staging, patch, blame, rebase todo)
    // is only rendered by the git frame. Render it INTO the half-width panel —
    // the center pane stays visible — instead of forcing the screen-filling
    // Full layout (which hides all chrome and feels like a trap). The accordion
    // list views keep the vertical skeleton below.
    if ui.width == crate::layout::PanelWidth::Half
        && ui.open.is_git_family()
        && matches!(
            ui.git.focus,
            super::gitui::GitView::Staging
                | super::gitui::GitView::CommitFiles
                | super::gitui::GitView::PatchBuilding
                | super::gitui::GitView::Blame
                | super::gitui::GitView::RebaseTodo
        )
    {
        return super::gitfull::build_git_full(model, ui, cols, rows, focused);
    }

    // Reserve 1 row for the tab bar at the top; account for it in the budget.
    let (tab_row, tab_spans) = tab_bar_row(ui, focused);
    let rows_for_body = rows.saturating_sub(1);

    let header = with_flow_chips(
        header_rows(model, focused, cols),
        ui,
        model.panel.merge.is_some(),
    );
    let ctx = section_ctx(model, ui, cols, rows_for_body, header.len());
    let content_raw = sections::content(ui.open, &ctx);
    let visible_secs: Vec<_> = ui
        .order
        .iter()
        .copied()
        .filter(|s| s.tab() == ui.tab)
        .collect();
    let plan = budget::allocate(
        rows_for_body,
        header.len(),
        content_raw.len(),
        visible_secs.len(),
    );

    let mut out: Vec<PanelRow> = Vec::new();
    out.push(tab_row);
    out.extend(header.into_iter().take(plan.header_rows));

    for (i, section) in visible_secs.iter().copied().enumerate() {
        let on = section == ui.open;
        let label_tok = if on {
            Tok::Slot(S::Accent)
        } else {
            Tok::Slot(S::Dim)
        };
        let mut label = seg(label_tok, section.label());
        if on {
            label = label.bold();
        }
        // Each accordion is numbered; the number is its open shortcut within the tab.
        let num = seg(
            Tok::Slot(if on { S::Accent } else { S::Ghost2 }),
            format!("{}", i + 1),
        )
        .bold();
        let mut srow = PanelRow::plain(Line::split(
            {
                let mut l = vec![sp(1), num, sp(1), label];
                l.push(sp(0));
                l
            },
            {
                let mut r = sections::summary(section, model);
                r.push(sp(1));
                r
            },
        ))
        .with_hit(PanelHit::OpenSection(section));
        if on {
            srow = srow.with_bg(Tok::Slot(S::Bg1));
        }
        out.push(srow);

        if on && (plan.content_rows > 0 || plan.overflow.is_some()) {
            out.push(PanelRow::blank());
            let granted = plan.content_rows;
            let (shown, more) = match plan.overflow {
                Some(hidden) => (granted.saturating_sub(1), Some(hidden)),
                None => (granted, None),
            };
            for row in content_raw.iter().take(shown) {
                let mut r = row.clone();
                r.line = indent(r.line);
                // The row-mode cursor: highlight the actionable row the panel
                // is parked on so Down/Up visibly walk the section's items.
                // A row that already carries a background (the expanded change
                // preview) keeps its stronger accent.
                if focused && r.bg.is_none() && r.hit == Some(PanelHit::Row(ui.open, ui.cursor)) {
                    r = r.with_bg(Tok::SelAccent);
                }
                out.push(r);
            }
            if let Some(hidden) = more {
                out.push(
                    PanelRow::plain(Line::segs(vec![
                        sp(4),
                        seg(Tok::Slot(S::Ghost2), format!("… +{hidden} more · e expand")),
                    ]))
                    .with_hit(PanelHit::Expand),
                );
            }
            out.push(PanelRow::blank());
        } else if plan.airy {
            out.push(PanelRow::blank());
        }
    }

    out.truncate(rows);
    PanelFrame {
        rows: out,
        rail: Vec::new(),
        tab_spans,
    }
}

/// The full view's horizontal rail: `1 changes · 2 git · …` wrapped to
/// `cols`, the open section accented. Returns the rows plus the rail-relative
/// hit spans (`build_full` absolutizes the row indices).
/// Only sections belonging to the active tab are shown.
fn rail_rows_and_spans(ui: &PanelUi, cols: usize) -> (Vec<PanelRow>, Vec<RailSpan>) {
    let mut rows: Vec<PanelRow> = Vec::new();
    let mut spans: Vec<RailSpan> = Vec::new();
    let mut cur: Vec<Seg> = vec![sp(1)];
    let mut x = 1usize;
    let visible: Vec<_> = ui
        .order
        .iter()
        .copied()
        .filter(|s| s.tab() == ui.tab)
        .collect();
    for (i, section) in visible.iter().copied().enumerate() {
        let on = section == ui.open;
        let num = format!("{}", i + 1);
        let label = section.label();
        let entry_w = num.chars().count() + 1 + label.chars().count();
        let sep_w = if x > 1 { 3 } else { 0 };
        if x + sep_w + entry_w > cols.saturating_sub(1) && x > 1 {
            rows.push(PanelRow::plain(Line::Segs(std::mem::take(&mut cur))));
            cur.push(sp(1));
            x = 1;
        } else if sep_w > 0 {
            cur.push(seg(Tok::Slot(S::Ghost3), " · "));
            x += 3;
        }
        spans.push(RailSpan {
            row: rows.len(),
            cols: x..x + entry_w,
            section,
        });
        cur.push(
            seg(
                Tok::Slot(if on { S::Accent } else { S::Ghost2 }),
                num.clone(),
            )
            .bold(),
        );
        cur.push(sp(1));
        let mut label_seg = seg(Tok::Slot(if on { S::Accent } else { S::Dim }), label);
        if on {
            label_seg = label_seg.bold();
        }
        cur.push(label_seg);
        x += entry_w;
    }
    if cur.len() > 1 {
        rows.push(PanelRow::plain(Line::Segs(cur)));
    }
    (rows, spans)
}

/// Apply the row-mode cursor highlight (shared by both frame layouts).
fn cursor_tint(mut row: PanelRow, focused: bool, ui: &PanelUi) -> PanelRow {
    if focused && row.bg.is_none() && row.hit == Some(PanelHit::Row(ui.open, ui.cursor)) {
        row = row.with_bg(Tok::SelAccent);
    }
    row
}

/// The full-width layout: tab bar, header, the horizontal rail, a rule seam,
/// then the open section's body filling every remaining row (bodies that
/// scroll read `ui.scroll` themselves; overflowing list bodies truncate to a
/// "+N more").
fn build_full(
    model: &FrameModel,
    ui: &PanelUi,
    cols: usize,
    rows: usize,
    focused: bool,
) -> PanelFrame {
    // Reserve 1 row for the tab bar.
    let (tab_row, tab_spans) = tab_bar_row(ui, focused);
    let rows_for_body = rows.saturating_sub(1);

    let header = with_flow_chips(
        header_rows(model, focused, cols),
        ui,
        model.panel.merge.is_some(),
    );
    let (rail, mut spans) = rail_rows_and_spans(ui, cols);
    let plan = budget::allocate_full(rows_for_body, header.len(), rail.len());

    let mut out: Vec<PanelRow> = Vec::new();
    out.push(tab_row);
    out.extend(header.into_iter().take(plan.header_rows));
    let rail_y0 = out.len();
    let kept = plan.rail_rows.min(rail.len());
    out.extend(rail.into_iter().take(kept));
    spans.retain(|s| s.row < kept);
    for s in &mut spans {
        s.row += rail_y0;
    }
    if plan.seam {
        out.push(PanelRow::plain(Line::Fill {
            ch: '─',
            fg: Tok::Slot(S::Ghost3),
        }));
        out.push(PanelRow::blank());
    }

    let ctx = SectionCtx {
        model,
        ui,
        cols: cols.saturating_sub(2),
        rows: plan.body_rows,
    };
    let content = sections::content(ui.open, &ctx);
    let pad_one = |line: Line| match line {
        Line::Segs(mut v) => {
            v.insert(0, sp(1));
            Line::Segs(v)
        }
        Line::Split { mut l, mut r } => {
            l.insert(0, sp(1));
            if !r.is_empty() {
                r.push(sp(1));
            }
            Line::Split { l, r }
        }
        other => other,
    };
    let push_row = |out: &mut Vec<PanelRow>, row: PanelRow| {
        let mut r = cursor_tint(row, focused, ui);
        r.line = pad_one(r.line);
        out.push(r);
    };
    if content.len() > plan.body_rows {
        let shown = plan.body_rows.saturating_sub(1);
        let hidden = content.len() - shown;
        for row in content.into_iter().take(shown) {
            push_row(&mut out, row);
        }
        if plan.body_rows > 0 {
            out.push(PanelRow::plain(Line::segs(vec![
                sp(1),
                seg(Tok::Slot(S::Ghost2), format!("… +{hidden} more")),
            ])));
        }
    } else {
        for row in content.into_iter() {
            push_row(&mut out, row);
        }
    }
    out.truncate(rows);
    PanelFrame {
        rows: out,
        rail: spans,
        tab_spans,
    }
}

/// The actionable (cursor-targetable) row count for an arbitrary section —
/// `Row(section, i)` hits in display order. Used to flow the cursor across
/// section boundaries and to skip empty accordions.
pub fn section_rows(
    section: crate::panel::Section,
    model: &FrameModel,
    ui: &PanelUi,
    cols: usize,
    rows: usize,
) -> usize {
    // Subtract 1 for the tab bar row in the row estimate.
    let ctx = section_ctx(model, ui, cols, rows.saturating_sub(1), 4);
    sections::content(section, &ctx)
        .iter()
        .filter(|r| matches!(r.hit, Some(PanelHit::Row(_, _))))
        .count()
}

/// The actionable row count for the open section (row-mode cursor targets).
pub fn actionable_rows(model: &FrameModel, ui: &PanelUi, cols: usize, rows: usize) -> usize {
    section_rows(ui.open, model, ui, cols, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::{ChangeRow, MergeBanner, PanelData, Section, Stage};

    fn model_with(panel: PanelData) -> FrameModel {
        FrameModel {
            panel,
            ..Default::default()
        }
    }

    fn change(path: &str, stage: Stage) -> ChangeRow {
        let (dir, name) = match path.rsplit_once('/') {
            Some((d, n)) => (format!("{d}/"), n.to_string()),
            None => (String::new(), path.to_string()),
        };
        ChangeRow {
            status: "M".into(),
            stage,
            dir,
            name,
            path: path.into(),
            added: 10,
            deleted: 2,
            incoming: false,
        }
    }

    fn text_of(line: &Line) -> String {
        let segs = |v: &[Seg]| v.iter().map(|s| s.text.clone()).collect::<String>();
        match line {
            Line::Blank => String::new(),
            Line::Fill { ch, .. } => ch.to_string(),
            Line::Segs(v) => segs(v),
            Line::Split { l, r } => format!("{}|{}", segs(l), segs(r)),
        }
    }

    #[test]
    fn frame_lists_all_sections_with_open_content() {
        let model = model_with(PanelData {
            branch: "main".into(),
            changes: vec![change("src/a.rs", Stage::Staged)],
            ..Default::default()
        });
        let ui = PanelUi::default(); // open = Changes, tab = Git
        let frame = build_panel(&model, &ui, 44, 50, true);
        let texts: Vec<String> = frame.rows.iter().map(|r| text_of(&r.line)).collect();
        let all = texts.join("\n");
        // Only Git-tab sections appear (Changes, Commits, Branches, Stash, Files).
        let git_secs = ui.tab_sections();
        for s in &git_secs {
            assert!(all.contains(s.label()), "{} missing:\n{all}", s.label());
        }
        // Work/System sections are not in the Git tab accordion.
        assert!(!all.contains("notifications"), "{all}");
        assert!(!all.contains("logs"), "{all}");
        // The open section's content (the change row) renders indented.
        assert!(all.contains("a.rs"), "{all}");
        // The footer is gone — no "more, one keystroke away" strip.
        assert!(!all.contains("MORE, ONE KEYSTROKE AWAY"), "{all}");
        // Tab bar row is present.
        assert!(all.contains("git"), "{all}");
        assert!(all.contains("work"), "{all}");
        assert!(all.contains("system"), "{all}");
        // Every accordion within the tab is numbered (the number is its jump shortcut).
        for n in 1..=git_secs.len() {
            assert!(
                all.contains(&format!("{n}")),
                "missing accordion number {n}: {all}"
            );
        }
        // Section rows carry their hits (one per tab-visible section).
        let hits: Vec<&PanelHit> = frame.rows.iter().filter_map(|r| r.hit.as_ref()).collect();
        assert_eq!(
            hits.iter()
                .filter(|h| matches!(h, PanelHit::OpenSection(_)))
                .count(),
            git_secs.len()
        );
        assert!(frame.rows.len() <= 50);
    }

    #[test]
    fn frame_renders_only_the_configured_order() {
        let model = model_with(PanelData {
            branch: "main".into(),
            changes: vec![change("src/a.rs", Stage::Staged)],
            ..Default::default()
        });
        // Git tab with only Changes visible; Pr is Work tab so it's invisible
        // in the Git accordion even if it's in order.
        let ui = PanelUi {
            order: vec![Section::Pr, Section::Changes],
            ..Default::default() // tab = Git
        };
        let frame = build_panel(&model, &ui, 44, 40, false);
        let sections: Vec<Section> = frame
            .rows
            .iter()
            .filter_map(|r| match r.hit {
                Some(PanelHit::OpenSection(s)) => Some(s),
                _ => None,
            })
            .collect();
        // Only Changes appears in the Git tab (Pr is Work tab).
        assert_eq!(sections, vec![Section::Changes]);
        let all: String = frame
            .rows
            .iter()
            .map(|r| text_of(&r.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all.contains("telemetry"), "{all}");
        assert!(!all.contains("tests"), "{all}");
    }

    #[test]
    fn merge_banner_renders_chip_conflicts_and_bar() {
        let model = model_with(PanelData {
            branch: "main".into(),
            ahead_behind: Some((3, 1)),
            merge: Some(MergeBanner {
                label: "MERGING".into(),
                onto: "origin/main".into(),
                unresolved: 2,
                total: Some(6),
            }),
            changes: vec![change("web/wp-config.php", Stage::Conflict)],
            ..Default::default()
        });
        let frame = build_panel(&model, &PanelUi::default(), 44, 50, false);
        let all: String = frame
            .rows
            .iter()
            .map(|r| text_of(&r.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains(" MERGING "), "{all}");
        assert!(all.contains("⇡3"), "{all}");
        assert!(all.contains("⇣1"), "{all}");
        assert!(all.contains("2 conflicts"), "{all}");
        assert!(all.contains("4"), "resolved 4/6: {all}");
        assert!(all.contains("/6"), "{all}");
    }

    // Regression: a merge in progress populates BOTH the hydrated header banner
    // (model.panel.merge) and the UI flow (ui.git.flow == Merge). Both used to
    // paint an amber MERGING chip on the same header row ("merging twice"). The
    // richer banner wins; the redundant flow chip is suppressed.
    #[test]
    fn merge_banner_and_flow_show_only_one_merging_chip() {
        use crate::panel::gitui::{GitFlow, SequencerUi};
        let model = model_with(PanelData {
            branch: "main".into(),
            merge: Some(MergeBanner {
                label: "MERGING".into(),
                onto: "feature".into(),
                unresolved: 1,
                total: Some(3),
            }),
            ..Default::default()
        });
        let mut ui = PanelUi::default();
        ui.git.flow = GitFlow::Merge(SequencerUi {
            onto: "feature".into(),
            conflict: true,
        });
        let frame = build_panel(&model, &ui, 44, 50, false);
        let all: String = frame
            .rows
            .iter()
            .map(|r| text_of(&r.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            all.matches("MERGING").count(),
            1,
            "exactly one MERGING chip expected, got:\n{all}"
        );
    }

    // Zero unresolved conflicts means the merge is ready to commit — a good
    // state — so the count reads green, not the alarming red used mid-conflict.
    #[test]
    fn merge_zero_conflicts_reads_green_not_red() {
        let model = model_with(PanelData {
            branch: "main".into(),
            merge: Some(MergeBanner {
                label: "MERGING".into(),
                onto: "feature".into(),
                unresolved: 0,
                total: Some(2),
            }),
            ..Default::default()
        });
        let frame = build_panel(&model, &PanelUi::default(), 44, 50, false);
        let conf = frame
            .rows
            .iter()
            .find_map(|r| match &r.line {
                Line::Segs(v) => v.iter().find(|s| s.text == "no conflicts"),
                _ => None,
            })
            .expect("expected a 'no conflicts' segment");
        assert_eq!(
            conf.fg,
            Tok::Hue(Hue::Green),
            "zero conflicts must be green"
        );
    }

    // The conflict count is the actionable bit; a long onto branch name must
    // never push it off the row. The branch name clips with an ellipsis instead.
    #[test]
    fn merge_line_clips_branch_not_conflict_count() {
        let model = model_with(PanelData {
            branch: "main".into(),
            merge: Some(MergeBanner {
                label: "MERGING".into(),
                onto: "fix/worktree-pane-mirror".into(),
                unresolved: 2,
                total: Some(6),
            }),
            ..Default::default()
        });
        let frame = build_panel(&model, &PanelUi::default(), 44, 50, false);
        let all: String = frame
            .rows
            .iter()
            .map(|r| text_of(&r.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("2 conflicts"), "count must survive: {all}");
        assert!(all.contains('…'), "branch name should clip: {all}");
        assert!(
            !all.contains("fix/worktree-pane-mirror"),
            "full branch must be clipped: {all}"
        );
    }

    #[test]
    fn overflow_adds_the_expand_row() {
        let many: Vec<ChangeRow> = (0..40)
            .map(|i| change(&format!("src/f{i:02}.rs"), Stage::Unstaged))
            .collect();
        let model = model_with(PanelData {
            branch: "main".into(),
            changes: many,
            ..Default::default()
        });
        let frame = build_panel(&model, &PanelUi::default(), 44, 30, false);
        let all: String = frame
            .rows
            .iter()
            .map(|r| text_of(&r.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("more · e expand"), "{all}");
        assert!(
            frame.rows.iter().any(|r| r.hit == Some(PanelHit::Expand)),
            "{all}"
        );
        assert!(frame.rows.len() <= 30);
    }

    #[test]
    fn tiny_panel_never_overflows_height() {
        let model = model_with(PanelData {
            branch: "main".into(),
            changes: vec![change("a.rs", Stage::Staged)],
            ..Default::default()
        });
        for rows in [0usize, 3, 7, 10, 14, 19, 24] {
            let frame = build_panel(&model, &PanelUi::default(), 44, rows, false);
            assert!(frame.rows.len() <= rows, "rows={rows}");
        }
    }

    #[test]
    fn open_section_row_is_tinted_and_selected_change_inlines_preview() {
        let model = model_with(PanelData {
            branch: "main".into(),
            changes: vec![change("src/a.rs", Stage::Unstaged)],
            ..Default::default()
        });
        let ui = PanelUi {
            chg_sel: Some(0),
            row_mode: true,
            ..Default::default() // tab = Git, open = Changes
        };
        let frame = build_panel(&model, &ui, 44, 50, true);
        // The open section row carries the tint bg.
        let open_row = frame
            .rows
            .iter()
            .find(|r| r.hit == Some(PanelHit::OpenSection(Section::Changes)))
            .unwrap();
        assert_eq!(open_row.bg, Some(Tok::Slot(S::Bg1)));
        // The selected change row is tinted and the preview placeholder shows
        // (no hunks fetched in this test).
        let all: String = frame
            .rows
            .iter()
            .map(|r| text_of(&r.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("loading hunks"), "{all}");
        let sel = frame
            .rows
            .iter()
            .find(|r| r.hit == Some(PanelHit::Row(Section::Changes, 0)))
            .unwrap();
        assert_eq!(sel.bg, Some(Tok::SelAccent));
    }

    #[test]
    fn cursor_on_impact_footer_tints_the_row() {
        use thegn_core::semantic::{EntityChange, EntityKind, EntitySummary, Touch};
        let mut model = model_with(PanelData {
            branch: "main".into(),
            changes: vec![change("src/a.rs", Stage::Unstaged)],
            ..Default::default()
        });
        model.panel.entities = Some(EntitySummary::new(vec![(
            "src/a.rs".into(),
            vec![EntityChange {
                kind: EntityKind::Function,
                name: "f".into(),
                added: 1,
                deleted: 0,
                touch: Touch::Added,
                start_line: 1,
            }],
        )]));
        // Cursor one past the single change row lands on the impact footer.
        let ui = PanelUi {
            row_mode: true,
            cursor: model.panel.changes.len(),
            ..Default::default()
        };
        let frame = build_panel(&model, &ui, 44, 50, true);
        let footer = frame
            .rows
            .iter()
            .find(|r| r.hit == Some(PanelHit::Row(Section::Changes, model.panel.changes.len())))
            .expect("impact footer row present");
        assert_eq!(footer.bg, Some(Tok::SelAccent));
    }
}
