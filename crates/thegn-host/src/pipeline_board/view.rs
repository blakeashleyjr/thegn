//! Turning a laid-out [`Board`] into drawable [`Line`]s.
//!
//! Every glyph comes from [`crate::caps::active_glyphs`] and every tone is a
//! `Tok` slot or hue — no literal of either kind appears here, so the board
//! degrades with the rest of the chrome (`[theme] glyphs = ascii`, a 16-colour
//! terminal) instead of drawing mojibake at the one surface a supervisor stares
//! at all day.
//!
//! Every line carries the row it selects, which is what makes the board
//! hit-testable and keeps the cursor anchored to a row IDENTITY rather than to
//! an index — the row list moves under the user on every roster sample.

use thegn_core::termcaps::GlyphSet;
use thegn_core::theme::Hue;

use super::layout::{Board, BoardRow, Column, Edge, Mode, StageHead, is_unstaged};
use crate::chrome::S;
use crate::seg::{Line, Seg, Tok, seg, sp};

/// One rendered body line plus the row a click (or the cursor) resolves to.
///
/// In [`Mode::Stacked`] that is simply the row on the line. In
/// [`Mode::Columns`] a line spans every column at once, so `row_id` is the
/// ACTIVE column's row on that line — the one `↑`/`↓` walks and the one the
/// cursor bar is painted on. A click elsewhere on the line resolves through the
/// [`Board`] itself (see `PipelineBoard::handle_click`), which knows the column
/// geometry this line has already flattened away.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoardLine {
    pub line: Line,
    pub row_id: Option<i64>,
}

impl BoardLine {
    fn plain(line: Line) -> BoardLine {
        BoardLine { line, row_id: None }
    }
}

/// Rows of chrome above the scrolled body: the stage-name rail and the
/// agent/`→ next` rail beneath it.
pub(crate) const RAIL_ROWS: usize = 2;

/// Truncate to `max` display cells with the capability-resolved ellipsis.
/// Width-correct (`seg::cells`/`seg::take_cols`), never a byte slice.
fn trunc(s: &str, max: usize) -> String {
    let gl = crate::caps::active_glyphs();
    if crate::seg::cells(s) <= max {
        return s.to_string();
    }
    let dots = crate::seg::cells(gl.ellipsis);
    if max <= dots {
        return crate::seg::take_cols(s, max).to_string();
    }
    let mut out = crate::seg::take_cols(s, max - dots).to_string();
    out.push_str(gl.ellipsis);
    out
}

/// Pad a seg run out to exactly `w` cells. Every run reaching here was built
/// from pre-truncated strings, so this only ever adds.
fn pad(mut segs: Vec<Seg>, w: usize) -> Vec<Seg> {
    let have = crate::seg::seg_width(&segs);
    if have < w {
        segs.push(sp(w - have));
    }
    segs
}

/// The board's tone for a roster status. The glyph and hue come from
/// [`AgentDispatchStatus::glyph_set`] — the one vocabulary the sidebar and the
/// CLI also read — except that this surface HAS a dim slot, so the inert
/// states take it rather than the palette's stand-in blue.
///
/// [`AgentDispatchStatus::glyph_set`]: thegn_core::issue::AgentDispatchStatus::glyph_set
fn status_mark(
    status: thegn_core::issue::AgentDispatchStatus,
    gl: &GlyphSet,
) -> (&'static str, Tok) {
    use thegn_core::issue::AgentDispatchStatus as St;
    let (glyph, hue) = status.glyph_set(gl);
    let tone = match status {
        St::Queued | St::Unknown => Tok::Slot(S::Dim),
        _ => Tok::Hue(hue),
    };
    (glyph, tone)
}

/// `2/3` — live rows over the stage's advisory concurrency. Purely a reading;
/// nothing here enforces the budget.
fn load_label(head: &StageHead) -> String {
    if head.configured {
        format!("{}/{}", head.live, head.concurrency)
    } else {
        format!("{}", head.live)
    }
}

