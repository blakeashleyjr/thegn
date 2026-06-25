//! The lazygit-grade Full-width git frame: when the panel is at
//! [`PanelWidth::Full`](crate::layout::PanelWidth) and the open section is
//! git-family, the whole body becomes a multi-region layout — the existing
//! frame header, a STATUS strip (branch, divergence, flow chips, the pending
//! spinner), a side column of the four git lists (FILES · BRANCHES · COMMITS
//! · STASH, the focused one getting the lion's share of rows), a
//! focus-dependent main region across a vertical `│` seam, and a context help
//! bar fed by the same `gitui::context_keys` tables that drive dispatch.
//!
//! Pure view-model: fixed `PanelData` + `GitUi` in, `PanelFrame` out. Side
//! list rows carry [`PanelHit::Row`] with their home [`Section`] so the
//! existing mouse/cursor machinery works unchanged; list headers carry
//! [`PanelHit::OpenSection`].

use superzej_core::rebase_todo::TodoAction;
use superzej_core::theme::Hue;
use superzej_core::util::age;
use superzej_core::viz;

use crate::chrome::{FrameModel, S};
use crate::seg::{self, Line, Seg, Tok, seg, sp};

use super::frame::PanelFrame;
use super::gitui::{GitFlow, GitView, RebaseUi, context_keys};
use super::sections::{PanelRow, filter_row, filtered_indices};
use super::{PanelData, PanelHit, PanelUi, Section, Stage, graph, staging};

/// Side column width including the vertical divider cell.
const SIDE_W: usize = 34;

// Token shorthands (local copies — the sections ones are private).
fn t() -> Tok {
    Tok::Slot(S::Text)
}
fn d() -> Tok {
    Tok::Slot(S::Dim)
}
fn f() -> Tok {
    Tok::Slot(S::Faint)
}
fn g() -> Tok {
    Tok::Slot(S::Ghost)
}
fn g2() -> Tok {
    Tok::Slot(S::Ghost2)
}
fn g3() -> Tok {
    Tok::Slot(S::Ghost3)
}
fn ac() -> Tok {
    Tok::Slot(S::Accent)
}
fn hue(h: Hue) -> Tok {
    Tok::Hue(h)
}

/// The non-cursor selection tint (same as the staging renderer's).
fn range_tint() -> Tok {
    Tok::Sel(Hue::Blue, 10)
}

/// The loading line for a region whose document fetch is still out (the
/// sections spinner pattern, copied locally).
fn spinner(tick: u64, what: &str) -> PanelRow {
    PanelRow::plain(Line::segs(vec![
        seg(ac(), viz::spin(tick).to_string()),
        seg(d(), format!(" loading {what}…")),
    ]))
}

/// The staging-document seam: the loop's generation-tagged fetches store
/// the live doc on `GitUi` (`stage_doc` for the working-tree diff,
/// `patch_doc` for a drilled commit's file); regions read whichever the
/// focus calls for and show a spinner while the fetch is out.
fn stage_doc(ui: &PanelUi) -> Option<&crate::panel::gitui::StageDocState> {
    match ui.git.focus {
        GitView::PatchBuilding => ui.git.patch_doc.as_ref(),
        _ => ui.git.stage_doc.as_ref(),
    }
}

/// Map a (possibly drilled) focus to the side list that anchors it.
fn home_list(focus: GitView) -> GitView {
    match focus {
        GitView::Files | GitView::Staging => GitView::Files,
        GitView::Branches => GitView::Branches,
        GitView::Stash => GitView::Stash,
        _ => GitView::Commits,
    }
}

/// Display-ordered source indices of a side list under its live filter.
fn list_indices(ui: &PanelUi, data: &PanelData, view: GitView) -> Vec<usize> {
    match view {
        GitView::Files => filtered_indices(ui, view, data.changes.len(), |i| {
            data.changes[i].path.clone()
        }),
        GitView::Branches => filtered_indices(ui, view, data.branches.len(), |i| {
            data.branches[i].name.clone()
        }),
        GitView::Commits => filtered_indices(ui, view, data.commits.len(), |i| {
            format!("{} {}", data.commits[i].short, data.commits[i].subject)
        }),
        GitView::Stash => filtered_indices(ui, view, data.stashes.len(), |i| {
            data.stashes[i].message.clone()
        }),
        _ => Vec::new(),
    }
}

/// A scroll window keeping `cursor` visible within `avail` rows.
fn window(len: usize, avail: usize, cursor: usize) -> std::ops::Range<usize> {
    if avail == 0 || len == 0 {
        return 0..0;
    }
    if len <= avail {
        return 0..len;
    }
    let cursor = cursor.min(len - 1);
    let skip = (cursor + 1).saturating_sub(avail).min(len - avail);
    skip..skip + avail
}

