//! The stacked-block drawing vocabulary shared by the bar-item detail popups
//! and the system-monitor modal.
//!
//! A [`Section`] is one block — a heading, a timeline graph, a table, a
//! key/value list, a multi-column grid, or a one-row sparkline — and a stack of
//! them renders top → bottom from a scroll-shifted origin. Every block
//! bounds-checks its own rows via [`put_line`], so rows scrolled above the box
//! or spilling past its bottom are simply dropped.
//!
//! Lifted out of `detail.rs` verbatim when the monitor arrived: both surfaces
//! draw identical blocks, and duplicating the drawing code is how the two would
//! have drifted apart.
//!
//! **[`Section::height`] is load-bearing.** It is what a scrollable container
//! measures its content against, so a block whose drawn height disagrees with
//! its reported height makes the tail of the stack silently unreachable — see
//! the doc comment on `detail::sections`.

use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::seg::{self, Line, Tok, seg};
use thegn_core::viz;

/// One block within a stack.
pub enum Section {
    /// A one-row dim label with an optional right-aligned note (a group header).
    Heading { label: String, note: Option<String> },
    /// A heading whose note carries its own tone (health/staleness), instead of
    /// the always-ghost note of [`Section::Heading`].
    HeadingToned {
        label: String,
        note: String,
        tone: Tok,
    },
    /// A timeline graph block (header + `height`-row plot + optional footer).
    Graph(GraphSection),
    /// A columnar breakdown (optional dim header row + body rows).
    Table(TableSection),
    /// A `key … value` block.
    KeyVal(Vec<(String, String, Tok)>),
    /// A multi-column `key value` grid: the wide-popup answer to
    /// [`Section::KeyVal`], whose value is right-aligned to the far edge and so
    /// reads as a lonely island once the box is 88 cells wide. Pairs flow
    /// ROW-MAJOR across `cols` columns and each column sizes its key and value
    /// independently, so a long value in column 2 never shoves column 1's values
    /// out of alignment. Same payload as `KeyVal`, so a block can migrate
    /// between the two by changing one word.
    Grid {
        cols: usize,
        cells: Vec<(String, String, Tok)>,
    },
    /// A one-row `label … sparkline value` (a compact inline trend).
    Sparkrow {
        label: String,
        spark: Vec<f32>,
        cur: String,
        tone: Tok,
    },
}

/// How a timeline block is drawn.
///
/// Lives on [`GraphSection`] rather than only in the monitor's preferences
/// because `Spark` collapses the plot to a **single row**, and
/// [`Section::height`] must agree with what is actually drawn. A style the
/// height calculation doesn't know about makes the tail of a stack unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphStyle {
    /// `viz::braille_graph` — a filled area. The long-standing look.
    #[default]
    Area,
    /// `viz::braille_line` — the curve alone. Reads better for a slow-moving
    /// signal (temperature, battery) that a filled area renders as a solid block.
    Line,
    /// `viz::sparkline` — one eighth-block row, whatever `height` says.
    /// Collapses the block so a twelve-sensor thermal tab fits without
    /// scrolling.
    Spark,
}

impl GraphStyle {
    pub const ALL: [GraphStyle; 3] = [GraphStyle::Area, GraphStyle::Line, GraphStyle::Spark];

    pub fn label(self) -> &'static str {
        match self {
            GraphStyle::Area => "area",
            GraphStyle::Line => "line",
            GraphStyle::Spark => "spark",
        }
    }

    /// Stable persistence slug — the same string as the label today, but named
    /// separately so relabelling the UI can never orphan a saved preference.
    pub fn key(self) -> &'static str {
        self.label()
    }

    pub fn from_key(s: &str) -> Option<GraphStyle> {
        GraphStyle::ALL.into_iter().find(|g| g.key() == s)
    }

    pub fn next(self) -> GraphStyle {
        let i = GraphStyle::ALL.iter().position(|g| *g == self).unwrap_or(0);
        GraphStyle::ALL[(i + 1) % GraphStyle::ALL.len()]
    }
}

/// A graph block: a header row, a plot, and an optional footer.
///
/// `height` is the *requested* plot height; the drawn height is
/// [`GraphSection::plot_rows`], which is 1 for [`GraphStyle::Spark`] regardless.
/// Always ask that method, never the field.
pub struct GraphSection {
    pub label: String,
    pub cur: String,
    pub footer: Option<String>,
    pub series: Vec<f32>,
    pub tone: Tok,
    pub height: usize,
    pub series2: Option<(Vec<f32>, Tok)>,
    pub style: GraphStyle,
    /// Lower edge of a min/max band. When present the plot is drawn as a band
    /// between `lo` and `series` — the honest rendering once one dot column
    /// covers many samples.
    pub lo: Option<Vec<f32>>,
    /// Axis gutter labels, top → bottom, already padded to one width. Empty for
    /// no gutter.
    pub axis: Vec<String>,
}