/// The two chrome rows above the body.
///
/// Row 0 is the stage names with their live load; row 1 is each stage's agent
/// with the `→ next` edge, which is what makes the rail read as a pipeline
/// rather than as a set of unrelated buckets. In [`Mode::Stacked`] the columns
/// are drawn down the page instead, so the rail collapses to a summary and a
/// rule.
pub(crate) fn rail(board: &Board, active_col: usize, width: usize) -> [Line; 2] {
    let gl = crate::caps::active_glyphs();
    if board.mode != Mode::Columns || board.columns.is_empty() {
        let live: usize = board.columns.iter().map(|c| c.head.live).sum();
        let total: usize = board.columns.iter().map(|c| c.head.total).sum();
        let left = vec![seg(
            Tok::Slot(S::Text),
            format!(
                "{} stage{}  {live} of {total} active",
                board.columns.len(),
                if board.columns.len() == 1 { "" } else { "s" },
            ),
        )];
        let right = vec![seg(Tok::Slot(S::Ghost), "narrow — stacked".to_string())];
        return [
            Line::split(left, right),
            Line::Fill {
                ch: gl.box_h.chars().next().unwrap_or(' '),
                fg: Tok::Slot(S::Faint),
            },
        ];
    }

    let cw = board.col_w;
    let mut names: Vec<Seg> = Vec::new();
    let mut flows: Vec<Seg> = Vec::new();
    for (i, col) in board.columns.iter().enumerate() {
        let head = &col.head;
        let load = load_label(head);
        let name_w = cw.saturating_sub(crate::seg::cells(&load) + 2);
        let name_tone = if i == active_col {
            Tok::Slot(S::Accent)
        } else if is_unstaged(head) || !head.configured {
            Tok::Slot(S::Dim)
        } else {
            Tok::Slot(S::Text)
        };
        names.push(seg(name_tone, trunc(&head.name, name_w)).bold());
        names.push(sp(1));
        names.push(seg(Tok::Slot(S::Faint), load));
        names = pad(names, cw * (i + 1));

        // `agent → next`: the next-stage arrow is what a supervisor reads the
        // rail for. The arrow sits at the column's right edge, pointing into
        // the neighbour it names.
        let arrow: Vec<Seg> = match &head.next {
            Some(n) => vec![
                seg(Tok::Slot(S::Faint), format!("{} ", gl.arrow_right)),
                seg(Tok::Slot(S::Dim), trunc(n, cw / 2)),
            ],
            None => Vec::new(),
        };
        let arrow_w = crate::seg::seg_width(&arrow);
        let agent_w = cw.saturating_sub(arrow_w + 2);
        if !head.agent.is_empty() {
            flows.push(seg(Tok::Slot(S::Ghost), trunc(&head.agent, agent_w)));
        }
        let used = crate::seg::seg_width(&flows).saturating_sub(cw * i);
        if arrow_w > 0 && used + arrow_w < cw {
            flows.push(sp(cw - used - arrow_w));
            flows.extend(arrow);
        }
        flows = pad(flows, cw * (i + 1));
    }
    [Line::segs(pad(names, width)), Line::segs(pad(flows, width))]
}

/// The scrolled board body.
///
/// `sel` is a row ID, never an index: the roster is re-sampled under the user,
/// and an index cursor would silently point at a different agent after every
/// change.
pub(crate) fn body(
    board: &Board,
    active_col: usize,
    sel: Option<i64>,
    width: usize,
) -> Vec<BoardLine> {
    match board.mode {
        Mode::Columns => columns_body(board, active_col, sel, width),
        Mode::Stacked => stacked_body(board, sel, width),
    }
}

fn columns_body(
    board: &Board,
    active_col: usize,
    sel: Option<i64>,
    width: usize,
) -> Vec<BoardLine> {
    let cw = board.col_w;
    let mut out: Vec<BoardLine> = Vec::new();
    for i in 0..board.tallest() {
        let mut segs: Vec<Seg> = Vec::new();
        for (ci, col) in board.columns.iter().enumerate() {
            segs = match col.rows.get(i) {
                Some(r) => {
                    let cell = row_cell(r, sel == Some(r.row.id), cw.saturating_sub(1));
                    let mut s = segs;
                    s.extend(cell);
                    pad(s, cw * ci + cw)
                }
                None => pad(segs, cw * ci + cw),
            };
        }
        out.push(BoardLine {
            line: Line::segs(pad(segs, width)),
            row_id: board
                .columns
                .get(active_col)
                .and_then(|c| c.rows.get(i))
                .map(|r| r.row.id),
        });
    }
    if out.is_empty() {
        out.push(BoardLine::plain(empty_line(board)));
    }
    out
}