fn short7(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The commits mark gutter (mirrors `graph::mark`, which is private).
fn commit_mark(sha: &str, ui: &PanelUi) -> Seg {
    if ui.git.clipboard.iter().any(|s| s == sha) {
        seg(hue(Hue::Teal), "❐")
    } else if ui.git.mark_base.as_deref() == Some(sha) {
        seg(hue(Hue::Magenta), "▶")
    } else if ui.git.diff_mark.as_deref() == Some(sha) {
        seg(hue(Hue::Blue), "◈")
    } else {
        sp(1)
    }
}

// ---- status region ----------------------------------------------------------

/// Flow chips for the status row: `REBASING`, `MERGING`, `CHERRY-PICK`,
/// `REVERTING`, `BISECT g/c`, `DIFF vs x`, `PATCH n lines`.
fn flow_chips(flow: &GitFlow, merge_banner: bool) -> Vec<Seg> {
    match flow {
        GitFlow::None => Vec::new(),
        GitFlow::Rebase(_) => vec![sp(1), Seg::chip(hue(Hue::Amber), " REBASING ")],
        // The frame header already carries the richer MERGING banner (from the
        // hydrated MERGE_HEAD state); repeating it in the STATUS strip painted a
        // second amber MERGING box. Suppress the strip chip when the banner shows.
        GitFlow::Merge(_) if merge_banner => Vec::new(),
        GitFlow::Merge(s) => {
            let label = if s.conflict {
                " MERGING ⚑ "
            } else {
                " MERGING "
            };
            vec![sp(1), Seg::chip(hue(Hue::Amber), label)]
        }
        GitFlow::CherryPick(s) => {
            let label = if s.conflict {
                " CHERRY-PICK ⚑ "
            } else {
                " CHERRY-PICK "
            };
            vec![sp(1), Seg::chip(hue(Hue::Purple), label)]
        }
        GitFlow::Revert(s) => {
            let label = if s.conflict {
                " REVERTING ⚑ "
            } else {
                " REVERTING "
            };
            vec![sp(1), Seg::chip(hue(Hue::Purple), label)]
        }
        GitFlow::Bisect(b) => vec![
            sp(1),
            Seg::chip(
                hue(Hue::Amber),
                format!(
                    " BISECT {}/{} ",
                    b.good,
                    b.culprit
                        .as_deref()
                        .map(short7)
                        .unwrap_or_else(|| "?".into())
                ),
            ),
        ],
        GitFlow::Patch(p) => vec![
            sp(1),
            Seg::chip(hue(Hue::Purple), format!(" PATCH {} lines ", p.marked())),
        ],
        GitFlow::Diffing(r) => vec![sp(1), Seg::chip(hue(Hue::Blue), format!(" DIFF vs {r} "))],
    }
}

/// The `1 STATUS` strip — ONE row (the frame header above already carries
/// the branch + divergence; repeating them was dead weight): the numbered
/// label, the live flow chips, and the pending spinner on the right.
fn status_rows(model: &FrameModel, ui: &PanelUi, focused: bool) -> Vec<PanelRow> {
    let data = &model.panel;
    let _ = focused;
    let mut right: Vec<Seg> = Vec::new();
    if let Some(p) = &ui.git.pending {
        right.push(seg(ac(), viz::spin(ui.docs.tick).to_string()));
        right.push(seg(d(), format!(" {}…", p.label)));
        right.push(sp(1));
    } else {
        right.push(seg(g2(), format!("{} stashed · ", data.stash_count)));
        right.push(seg(g2(), format!("{} changed", data.changes.len())));
        right.push(sp(1));
    }
    let mut l = vec![sp(1), seg(g2(), "1").bold(), sp(1), seg(d(), "STATUS")];
    l.extend(flow_chips(&ui.git.flow, data.merge.is_some()));
    vec![PanelRow::plain(Line::split(l, right))]
}

// ---- side column ------------------------------------------------------------

/// One side-list item row (no tint/hit — the caller attaches those).
fn item_row(data: &PanelData, view: GitView, src: usize) -> PanelRow {
    let segs = match view {
        GitView::Files => {
            let c = &data.changes[src];
            let tok = match c.stage {
                Stage::Staged => hue(Hue::Green),
                Stage::Conflict => hue(Hue::Red),
                Stage::Untracked => g(),
                Stage::Unstaged => hue(Hue::Amber),
            };
            vec![
                sp(1),
                seg(tok, format!("{:>2}", c.status)),
                sp(1),
                // Path prefix is a label, not scaffolding — `faint`, not the
                // `ghost2` floor (see the changes section).
                seg(f(), c.dir.clone()),
                seg(d(), c.name.clone()),
            ]
        }
        GitView::Branches => {
            let b = &data.branches[src];
            let mut v = vec![
                sp(1),
                if b.is_head { seg(ac(), "*") } else { sp(1) },
                sp(1),
                seg(if b.is_head { t() } else { d() }, b.name.clone()),
            ];
            if b.ahead > 0 {
                v.push(seg(hue(Hue::Green), format!(" ⇡{}", b.ahead)));
            }
            if b.behind > 0 {
                v.push(seg(hue(Hue::Red), format!(" ⇣{}", b.behind)));
            }
            if b.pr.is_some() {
                v.push(seg(hue(Hue::Green), " ⬤"));
            }
            v
        }
        GitView::Stash => {
            let s = &data.stashes[src];
            vec![
                sp(1),
                seg(ac(), format!("{}", s.index)),
                sp(1),
                seg(d(), s.message.clone()),
            ]
        }
        _ => Vec::new(),
    };
    PanelRow::plain(Line::Segs(segs))
}

/// Split `body` rows across the four lists. Every list needs its header;
/// beyond that the focused list is satisfied first, then the others in
/// usefulness order (commits, branches, files, stash) — so a short focused
/// list (a clean tree, an empty stash) hands its slack to the commit log
/// instead of leaving the column blank.
fn allocate_side(needs: &[usize; 4], focus: usize, body: usize) -> [usize; 4] {
    // Floor: header + up to 2 items each.
    let mut alloc = [0usize; 4];
    for i in 0..4 {
        alloc[i] = needs[i].min(3);
    }
    let mut used: usize = alloc.iter().sum();
    if used > body {
        // Tight: shed from the bottom of the stack (stash first), down to
        // bare headers, then drop headers entirely.
        for floor in [1usize, 0] {
            for i in (0..4).rev() {
                while alloc[i] > floor && used > body {
                    alloc[i] -= 1;
                    used -= 1;
                }
            }
        }
        return alloc;
    }
    // Grow toward each list's full need: focused first, then by usefulness.
    let order = [focus, 2, 1, 0, 3];
    let mut seen = [false; 4];
    for i in order {
        if seen[i] {
            continue;
        }
        seen[i] = true;
        let grow = (needs[i].saturating_sub(alloc[i])).min(body - used);
        alloc[i] += grow;
        used += grow;
        if used == body {
            break;
        }
    }
    alloc
}

/// The four stacked side lists: `[n] NAME` headers (accent when focused),
/// items with `PanelHit::Row(section, display_idx)`, cursor/range tints on
/// the focused list, and [`allocate_side`] spreading the column's full
/// height across them.
fn side_rows(model: &FrameModel, ui: &PanelUi, focused: bool, body: usize) -> Vec<PanelRow> {
    let data = &model.panel;
    let lists: [(GitView, Section, &str); 4] = [
        (GitView::Files, Section::Changes, "FILES"),
        (GitView::Branches, Section::Branches, "BRANCHES"),
        (GitView::Commits, Section::Commits, "COMMITS"),
        (GitView::Stash, Section::Stash, "STASH"),
    ];
    let focus_list = home_list(ui.git.focus);
    let focus_pos = lists
        .iter()
        .position(|(v, _, _)| *v == focus_list)
        .unwrap_or(2);
    let indices: Vec<Vec<usize>> = lists
        .iter()
        .map(|(v, _, _)| list_indices(ui, data, *v))
        .collect();
    // Breathing room between lists when the column is tall enough.
    let gaps = if body >= 20 { 3 } else { 0 };
    let needs: [usize; 4] = std::array::from_fn(|i| {
        let mut n = indices[i].len() + 1;
        if lists[i].0 == focus_list && ui.git.filter.as_ref().is_some_and(|f| f.view == focus_list)
        {
            n += 1;
        }
        n
    });
    let allocs = allocate_side(&needs, focus_pos, body.saturating_sub(gaps));

    let mut out: Vec<PanelRow> = Vec::new();
    for (li, (view, section, name)) in lists.iter().enumerate() {
        let ix = &indices[li];
        let is_focus = *view == focus_list;
        let alloc = allocs[li];
        if alloc == 0 {
            continue;
        }
        if gaps > 0 && li > 0 {
            out.push(PanelRow::blank());
        }
        let mut label = seg(if is_focus { ac() } else { d() }, *name);
        if is_focus {
            label = label.bold();
        }
        let mut header = vec![
            sp(1),
            seg(if is_focus { ac() } else { g2() }, format!("{}", li + 2)).bold(),
            sp(1),
            label,
        ];
        if !ix.is_empty() {
            let visible = alloc.saturating_sub(1);
            header.push(seg(
                g3(),
                if ix.len() > visible {
                    format!(" {}/{}", visible.min(ix.len()), ix.len())
                } else {
                    format!(" {}", ix.len())
                },
            ));
        }
        out.push(PanelRow::plain(Line::segs(header)).with_hit(PanelHit::OpenSection(*section)));
        let mut shown = alloc.saturating_sub(1);
        if is_focus && let Some(fr) = filter_row(ui, *view, ix.len()) {
            out.push(fr);
            shown = shown.saturating_sub(1);
        }
        let cursor = ui.git.cur.get(*view).min(ix.len().saturating_sub(1));
        let on_view = ui.git.focus == *view;
        for di in window(ix.len(), shown, if is_focus { cursor } else { 0 }) {
            let mut row = if *view == GitView::Commits {
                // The commits item gets its live mark gutter here.
                let c = &data.commits[ix[di]];
                PanelRow::plain(Line::segs(vec![
                    sp(1),
                    commit_mark(&c.sha, ui),
                    sp(1),
                    seg(ac(), c.short.clone()),
                    sp(1),
                    seg(d(), c.subject.clone()),
                ]))
            } else {
                item_row(data, *view, ix[di])
            }
            .with_hit(PanelHit::Row(*section, di));
            if is_focus && di == cursor {
                row = row.with_bg(if focused && on_view {
                    Tok::SelAccent
                } else {
                    range_tint()
                });
            } else if is_focus
                && on_view
                && ui.git.sel_anchor.is_some()
                && ui.git.selection().contains(&di)
            {
                row = row.with_bg(range_tint());
            }
            out.push(row);
        }
    }
    out
}

// ---- main region ------------------------------------------------------------

/// The staging main region: the live diff doc (cursor-windowed), a loading
/// spinner while the fetch is out, or — outside the drill — the selected
/// file's summary plus the "enter to stage lines" hint.
fn staging_region(
    data: &PanelData,
    ui: &PanelUi,
    focused: bool,
    w: usize,
    avail: usize,
) -> Vec<PanelRow> {
    match (&ui.git.staging, stage_doc(ui)) {
        (Some(st), Some(state)) if state.path == st.path && state.pane == st.pane => {
            let pane = match st.pane {
                crate::panel::gitui::StagePane::Unstaged => "UNSTAGED",
                crate::panel::gitui::StagePane::Staged => "STAGED",
            };
            let mut out = vec![PanelRow::plain(Line::segs(vec![
                seg(ac(), st.path.clone()).bold(),
                seg(g2(), format!(" — {pane}")),
                seg(g3(), " · tab to switch pane"),
            ]))];
            let opts = staging::RenderOpts {
                cursor: st.cursor,
                sel: st.anchor.map(|_| st.selection()),
                marks: None,
                focused,
                cols: w,
            };
            let (rows, cur) = staging::rows(&state.doc, &opts);
            let win = window(rows.len(), avail.saturating_sub(out.len()), cur);
            out.extend(rows[win].iter().cloned());
            out
        }
        (Some(st), _) => vec![
            PanelRow::plain(Line::segs(vec![seg(ac(), st.path.clone()).bold()])),
            spinner(ui.docs.tick, "diff"),
        ],
        (None, _) => {
            let ix = list_indices(ui, data, GitView::Files);
            let cur = ui
                .git
                .cur
                .get(GitView::Files)
                .min(ix.len().saturating_sub(1));
            match ix.get(cur).map(|&s| &data.changes[s]) {
                Some(c) => vec![
                    PanelRow::plain(Line::segs(vec![
                        seg(d(), c.status.clone()),
                        sp(1),
                        seg(t(), c.path.clone()).bold(),
                        sp(2),
                        seg(hue(Hue::Green), format!("+{}", c.added)),
                        sp(1),
                        seg(hue(Hue::Red), format!("−{}", c.deleted)),
                    ])),
                    PanelRow::blank(),
                    PanelRow::plain(Line::segs(vec![seg(g2(), "enter to stage lines")])),
                ],
                // A clean tree has nothing to stage — the commit graph is a
                // far better use of the main region than one dim line.
                None => {
                    let mut out = vec![
                        PanelRow::plain(Line::segs(vec![seg(g2(), "working tree clean")])),
                        PanelRow::blank(),
                    ];
                    out.extend(commits_region(data, ui, w, avail.saturating_sub(out.len())));
                    out
                }
            }
        }
    }
}

/// The commits main region: the lane graph with marks, the selected commit
/// tinted, and a `DIFF vs <ref>` banner while diffing.
fn commits_region(data: &PanelData, ui: &PanelUi, w: usize, avail: usize) -> Vec<PanelRow> {
    let mut out: Vec<PanelRow> = Vec::new();
    if let GitFlow::Diffing(r) = &ui.git.flow {
        out.push(PanelRow::plain(Line::segs(vec![Seg::chip(
            hue(Hue::Blue),
            format!(" DIFF vs {r} "),
        )])));
    }
    let ix = list_indices(ui, data, GitView::Commits);
    let cur = ui
        .git
        .cur
        .get(GitView::Commits)
        .min(ix.len().saturating_sub(1));
    let sel_src = ix.get(cur).copied();
    let layout = graph::layout(&data.commits);
    let marks = graph::GraphMarks {
        copied: &ui.git.clipboard,
        base: ui.git.mark_base.as_deref(),
        diff_mark: ui.git.diff_mark.as_deref(),
    };
    let lines = graph::rows(&data.commits, &layout, &marks, sel_src, w);
    let win = window(
        lines.len(),
        avail.saturating_sub(out.len()),
        sel_src.unwrap_or(0),
    );
    out.extend(lines[win].iter().cloned().map(PanelRow::plain));
    out
}

/// The branches main region: a cheap linear commit list titled with the
/// selected branch.
fn branches_region(data: &PanelData, ui: &PanelUi, avail: usize) -> Vec<PanelRow> {
    let ix = list_indices(ui, data, GitView::Branches);
    let cur = ui
        .git
        .cur
        .get(GitView::Branches)
        .min(ix.len().saturating_sub(1));
    let title = ix
        .get(cur)
        .map(|&s| data.branches[s].name.clone())
        .unwrap_or_else(|| data.branch.clone());
    let mut out = vec![PanelRow::plain(Line::segs(vec![
        seg(ac(), title).bold(),
        seg(g2(), " — recent commits"),
    ]))];
    for c in data.commits.iter().take(avail.saturating_sub(1)) {
        out.push(PanelRow::plain(Line::split(
            vec![
                seg(ac(), c.short.clone()),
                sp(1),
                seg(d(), c.subject.clone()),
            ],
            vec![seg(g2(), age(c.date))],
        )));
    }
    out
}

/// The stash main region: the selected stash's message + age + a hint.
fn stash_region(data: &PanelData, ui: &PanelUi) -> Vec<PanelRow> {
    let ix = list_indices(ui, data, GitView::Stash);
    let cur = ui
        .git
        .cur
        .get(GitView::Stash)
        .min(ix.len().saturating_sub(1));
    match ix.get(cur).map(|&s| &data.stashes[s]) {
        Some(s) => vec![
            PanelRow::plain(Line::segs(vec![
                seg(ac(), format!("stash@{{{}}}", s.index)).bold(),
                sp(2),
                seg(g2(), age(s.date)),
            ])),
            PanelRow::plain(Line::segs(vec![seg(d(), s.message.clone())])),
            PanelRow::blank(),
            PanelRow::plain(Line::segs(vec![seg(
                g2(),
                "enter diff · space apply · p pop · d drop",
            )])),
        ],
        None => vec![PanelRow::plain(Line::segs(vec![seg(g2(), "no stashes")]))],
    }
}

/// The drilled commit's FILE LIST (`enter` on a commit): one row per changed
/// path with its diffstat, the cursor row tinted.
fn commit_files_region(ui: &PanelUi, focused: bool, avail: usize) -> Vec<PanelRow> {
    let mut out: Vec<PanelRow> = Vec::new();
    if let Some(sha) = &ui.git.drilled_commit {
        out.push(PanelRow::plain(Line::segs(vec![
            seg(ac(), short7(sha)).bold(),
            seg(g2(), format!(" — {} file(s)", ui.git.commit_files.len())),
            seg(g3(), " · enter to build a patch"),
        ])));
    }
    if ui.git.commit_files.is_empty() {
        out.push(spinner(ui.docs.tick, "commit files"));
        return out;
    }
    let cursor = ui
        .git
        .cur
        .get(GitView::CommitFiles)
        .min(ui.git.commit_files.len().saturating_sub(1));
    let win = window(
        ui.git.commit_files.len(),
        avail.saturating_sub(out.len()),
        cursor,
    );
    for i in win {
        let (path, added, deleted) = &ui.git.commit_files[i];
        let mut row = PanelRow::plain(Line::split(
            vec![sp(1), seg(d(), path.clone())],
            vec![
                seg(hue(Hue::Green), format!("+{added}")),
                sp(1),
                seg(hue(Hue::Red), format!("−{deleted}")),
                sp(1),
            ],
        ));
        if i == cursor {
            row = row.with_bg(if focused {
                Tok::SelAccent
            } else {
                range_tint()
            });
        }
        out.push(row);
    }
    out
}

/// The patch-building region: one file of the drilled commit's diff with the
/// custom-patch marks (`◆`) and the line cursor.
fn patch_region(ui: &PanelUi, focused: bool, w: usize, avail: usize) -> Vec<PanelRow> {
    let mut out: Vec<PanelRow> = Vec::new();
    if let Some(sha) = &ui.git.drilled_commit {
        let marked = match &ui.git.flow {
            GitFlow::Patch(p) => p.marked(),
            _ => 0,
        };
        out.push(PanelRow::plain(Line::segs(vec![
            seg(ac(), short7(sha)).bold(),
            seg(g2(), " — mark lines for the patch"),
            seg(hue(Hue::Purple), format!(" ◆{marked}")),
            seg(g3(), " · C-p patch menu"),
        ])));
    }
    match stage_doc(ui) {
        Some(state) => {
            let st = ui.git.staging.as_ref();
            let marks = match &ui.git.flow {
                GitFlow::Patch(p) => p.marks.get(&p.path),
                _ => None,
            };
            let opts = staging::RenderOpts {
                cursor: st.map(|s| s.cursor).unwrap_or(0),
                sel: st.and_then(|s| s.anchor.map(|_| s.selection())),
                marks,
                focused,
                cols: w,
            };
            let (rows, cur) = staging::rows(&state.doc, &opts);
            let win = window(rows.len(), avail.saturating_sub(out.len()), cur);
            out.extend(rows[win].iter().cloned());
        }
        None => out.push(spinner(ui.docs.tick, "patch")),
    }
    out
}

/// The action chip of a rebase-todo entry: text + color.
fn todo_chip(a: &TodoAction) -> (&'static str, Tok) {
    match a {
        TodoAction::Pick => ("pick", g()),
        TodoAction::Squash => ("squash", hue(Hue::Amber)),
        TodoAction::Fixup | TodoAction::FixupC => ("fixup", hue(Hue::Amber)),
        TodoAction::Drop => ("drop", hue(Hue::Red)),
        TodoAction::Edit => ("edit", hue(Hue::Blue)),
        TodoAction::Reword => ("reword", hue(Hue::Blue)),
        TodoAction::Break => ("break", g2()),
        TodoAction::Exec(_) => ("exec", g2()),
        TodoAction::Label(_) => ("label", g2()),
        TodoAction::Reset(_) => ("reset", g2()),
        TodoAction::Merge(_) => ("merge", g2()),
        TodoAction::UpdateRef(_) => ("update-ref", g2()),
        TodoAction::Noop => ("noop", g2()),
        TodoAction::Unknown(_) => ("?", g2()),
    }
}

/// The interactive-rebase TODO editor rows: action chip + short sha +
/// subject (struck for drops), the cursor row tinted, and an amber conflict
/// banner while the running rebase is stopped.
fn rebase_rows(r: &RebaseUi, focused: bool, avail: usize) -> Vec<PanelRow> {
    let mut out: Vec<PanelRow> = Vec::new();
    if r.conflict {
        out.push(PanelRow::plain(Line::segs(vec![
            seg(hue(Hue::Amber), "resolve conflicts, then m → continue").bold(),
        ])));
    }
    if r.running {
        // The live-progress line: what already ran, where git stopped, and
        // whether the editor below reflects the on-disk todo yet (edits are
        // locked until the live read lands).
        let mut segs = vec![seg(
            g2(),
            format!("{} done · {} pending", r.done, r.todos.len()),
        )];
        if let Some(sha) = &r.stopped_sha {
            segs.push(seg(g2(), " · stopped at "));
            segs.push(seg(ac(), short7(sha)));
        }
        if !r.todos_synced {
            segs.push(seg(hue(Hue::Amber), " · syncing live todo…"));
        }
        out.push(PanelRow::plain(Line::segs(segs)));
    }
    let win = window(r.todos.len(), avail.saturating_sub(out.len()), r.cursor);
    for i in win {
        let e = &r.todos[i];
        let (name, tok) = todo_chip(&e.action);
        let dropped = matches!(e.action, TodoAction::Drop);
        let mut subject = seg(d(), e.subject.clone());
        if dropped {
            subject = subject.strike();
        }
        let mut row = PanelRow::plain(Line::segs(vec![
            seg(tok, format!("{name:<7}")).bold(),
            seg(ac(), short7(&e.sha)),
            sp(1),
            subject,
        ]));
        if i == r.cursor {
            row = row.with_bg(if focused {
                Tok::SelAccent
            } else {
                range_tint()
            });
        }
        out.push(row);
    }
    out
}

/// Dispatch the main region off the git focus (a running rebase, or the TODO
/// editor itself, owns it regardless of which list anchors the focus).
fn main_rows(
    model: &FrameModel,
    ui: &PanelUi,
    focused: bool,
    w: usize,
    avail: usize,
) -> Vec<PanelRow> {
    let data = &model.panel;
    if let GitFlow::Rebase(r) = &ui.git.flow
        && (ui.git.focus == GitView::RebaseTodo || r.running)
    {
        return rebase_rows(r, focused, avail);
    }
    match ui.git.focus {
        GitView::Files | GitView::Staging => staging_region(data, ui, focused, w, avail),
        GitView::Commits => commits_region(data, ui, w, avail),
        GitView::CommitFiles => commit_files_region(ui, focused, avail),
        GitView::PatchBuilding => patch_region(ui, focused, w, avail),
        GitView::Branches => branches_region(data, ui, avail),
        GitView::Stash => stash_region(data, ui),
        GitView::RebaseTodo => vec![PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "no rebase in progress",
        )]))],
        GitView::Blame => blame_region(ui, focused, avail),
    }
}

