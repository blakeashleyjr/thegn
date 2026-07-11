//! The line-staging view model + renderer for the git panel: flattens a
//! parsed unified diff ([`thegn_core::patch`]) into addressable display
//! lines, renders them as panel rows with a line cursor / range highlight /
//! patch marks, and converts UI selections back into a core
//! [`Selection`] for the apply path.
//!
//! The flattened index space is shared with [`StagingUi`](
//! crate::panel::gitui::StagingUi) cursors and [`PatchUi`](
//! crate::panel::gitui::PatchUi) marks — both address lines of the SAME
//! `parse_patch` run the apply path consumes, so selections can never drift
//! from the constructed patch. Pure view-model: no I/O, no palette reads.
#![allow(dead_code)] // the staging-view wiring in the event loop lands separately

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use thegn_core::patch::{FileKind, LineKind, PatchHunk, Selection, parse_patch};
use thegn_core::theme::Hue;

use crate::chrome::S;
use crate::panel::sections::PanelRow;
use crate::seg::{Line, Seg, Tok, seg, sp};

// Local copies of the sections token shorthands (those are private to
// `sections/`; this module is a sibling).
fn d() -> Tok {
    Tok::Slot(S::Dim)
}
fn f() -> Tok {
    Tok::Slot(S::Faint)
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

/// The range-highlight tint (the cursor itself gets [`Tok::SelAccent`] when
/// the panel is focused).
fn range_tint() -> Tok {
    Tok::Sel(Hue::Blue, 10)
}

/// One addressable line of the staging diff, in flattened display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageLine {
    /// For header rows this is a placeholder ([`LineKind::Context`]) —
    /// `is_header` is the discriminant.
    pub kind: LineKind,
    /// Index into `PatchFile.hunks`.
    pub hunk: usize,
    /// Index into `hunks[hunk].lines` (0 and meaningless for header rows).
    pub line: usize,
    /// Computed old-side line number.
    pub old_no: Option<u32>,
    /// Computed new-side line number.
    pub new_no: Option<u32>,
    pub text: String,
    /// True for the synthesized hunk-header rows.
    pub is_header: bool,
}

/// The flattened, address-stable staging document for ONE file's diff.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StageDoc {
    pub path: String,
    /// Hunk-header rows interleaved with their body lines.
    pub lines: Vec<StageLine>,
    pub binary: bool,
}

/// The display text of a hunk header: the verbatim `@@` line (which already
/// carries the heading) or a synthesized one for programmatic hunks.
fn header_text(h: &PatchHunk) -> String {
    if !h.header_line.is_empty() {
        return h.header_line.clone();
    }
    let mut s = format!(
        "@@ -{},{} +{},{} @@",
        h.old_start, h.old_count, h.new_start, h.new_count
    );
    if !h.heading.is_empty() {
        s.push(' ');
        s.push_str(&h.heading);
    }
    s
}

/// Flatten `parse_patch(diff)` (first file) into display lines: for each
/// hunk, one `is_header` row, then its body lines with running old/new line
/// numbers (Context consumes both sides, Del the old, Add the new, `\ No
/// newline` markers neither). Markers are included (rendered dim) but are
/// never selectable.
pub fn build(path: &str, diff: &str) -> StageDoc {
    let files = parse_patch(diff);
    let Some(file) = files.first() else {
        return StageDoc {
            path: path.to_string(),
            ..StageDoc::default()
        };
    };
    let mut lines: Vec<StageLine> = Vec::new();
    for (hi, h) in file.hunks.iter().enumerate() {
        lines.push(StageLine {
            kind: LineKind::Context,
            hunk: hi,
            line: 0,
            old_no: None,
            new_no: None,
            text: header_text(h),
            is_header: true,
        });
        let (mut old, mut new) = (h.old_start, h.new_start);
        for (li, l) in h.lines.iter().enumerate() {
            let (old_no, new_no) = match l.kind {
                LineKind::Context => {
                    let at = (Some(old), Some(new));
                    old += 1;
                    new += 1;
                    at
                }
                LineKind::Del => {
                    let at = (Some(old), None);
                    old += 1;
                    at
                }
                LineKind::Add => {
                    let at = (None, Some(new));
                    new += 1;
                    at
                }
                LineKind::NoNewlineOld | LineKind::NoNewlineNew => (None, None),
            };
            lines.push(StageLine {
                kind: l.kind,
                hunk: hi,
                line: li,
                old_no,
                new_no,
                text: l.text.clone(),
                is_header: false,
            });
        }
    }
    StageDoc {
        path: path.to_string(),
        lines,
        binary: file.kind == FileKind::Binary,
    }
}

