//! The commit graph for the Full git frame: lane assignment (the classic
//! active-lanes walk over `git log` parents, newest first) plus seg-line
//! rendering with a lane-colored glyph gutter and author-colored entries.
//!
//! Pure — no I/O, no git; callers feed [`crate::panel::CommitRow`]s.
//!
//! Glyph conventions (the tests encode these):
//! - `●` the commit's own lane (`◉` when the commit carries refs);
//! - `│` any other active lane passing through the row;
//! - `┘` a lane closing into this row's commit (a merge join);
//! - ` ` an inactive interior column (trailing blanks are trimmed).
//!
//! Lanes are unbounded in the model but clamped to [`MAX_LANES`] (8) columns
//! at render time: overflow columns collapse into the last one, and an
//! overflowing commit glyph wins that cell. Lane color is
//! `HUE_CYCLE[lane % 8]` — a stable cycle over the eight palette hues.
#![allow(dead_code)] // wired into the Full git frame by a concurrent change

use superzej_core::theme::Hue;
use superzej_core::util::age;

use crate::chrome::S;
use crate::panel::CommitRow;
use crate::panel::sections::commits::author_hue;
use crate::seg::{Line, Seg, Tok, seg, sp};

/// Rendered lane-column cap; wider lanes collapse into the last column.
pub const MAX_LANES: usize = 8;

/// Stable lane → hue cycle (all eight real palette hues).
pub const HUE_CYCLE: [Hue; 8] = [
    Hue::Teal,
    Hue::Magenta,
    Hue::Purple,
    Hue::Green,
    Hue::Amber,
    Hue::Red,
    Hue::Blue,
    Hue::Orange,
];

fn hue(h: Hue) -> Tok {
    Tok::Hue(h)
}

/// Initials for the author column ("Blake Ashley" → "BA") — same style as the
/// commits section.
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// One commit's resolved graph row: its lane, the glyph cells to paint
/// before the text, and pass-through display fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    pub sha: String,
    pub lane: usize,
    /// One colored glyph per lane column: (char, lane_color_index).
    pub cells: Vec<(char, usize)>,
}

/// Assign lanes to commits (newest first, as `log` returns them) using the
/// standard active-lanes algorithm: a `Vec<Option<String>>` holds the
/// "expected sha" per lane. A commit takes the first lane expecting its sha
/// (or the first free lane when none does — a new branch tip); every other
/// lane expecting it closes into it (a `┘` join), the commit's lane is
/// re-seeded with its first parent, and extra parents (a merge commit) open
/// new lanes seeded with them — unless a lane already expects that parent.
pub fn layout(commits: &[CommitRow]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut out: Vec<GraphRow> = Vec::with_capacity(commits.len());
    for c in commits {
        // Every lane expecting this commit; the first is its home.
        let hits: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| (l.as_deref() == Some(c.sha.as_str())).then_some(i))
            .collect();
        let lane = match hits.first() {
            Some(&i) => i,
            None => match lanes.iter().position(Option::is_none) {
                Some(i) => i,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };
        // Cells reflect this row's state, before parents re-seed the lanes.
        let glyph = if c.refs.is_empty() { '●' } else { '◉' };
        let mut cells: Vec<(char, usize)> = (0..lanes.len())
            .map(|i| {
                let ch = if i == lane {
                    glyph
                } else if hits.contains(&i) {
                    '┘'
                } else if lanes[i].is_some() {
                    '│'
                } else {
                    ' '
                };
                (ch, i)
            })
            .collect();
        while cells.last().is_some_and(|(ch, _)| *ch == ' ') {
            cells.pop();
        }
        // Merging lanes close; the commit's lane continues as its first
        // parent (or closes for a root commit).
        for &h in hits.iter().skip(1) {
            lanes[h] = None;
        }
        lanes[lane] = c.parents.first().cloned();
        // Extra parents open new lanes unless one already expects them.
        for p in c.parents.iter().skip(1) {
            if lanes.iter().flatten().any(|s| s == p) {
                continue;
            }
            match lanes.iter().position(Option::is_none) {
                Some(i) => lanes[i] = Some(p.clone()),
                None => lanes.push(Some(p.clone())),
            }
        }
        while lanes.last().is_some_and(|l| l.is_none()) {
            lanes.pop();
        }
        out.push(GraphRow {
            sha: c.sha.clone(),
            lane,
            cells,
        });
    }
    out
}

/// The lazygit-style gutter marks, matched by full sha.
pub struct GraphMarks<'a> {
    /// Cherry-pick clipboard (`❐`, teal).
    pub copied: &'a [String],
    /// Rebase/range base (`▶`, magenta).
    pub base: Option<&'a str>,
    /// Diff mark (`◈`, blue).
    pub diff_mark: Option<&'a str>,
}