/// Annotated blame: one row per source line, coloured by author, with age and
/// SHA prefix on the left. The cursor row gets the accent tint.
fn blame_region(ui: &PanelUi, focused: bool, avail: usize) -> Vec<PanelRow> {
    if ui.git.blame_rows.is_empty() {
        return if ui.git.blame_path.is_some() {
            vec![spinner(0, "blame")]
        } else {
            vec![PanelRow::plain(Line::segs(vec![seg(
                g2(),
                "no file selected",
            )]))]
        };
    }
    let rows = &ui.git.blame_rows;
    let cur = ui.git.blame_cursor.min(rows.len().saturating_sub(1));
    let win = window(rows.len(), avail, cur);
    let win_start = win.start;
    rows[win.clone()]
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let src_i = win_start + i;
            let is_cur = focused && src_i == cur;
            // FNV1a hash of the author name → deterministic colour per author.
            let author_hue = {
                let mut h: u32 = 0x811c9dc5;
                for b in r.author.bytes() {
                    h ^= u32::from(b);
                    h = h.wrapping_mul(0x01000193);
                }
                BLAME_HUES[(h as usize) % BLAME_HUES.len()]
            };
            let short_sha = if r.sha.len() >= 7 {
                &r.sha[..7]
            } else {
                &r.sha
            };
            let lineno_s = format!("{:>5}", r.lineno);
            let age_s = age(r.date);
            let segs = vec![
                seg(
                    if is_cur { ac() } else { hue(author_hue) },
                    short_sha.to_string(),
                ),
                sp(1),
                seg(g2(), age_s),
                sp(1),
                seg(d(), lineno_s),
                sp(1),
                seg(if is_cur { t() } else { d() }, r.content.clone()),
            ];
            let mut row = PanelRow::plain(Line::segs(segs));
            if is_cur {
                row = row.with_bg(if focused {
                    Tok::SelAccent
                } else {
                    range_tint()
                });
            }
            row
        })
        .collect()
}