fn stacked_body(board: &Board, sel: Option<i64>, width: usize) -> Vec<BoardLine> {
    let mut out: Vec<BoardLine> = Vec::new();
    for (i, col) in board.columns.iter().enumerate() {
        if i > 0 {
            out.push(BoardLine::plain(Line::Blank));
        }
        out.push(BoardLine::plain(group_head(col, width)));
        for r in &col.rows {
            out.push(BoardLine {
                line: Line::segs(row_cell(r, sel == Some(r.row.id), width)),
                row_id: Some(r.row.id),
            });
        }
    }
    if out.is_empty() {
        out.push(BoardLine::plain(empty_line(board)));
    }
    out
}

/// What a board with nothing to draw says. Never blank: an empty box reads as
/// broken, and which of the two emptinesses it is (nothing configured vs
/// nothing dispatched) is exactly what the user needs to know.
fn empty_line(board: &Board) -> Line {
    let msg = if board.columns.is_empty() {
        "No dispatches yet, and no [[pipeline.stages]] configured."
    } else {
        "Every configured stage is empty — nothing dispatched yet."
    };
    Line::segs(vec![seg(Tok::Slot(S::Ghost), msg.to_string())])
}

/// A stage's heading in [`Mode::Stacked`], carrying the same facts the columns
/// rail carries side by side.
fn group_head(col: &Column, width: usize) -> Line {
    let gl = crate::caps::active_glyphs();
    let head = &col.head;
    let mut left = vec![
        seg(Tok::Slot(S::Accent), trunc(&head.name, width / 2)).bold(),
        sp(1),
        seg(Tok::Slot(S::Faint), load_label(head)),
    ];
    if !head.agent.is_empty() {
        left.push(sp(2));
        left.push(seg(Tok::Slot(S::Ghost), trunc(&head.agent, width / 4)));
    }
    let right = match &head.next {
        Some(n) => vec![seg(
            Tok::Slot(S::Dim),
            format!("{} {}", gl.arrow_right, trunc(n, width / 4)),
        )],
        None => Vec::new(),
    };
    Line::split(left, right)
}

/// One row, fitted to exactly `w` cells.
///
/// Layout, left to right, every part a fixed width so columns stay aligned:
/// the cursor bar, the edge mark (inbound arrow, tree connector, or a space),
/// the status glyph, the worktree name, then the outbound tick and the age.
fn row_cell(r: &BoardRow, selected: bool, w: usize) -> Vec<Seg> {
    let gl = crate::caps::active_glyphs();
    let mut segs: Vec<Seg> = Vec::new();

    // The cursor bar: a row that LOOKS selectable is the difference between a
    // table and a control surface.
    segs.push(if selected {
        seg(Tok::Slot(S::Accent), gl.half_block_r)
    } else {
        sp(1)
    });

    // Edge mark. `Edge::None` still costs a cell — exact alignment is what
    // makes the columns read as columns.
    match r.edge {
        Edge::Inbound => segs.push(seg(Tok::Hue(Hue::Teal), gl.arrow_right)),
        Edge::Child { last } => {
            let indent = (r.row.depth as usize).saturating_sub(1).min(w / 4);
            if indent > 0 {
                segs.push(sp(indent * 2));
            }
            segs.push(seg(
                Tok::Slot(S::Faint),
                if last { gl.tree_corner } else { gl.tree_tee },
            ));
        }
        Edge::None => segs.push(sp(1)),
    }

    let (glyph, tone) = status_mark(r.row.status, gl);
    segs.push(seg(tone, glyph));
    segs.push(sp(1));

    // Right cluster: the outbound tick, then the age (red once the stage's
    // advisory budget has elapsed — a cue, not an action).
    let tick = if r.outbound { gl.arrow_right } else { " " };
    let age_tone = if r.stalled {
        Tok::Hue(Hue::Red)
    } else {
        Tok::Slot(S::Dim)
    };
    let right_w = crate::seg::cells(tick) + crate::seg::cells(&r.row.age) + 1;
    let used = crate::seg::seg_width(&segs);

    // The label is the worktree basename — the sidebar's own row identity, so
    // the two surfaces name the same thing. An empty path (a dispatch recorded
    // with no worktree) falls back to the issue it came from.
    let raw = if r.row.worktree.is_empty() {
        &r.row.issue_id
    } else {
        &r.row.worktree
    };
    let label_w = w.saturating_sub(used + right_w);
    let label_tone = if selected {
        Tok::Slot(S::Accent)
    } else {
        Tok::Slot(S::Text)
    };
    let label = trunc(raw, label_w);
    let label_cells = crate::seg::cells(&label);
    let mut lab = seg(label_tone, label);
    if selected {
        lab = lab.bold();
    }
    segs.push(lab);
    segs.push(sp(label_w.saturating_sub(label_cells) + 1));
    segs.push(seg(Tok::Slot(S::Faint), tick));
    segs.push(seg(age_tone, r.row.age.clone()));
    pad(segs, w)
}