/// Whether `idx` addresses a selectable change — a body line that
/// contributes to a [`Selection`] (Add/Del only).
pub fn selectable(doc: &StageDoc, idx: usize) -> bool {
    doc.lines
        .get(idx)
        .is_some_and(|l| !l.is_header && matches!(l.kind, LineKind::Add | LineKind::Del))
}

/// Whether the cursor may rest on `idx`: any body line including context
/// (the cursor walks every line like lazygit), excluding headers and `\ No
/// newline` markers.
pub fn cursorable(doc: &StageDoc, idx: usize) -> bool {
    doc.lines.get(idx).is_some_and(|l| {
        !l.is_header && matches!(l.kind, LineKind::Context | LineKind::Add | LineKind::Del)
    })
}

/// Clamp a cursor to the nearest cursorable line (used after re-fetch).
/// Ties prefer the line below. `0` when nothing is cursorable.
pub fn clamp_cursor(doc: &StageDoc, idx: usize) -> usize {
    if doc.lines.is_empty() {
        return 0;
    }
    let idx = idx.min(doc.lines.len() - 1);
    if cursorable(doc, idx) {
        return idx;
    }
    for dist in 1..doc.lines.len() {
        if cursorable(doc, idx + dist) {
            return idx + dist;
        }
        if let Some(i) = idx.checked_sub(dist)
            && cursorable(doc, i)
        {
            return i;
        }
    }
    0
}

/// The inclusive flattened range of the hunk containing `idx` (body lines
/// only, excluding the header row) — the `a` select-hunk target.
pub fn hunk_range(doc: &StageDoc, idx: usize) -> Option<RangeInclusive<usize>> {
    let h = doc.lines.get(idx)?.hunk;
    let body = |l: &StageLine| l.hunk == h && !l.is_header;
    let first = doc.lines.iter().position(body)?;
    let last = doc.lines.iter().rposition(body)?;
    Some(first..=last)
}

/// First cursorable line of the next hunk relative to `idx` (`]`).
pub fn next_hunk(doc: &StageDoc, idx: usize) -> Option<usize> {
    let h = doc.lines.get(idx)?.hunk;
    (idx + 1..doc.lines.len()).find(|&i| doc.lines[i].hunk > h && cursorable(doc, i))
}

/// First cursorable line of the previous hunk relative to `idx` (`[`).
pub fn prev_hunk(doc: &StageDoc, idx: usize) -> Option<usize> {
    let h = doc.lines.get(idx)?.hunk;
    let tail = (0..idx.min(doc.lines.len()))
        .rev()
        .find(|&i| doc.lines[i].hunk < h && cursorable(doc, i))?;
    let ph = doc.lines[tail].hunk;
    (0..=tail).find(|&i| doc.lines[i].hunk == ph && cursorable(doc, i))
}

/// The `(hunk, line)` pairs for every changed line of the hunk containing
/// `cursor` — the target for a one-key "revert hunk" discard (item 602).
/// Headers and context lines are skipped; empty when the cursor isn't on a
/// hunk body.
pub fn hunk_revert_indices(doc: &StageDoc, cursor: usize) -> Vec<(usize, usize)> {
    let Some(range) = hunk_range(doc, cursor) else {
        return Vec::new();
    };
    range
        .filter(|&i| selectable(doc, i))
        .map(|i| {
            let l = &doc.lines[i];
            (l.hunk, l.line)
        })
        .collect()
}

/// Convert a set of flattened indices into a core [`Selection`] over the
/// SAME parse (headers, markers, and context are skipped automatically).
pub fn to_selection(doc: &StageDoc, indices: impl IntoIterator<Item = usize>) -> Selection {
    let mut sel = Selection::default();
    for i in indices {
        if selectable(doc, i) {
            let l = &doc.lines[i];
            sel.insert(l.hunk, l.line);
        }
    }
    sel
}