/// Hue palette for blame author colouring. Cycles across authors by name hash.
const BLAME_HUES: [Hue; 8] = Hue::ALL;

// ---- composition ------------------------------------------------------------

/// Flatten a line spec into segs fitted (cut, not padded) to `w` cells.
fn flatten(line: &Line, w: usize) -> Vec<Seg> {
    match line {
        Line::Blank => Vec::new(),
        Line::Fill { ch, fg } => vec![seg(*fg, ch.to_string().repeat(w))],
        Line::Segs(v) => seg::cut(v, w),
        Line::Split { l, r } => {
            let r2 = seg::cut(r, w);
            let rw = seg::seg_width(&r2);
            let avail = w.saturating_sub(rw + usize::from(rw > 0));
            let mut out = seg::cut(l, avail);
            let lw = seg::seg_width(&out);
            out.push(sp(w.saturating_sub(lw + rw)));
            out.extend(r2);
            out
        }
    }
}

/// Apply a row background per-seg (padding to `w` first) so a side tint never
/// bleeds across the divider into the main region (and vice versa).
fn tint_segs(mut segs: Vec<Seg>, w: usize, bg: Option<Tok>) -> Vec<Seg> {
    if let Some(b) = bg {
        let used = seg::seg_width(&segs);
        if w > used {
            segs.push(sp(w - used));
        }
        for s in &mut segs {
            if s.bg.is_none() {
                s.bg = Some(b);
            }
        }
    }
    segs
}