/// The footer legend: EVERY key the board binds, and nothing it doesn't.
///
/// Deliberately 7-bit. `↑`/`↓`/`←`/`→` are bound as aliases of `k`/`j`/`h`/`l`
/// and the arrow glyph set has no left arrow to spell them with, so the legend
/// names the letters and `docs/help/pipeline-board.md` documents the arrows.
pub(crate) fn legend(frozen: bool, hide_finished: bool) -> Line {
    let hint = |s: &str| seg(Tok::Slot(S::Ghost), format!(" {s}  "));
    let left = vec![
        Seg::key("kj"),
        hint("row"),
        Seg::key("hl"),
        hint("stage"),
        Seg::key("enter"),
        hint("open"),
        Seg::key("spc"),
        hint(if frozen { "live" } else { "freeze" }),
        Seg::key("x"),
        seg(
            Tok::Slot(S::Ghost),
            if hide_finished {
                " show finished".to_string()
            } else {
                " hide finished".to_string()
            },
        ),
    ];
    let right = if frozen {
        vec![seg(Tok::Slot(S::Accent), "frozen".to_string())]
    } else {
        vec![seg(Tok::Slot(S::Ghost), "esc close".to_string())]
    };
    Line::split(left, right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor_pipeline::ordered_rows;
    use thegn_core::config_pipeline::PipelineStage;
    use thegn_core::issue::{AgentDispatch, AgentDispatchStatus};
    use thegn_core::termcaps::UnicodeLevel;

    fn stage(name: &str, next: Option<&str>) -> PipelineStage {
        PipelineStage {
            name: name.into(),
            agent: format!("{name}-agent"),
            concurrency: 2,
            timeout_secs: 60,
            next: next.map(str::to_string),
            ..PipelineStage::default()
        }
    }

    fn d(id: i64, st: Option<&str>, parent: Option<i64>, at: i64) -> AgentDispatch {
        AgentDispatch {
            id,
            issue_id: format!("THE-{id}"),
            worktree_path: format!("/wt/w{id}"),
            agent_name: format!("a{id}"),
            dispatched_at_ms: at,
            status: AgentDispatchStatus::Running,
            stage: st.map(str::to_string),
            parent_id: parent,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
        }
    }

    fn fixture(width: usize) -> Board {
        let stages = [stage("architect", Some("code")), stage("code", None)];
        let names: Vec<String> = stages.iter().map(|s| s.name.clone()).collect();
        let rows = ordered_rows(
            &[
                d(1, Some("architect"), None, 0),
                d(2, Some("code"), Some(1), 10),
                d(3, Some("code"), Some(2), 20),
                d(4, None, None, 30),
            ],
            &names,
            120_000,
        );
        super::super::layout::board(&rows, &stages, width, 120_000, false)
    }

    /// Every line the renderer emits, flattened to text.
    fn text(lines: &[BoardLine]) -> String {
        lines
            .iter()
            .map(|l| line_text(&l.line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_text(l: &Line) -> String {
        match l {
            Line::Blank => String::new(),
            Line::Segs(s) => s.iter().map(|g| g.text.clone()).collect(),
            Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
                let mut out: String = l.iter().map(|g| g.text.clone()).collect();
                out.push_str(&r.iter().map(|g| g.text.clone()).collect::<String>());
                out
            }
            Line::Fill { ch, .. } => ch.to_string(),
        }
    }

    #[test]
    fn the_board_degrades_to_seven_bit_on_the_ascii_ladder() {
        // Same board, both rungs of the caps ladder: identical line count (the
        // ASCII fallback is width-1 for every glyph the board uses), and the
        // ASCII render carries no byte a non-UTF-8 terminal would mangle.
        let (uni_lines, uni_rail) =
            crate::caps::test_override::with_unicode(UnicodeLevel::Full, || {
                let b = fixture(120);
                (body(&b, 0, Some(1), 120), rail(&b, 0, 120))
            });
        let (ascii_lines, ascii_rail) =
            crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
                let b = fixture(120);
                (body(&b, 0, Some(1), 120), rail(&b, 0, 120))
            });
        assert_eq!(
            uni_lines.len(),
            ascii_lines.len(),
            "line count must not move"
        );
        assert_eq!(
            uni_lines.iter().map(|l| l.row_id).collect::<Vec<_>>(),
            ascii_lines.iter().map(|l| l.row_id).collect::<Vec<_>>(),
            "the hit map must not move either"
        );
        let ascii = format!(
            "{}\n{}\n{}\n{}",
            text(&ascii_lines),
            line_text(&ascii_rail[0]),
            line_text(&ascii_rail[1]),
            line_text(&legend(false, false)),
        );
        assert!(
            ascii.is_ascii(),
            "the ASCII ladder must render 7-bit, got: {ascii:?}"
        );
        // …and the Unicode rung really is drawing the richer glyphs, so the
        // test above isn't passing because both rungs are plain.
        assert!(!text(&uni_lines).is_ascii());
        assert!(!line_text(&uni_rail[0]).is_ascii() || !line_text(&uni_rail[1]).is_ascii());
    }

    #[test]
    fn the_footer_legend_names_every_bound_key() {
        let l = line_text(&legend(false, false));
        for key in ["kj", "hl", "enter", "spc", "x", "esc"] {
            assert!(l.contains(key), "legend is missing `{key}`: {l}");
        }
        // The two toggles say what they will DO next, not what they are.
        assert!(l.contains("freeze") && l.contains("hide finished"));
        let l = line_text(&legend(true, true));
        assert!(l.contains("live") && l.contains("show finished") && l.contains("frozen"));
    }

    #[test]
    fn columns_mode_lines_carry_the_active_columns_row() {
        let b = fixture(120);
        assert_eq!(b.mode, Mode::Columns);
        // Column 0 (architect) has one row; column 1 (code) has two.
        let lines = body(&b, 0, None, 120);
        assert_eq!(lines.len(), b.tallest());
        assert_eq!(lines[0].row_id, Some(1));
        assert_eq!(lines[1].row_id, None, "column 0 ran out of rows");
        // Switching the active column re-points the hit map without moving the
        // lines — the cursor follows the column, the board does not reflow.
        let lines = body(&b, 1, None, 120);
        assert_eq!(
            lines.iter().map(|l| l.row_id).collect::<Vec<_>>(),
            vec![Some(2), Some(3)]
        );
    }

    #[test]
    fn stacked_mode_lists_every_stage_and_every_row() {
        let b = fixture(20); // far below MIN_COL_W * columns
        assert_eq!(b.mode, Mode::Stacked);
        let lines = body(&b, 0, None, 20);
        let ids: Vec<i64> = lines.iter().filter_map(|l| l.row_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4], "every row is reachable when stacked");
        let t = text(&lines);
        for stage in ["architect", "code", "unstaged"] {
            assert!(t.contains(stage), "stage `{stage}` missing: {t}");
        }
    }

    #[test]
    fn every_line_is_exactly_the_board_width() {
        // Alignment is the whole premise of a column board: one short line and
        // every column below it reads as belonging to the wrong stage.
        for w in [40usize, 61, 80, 120] {
            let b = fixture(w);
            for l in body(&b, 0, Some(2), w) {
                if let Line::Segs(s) = &l.line {
                    assert_eq!(
                        crate::seg::seg_width(s),
                        w,
                        "line not fitted at width {w}: {:?}",
                        line_text(&l.line)
                    );
                }
            }
            if let Line::Segs(s) = &rail(&b, 0, w)[0] {
                assert_eq!(crate::seg::seg_width(s), w, "rail not fitted at width {w}");
            }
        }
    }

    #[test]
    fn an_empty_board_says_why_rather_than_drawing_a_void() {
        let b = super::super::layout::board(&[], &[], 80, 0, false);
        let lines = body(&b, 0, None, 80);
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0].line).contains("No dispatches yet"));
        assert_eq!(lines[0].row_id, None);
    }
}