/// Render parameters for [`rows`].
#[derive(Debug, Clone)]
pub struct RenderOpts<'a> {
    /// The flattened line cursor.
    pub cursor: usize,
    /// The live cursor selection range (anchor-aware).
    pub sel: Option<RangeInclusive<usize>>,
    /// Custom-patch building marks; `Some` also reserves the `◆` column.
    pub marks: Option<&'a BTreeSet<usize>>,
    /// Whether the panel owns focus — controls the cursor's accent tint.
    pub focused: bool,
    /// Usable content columns (long lines clip; the seg layer re-cuts).
    pub cols: usize,
}

/// Width of the fixed gutter: old(4) + sp + new(4) + sp + sign(2).
const GUTTER: usize = 12;

/// `{n:>4}` or four spaces.
fn num(n: Option<u32>) -> String {
    match n {
        Some(n) => format!("{n:>4}"),
        None => "    ".to_string(),
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Render the doc as panel rows: `±` gutter (green/red), dim old/new line
/// numbers, faint-italic hunk headers, the cursor row accent-tinted via bg
/// (when `focused`), range rows tinted, marked rows with a `◆` gutter.
/// Returns `(rows, cursor_row_index_in_rows)` so the caller can scroll to it.
pub fn rows(doc: &StageDoc, opts: &RenderOpts) -> (Vec<PanelRow>, usize) {
    if doc.binary {
        let row = PanelRow::plain(Line::segs(vec![seg(
            d(),
            "binary file — not line-stageable",
        )]));
        return (vec![row], 0);
    }
    let mark_w = if opts.marks.is_some() { 2 } else { 0 };
    let text_w = opts.cols.saturating_sub(mark_w + GUTTER);
    let mut out: Vec<PanelRow> = Vec::with_capacity(doc.lines.len());
    for (i, l) in doc.lines.iter().enumerate() {
        let mut segs: Vec<Seg> = Vec::new();
        if let Some(marks) = opts.marks {
            segs.push(if marks.contains(&i) {
                seg(ac(), "◆ ")
            } else {
                sp(2)
            });
        }
        if l.is_header {
            segs.push(seg(f(), l.text.clone()).italic());
        } else {
            segs.push(seg(g3(), num(l.old_no)));
            segs.push(sp(1));
            segs.push(seg(g3(), num(l.new_no)));
            segs.push(sp(1));
            let (glyph, tok) = match l.kind {
                LineKind::Add => ("+", hue(Hue::Green)),
                LineKind::Del => ("−", hue(Hue::Red)),
                LineKind::Context => (" ", d()),
                LineKind::NoNewlineOld | LineKind::NoNewlineNew => ("\\", g2()),
            };
            segs.push(seg(tok, format!("{glyph} ")));
            segs.push(seg(tok, clip(&l.text, text_w)));
        }
        let mut row = PanelRow::plain(Line::segs(segs));
        let in_sel = opts.sel.as_ref().is_some_and(|r| r.contains(&i));
        if i == opts.cursor {
            row = row.with_bg(if opts.focused {
                Tok::SelAccent
            } else {
                range_tint()
            });
        } else if in_sel {
            row = row.with_bg(range_tint());
        }
        out.push(row);
    }
    let cursor_row = opts.cursor.min(out.len().saturating_sub(1));
    (out, cursor_row)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Join lines with `\n`, terminated like git output.
    fn diff(lines: &[&str]) -> String {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    }

    /// Three hunks: mixed change block, multi-del block, add + EOF marker.
    /// Flattened: 0 hdr, 1 ctx a1, 2 del a2, 3 add A2, 4 add A2b, 5–7 ctx,
    /// 8 hdr, 9 ctx b1, 10 del b2, 11 del b3, 12 add B2, 13 ctx b4,
    /// 14 hdr, 15 ctx c1, 16 ctx c2, 17 add C3, 18 ctx c3, 19 marker.
    fn fixture() -> StageDoc {
        build(
            "src/demo.rs",
            &diff(&[
                "diff --git a/src/demo.rs b/src/demo.rs",
                "index 1111111..2222222 100644",
                "--- a/src/demo.rs",
                "+++ b/src/demo.rs",
                "@@ -1,5 +1,6 @@",
                " a1",
                "-a2",
                "+A2",
                "+A2b",
                " a3",
                " a4",
                " a5",
                "@@ -10,4 +11,3 @@ fn mid() {",
                " b1",
                "-b2",
                "-b3",
                "+B2",
                " b4",
                "@@ -20,3 +20,4 @@",
                " c1",
                " c2",
                "+C3",
                " c3",
                "\\ No newline at end of file",
            ]),
        )
    }

    fn text(r: &PanelRow) -> String {
        match &r.line {
            Line::Segs(v) => v.iter().map(|s| s.text.clone()).collect(),
            _ => String::new(),
        }
    }

    #[test]
    fn build_flattens_headers_and_line_numbers() {
        let doc = fixture();
        assert_eq!(doc.path, "src/demo.rs");
        assert!(!doc.binary);
        assert_eq!(doc.lines.len(), 20);

        // Header rows interleave at each hunk start, verbatim incl. heading.
        for (i, h, txt) in [
            (0usize, 0usize, "@@ -1,5 +1,6 @@"),
            (8, 1, "@@ -10,4 +11,3 @@ fn mid() {"),
            (14, 2, "@@ -20,3 +20,4 @@"),
        ] {
            let l = &doc.lines[i];
            assert!(l.is_header, "line {i}");
            assert_eq!((l.hunk, l.text.as_str()), (h, txt));
            assert_eq!((l.old_no, l.new_no), (None, None));
        }

        // Context consumes both sides.
        let l = &doc.lines[1];
        assert_eq!(
            (l.kind, l.hunk, l.line, l.old_no, l.new_no, l.text.as_str()),
            (LineKind::Context, 0, 0, Some(1), Some(1), "a1")
        );
        // Del consumes the old side only.
        let l = &doc.lines[2];
        assert_eq!(
            (l.kind, l.old_no, l.new_no, l.text.as_str()),
            (LineKind::Del, Some(2), None, "a2")
        );
        // Add consumes the new side only.
        let l = &doc.lines[3];
        assert_eq!((l.kind, l.old_no, l.new_no), (LineKind::Add, None, Some(2)));
        assert_eq!((doc.lines[4].old_no, doc.lines[4].new_no), (None, Some(3)));
        // Context after a change block resumes both counters.
        assert_eq!(
            (doc.lines[5].old_no, doc.lines[5].new_no),
            (Some(3), Some(4))
        );
        // Hunk 1 starts at its own offsets; two dels then an add.
        assert_eq!(
            (doc.lines[9].old_no, doc.lines[9].new_no),
            (Some(10), Some(11))
        );
        assert_eq!(
            (doc.lines[11].old_no, doc.lines[11].new_no),
            (Some(12), None)
        );
        let l = &doc.lines[12];
        assert_eq!(
            (l.kind, l.hunk, l.line, l.new_no),
            (LineKind::Add, 1, 3, Some(12))
        );
        // Hunk 2's tail context.
        assert_eq!(
            (doc.lines[18].old_no, doc.lines[18].new_no),
            (Some(22), Some(23))
        );
    }

    #[test]
    fn build_includes_marker_lines_unselectable() {
        let doc = fixture();
        let m = &doc.lines[19];
        assert_eq!(m.kind, LineKind::NoNewlineOld);
        assert!(!m.is_header);
        assert_eq!((m.old_no, m.new_no), (None, None));
        assert!(!selectable(&doc, 19));
        assert!(!cursorable(&doc, 19));
    }

    #[test]
    fn build_binary_and_empty_diffs() {
        let doc = build(
            "img.png",
            &diff(&[
                "diff --git a/img.png b/img.png",
                "index 1111111..2222222 100644",
                "Binary files a/img.png and b/img.png differ",
            ]),
        );
        assert!(doc.binary);
        assert!(doc.lines.is_empty());

        let doc = build("x.rs", "");
        assert_eq!(doc.path, "x.rs");
        assert!(!doc.binary);
        assert!(doc.lines.is_empty());
    }

    #[test]
    fn selectable_vs_cursorable() {
        let doc = fixture();
        // Headers: neither.
        assert!(!selectable(&doc, 0) && !cursorable(&doc, 0));
        assert!(!selectable(&doc, 8) && !cursorable(&doc, 8));
        // Context: cursorable but not selectable.
        assert!(!selectable(&doc, 1) && cursorable(&doc, 1));
        assert!(!selectable(&doc, 18) && cursorable(&doc, 18));
        // Add/Del: both.
        assert!(selectable(&doc, 2) && cursorable(&doc, 2));
        assert!(selectable(&doc, 17) && cursorable(&doc, 17));
        // Out of range: neither.
        assert!(!selectable(&doc, 99) && !cursorable(&doc, 99));
    }

    #[test]
    fn clamp_cursor_snaps_to_nearest_cursorable() {
        let doc = fixture();
        // On a cursorable line: identity.
        assert_eq!(clamp_cursor(&doc, 5), 5);
        // A header snaps to the adjacent body line.
        assert_eq!(clamp_cursor(&doc, 0), 1);
        assert_eq!(clamp_cursor(&doc, 8), 9);
        // The trailing marker snaps back.
        assert_eq!(clamp_cursor(&doc, 19), 18);
        // Past the end (post-refetch shrink) clamps in.
        assert_eq!(clamp_cursor(&doc, 500), 18);
        // Empty / non-cursorable docs bottom out at 0.
        assert_eq!(clamp_cursor(&StageDoc::default(), 7), 0);
    }

    #[test]
    fn hunk_range_covers_body_excluding_header() {
        let doc = fixture();
        // From any line of the hunk — body, header, marker.
        assert_eq!(hunk_range(&doc, 2), Some(1..=7));
        assert_eq!(hunk_range(&doc, 0), Some(1..=7));
        assert_eq!(hunk_range(&doc, 10), Some(9..=13));
        assert_eq!(hunk_range(&doc, 13), Some(9..=13));
        // Hunk 2's range includes the marker (to_selection drops it).
        assert_eq!(hunk_range(&doc, 19), Some(15..=19));
        assert_eq!(hunk_range(&doc, 99), None);
    }

    #[test]
    fn next_and_prev_hunk_walk_first_body_lines() {
        let doc = fixture();
        assert_eq!(next_hunk(&doc, 1), Some(9));
        assert_eq!(next_hunk(&doc, 0), Some(9)); // from a header too
        assert_eq!(next_hunk(&doc, 13), Some(15));
        assert_eq!(next_hunk(&doc, 15), None); // last hunk
        assert_eq!(next_hunk(&doc, 99), None);

        assert_eq!(prev_hunk(&doc, 15), Some(9));
        assert_eq!(prev_hunk(&doc, 19), Some(9)); // from the marker
        assert_eq!(prev_hunk(&doc, 13), Some(1));
        assert_eq!(prev_hunk(&doc, 1), None); // first hunk
        assert_eq!(prev_hunk(&doc, 99), None);
    }

    #[test]
    fn to_selection_maps_changes_and_drops_the_rest() {
        let doc = fixture();
        // Headers (0, 8), context (1), marker (19) all drop silently.
        let sel = to_selection(&doc, [0, 1, 2, 3, 8, 10, 17, 19]);
        assert_eq!(sel.len(), 4);
        assert!(sel.contains(0, 1)); // idx 2 = -a2
        assert!(sel.contains(0, 2)); // idx 3 = +A2
        assert!(sel.contains(1, 1)); // idx 10 = -b2
        assert!(sel.contains(2, 2)); // idx 17 = +C3
        assert!(!sel.contains(0, 0)); // the context line never lands

        assert!(to_selection(&doc, [0, 1, 19]).is_empty());

        // A whole-hunk range converts to exactly the hunk's changes.
        let r = hunk_range(&doc, 10).unwrap();
        let sel = to_selection(&doc, r);
        assert_eq!(sel.len(), 3);
        assert!(sel.contains(1, 1) && sel.contains(1, 2) && sel.contains(1, 3));
    }

    #[test]
    fn hunk_revert_indices_targets_the_cursor_hunks_changes() {
        let doc = fixture();
        // Cursor anywhere in hunk 0 → exactly hunk 0's changed lines.
        assert_eq!(
            hunk_revert_indices(&doc, 3),
            vec![(0, 1), (0, 2), (0, 3)],
            "hunk 0: -a2, +A2, +A2b"
        );
        // Cursor in hunk 1 → that hunk's changes only (context dropped).
        assert_eq!(hunk_revert_indices(&doc, 10), vec![(1, 1), (1, 2), (1, 3)]);
        // A header row still resolves to its hunk's changes.
        assert_eq!(hunk_revert_indices(&doc, 0), vec![(0, 1), (0, 2), (0, 3)]);
        // Out-of-range cursor → nothing.
        assert!(hunk_revert_indices(&doc, 999).is_empty());
    }

    #[test]
    fn rows_render_gutters_tints_and_cursor_index() {
        let doc = fixture();
        let (rows, cur) = rows(
            &doc,
            &RenderOpts {
                cursor: 2,
                sel: Some(2..=4),
                marks: None,
                focused: true,
                cols: 80,
            },
        );
        assert_eq!(rows.len(), doc.lines.len());
        assert_eq!(cur, 2);

        // Header row text, faint italic.
        assert!(text(&rows[0]).contains("@@ -1,5 +1,6 @@"));
        let Line::Segs(v) = &rows[0].line else {
            panic!("header is segs")
        };
        assert!(v[0].italic);

        // Gutter glyphs + line numbers + body text.
        let del = text(&rows[2]);
        assert!(del.contains('−') && del.contains("a2"), "{del:?}");
        assert!(del.contains("   2"), "old line number: {del:?}");
        let add = text(&rows[3]);
        assert!(add.contains('+') && add.contains("A2"), "{add:?}");
        let ctx = text(&rows[1]);
        assert!(ctx.contains("   1    1") && ctx.contains("a1"), "{ctx:?}");
        // The marker renders as a dim `\` row.
        assert!(text(&rows[19]).contains('\\'));

        // Cursor row carries the accent tint, range rows the hue tint.
        assert_eq!(rows[2].bg, Some(Tok::SelAccent));
        assert_eq!(rows[3].bg, Some(Tok::Sel(Hue::Blue, 10)));
        assert_eq!(rows[4].bg, Some(Tok::Sel(Hue::Blue, 10)));
        assert_eq!(rows[5].bg, None);

        // Unfocused: the cursor demotes to the range tint.
        let (rows, _) = super::rows(
            &doc,
            &RenderOpts {
                cursor: 2,
                sel: None,
                marks: None,
                focused: false,
                cols: 80,
            },
        );
        assert_eq!(rows[2].bg, Some(Tok::Sel(Hue::Blue, 10)));
        assert_eq!(rows[3].bg, None);
    }

    #[test]
    fn rows_marks_get_a_diamond_gutter() {
        let doc = fixture();
        let marks: BTreeSet<usize> = [3, 17].into_iter().collect();
        let (rows, _) = rows(
            &doc,
            &RenderOpts {
                cursor: 3,
                sel: None,
                marks: Some(&marks),
                focused: true,
                cols: 80,
            },
        );
        assert!(text(&rows[3]).starts_with("◆ "), "{:?}", text(&rows[3]));
        assert!(text(&rows[17]).starts_with("◆ "));
        // Unmarked rows keep the column blank so bodies stay aligned.
        assert!(text(&rows[4]).starts_with("  "));
        assert!(text(&rows[0]).starts_with("  @@"));
    }

    #[test]
    fn rows_binary_placeholder_and_cursor_clamp() {
        let doc = build(
            "img.png",
            &diff(&[
                "diff --git a/img.png b/img.png",
                "Binary files a/img.png and b/img.png differ",
            ]),
        );
        let (rows, cur) = rows(
            &doc,
            &RenderOpts {
                cursor: 5,
                sel: None,
                marks: None,
                focused: true,
                cols: 40,
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(cur, 0);
        assert!(text(&rows[0]).contains("binary"));

        // A stale cursor past the end clamps to the last row index.
        let doc = fixture();
        let (rows, cur) = super::rows(
            &doc,
            &RenderOpts {
                cursor: 999,
                sel: None,
                marks: None,
                focused: true,
                cols: 80,
            },
        );
        assert_eq!(cur, rows.len() - 1);
    }
}