/// Zip the side and main columns into single rows across a `│` seam; the
/// fused row inherits the side row's hit target.
fn fuse(
    side: &[PanelRow],
    main: &[PanelRow],
    body: usize,
    side_w: usize,
    main_w: usize,
) -> Vec<PanelRow> {
    let blank = PanelRow::blank();
    (0..body)
        .map(|i| {
            let s = side.get(i).unwrap_or(&blank);
            let m = main.get(i).unwrap_or(&blank);
            let mut segs = tint_segs(flatten(&s.line, side_w), side_w, s.bg);
            let used = seg::seg_width(&segs);
            segs.push(sp(side_w.saturating_sub(used)));
            segs.push(seg(g3(), "│"));
            segs.extend(tint_segs(flatten(&m.line, main_w), main_w, m.bg));
            let mut row = PanelRow::plain(Line::Segs(segs));
            if let Some(h) = s.hit {
                row = row.with_hit(h);
            }
            row
        })
        .collect()
}

/// One dim help row from the focused context's key table (the same data that
/// drives dispatch): `chord label · chord label …`.
fn help_row(view: GitView) -> PanelRow {
    let mut segs: Vec<Seg> = vec![sp(1)];
    for (i, ck) in context_keys(view).iter().take(8).enumerate() {
        if i > 0 {
            segs.push(seg(g3(), " · "));
        }
        segs.push(seg(d(), ck.chord));
        segs.push(seg(g2(), format!(" {}", ck.label)));
    }
    PanelRow::plain(Line::Segs(segs))
}