impl GraphSection {
    /// Rows the plot actually occupies. Spark is always one.
    pub fn plot_rows(&self) -> usize {
        match self.style {
            GraphStyle::Spark => 1,
            _ => self.height,
        }
    }
}

impl Default for GraphSection {
    fn default() -> Self {
        GraphSection {
            label: String::new(),
            cur: String::new(),
            footer: None,
            series: Vec::new(),
            tone: Tok::Slot(S::Text),
            height: 4,
            series2: None,
            style: GraphStyle::Area,
            lo: None,
            axis: Vec::new(),
        }
    }
}

/// A table cell: left-aligned text, or a filled bar (`frac` of `width` cells,
/// drawn with [`viz::bar_track`]).
pub enum Cell {
    Text(String, Tok),
    Bar(f32, usize, Tok),
}

impl Cell {
    /// Display width the cell occupies in its column.
    fn width(&self) -> usize {
        match self {
            Cell::Text(s, _) => crate::seg::cells(s),
            Cell::Bar(_, w, _) => *w,
        }
    }
}

/// A columnar breakdown: an optional header row plus body rows of [`Cell`]s.
pub struct TableSection {
    pub header: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

impl Section {
    /// Row count this section occupies when stacked.
    pub(crate) fn height(&self) -> usize {
        match self {
            Section::Heading { .. } | Section::HeadingToned { .. } | Section::Sparkrow { .. } => 1,
            Section::Graph(g) => 1 + g.plot_rows() + g.footer.is_some() as usize,
            Section::Table(t) => (!t.header.is_empty()) as usize + t.rows.len(),
            Section::KeyVal(rows) => rows.len(),
            Section::Grid { cols, cells } => cells.len().div_ceil((*cols).max(1)),
        }
    }
}

pub(crate) fn panel() -> Tok {
    Tok::Slot(S::Panel)
}

/// Draw `line` at row `y` only when it falls inside the clip rect's rows — the
/// bounds check that makes a stacked/scrolled Sections popup clip cleanly at its
/// top and bottom edges (rows above/below the box are simply skipped).
pub(crate) fn put_line(
    surface: &mut Surface,
    clip: Rect,
    x: usize,
    y: i64,
    w: usize,
    line: &Line,
    pad: Tok,
) {
    if y < clip.y as i64 || y >= (clip.y + clip.rows) as i64 {
        return;
    }
    seg::draw_line(surface, x, y as usize, w, line, pad);
}

pub(crate) fn draw_graph_block(
    surface: &mut Surface,
    clip: Rect,
    x: usize,
    y0: i64,
    w: usize,
    g: &GraphSection,
) {
    // Header: label (dim) … current value (toned).
    put_line(
        surface,
        clip,
        x,
        y0,
        w,
        &Line::split(
            vec![seg(Tok::Slot(S::Dim), g.label.clone())],
            vec![seg(g.tone, g.cur.clone()).bold()],
        ),
        panel(),
    );
    let plot_top = y0 + 1;
    let rows = g.plot_rows();
    // The axis gutter steals width from the plot, so it must be subtracted
    // BEFORE the series is drawn — otherwise the plot overruns the box.
    let gutter = g
        .axis
        .iter()
        .map(|s| crate::seg::cells(s))
        .max()
        .unwrap_or(0);
    let gutter = if gutter > 0 { gutter + 1 } else { 0 };
    let plot_x = x + gutter.min(w);
    let plot_w = w.saturating_sub(gutter);
    if rows > 0 && plot_w > 0 {
        for (i, label) in g.axis.iter().enumerate().take(rows) {
            put_line(
                surface,
                clip,
                x,
                plot_top + i as i64,
                gutter,
                &Line::segs(vec![seg(Tok::Slot(S::Ghost), label.clone())]),
                panel(),
            );
        }
        match &g.series2 {
            // Two series share the block: the first gets the top half, the
            // second the bottom (rx over tx).
            Some((s2, tone2)) => {
                let top_h = rows.div_ceil(2);
                let bot_h = rows - top_h;
                draw_plot(
                    surface, clip, plot_x, plot_top, plot_w, top_h, &g.series, None, g.tone,
                    g.style,
                );
                if bot_h > 0 {
                    draw_plot(
                        surface,
                        clip,
                        plot_x,
                        plot_top + top_h as i64,
                        plot_w,
                        bot_h,
                        s2,
                        None,
                        *tone2,
                        g.style,
                    );
                }
            }
            None => draw_plot(
                surface,
                clip,
                plot_x,
                plot_top,
                plot_w,
                rows,
                &g.series,
                g.lo.as_deref(),
                g.tone,
                g.style,
            ),
        }
    }
    if let Some(f) = &g.footer {
        put_line(
            surface,
            clip,
            x,
            y0 + 1 + rows as i64,
            w,
            &Line::segs(vec![seg(Tok::Slot(S::Ghost), f.clone())]),
            panel(),
        );
    }
}

/// Draw one plot at `(x, y)` in the requested style.
///
/// `lo` present renders a min/max band instead of a single edge — the honest
/// form once one dot column covers many samples, since a lone `max` hides how
/// quiet the rest of the bucket was.
#[allow(clippy::too_many_arguments)] // one call site; splitting it would only hide the geometry
fn draw_plot(
    surface: &mut Surface,
    clip: Rect,
    x: usize,
    y: i64,
    w: usize,
    h: usize,
    vals: &[f32],
    lo: Option<&[f32]>,
    tone: Tok,
    style: GraphStyle,
) {
    let rows = match style {
        GraphStyle::Spark => vec![viz::sparkline(&viz::fit(vals, w))],
        GraphStyle::Line => viz::braille_line(vals, w, h),
        GraphStyle::Area => match lo {
            Some(lo) => viz::braille_band(lo, vals, w, h),
            None => viz::braille_graph(vals, w, h),
        },
    };
    for (i, row) in rows.into_iter().enumerate() {
        put_line(
            surface,
            clip,
            x,
            y + i as i64,
            w,
            &Line::segs(vec![seg(tone, row)]),
            panel(),
        );
    }
}

/// Draw one block at row `y0`, clipped to `clip`.
fn draw_section(surface: &mut Surface, clip: Rect, x: usize, y0: i64, w: usize, sec: &Section) {
    match sec {
        Section::Heading { label, note } => {
            let line = match note {
                Some(n) => Line::split(
                    vec![seg(Tok::Slot(S::Dim), label.clone())],
                    vec![seg(Tok::Slot(S::Ghost), n.clone())],
                ),
                None => Line::segs(vec![seg(Tok::Slot(S::Dim), label.clone())]),
            };
            put_line(surface, clip, x, y0, w, &line, panel());
        }
        Section::HeadingToned { label, note, tone } => {
            let line = Line::split(
                vec![seg(Tok::Slot(S::Dim), label.clone())],
                vec![seg(*tone, note.clone())],
            );
            put_line(surface, clip, x, y0, w, &line, panel());
        }
        Section::Graph(g) => draw_graph_block(surface, clip, x, y0, w, g),
        Section::Table(t) => draw_table(surface, clip, x, y0, w, t),
        Section::KeyVal(rows) => {
            for (i, (k, v, tone)) in rows.iter().enumerate() {
                put_line(
                    surface,
                    clip,
                    x,
                    y0 + i as i64,
                    w,
                    &Line::split(
                        vec![seg(Tok::Slot(S::Dim), k.clone())],
                        vec![seg(*tone, v.clone())],
                    ),
                    panel(),
                );
            }
        }
        Section::Grid { cols, cells } => draw_grid(surface, clip, x, y0, w, *cols, cells),
        Section::Sparkrow {
            label,
            spark,
            cur,
            tone,
        } => {
            put_line(
                surface,
                clip,
                x,
                y0,
                w,
                &Line::split(
                    vec![seg(Tok::Slot(S::Dim), label.clone())],
                    vec![
                        seg(*tone, viz::sparkline(spark)),
                        seg(*tone, format!(" {cur}")).bold(),
                    ],
                ),
                panel(),
            );
        }
    }
}

/// Blank spacer row between stacked sections. An empty [`Section::Heading`]
/// already draws a height-1 blank line, so this needs no variant of its own.
pub(crate) fn spacer() -> Section {
    Section::Heading {
        label: String::new(),
        note: None,
    }
}

/// Cells of breathing room between one grid column's value and the next
/// column's key.
const GRID_GUTTER: usize = 2;

/// Per-column `(key width, value width)` for a row-major grid, each column sized
/// to its OWN widest key and value — so a long value in column 2 never shifts
/// column 1's alignment. Pure; widths are display cells, not char counts.
pub(crate) fn grid_widths(
    cols: usize,
    cells: &[(String, String, Tok)],
) -> (Vec<usize>, Vec<usize>) {
    let cols = cols.max(1);
    let (mut kw, mut vw) = (vec![0usize; cols], vec![0usize; cols]);
    for (i, (k, v, _)) in cells.iter().enumerate() {
        let c = i % cols;
        kw[c] = kw[c].max(crate::seg::cells(k));
        vw[c] = vw[c].max(crate::seg::cells(v));
    }
    (kw, vw)
}

/// Draw a row-major `key value` grid at row `y0`, clipped to `clip`. Each column
/// is `key` (dim, padded) + one space + `value` (toned, padded), separated by
/// [`GRID_GUTTER`]. A column whose pitch would spill past `w` is DROPPED whole
/// rather than wrapped — the popup clamps its own width, so this only bites on a
/// terminal narrower than the requested box.
fn draw_grid(
    surface: &mut Surface,
    clip: Rect,
    x: usize,
    y0: i64,
    w: usize,
    cols: usize,
    cells: &[(String, String, Tok)],
) {
    let cols = cols.max(1);
    let (kw, vw) = grid_widths(cols, cells);
    // How many columns actually fit: accumulate each column's pitch (key + space
    // + value, plus a gutter before every column after the first) until it would
    // exceed the available width. At least one column always draws.
    let mut fit = 0usize;
    let mut used = 0usize;
    for c in 0..cols {
        let pitch = kw[c] + 1 + vw[c] + if c == 0 { 0 } else { GRID_GUTTER };
        if c > 0 && used + pitch > w {
            break;
        }
        used += pitch;
        fit = c + 1;
    }
    for (r, row) in cells.chunks(cols).enumerate() {
        let mut segs = Vec::new();
        for (c, (k, v, tone)) in row.iter().enumerate().take(fit) {
            if c > 0 {
                segs.push(seg(panel(), " ".repeat(GRID_GUTTER)));
            }
            // Pad by display width: `{:<n$}` counts chars, which drifts on wide
            // glyphs — pad explicitly instead.
            let kpad = kw[c].saturating_sub(crate::seg::cells(k));
            segs.push(seg(Tok::Slot(S::Dim), format!("{k}{} ", " ".repeat(kpad))));
            let vpad = vw[c].saturating_sub(crate::seg::cells(v));
            segs.push(seg(*tone, format!("{v}{}", " ".repeat(vpad))));
        }
        put_line(
            surface,
            clip,
            x,
            y0 + r as i64,
            w,
            &Line::segs(segs),
            panel(),
        );
    }
}

/// Draw a table: per-column widths sized to the widest cell (a `Bar` counts as
/// its cell width), a dim header row when present, then body rows. Columns are
/// packed left → right with a one-space gap; a `Cell::Bar` renders as a filled
/// bar plus its `░` track.
fn draw_table(surface: &mut Surface, clip: Rect, x: usize, y0: i64, w: usize, t: &TableSection) {
    let ncol = t
        .rows
        .iter()
        .map(|r| r.len())
        .chain(std::iter::once(t.header.len()))
        .max()
        .unwrap_or(0);
    let mut colw = vec![0usize; ncol];
    for (i, h) in t.header.iter().enumerate() {
        colw[i] = colw[i].max(crate::seg::cells(h));
    }
    for row in &t.rows {
        for (i, c) in row.iter().enumerate() {
            colw[i] = colw[i].max(c.width());
        }
    }
    let mut y = y0;
    if !t.header.is_empty() {
        let mut segs = Vec::new();
        for (i, h) in t.header.iter().enumerate() {
            segs.push(seg(Tok::Slot(S::Ghost), format!("{:<w$} ", h, w = colw[i])));
        }
        put_line(surface, clip, x, y, w, &Line::segs(segs), panel());
        y += 1;
    }
    for row in &t.rows {
        let mut segs = Vec::new();
        for (i, cell) in row.iter().enumerate() {
            let cw = colw[i];
            match cell {
                Cell::Text(s, tone) => {
                    segs.push(seg(*tone, format!("{s:<cw$} ")));
                }
                Cell::Bar(frac, bw, tone) => {
                    let (bar, track) = viz::bar_track(*frac, *bw);
                    segs.push(seg(*tone, bar));
                    segs.push(seg(Tok::Slot(S::Ghost), format!("{track} ")));
                }
            }
        }
        put_line(surface, clip, x, y, w, &Line::segs(segs), panel());
        y += 1;
    }
}

/// Paint a stack of sections: walk them top → bottom from a `scroll`-shifted
/// origin. Rows outside `inner` are dropped by [`put_line`], so the stack clips
/// cleanly at both edges.
///
/// Takes a slice rather than a named container so both the detail popup and the
/// monitor can call it with whatever they hold.
pub(crate) fn render_stack(surface: &mut Surface, inner: Rect, scroll: usize, secs: &[Section]) {
    let mut y = inner.y as i64 - scroll as i64;
    for sec in secs {
        draw_section(surface, inner, inner.x, y, inner.cols, sec);
        y += sec.height() as i64;
    }
}

/// Total stacked height of a section list — what a scrollable container must
/// measure its viewport against.
pub(crate) fn stack_height(secs: &[Section]) -> usize {
    secs.iter().map(Section::height).sum()
}