/// The one-cell mark gutter, mirroring the commits section: copied ❐ teal,
/// base ▶ magenta, diff ◈ blue, else a space.
fn mark(c: &CommitRow, marks: &GraphMarks) -> Seg {
    if marks.copied.iter().any(|s| s == &c.sha) {
        seg(hue(Hue::Teal), "❐")
    } else if marks.base == Some(c.sha.as_str()) {
        seg(hue(Hue::Magenta), "▶")
    } else if marks.diff_mark == Some(c.sha.as_str()) {
        seg(hue(Hue::Blue), "◈")
    } else {
        sp(1)
    }
}

/// Clamp a row's cells to [`MAX_LANES`] columns; an overflowing commit glyph
/// collapses into the last column (keeping its own lane color).
fn clamp_cells(cells: &[(char, usize)]) -> Vec<(char, usize)> {
    if cells.len() <= MAX_LANES {
        return cells.to_vec();
    }
    let overflow_commit = cells[MAX_LANES..]
        .iter()
        .find(|(ch, _)| *ch == '●' || *ch == '◉')
        .copied();
    let mut out = cells[..MAX_LANES].to_vec();
    if let Some(cell) = overflow_commit {
        out[MAX_LANES - 1] = cell;
    }
    out
}

/// Render commits + their graph rows into seg lines for the Full git frame:
/// `[mark][graph cells][short sha] [subject] [(refs)] … [author initials] [age]`.
/// Graph cells are lane-colored (`HUE_CYCLE[lane % 8]`), the gutter is padded
/// to the batch's widest (clamped) row so the text columns align, the author
/// initials take [`author_hue`], and the `selected` display row renders its
/// sha + subject bold on the accent selection tint. The left cluster is
/// pre-cut to `cols` (draw-time `Line::Split` fitting still applies); small
/// or zero `cols` never panic.
pub fn rows(
    commits: &[CommitRow],
    graph: &[GraphRow],
    marks: &GraphMarks,
    selected: Option<usize>,
    cols: usize,
) -> Vec<Line> {
    let gutter = graph
        .iter()
        .map(|g| g.cells.len().min(MAX_LANES))
        .max()
        .unwrap_or(1)
        .max(1);
    commits
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let sel = selected == Some(i);
            let mut l: Vec<Seg> = vec![mark(c, marks)];
            let cells = graph
                .get(i)
                .map(|g| clamp_cells(&g.cells))
                .unwrap_or_default();
            let used = cells.len();
            for (ch, lane_idx) in cells {
                l.push(seg(
                    hue(HUE_CYCLE[lane_idx % HUE_CYCLE.len()]),
                    ch.to_string(),
                ));
            }
            if gutter > used {
                l.push(sp(gutter - used));
            }
            l.push(sp(1));
            let (sha, subject) = if sel {
                (
                    seg(Tok::Slot(S::Accent), c.short.clone())
                        .bold()
                        .bg(Tok::SelAccent),
                    seg(Tok::Slot(S::Text), c.subject.clone())
                        .bold()
                        .bg(Tok::SelAccent),
                )
            } else {
                (
                    seg(Tok::Slot(S::Accent), c.short.clone()),
                    seg(Tok::Slot(S::Dim), c.subject.clone()),
                )
            };
            l.push(sha);
            l.push(sp(1));
            l.push(subject);
            if !c.refs.is_empty() {
                l.push(sp(1));
                l.push(seg(hue(Hue::Amber), format!("({})", c.refs)));
            }
            let r = vec![
                seg(hue(author_hue(&c.author)), initials(&c.author)),
                sp(1),
                seg(Tok::Slot(S::Ghost2), age(c.date)),
            ];
            Line::split(crate::seg::cut(&l, cols), r)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cr(sha: &str, parents: &[&str], refs: &str) -> CommitRow {
        CommitRow {
            sha: sha.into(),
            short: sha.chars().take(7).collect(),
            subject: format!("subject {sha}"),
            author: "Blake Ashley".into(),
            date: 1_700_000_000,
            refs: refs.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn split(line: &Line) -> (&[Seg], &[Seg]) {
        match line {
            Line::Split { l, r } => (l, r),
            other => panic!("expected a split line, got {other:?}"),
        }
    }

    #[test]
    fn layout_linear_history_stays_in_lane_zero() {
        let commits = vec![
            cr("c3", &["c2"], ""),
            cr("c2", &["c1"], ""),
            cr("c1", &[], ""),
        ];
        let g = layout(&commits);
        assert_eq!(g.len(), 3);
        for (row, c) in g.iter().zip(&commits) {
            assert_eq!(row.sha, c.sha);
            assert_eq!(row.lane, 0);
            assert_eq!(row.cells, vec![('●', 0)]);
        }
    }

    #[test]
    fn layout_branch_and_merge_opens_then_closes_lane_one() {
        // git log topological order: the merge first, then both sides
        // (first-parent side `a` newer), then the shared base.
        let commits = vec![
            cr("m", &["a", "b"], ""),
            cr("a", &["base"], ""),
            cr("b", &["base"], ""),
            cr("base", &[], ""),
        ];
        let g = layout(&commits);
        let lanes: Vec<usize> = g.iter().map(|r| r.lane).collect();
        assert_eq!(lanes, vec![0, 0, 1, 0]);
        // The merge row predates lane 1 (its second parent opens it below).
        assert_eq!(g[0].cells, vec![('●', 0)]);
        // Both sides ride two active lanes.
        assert_eq!(g[1].cells, vec![('●', 0), ('│', 1)]);
        assert_eq!(g[2].cells, vec![('│', 0), ('●', 1)]);
        // The base merges lane 1 back in: a close glyph.
        assert_eq!(g[3].cells, vec![('●', 0), ('┘', 1)]);
        assert!(g.iter().any(|r| r.cells.len() == 2));
    }

    #[test]
    fn layout_orphan_tip_takes_a_free_lane() {
        let commits = vec![cr("a", &["b"], ""), cr("x", &["y"], ""), cr("b", &[], "")];
        let g = layout(&commits);
        assert_eq!(g[0].lane, 0);
        // Nobody expects `x`: lane 0 is busy waiting for `b`, so it opens 1.
        assert_eq!(g[1].lane, 1);
        assert_eq!(g[1].cells, vec![('│', 0), ('●', 1)]);
        assert_eq!(g[2].lane, 0);
        assert_eq!(g[2].cells, vec![('●', 0), ('│', 1)]);
    }

    #[test]
    fn layout_refs_row_uses_ringed_glyph() {
        let commits = vec![cr("c2", &["c1"], "main, origin/main"), cr("c1", &[], "")];
        let g = layout(&commits);
        assert_eq!(g[0].cells, vec![('◉', 0)]);
        assert_eq!(g[1].cells, vec![('●', 0)]);
    }

    #[test]
    fn rows_marks_selection_initials_and_small_cols() {
        let commits = vec![
            cr("aaa", &["bbb"], "main"),
            cr("bbb", &["ccc"], ""),
            cr("ccc", &[], ""),
        ];
        let g = layout(&commits);
        let copied = vec!["aaa".to_string()];
        let marks = GraphMarks {
            copied: &copied,
            base: Some("bbb"),
            diff_mark: Some("ccc"),
        };
        let lines = rows(&commits, &g, &marks, Some(1), 80);
        assert_eq!(lines.len(), commits.len());
        // Mark gutter glyphs per matching sha.
        let (l0, r0) = split(&lines[0]);
        let (l1, _) = split(&lines[1]);
        let (l2, _) = split(&lines[2]);
        assert_eq!((l0[0].text.as_str(), l0[0].fg), ("❐", Tok::Hue(Hue::Teal)));
        assert_eq!(
            (l1[0].text.as_str(), l1[0].fg),
            ("▶", Tok::Hue(Hue::Magenta))
        );
        assert_eq!((l2[0].text.as_str(), l2[0].fg), ("◈", Tok::Hue(Hue::Blue)));
        // Lane-0 graph cell is the first hue of the cycle; refs use ◉.
        assert_eq!(
            (l0[1].text.as_str(), l0[1].fg),
            ("◉", Tok::Hue(HUE_CYCLE[0]))
        );
        // The selected display row tints sha + subject bold on the accent sel.
        let subject = l1.iter().find(|s| s.text == "subject bbb").unwrap();
        assert!(subject.bold);
        assert_eq!(subject.bg, Some(Tok::SelAccent));
        let unselected = l0.iter().find(|s| s.text == "subject aaa").unwrap();
        assert!(!unselected.bold);
        assert_eq!(unselected.bg, None);
        // Author initials ride the right cluster, author-hued.
        assert_eq!(r0[0].text, "BA");
        assert_eq!(r0[0].fg, Tok::Hue(author_hue("Blake Ashley")));
        // Tiny / zero widths truncate without panicking.
        for cols in [0, 1, 3, 7] {
            assert_eq!(rows(&commits, &g, &marks, None, cols).len(), 3);
        }
    }

    #[test]
    fn rows_clamps_wide_lanes_to_max_columns() {
        // Nine orphan tips force lanes 0..=8; the lane-8 commit's glyph must
        // collapse into the last rendered column.
        let commits: Vec<CommitRow> = (0..9)
            .map(|i| cr(&format!("t{i}"), &[&format!("p{i}")], ""))
            .collect();
        let g = layout(&commits);
        assert_eq!(g[8].lane, 8);
        assert_eq!(g[8].cells.len(), 9);
        let lines = rows(&commits, &g, &marks_none(), None, 200);
        let (l, _) = split(&lines[8]);
        let glyphs: Vec<&str> = l
            .iter()
            .filter(|s| matches!(s.text.as_str(), "●" | "◉" | "│" | "┘"))
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(glyphs.len(), MAX_LANES);
        // The commit glyph won the last column, colored by its true lane.
        let last = l
            .iter()
            .rfind(|s| matches!(s.text.as_str(), "●" | "◉" | "│" | "┘"))
            .unwrap();
        assert_eq!(last.text, "●");
        assert_eq!(last.fg, Tok::Hue(HUE_CYCLE[8 % HUE_CYCLE.len()]));
    }

    fn marks_none() -> GraphMarks<'static> {
        GraphMarks {
            copied: &[],
            base: None,
            diff_mark: None,
        }
    }
}