/// Assemble the Full git frame for a `cols` × `rows` rect: frame header,
/// STATUS strip, the side│main column zone, and the context help bar.
pub(super) fn build_git_full(
    model: &FrameModel,
    ui: &PanelUi,
    cols: usize,
    rows: usize,
    focused: bool,
) -> PanelFrame {
    let header = super::frame::header_rows(model, focused);
    let mut out: Vec<PanelRow> = Vec::new();
    let keep = if rows >= 16 {
        header.len()
    } else {
        header.len().min(1)
    };
    out.extend(header.into_iter().take(keep));
    out.extend(status_rows(model, ui, focused));
    let body = rows.saturating_sub(out.len() + 1);
    if body > 0 {
        let side_w = SIDE_W.saturating_sub(1).min(cols.saturating_sub(2));
        let main_w = cols.saturating_sub(side_w + 1);
        let side = side_rows(model, ui, focused, body);
        let main = main_rows(model, ui, focused, main_w, body);
        out.extend(fuse(&side, &main, body, side_w, main_w));
        out.push(help_row(ui.git.focus));
    }
    out.truncate(rows);
    PanelFrame {
        rows: out,
        rail: Vec::new(),
        tab_spans: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PanelWidth;
    use crate::panel::gitui::{PendingOp, StagingUi};
    use crate::panel::{BranchRow, ChangeRow, CommitRow, StashRow};
    use superzej_core::rebase_todo::TodoEntry;

    fn change(path: &str) -> ChangeRow {
        let (dir, name) = match path.rsplit_once('/') {
            Some((d, n)) => (format!("{d}/"), n.to_string()),
            None => (String::new(), path.to_string()),
        };
        ChangeRow {
            status: "M".into(),
            stage: Stage::Unstaged,
            dir,
            name,
            path: path.into(),
            added: 5,
            deleted: 1,
        }
    }

    fn commit(i: usize) -> CommitRow {
        CommitRow {
            sha: format!("sha{i:02}aaaaaaaaaa"),
            short: format!("sha{i:02}aa"),
            subject: format!("feat: change {i}"),
            author: "Blake Ashley".into(),
            date: 1_700_000_000,
            refs: String::new(),
            parents: vec![format!("sha{:02}aaaaaaaaaa", i + 1)],
        }
    }

    fn model() -> FrameModel {
        let mut m = FrameModel::default();
        m.panel.branch = "feat/full".into();
        m.panel.ahead_behind = Some((2, 1));
        m.panel.changes = vec![change("src/a.rs"), change("src/b.rs")];
        m.panel.branches = vec![
            BranchRow {
                name: "feat/full".into(),
                is_head: true,
                upstream: Some("origin/feat/full".into()),
                ahead: 2,
                behind: 1,
                upstream_gone: false,
                sha: "sha00aaaaaaaaaa".into(),
                date: 1_700_000_000,
                subject: "feat: change 0".into(),
                pr: Some(crate::panel::PrBadge {
                    number: 7,
                    state: "OPEN".into(),
                    is_draft: false,
                    url: "https://github.com/o/r/pull/7".into(),
                }),
            },
            BranchRow {
                name: "main".into(),
                is_head: false,
                upstream: Some("origin/main".into()),
                ahead: 0,
                behind: 0,
                upstream_gone: false,
                sha: "sha07aaaaaaaaaa".into(),
                date: 1_700_000_000,
                subject: "feat: change 7".into(),
                pr: None,
            },
        ];
        m.panel.commits = (0..8).map(commit).collect();
        m.panel.stashes = vec![StashRow {
            index: 0,
            sha: "sha09aaaaaaaaaa".into(),
            date: 1_700_000_000,
            message: "WIP on main: tinkering".into(),
        }];
        m
    }

    fn ui(focus: GitView) -> PanelUi {
        let mut u = PanelUi {
            open: Section::Commits,
            width: PanelWidth::Full,
            row_mode: true,
            ..Default::default()
        };
        u.git.focus = focus;
        u
    }

    #[test]
    fn side_allocation_fills_the_column_and_slack_flows_to_commits() {
        // needs: files 3, branches 3, commits 9, stash 2 (header + items).
        let needs = [3usize, 3, 9, 2];
        // Focused FILES is short: its full need is honored and the slack
        // beyond every list's need would remain; commits absorbs the rest.
        let alloc = allocate_side(&needs, 0, 17);
        assert_eq!(alloc, [3, 3, 9, 2], "all needs fit exactly");
        // Tighter: floor everyone, satisfy focus (files), grow commits with
        // what remains.
        let alloc = allocate_side(&needs, 0, 12);
        assert_eq!(alloc[0], 3, "focused need met");
        assert_eq!(alloc[3], 2, "stash kept its floor");
        assert!(alloc[2] > 3, "commits absorbed the slack: {alloc:?}");
        assert_eq!(alloc.iter().sum::<usize>(), 12, "column filled");
        // Very tight: shed from the bottom of the stack toward bare headers.
        let alloc = allocate_side(&needs, 0, 6);
        assert_eq!(alloc.iter().sum::<usize>(), 6);
        assert!(alloc.iter().all(|&a| a >= 1), "headers survive: {alloc:?}");
        // A huge focused commits list eats everything beyond the floors.
        let alloc = allocate_side(&[3, 3, 60, 2], 2, 30);
        assert_eq!(alloc[2], 30 - 3 - 3 - 2, "{alloc:?}");
    }

    #[test]
    fn clean_tree_files_focus_falls_back_to_the_commit_graph() {
        let mut m = model();
        m.panel.changes.clear();
        let u = ui(GitView::Files);
        let frame = build_git_full(&m, &u, 120, 40, true);
        let all = all_text(&frame);
        assert!(all.contains("working tree clean"), "{all}");
        // The main region carries the commit graph instead of dead space.
        assert!(all.contains("feat: change 7"), "graph rendered: {all}");
        // And the side column still shows every list with its count badge.
        assert!(all.contains("COMMITS 8"), "{all}");
        assert!(all.contains("BRANCHES 2"), "{all}");
    }

    // Regression: with both the hydrated header banner and the UI merge flow
    // active, the Full frame must show exactly one amber MERGING chip — the
    // header banner — not also repeat it in the STATUS strip ("merging twice").
    #[test]
    fn full_frame_shows_one_merging_chip_when_banner_and_flow_active() {
        use crate::panel::MergeBanner;
        use crate::panel::gitui::SequencerUi;
        let mut m = model();
        m.panel.merge = Some(MergeBanner {
            label: "MERGING".into(),
            onto: "main".into(),
            unresolved: 1,
            total: Some(2),
        });
        let mut u = ui(GitView::Files);
        u.git.flow = GitFlow::Merge(SequencerUi {
            onto: "main".into(),
            conflict: true,
        });
        let frame = build_git_full(&m, &u, 120, 40, true);
        let all = all_text(&frame);
        assert_eq!(
            all.matches("MERGING").count(),
            1,
            "exactly one MERGING chip expected, got:\n{all}"
        );
    }

    #[test]
    fn status_strip_is_one_row_and_does_not_repeat_the_branch() {
        let m = model();
        let u = ui(GitView::Files);
        let frame = build_git_full(&m, &u, 120, 40, true);
        // The branch shows in the frame header and the BRANCHES list row —
        // and nowhere else (the old STATUS strip repeated it on a third row).
        let branch_rows = frame
            .rows
            .iter()
            .filter(|r| match &r.line {
                Line::Segs(v) | Line::Split { l: v, .. } => v.iter().any(|s| s.text == "feat/full"),
                _ => false,
            })
            .count();
        assert_eq!(branch_rows, 2, "header + branches list only");
        // The STATUS row itself carries chips/summary, not the branch.
        let status_row = frame
            .rows
            .iter()
            .find_map(|r| match &r.line {
                Line::Segs(v) | Line::Split { l: v, .. }
                    if v.iter().any(|s| s.text == "STATUS") =>
                {
                    Some(v.iter().map(|s| s.text.clone()).collect::<String>())
                }
                _ => None,
            })
            .expect("status row rendered");
        assert!(!status_row.contains("feat/full"), "{status_row}");
    }

    fn segs_text(v: &[Seg]) -> String {
        v.iter().map(|s| s.text.clone()).collect()
    }

    fn text_of(line: &Line) -> String {
        match line {
            Line::Blank => String::new(),
            Line::Fill { ch, .. } => ch.to_string(),
            Line::Segs(v) => segs_text(v),
            Line::Split { l, r } => format!("{}|{}", segs_text(l), segs_text(r)),
        }
    }

    fn all_text(frame: &PanelFrame) -> String {
        frame
            .rows
            .iter()
            .map(|r| text_of(&r.line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn all_five_region_headers_render_and_fit() {
        let m = model();
        let frame = build_git_full(&m, &ui(GitView::Commits), 120, 40, true);
        let all = all_text(&frame);
        for name in ["STATUS", "FILES", "BRANCHES", "COMMITS", "STASH"] {
            assert!(all.contains(name), "{name} missing:\n{all}");
        }
        assert!(frame.rows.len() <= 40);
        // Branch + divergence ride the status strip.
        assert!(all.contains("feat/full"), "{all}");
        assert!(all.contains("⇡2"), "{all}");
        assert!(all.contains("⇣1"), "{all}");
        // List headers carry section hits for the mouse machinery.
        for s in [
            Section::Changes,
            Section::Branches,
            Section::Commits,
            Section::Stash,
        ] {
            assert!(
                frame
                    .rows
                    .iter()
                    .any(|r| r.hit == Some(PanelHit::OpenSection(s))),
                "{s:?} header hit missing"
            );
        }
    }

    #[test]
    fn focused_list_gets_accent_header_and_biggest_allocation() {
        let m = model();
        let frame = build_git_full(&m, &ui(GitView::Commits), 120, 40, true);
        // The COMMITS header label is accented; a dim list's isn't.
        let find_label = |name: &str| {
            frame
                .rows
                .iter()
                .find_map(|r| match &r.line {
                    Line::Segs(v) => v.iter().find(|s| s.text == name).cloned(),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} header not found"))
        };
        assert_eq!(find_label("COMMITS").fg, Tok::Slot(S::Accent));
        assert!(find_label("COMMITS").bold);
        assert_eq!(find_label("FILES").fg, Tok::Slot(S::Dim));
        // The focused list shows more rows than any dim one.
        let count = |s: Section| {
            frame
                .rows
                .iter()
                .filter(|r| matches!(r.hit, Some(PanelHit::Row(sec, _)) if sec == s))
                .count()
        };
        let commits = count(Section::Commits);
        assert!(commits > count(Section::Changes), "{commits}");
        assert!(commits > count(Section::Branches), "{commits}");
        assert!(commits > count(Section::Stash), "{commits}");
        assert_eq!(commits, m.panel.commits.len()); // all 8 fit when focused
    }

    #[test]
    fn commits_cursor_row_carries_hit_and_accent_tint() {
        let m = model();
        let mut u = ui(GitView::Commits);
        u.git.cur.set(GitView::Commits, 2);
        let frame = build_git_full(&m, &u, 120, 40, true);
        let row = frame
            .rows
            .iter()
            .find(|r| r.hit == Some(PanelHit::Row(Section::Commits, 2)))
            .expect("cursor row hit missing");
        // The tint is per-seg (so it never bleeds across the divider).
        let Line::Segs(v) = &row.line else {
            panic!("fused rows are segs")
        };
        assert!(
            v.iter().any(|s| s.bg == Some(Tok::SelAccent)),
            "no SelAccent seg on the cursor row"
        );
        // The divider seam renders between the columns.
        assert!(v.iter().any(|s| s.text == "│"));
    }

    #[test]
    fn rebase_flow_renders_todo_chips_and_conflict_banner() {
        use superzej_core::rebase_todo::TodoAction;
        let m = model();
        let mut u = ui(GitView::RebaseTodo);
        u.git.flow = GitFlow::Rebase(RebaseUi {
            base: "main".into(),
            todos: vec![
                TodoEntry {
                    action: TodoAction::Pick,
                    sha: "sha00aaaaaaaaaa".into(),
                    subject: "feat: change 0".into(),
                },
                TodoEntry {
                    action: TodoAction::Squash,
                    sha: "sha01aaaaaaaaaa".into(),
                    subject: "feat: change 1".into(),
                },
                TodoEntry {
                    action: TodoAction::Drop,
                    sha: "sha02aaaaaaaaaa".into(),
                    subject: "feat: change 2".into(),
                },
            ],
            cursor: 1,
            running: true,
            conflict: true,
            todos_synced: true,
            done: 2,
            stopped_sha: Some("sha01aaaaaaaaaa".into()),
            ..Default::default()
        });
        let frame = build_git_full(&m, &u, 120, 40, true);
        let all = all_text(&frame);
        assert!(all.contains("pick"), "{all}");
        assert!(all.contains("squash"), "{all}");
        assert!(all.contains("drop"), "{all}");
        assert!(
            all.contains("resolve conflicts, then m → continue"),
            "{all}"
        );
        assert!(all.contains("REBASING"), "{all}");
        // The live-progress line: done/pending counts + stop point; no
        // "syncing" notice once the live read landed.
        assert!(all.contains("2 done · 3 pending"), "{all}");
        assert!(all.contains("stopped at sha01aa"), "{all}");
        assert!(!all.contains("syncing live todo"), "{all}");
        // An unsynced editor surfaces the lock.
        if let GitFlow::Rebase(r) = &mut u.git.flow {
            r.todos_synced = false;
        }
        let unsynced = all_text(&build_git_full(&m, &u, 120, 40, true));
        assert!(unsynced.contains("syncing live todo"), "{unsynced}");
        // The dropped subject is struck; the cursor todo is tinted.
        let seg_named = |name: &str| {
            frame
                .rows
                .iter()
                .find_map(|r| match &r.line {
                    Line::Segs(v) => v.iter().find(|s| s.text == name).cloned(),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} not rendered"))
        };
        assert!(seg_named("feat: change 2").strike);
        assert_eq!(seg_named("feat: change 1").bg, Some(Tok::SelAccent));
    }

    #[test]
    fn pending_op_renders_its_label_and_flow_chips_show() {
        let m = model();
        let mut u = ui(GitView::Commits);
        u.git.pending = Some(PendingOp {
            label: "rebasing".into(),
        });
        u.git.flow = GitFlow::Diffing("main".into());
        let frame = build_git_full(&m, &u, 120, 40, true);
        let all = all_text(&frame);
        assert!(all.contains("rebasing…"), "{all}");
        // The Diffing flow shows both the status chip and the main banner.
        assert!(all.contains("DIFF vs main"), "{all}");
    }

    #[test]
    fn help_row_shows_the_focused_contexts_first_chords() {
        let m = model();
        let frame = build_git_full(&m, &ui(GitView::Files), 120, 40, true);
        let all = all_text(&frame);
        // Files context: space stage · enter diff · …
        assert!(all.contains("space stage"), "{all}");
        assert!(all.contains("enter diff"), "{all}");
        // The Files focus main region shows the selected file's summary.
        assert!(all.contains("enter to stage lines"), "{all}");
        assert!(all.contains("src/a.rs"), "{all}");
        // A staging drill without a doc yet shows the loading spinner.
        let mut u = ui(GitView::Staging);
        u.git.staging = Some(StagingUi::new("src/a.rs"));
        let all = all_text(&build_git_full(&m, &u, 120, 40, true));
        assert!(all.contains("loading diff…"), "{all}");
    }

    #[test]
    fn small_geometries_never_panic_or_overflow() {
        let m = model();
        for focus in [
            GitView::Files,
            GitView::Branches,
            GitView::Commits,
            GitView::Stash,
            GitView::Staging,
            GitView::CommitFiles,
            GitView::PatchBuilding,
            GitView::RebaseTodo,
        ] {
            for (cols, rows) in [(40, 8), (0, 0), (10, 3), (34, 5), (80, 20)] {
                let frame = build_git_full(&m, &ui(focus), cols, rows, true);
                assert!(frame.rows.len() <= rows, "{focus:?} {cols}x{rows}");
            }
        }
        // An empty data model is fine too.
        let empty = FrameModel::default();
        let frame = build_git_full(&empty, &ui(GitView::Commits), 120, 40, false);
        assert!(frame.rows.len() <= 40);
    }

    #[test]
    fn build_panel_dispatches_git_family_full_into_this_layout() {
        let m = model();
        let u = ui(GitView::Commits); // open = Commits, width = Full
        let frame = super::super::frame::build_panel(&m, &u, 120, 40, true);
        let all = all_text(&frame);
        assert!(all.contains("STATUS"), "{all}");
        assert!(all.contains("STASH"), "{all}");
        // A non-git section at Full keeps the rail layout (no STATUS strip).
        let mut keys = ui(GitView::Commits);
        keys.open = Section::Keys;
        let frame = super::super::frame::build_panel(&m, &keys, 120, 40, true);
        assert!(!all_text(&frame).contains("STATUS"));
    }
}
