//! Reusable, capability-aware raster primitives for system-monitor views.
//!
//! This module is pure: it produces character cells and gradient levels but
//! does not choose terminal colors or perform terminal I/O. Raster glyphs live
//! in the three tables below; draw sites select a table from terminal caps.

use crate::termcaps::UnicodeLevel;

/// Raster glyphs and degradation choices for a terminal's Unicode level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterTable {
    /// Whether area, line, band and mirrored use braille dot columns.
    pub braille: bool,
    /// Eighth-block fills, ordered from one eighth through full.
    pub ramp8: [char; 8],
    /// Left-to-right eighth-block fills, ordered from one eighth through full.
    pub hbar8: [char; 8],
    /// Upper half-block, used to pack two heatmap instances per row.
    pub half_upper: char,
    /// Lower half-block, used when only the second packed instance is present.
    pub half_lower: char,
    /// ASCII density choices from light to full.
    pub shade4: [char; 4],
    /// Vertical column raster steps, empty to full.
    pub cols: &'static [char],
    /// Heatmap density choices, empty first.
    pub heat: &'static [char],
    /// Sparkline steps, low to high.
    pub spark: &'static [char],
    /// Empty meter track glyph.
    pub track: char,
    /// Blank cell glyph.
    pub blank: char,
    /// Space used for empty column-raster cells.
    pub cell_blank: char,
}

const RAMP8: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const HBAR8: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const SHADE4: [char; 4] = ['░', '▒', '▓', '█'];
const FULL_COLS: &[char] = &['▂', '▄', '▆', '█'];
const FULL_HEAT: &[char] = &[' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '▓', '█'];
const FULL_SPARK: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const ASCII_COLS: &[char] = &['.', ':', '|', '#'];
const ASCII_HEAT: &[char] = &[' ', '.', ':', '-', '=', '+', '*', '#', '%', '@'];
const ASCII_SPARK: &[char] = &['_', '.', '-', '=', '#'];

impl RasterTable {
    /// Full Unicode raster set.
    pub const FULL: Self = Self {
        braille: true,
        ramp8: RAMP8,
        hbar8: HBAR8,
        half_upper: '▀',
        half_lower: '▄',
        shade4: SHADE4,
        cols: FULL_COLS,
        heat: FULL_HEAT,
        spark: FULL_SPARK,
        track: '░',
        blank: '⠀',
        cell_blank: ' ',
    };

    /// Basic Unicode retains the same raster glyphs as full Unicode.
    pub const BASIC: Self = Self::FULL;

    /// ASCII fallback raster set.
    pub const ASCII: Self = Self {
        braille: false,
        ramp8: ['.', ':', '-', '=', '+', '*', '#', '#'],
        hbar8: ['.', ':', '-', '=', '+', '*', '#', '#'],
        half_upper: '#',
        half_lower: '#',
        shade4: ['.', '-', '=', '#'],
        cols: ASCII_COLS,
        heat: ASCII_HEAT,
        spark: ASCII_SPARK,
        track: '-',
        blank: ' ',
        cell_blank: ' ',
    };

    /// Selects the raster set matching terminal Unicode support.
    pub fn for_level(level: UnicodeLevel) -> &'static Self {
        match level {
            UnicodeLevel::Full => &Self::FULL,
            UnicodeLevel::Basic => &Self::BASIC,
            UnicodeLevel::Ascii => &Self::ASCII,
        }
    }
}

/// One rendered terminal cell. `fg` is a ramp level or a stacked-band index;
/// `bg` is the optional second stacked-band index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RCell {
    pub ch: char,
    pub fg: u8,
    pub bg: Option<u8>,
}

impl RCell {
    const fn blank(ch: char) -> Self {
        Self {
            ch,
            fg: 0,
            bg: None,
        }
    }
}

/// A reusable row-major cell buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellBuf {
    w: usize,
    h: usize,
    cells: Vec<RCell>,
    spans: Vec<(i32, i32)>,
    spans2: Vec<(i32, i32)>,
}

impl CellBuf {
    /// Creates an empty buffer.
    pub const fn new() -> Self {
        Self {
            w: 0,
            h: 0,
            cells: Vec::new(),
            spans: Vec::new(),
            spans2: Vec::new(),
        }
    }

    /// Resizes and clears the buffer, retaining its allocation when possible.
    pub fn reset(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.cells.resize(w.saturating_mul(h), RCell::blank(' '));
        self.cells.fill(RCell::blank(' '));
    }

    /// Buffer width in cells.
    pub const fn width(&self) -> usize {
        self.w
    }

    /// Buffer height in cells.
    pub const fn height(&self) -> usize {
        self.h
    }

    /// Returns a row, or an empty slice when `r` is out of bounds.
    pub fn row(&self, r: usize) -> &[RCell] {
        if r >= self.h {
            return &[];
        }
        let start = r * self.w;
        &self.cells[start..start + self.w]
    }

    /// Returns one cell when its coordinates are in bounds.
    pub fn get(&self, c: usize, r: usize) -> Option<&RCell> {
        (c < self.w && r < self.h).then(|| &self.cells[r * self.w + c])
    }

    fn put(&mut self, c: usize, r: usize, cell: RCell) {
        if c < self.w && r < self.h {
            self.cells[r * self.w + c] = cell;
        }
    }
}

const BRDOTS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

fn clamp01(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

fn dot_offset(len: usize, w: usize) -> usize {
    w.saturating_mul(2)
        .saturating_sub(len.min(w.saturating_mul(2)))
}

fn sample_offset(len: usize, w: usize) -> usize {
    w.saturating_sub(len.min(w))
}

fn aligned_sample(values: &[f32], column: usize, width: usize) -> Option<f32> {
    let destination = sample_offset(values.len(), width);
    if column < destination || column - destination >= values.len().min(width) {
        return None;
    }
    values
        .get(values.len().saturating_sub(width) + column - destination)
        .copied()
}

fn has_braille_sample(len: usize, w: usize, cell: usize) -> bool {
    let capacity = w.saturating_mul(2);
    let start = dot_offset(len, w);
    (0..2).any(|dot| {
        let index = cell.saturating_mul(2).saturating_add(dot);
        (start..start.saturating_add(len.min(capacity))).contains(&index)
    })
}

fn level_for_dot(top_dot: i32, total: usize) -> u8 {
    if top_dot < 0 || total == 0 {
        return 0;
    }
    ((((top_dot as usize + 1) * 8).div_ceil(total)).saturating_sub(1)).min(7) as u8
}

/// Braille span modes share one inclusive-range renderer.
#[derive(Clone, Copy)]
enum SpanMode {
    Area,
    Line,
    Band,
}

fn spans_for(
    vals: &[f32],
    w: usize,
    h: usize,
    mode: SpanMode,
    other: &[f32],
    spans: &mut Vec<(i32, i32)>,
) {
    let total = h.saturating_mul(4);
    spans.clear();
    if total == 0 {
        return;
    }
    let capacity = w.saturating_mul(2);
    let dest = dot_offset(vals.len(), w);
    let dest_other = dot_offset(other.len(), w);
    let source = vals.len().saturating_sub(capacity);
    let source_other = other.len().saturating_sub(capacity);
    spans.reserve(w.saturating_mul(2));
    spans.extend((0..w.saturating_mul(2)).map(|i| {
        let has_value = i >= dest && i - dest < vals.len().min(capacity);
        let v = clamp01(if has_value {
            vals[source + i - dest]
        } else {
            0.0
        });
        match mode {
            SpanMode::Area => {
                let n = ((v * total as f32).round() as i32).max(if v > 0.001 { 1 } else { 0 });
                (0, n - 1)
            }
            SpanMode::Line => {
                if !has_value {
                    return (0, -1);
                }
                let y = ((v * total as f32).floor() as i32).min(total as i32 - 1);
                let prev_i = if i <= dest { i } else { i - 1 };
                let prev = clamp01(vals[source + prev_i - dest]);
                let py = ((prev * total as f32).floor() as i32).min(total as i32 - 1);
                (y.min(py), y.max(py))
            }
            SpanMode::Band => {
                let has_hi = i >= dest_other && i - dest_other < other.len().min(capacity);
                if !has_value && !has_hi {
                    return (0, -1);
                }
                let hi = clamp01(if has_hi {
                    other[source_other + i - dest_other]
                } else {
                    0.0
                });
                let (lo, hi) = (v.min(hi), v.max(hi));
                let bot = ((lo * total as f32).floor() as i32).clamp(0, total as i32 - 1);
                let top = ((hi * total as f32).ceil() as i32 - 1).clamp(bot, total as i32 - 1);
                (bot, top)
            }
        }
    }));
}

fn draw_braille_spans(table: &RasterTable, gradient: bool, buf: &mut CellBuf) {
    let CellBuf {
        w, h, cells, spans, ..
    } = buf;
    let total = h.saturating_mul(4);
    for r in 0..*h {
        for c in 0..*w {
            let mut code = 0u32;
            let mut top = -1i32;
            for (col, bits) in BRDOTS.iter().enumerate() {
                let (lo, hi) = spans.get(c * 2 + col).copied().unwrap_or((0, -1));
                for (dr, bit) in bits.iter().enumerate() {
                    let y = ((*h - 1 - r) * 4 + (3 - dr)) as i32;
                    if y >= lo && y <= hi {
                        code |= *bit as u32;
                        top = top.max(y);
                    }
                }
            }
            let ch = char::from_u32(0x2800 + code).unwrap_or(table.blank);
            cells[r * *w + c] = RCell {
                ch: if code == 0 {
                    char::from_u32(0x2800).unwrap_or(table.blank)
                } else {
                    ch
                },
                fg: if gradient {
                    level_for_dot(top, total)
                } else {
                    0
                },
                bg: None,
            };
        }
    }
}

fn column_area(
    vals: &[f32],
    steps: &[char],
    table: &RasterTable,
    buf: &mut CellBuf,
    mirrored_top: bool,
) {
    if steps.is_empty() {
        return;
    }
    for c in 0..buf.w {
        let value = clamp01(aligned_sample(vals, c, buf.w).unwrap_or(0.0));
        for r in 0..buf.h {
            let distance = if mirrored_top { r } else { buf.h - 1 - r };
            buf.put(c, r, column_cell(value, distance, buf.h, steps, table));
        }
    }
}

fn column_cell(
    value: f32,
    distance: usize,
    height: usize,
    steps: &[char],
    table: &RasterTable,
) -> RCell {
    let units = value * height as f32;
    let full = units.floor() as usize;
    let fraction = units - full as f32;
    let rem = ((fraction * steps.len() as f32).ceil() as usize).saturating_sub(1);
    let gradient_level = |height_position: f32| {
        ((height_position * 8.0 / height as f32).ceil() as i32 - 1).clamp(0, 7) as u8
    };
    let (ch, fg) = if distance < full {
        (
            *steps.last().unwrap_or(&table.cell_blank),
            gradient_level((distance + 1) as f32),
        )
    } else if distance == full && full < height && fraction > 0.0 {
        (
            steps[rem.min(steps.len() - 1)],
            gradient_level(distance as f32 + fraction),
        )
    } else {
        (table.cell_blank, 0)
    };
    RCell { ch, fg, bg: None }
}

/// Draws a filled area graph, with two braille samples per cell or ASCII columns.
pub fn area(vals: &[f32], table: &RasterTable, buf: &mut CellBuf) {
    if table.braille {
        spans_for(vals, buf.w, buf.h, SpanMode::Area, &[], &mut buf.spans);
        draw_braille_spans(table, true, buf);
        for row in 0..buf.h {
            for col in 0..buf.w {
                if !has_braille_sample(vals.len(), buf.w, col) {
                    buf.put(col, row, RCell::blank(table.cell_blank));
                }
            }
        }
    } else {
        columns(vals, table.cols, table, buf);
    }
}

/// Draws a connected line graph, with two braille samples per cell where available.
pub fn line(vals: &[f32], table: &RasterTable, buf: &mut CellBuf) {
    if table.braille {
        spans_for(vals, buf.w, buf.h, SpanMode::Line, &[], &mut buf.spans);
        draw_braille_spans(table, false, buf);
        for row in 0..buf.h {
            for col in 0..buf.w {
                if !has_braille_sample(vals.len(), buf.w, col) {
                    buf.put(col, row, RCell::blank(table.cell_blank));
                }
            }
        }
    } else {
        buf.reset(buf.w, buf.h);
        for c in 0..buf.w {
            let Some(value) = aligned_sample(vals, c, buf.w) else {
                continue;
            };
            let v = clamp01(value);
            if buf.h == 0 || table.cols.is_empty() {
                continue;
            }
            let y = ((1.0 - v) * (buf.h - 1) as f32).round() as usize;
            let idx = (((v * table.cols.len() as f32).ceil() as usize).saturating_sub(1))
                .min(table.cols.len() - 1);
            buf.put(
                c,
                y,
                RCell {
                    ch: table.cols[idx],
                    fg: 0,
                    bg: None,
                },
            );
        }
    }
}

/// Draws an inclusive min–max band per sample column.
pub fn band(lo: &[f32], hi: &[f32], table: &RasterTable, buf: &mut CellBuf) {
    if table.braille {
        spans_for(lo, buf.w, buf.h, SpanMode::Band, hi, &mut buf.spans);
        draw_braille_spans(table, false, buf);
        for row in 0..buf.h {
            for col in 0..buf.w {
                if !has_braille_sample(lo.len().max(hi.len()), buf.w, col) {
                    buf.put(col, row, RCell::blank(table.cell_blank));
                }
            }
        }
    } else {
        buf.reset(buf.w, buf.h);
        for c in 0..buf.w {
            let low = clamp01(aligned_sample(lo, c, buf.w).unwrap_or(0.0));
            let high = clamp01(aligned_sample(hi, c, buf.w).unwrap_or(0.0));
            let lower = (low.min(high) * buf.h as f32).floor() as usize;
            let upper = (high.max(low) * buf.h as f32).ceil() as usize;
            for r in 0..buf.h {
                let d = buf.h - 1 - r;
                if d >= lower && d < upper {
                    let coverage = (high.max(low) - low.min(high)).max(1.0 / buf.h.max(1) as f32);
                    let idx = ((coverage * table.cols.len() as f32).ceil() as usize)
                        .saturating_sub(1)
                        .min(table.cols.len().saturating_sub(1));
                    if !table.cols.is_empty() {
                        buf.put(
                            c,
                            r,
                            RCell {
                                ch: table.cols[idx],
                                fg: 0,
                                bg: None,
                            },
                        );
                    }
                }
            }
        }
    }
}

/// Draws one value per column using the supplied bottom-up raster steps.
pub fn columns(vals: &[f32], steps: &[char], table: &RasterTable, buf: &mut CellBuf) {
    buf.reset(buf.w, buf.h);
    column_area(vals, steps, table, buf, false);
}

/// Mirrors upper and lower values away from a horizontal axis.
pub fn mirrored(up: &[f32], down: &[f32], table: &RasterTable, buf: &mut CellBuf) {
    let upper_h = buf.h.div_ceil(2);
    let lower_h = buf.h / 2;
    buf.reset(buf.w, buf.h);
    if table.braille {
        spans_for(up, buf.w, upper_h, SpanMode::Area, &[], &mut buf.spans);
        spans_for(down, buf.w, lower_h, SpanMode::Area, &[], &mut buf.spans2);
        draw_braille_spans_mirrored(upper_h, table, buf);
        for r in 0..buf.h {
            let len = if r < upper_h { up.len() } else { down.len() };
            for c in 0..buf.w {
                if !has_braille_sample(len, buf.w, c) {
                    buf.put(c, r, RCell::blank(table.cell_blank));
                }
            }
        }
    } else if !table.cols.is_empty() {
        for c in 0..buf.w {
            let upper_value = clamp01(aligned_sample(up, c, buf.w).unwrap_or(0.0));
            let lower_value = clamp01(aligned_sample(down, c, buf.w).unwrap_or(0.0));
            for r in 0..upper_h {
                buf.put(
                    c,
                    r,
                    column_cell(upper_value, upper_h - 1 - r, upper_h, table.cols, table),
                );
            }
            for r in 0..lower_h {
                buf.put(
                    c,
                    upper_h + r,
                    column_cell(lower_value, r, lower_h, table.cols, table),
                );
            }
        }
    }
}

fn draw_braille_spans_mirrored(upper_h: usize, table: &RasterTable, buf: &mut CellBuf) {
    let CellBuf {
        w,
        h,
        cells,
        spans,
        spans2,
        ..
    } = buf;
    for r in 0..*h {
        for c in 0..*w {
            let mut code = 0u32;
            let mut top_dot = -1i32;
            for (col, bits) in BRDOTS.iter().enumerate() {
                let i = c * 2 + col;
                for (dr, bit) in bits.iter().enumerate() {
                    let local = if r < upper_h {
                        ((upper_h - 1 - r) * 4 + (3 - dr)) as i32
                    } else {
                        ((r - upper_h) * 4 + dr) as i32
                    };
                    let span = if r < upper_h {
                        spans.get(i).copied().unwrap_or((0, -1))
                    } else {
                        spans2.get(i).copied().unwrap_or((0, -1))
                    };
                    if local >= span.0 && local <= span.1 {
                        code |= *bit as u32;
                        top_dot = top_dot.max(local);
                    }
                }
            }
            let ch = char::from_u32(0x2800 + code).unwrap_or(table.blank);
            cells[r * *w + c] = RCell {
                ch,
                fg: level_for_dot(
                    top_dot,
                    if r < upper_h {
                        upper_h * 4
                    } else {
                        (*h - upper_h) * 4
                    },
                ),
                bg: None,
            };
        }
    }
}

/// Draws N stacked bands over the buffer height, clamping their total to one.
///
/// If an inter-band boundary and the stack's top both fall inside one cell,
/// the cell uses the colour of the band with the largest coverage. Equal
/// coverage keeps the lower-index band. The single glyph cannot represent all
/// three regions in that case.
pub fn stacked(bands: &[&[f32]], table: &RasterTable, buf: &mut CellBuf) {
    buf.reset(buf.w, buf.h);
    if buf.w == 0 || buf.h == 0 || bands.is_empty() {
        return;
    }
    for c in 0..buf.w {
        for r in 0..buf.h {
            let bottom = (buf.h - 1 - r) as f32 / buf.h as f32;
            let top = (buf.h - r) as f32 / buf.h as f32;
            let middle = (bottom + top) / 2.0;
            let mut cumulative = 0.0f32;
            let mut largest = (0.0f32, 0usize);
            let mut boundary = None;
            let mut middle_band = None;
            for (i, band) in bands.iter().enumerate() {
                let start = cumulative;
                let amount = clamp01(aligned_sample(band, c, buf.w).unwrap_or(0.0));
                cumulative = (cumulative + amount).min(1.0);
                let coverage = (cumulative.min(top) - start.max(bottom)).max(0.0);
                if coverage > largest.0 {
                    largest = (coverage, i);
                }
                if i + 1 < bands.len()
                    && boundary.is_none()
                    && cumulative > bottom
                    && cumulative < top
                {
                    boundary = Some((i + 1, cumulative));
                }
                if middle >= start && middle < cumulative {
                    middle_band = Some(i);
                }
            }
            if largest.0 == 0.0 {
                continue;
            }
            if !table.braille {
                let density = ((largest.0 * buf.h as f32 * 4.0).ceil() as usize)
                    .saturating_sub(1)
                    .min(3);
                buf.put(
                    c,
                    r,
                    RCell {
                        ch: table.shade4[density],
                        fg: largest.1 as u8,
                        bg: None,
                    },
                );
                continue;
            }
            let total_inside_cell = cumulative > bottom && cumulative < top;
            if total_inside_cell && boundary.is_some() {
                let eighths = ((cumulative - bottom) * buf.h as f32 * 8.0).round() as usize;
                buf.put(
                    c,
                    r,
                    RCell {
                        ch: table.ramp8[eighths.saturating_sub(1).min(7)],
                        fg: largest.1 as u8,
                        bg: None,
                    },
                );
            } else if let Some((i, at)) = boundary {
                let eighths = ((at - bottom) * buf.h as f32 * 8.0).round() as usize;
                let ch = if eighths == 0 {
                    table.cell_blank
                } else {
                    table.ramp8[(eighths - 1).min(7)]
                };
                buf.put(
                    c,
                    r,
                    RCell {
                        ch,
                        fg: (i - 1) as u8,
                        bg: Some(i as u8),
                    },
                );
            } else if total_inside_cell {
                let eighths = ((cumulative - bottom) * buf.h as f32 * 8.0).round() as usize;
                buf.put(
                    c,
                    r,
                    RCell {
                        ch: table.ramp8[eighths.saturating_sub(1).min(7)],
                        fg: largest.1 as u8,
                        bg: None,
                    },
                );
            } else if let Some(i) = middle_band {
                buf.put(
                    c,
                    r,
                    RCell {
                        ch: table.ramp8[7],
                        fg: i as u8,
                        bg: None,
                    },
                );
            }
        }
    }
}

/// Draws a time-by-instance heatmap. Unicode packs two instances into each row.
/// Width is the longest row; shorter rows align their samples to its right edge.
pub fn heatmap(rows: &[&[f32]], table: &RasterTable, buf: &mut CellBuf) {
    let w = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    let h = if table.braille {
        rows.len().div_ceil(2)
    } else {
        rows.len()
    };
    buf.reset(w, h);
    if table.braille {
        for r in 0..h {
            let row_a = rows.get(r * 2).copied().unwrap_or(&[]);
            let row_b = rows.get(r * 2 + 1).copied().unwrap_or(&[]);
            for c in 0..w {
                let a = aligned_sample(row_a, c, w);
                let b = aligned_sample(row_b, c, w);
                let a = a.filter(|value| !value.is_nan());
                let b = b.filter(|value| !value.is_nan());
                if a.is_none() && b.is_none() {
                    continue;
                }
                let fg = a
                    .or(b)
                    .map(|v| (clamp01(v) * 7.0).round() as u8)
                    .unwrap_or(0);
                let bg = if a.is_some() {
                    b.map(|v| (clamp01(v) * 7.0).round() as u8)
                } else {
                    None
                };
                buf.put(
                    c,
                    r,
                    RCell {
                        ch: if a.is_some() {
                            table.half_upper
                        } else {
                            table.half_lower
                        },
                        fg,
                        bg,
                    },
                );
            }
        }
    } else {
        for (r, row) in rows.iter().enumerate() {
            for c in 0..w {
                let Some(v) = aligned_sample(row, c, w) else {
                    continue;
                };
                if v.is_nan() {
                    continue;
                }
                let value = clamp01(v);
                let density =
                    (value * (table.heat.len().saturating_sub(1)) as f32).round() as usize;
                buf.put(
                    c,
                    r,
                    RCell {
                        ch: table.heat[density],
                        fg: (value * 7.0).round() as u8,
                        bg: None,
                    },
                );
            }
        }
    }
}

/// Draws one sparkline row.
pub fn sparkline(vals: &[f32], table: &RasterTable, buf: &mut CellBuf) {
    sparkline_width(vals, vals.len(), table, buf);
}

/// Draws a one-row sparkline aligned within an explicit cell capacity.
pub fn sparkline_width(vals: &[f32], w: usize, table: &RasterTable, buf: &mut CellBuf) {
    buf.reset(w, 1);
    if table.spark.is_empty() {
        return;
    }
    let off = sample_offset(vals.len(), w);
    let start = vals.len().saturating_sub(w);
    for (index, &v) in vals[start..].iter().enumerate() {
        let c = off + index;
        let value = clamp01(v);
        let idx = (value * (table.spark.len() - 1) as f32).round() as usize;
        buf.put(
            c,
            0,
            RCell {
                ch: table.spark[idx],
                fg: (value * 7.0).round() as u8,
                bg: None,
            },
        );
    }
}

/// Draws a filled meter and track in one row, retaining fractional eighths.
pub fn meter(frac: f32, w: usize, table: &RasterTable, buf: &mut CellBuf) {
    buf.reset(w, 1);
    let value = clamp01(frac);
    let cells = value * w as f32;
    let full = (cells.floor() as usize).min(w);
    let rem = ((cells - full as f32) * 8.0).round() as usize;
    for c in 0..w {
        let fg = (c.saturating_mul(8).checked_div(w).unwrap_or(0)).min(7) as u8;
        let ch = if c < full {
            table.hbar8[7]
        } else if c == full && rem > 0 {
            table.hbar8[(rem - 1).min(7)]
        } else {
            table.track
        };
        buf.put(c, 0, RCell { ch, fg, bg: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthChar;

    fn chars(buf: &CellBuf) -> Vec<String> {
        (0..buf.height())
            .map(|r| buf.row(r).iter().map(|cell| cell.ch).collect())
            .collect()
    }

    fn glyphs_of(table: &RasterTable) -> impl Iterator<Item = char> + '_ {
        table
            .ramp8
            .iter()
            .chain(table.hbar8.iter())
            .chain(table.shade4.iter())
            .copied()
            .chain([
                table.half_upper,
                table.half_lower,
                table.track,
                table.blank,
                table.cell_blank,
            ])
            .chain(table.cols.iter().copied())
            .chain(table.heat.iter().copied())
            .chain(table.spark.iter().copied())
    }

    #[test]
    fn full_area_matches_viz_braille_graph() {
        for (vals, w, h) in [
            (&[0.0, 0.25, 0.5, 1.0][..], 2, 2),
            (&[0.2, 0.4, 0.6, 0.8, 1.0][..], 2, 3),
        ] {
            let mut buf = CellBuf::new();
            buf.reset(w, h);
            area(vals, &RasterTable::FULL, &mut buf);
            assert_eq!(chars(&buf), crate::viz::braille_graph(vals, w, h));
        }
    }

    #[test]
    fn full_line_matches_viz_braille_line() {
        let vals = [0.0, 0.3, 0.8, 0.4];
        let (w, h) = (2, 3);
        let mut buf = CellBuf::new();
        buf.reset(w, h);
        line(&vals, &RasterTable::FULL, &mut buf);
        assert_eq!(chars(&buf), crate::viz::braille_line(&vals, w, h));
    }

    #[test]
    fn full_band_matches_viz_braille_band() {
        let lo = [0.1, 0.3, 0.6, 0.4];
        let hi = [0.5, 0.7, 0.9, 0.8];
        let (w, h) = (2, 3);
        let mut buf = CellBuf::new();
        buf.reset(w, h);
        band(&lo, &hi, &RasterTable::FULL, &mut buf);
        assert_eq!(chars(&buf), crate::viz::braille_band(&lo, &hi, w, h));
    }

    #[test]
    fn ascii_area_line_band_and_mirror_have_golden_rows() {
        let mut b = CellBuf::new();
        b.reset(4, 1);
        area(&[0.0, 0.25, 0.5, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" .:#"]);
        b.reset(4, 2);
        line(&[0.0, 0.25, 0.5, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["   #", "..: "]);
        b.reset(2, 2);
        band(&[0.0, 0.5], &[0.5, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" :", ": "]);
        b.reset(1, 4);
        mirrored(&[1.0], &[1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["#", "#", "#", "#"]);
    }

    #[test]
    fn short_series_right_align_across_rasters_and_unicode_tables() {
        for table in [&RasterTable::FULL, &RasterTable::BASIC, &RasterTable::ASCII] {
            let blank = table.cell_blank;
            let mut b = CellBuf::new();

            b.reset(4, 1);
            area(&[1.0], table, &mut b);
            assert_eq!(
                b.row(0)[..3].iter().map(|cell| cell.ch).collect::<String>(),
                blank.to_string().repeat(3)
            );
            assert_ne!(b.row(0)[3].ch, blank);

            b.reset(4, 1);
            line(&[1.0], table, &mut b);
            assert_eq!(
                b.row(0)[..3].iter().map(|cell| cell.ch).collect::<String>(),
                blank.to_string().repeat(3)
            );
            assert_ne!(b.row(0)[3].ch, blank);

            b.reset(4, 1);
            columns(&[1.0], &table.ramp8, table, &mut b);
            assert_eq!(
                b.row(0)[..3].iter().map(|cell| cell.ch).collect::<String>(),
                blank.to_string().repeat(3)
            );
            assert_ne!(b.row(0)[3].ch, blank);

            b.reset(4, 1);
            band(&[0.5], &[1.0], table, &mut b);
            assert_eq!(
                b.row(0)[..3].iter().map(|cell| cell.ch).collect::<String>(),
                blank.to_string().repeat(3)
            );
            assert_ne!(b.row(0)[3].ch, blank);

            b.reset(4, 2);
            mirrored(&[1.0], &[1.0], table, &mut b);
            for row in 0..2 {
                assert_eq!(
                    b.row(row)[..3]
                        .iter()
                        .map(|cell| cell.ch)
                        .collect::<String>(),
                    blank.to_string().repeat(3)
                );
                assert_ne!(b.row(row)[3].ch, blank);
            }

            b.reset(4, 1);
            stacked(&[&[1.0]], table, &mut b);
            assert_eq!(
                b.row(0)[..3].iter().map(|cell| cell.ch).collect::<String>(),
                blank.to_string().repeat(3)
            );
            assert_ne!(b.row(0)[3].ch, blank);

            let rows = [
                &[0.0, 0.0, 0.0, 0.0][..],
                &[0.0, 0.0, 0.0, 0.0][..],
                &[1.0][..],
            ];
            heatmap(&rows, table, &mut b);
            let populated_row = if table.braille { 1 } else { 2 };
            assert_eq!(
                b.row(populated_row)[..3]
                    .iter()
                    .map(|cell| cell.ch)
                    .collect::<String>(),
                blank.to_string().repeat(3)
            );
            assert_ne!(b.row(populated_row)[3].ch, blank);

            sparkline_width(&[1.0], 4, table, &mut b);
            assert_eq!(
                b.row(0)[..3].iter().map(|cell| cell.ch).collect::<String>(),
                blank.to_string().repeat(3)
            );
            assert_ne!(b.row(0)[3].ch, blank);
        }
    }

    #[test]
    fn short_raster_glyphs_are_golden_and_long_inputs_keep_newest_samples() {
        for table in [&RasterTable::FULL, &RasterTable::BASIC] {
            let mut b = CellBuf::new();
            b.reset(1, 1);
            area(&[1.0], table, &mut b);
            assert_eq!(chars(&b), ["\u{28b8}"]);
        }

        let mut b = CellBuf::new();
        b.reset(4, 1);
        area(&[0.5, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["  :#"]);

        b.reset(2, 1);
        columns(
            &[0.0, 0.0, 1.0],
            &RasterTable::ASCII.ramp8,
            &RasterTable::ASCII,
            &mut b,
        );
        assert_eq!(chars(&b), [" #"]);

        b.reset(2, 1);
        stacked(&[&[0.0, 0.0, 1.0]], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" #"]);

        let rows = [&[0.0, 0.0, 1.0][..], &[1.0][..]];
        heatmap(&rows, &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["  @", "  @"]);

        sparkline_width(&[0.0, 0.0, 1.0], 2, &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["_#"]);
    }

    #[test]
    fn ascii_renderer_goldens_pin_short_exact_and_newest_long_samples() {
        let mut b = CellBuf::new();

        // One sample in a two-column raster is right-aligned.
        b.reset(2, 2);
        area(&[0.25], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["  ", " :"]);
        b.reset(2, 2);
        line(&[0.25], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["  ", " ."]);
        b.reset(2, 2);
        band(&[0.25], &[0.75], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" :", " :"]);
        b.reset(2, 2);
        columns(
            &[0.25],
            RasterTable::ASCII.cols,
            &RasterTable::ASCII,
            &mut b,
        );
        assert_eq!(chars(&b), ["  ", " :"]);
        b.reset(2, 2);
        mirrored(&[0.25], &[0.25], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" .", " ."]);
        b.reset(2, 2);
        stacked(&[&[0.25]], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["  ", " -"]);
        sparkline_width(&[0.5], 2, &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" -"]);

        // Exact-width inputs map sample i to column i.
        b.reset(2, 2);
        area(&[0.0, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" #", " #"]);
        b.reset(2, 2);
        line(&[0.0, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" #", ". "]);
        b.reset(2, 2);
        band(&[0.0, 0.25], &[0.5, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" |", ":|"]);
        b.reset(2, 2);
        columns(
            &[0.0, 1.0],
            RasterTable::ASCII.cols,
            &RasterTable::ASCII,
            &mut b,
        );
        assert_eq!(chars(&b), [" #", " #"]);
        b.reset(2, 2);
        mirrored(&[0.0, 1.0], &[0.0, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" #", " #"]);
        b.reset(2, 2);
        stacked(&[&[0.0, 1.0]], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" #", " #"]);
        sparkline_width(&[0.0, 1.0], 2, &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["_#"]);

        // Values before the two-cell window are discarded; the latest pair
        // determines every renderer's rightmost two columns.
        b.reset(2, 2);
        columns(
            &[0.0, 0.0, 0.5, 1.0],
            RasterTable::ASCII.cols,
            &RasterTable::ASCII,
            &mut b,
        );
        assert_eq!(chars(&b), [" #", "##"]);
        b.reset(2, 2);
        area(&[0.0, 0.0, 0.5, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" #", "##"]);
        b.reset(2, 2);
        line(&[0.0, 0.0, 0.5, 1.0], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" #", ": "]);
        b.reset(2, 2);
        band(
            &[0.0, 0.0, 0.25, 0.5],
            &[0.0, 0.0, 0.75, 1.0],
            &RasterTable::ASCII,
            &mut b,
        );
        assert_eq!(chars(&b), ["::", ": "]);
        b.reset(2, 2);
        mirrored(
            &[0.0, 0.0, 0.5, 1.0],
            &[0.0, 0.0, 0.5, 1.0],
            &RasterTable::ASCII,
            &mut b,
        );
        assert_eq!(chars(&b), [":#", ":#"]);
        b.reset(2, 2);
        stacked(&[&[0.0, 0.0, 0.5, 1.0]], &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" #", "##"]);
        sparkline_width(&[0.0, 0.0, 0.5, 1.0], 2, &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), ["-#"]);

        let heat_rows = [&[0.0, 0.5, 1.0][..], &[0.5][..]];
        heatmap(&heat_rows, &RasterTable::ASCII, &mut b);
        assert_eq!(chars(&b), [" +@", "  +"]);
    }

    #[test]
    fn full_and_basic_short_and_exact_unicode_glyph_goldens() {
        for table in [&RasterTable::FULL, &RasterTable::BASIC] {
            let mut b = CellBuf::new();
            b.reset(1, 1);
            area(&[1.0], table, &mut b);
            assert_eq!(chars(&b), ["⢸"]);
            area(&[1.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["⣿"]);

            line(&[1.0], table, &mut b);
            assert_eq!(chars(&b), ["⠈"]);
            line(&[1.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["⠉"]);

            band(&[0.0], &[1.0], table, &mut b);
            assert_eq!(chars(&b), ["⢸"]);
            band(&[0.0, 0.0], &[1.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["⣿"]);

            columns(&[1.0], &table.ramp8, table, &mut b);
            assert_eq!(chars(&b), ["█"]);
            b.reset(2, 1);
            columns(&[1.0], &table.ramp8, table, &mut b);
            assert_eq!(chars(&b), [" █"]);
            columns(&[1.0, 1.0], &table.ramp8, table, &mut b);
            assert_eq!(chars(&b), ["█"]);

            b.reset(1, 2);
            mirrored(&[1.0], &[1.0], table, &mut b);
            assert_eq!(chars(&b), ["⢸", "⢸"]);
            mirrored(&[1.0, 1.0], &[1.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["⣿", "⣿"]);

            b.reset(1, 1);
            stacked(&[&[1.0]], table, &mut b);
            assert_eq!(chars(&b), ["█"]);
            b.reset(2, 1);
            stacked(&[&[1.0]], table, &mut b);
            assert_eq!(chars(&b), [" █"]);
            b.reset(1, 1);
            stacked(&[&[1.0, 1.0]], table, &mut b);
            assert_eq!(chars(&b), ["█"]);

            let short_heat = [&[1.0][..]];
            heatmap(&short_heat, table, &mut b);
            assert_eq!(chars(&b), ["▀"]);
            let exact_heat = [&[1.0, 1.0][..]];
            heatmap(&exact_heat, table, &mut b);
            assert_eq!(chars(&b), ["▀▀"]);
            let long_heat = [&[0.0, 0.5, 1.0][..], &[1.0][..], &[0.5][..]];
            heatmap(&long_heat, table, &mut b);
            assert_eq!(chars(&b), ["▀▀▀", "  ▀"]);

            sparkline_width(&[1.0], 1, table, &mut b);
            assert_eq!(chars(&b), ["█"]);
            sparkline_width(&[1.0], 2, table, &mut b);
            assert_eq!(chars(&b), [" █"]);
            sparkline_width(&[1.0, 1.0], 1, table, &mut b);
            assert_eq!(chars(&b), ["█"]);

            // The two newest braille samples fill the right dot column; the
            // older full samples must not survive the one-cell window.
            area(&[1.0, 1.0, 0.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["⢸"]);
            line(&[1.0, 1.0, 0.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["⣸"]);
            band(&[1.0, 1.0, 0.0, 0.0], &[1.0, 1.0, 0.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["⣸"]);
            columns(&[0.0, 0.0, 1.0], &table.ramp8, table, &mut b);
            assert_eq!(chars(&b), ["█"]);
            b.reset(1, 2);
            mirrored(&[1.0, 1.0, 0.0, 1.0], &[1.0, 1.0, 0.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["⢸", "⢸"]);
            b.reset(1, 1);
            stacked(&[&[0.0, 0.0, 1.0]], table, &mut b);
            assert_eq!(chars(&b), ["█"]);
            sparkline_width(&[0.0, 0.0, 1.0], 1, table, &mut b);
            assert_eq!(chars(&b), ["█"]);
        }
    }

    #[test]
    fn full_basic_and_ascii_golden_for_remaining_rasters() {
        let mut b = CellBuf::new();
        for table in [&RasterTable::FULL, &RasterTable::BASIC, &RasterTable::ASCII] {
            sparkline(&[0.0, 0.5, 1.0], table, &mut b);
            assert_eq!(chars(&b), [if table.braille { "▁▅█" } else { "_-#" }]);
            meter(0.625, 4, table, &mut b);
            assert_eq!(
                chars(&b),
                [if table.braille {
                    "██▌░"
                } else {
                    "##=-"
                }]
            );
            b.reset(1, 4);
            stacked(&[&[0.25]], table, &mut b);
            assert_eq!(
                chars(&b),
                if table.braille {
                    [" ", " ", " ", "█"]
                } else {
                    [" ", " ", " ", "#"]
                }
            );
            let instances = [&[0.0, 0.5][..], &[1.0, 0.25][..], &[0.75][..]];
            heatmap(&instances, table, &mut b);
            assert_eq!(b.height(), if table.braille { 2 } else { 3 });
            assert_eq!(b.width(), 2);
            if table.braille {
                assert_eq!(chars(&b)[0], "▀▀");
            } else {
                assert_eq!(chars(&b), [" +", "@:", " #"]);
            }
        }
    }

    #[test]
    fn full_and_basic_area_line_band_and_mirror_golden_rows() {
        for table in [&RasterTable::FULL, &RasterTable::BASIC] {
            let mut b = CellBuf::new();
            b.reset(1, 1);
            area(&[0.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["\u{28b8}"]);
            line(&[0.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["\u{28f8}"]);
            band(&[0.0, 0.0], &[0.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["\u{28f8}"]);
            b.reset(1, 2);
            mirrored(&[1.0, 1.0], &[1.0, 1.0], table, &mut b);
            assert_eq!(chars(&b), ["\u{28ff}", "\u{28ff}"]);
        }
    }

    #[test]
    fn cell_buffer_access_and_columns_cover_bounds_and_steps() {
        let mut b = CellBuf::new();
        assert_eq!(b.width(), 0);
        assert_eq!(b.height(), 0);
        assert!(b.row(0).is_empty());
        assert_eq!(b.get(0, 0), None);
        b.reset(2, 2);
        assert_eq!(b.get(1, 1), Some(&RCell::blank(' ')));
        columns(
            &[0.5, 1.0],
            &RasterTable::FULL.ramp8,
            &RasterTable::FULL,
            &mut b,
        );
        assert_eq!(chars(&b), [" █", "██"]);
        columns(&[1.0], &[], &RasterTable::FULL, &mut b);
        assert!(chars(&b).iter().all(|row| row.chars().all(|ch| ch == ' ')));
    }

    #[test]
    fn gradient_levels_are_monotone_by_row() {
        for (table, steps) in [
            (&RasterTable::ASCII, RasterTable::ASCII.cols),
            (&RasterTable::FULL, &RasterTable::FULL.ramp8[..]),
        ] {
            let mut b = CellBuf::new();
            b.reset(1, 8);
            columns(&[1.0], steps, table, &mut b);
            let levels: Vec<u8> = (0..8).rev().map(|r| b.row(r)[0].fg).collect();
            assert!(levels.windows(2).all(|pair| pair[0] <= pair[1]));
            assert_eq!(levels, (0..8).collect::<Vec<_>>());
            assert_eq!(levels[7], 7);
        }
    }

    #[test]
    fn stacked_boundary_within_one_eighth() {
        for step in 7..=93 {
            let lower = step as f32 / 100.0;
            let upper = 1.0 - lower;
            let mut b = CellBuf::new();
            b.reset(1, 1);
            stacked(&[&[lower], &[upper]], &RasterTable::FULL, &mut b);
            let ch = b.row(0)[0].ch;
            let got = if ch == '⠀' {
                0.0
            } else {
                (RasterTable::FULL
                    .ramp8
                    .iter()
                    .position(|glyph| *glyph == ch)
                    .unwrap_or(7)
                    + 1) as f32
                    / 8.0
            };
            assert!((got - lower).abs() <= 1.0 / 8.0);
            assert!(b.row(0)[0].bg.is_some());
        }

        for step in 0..=10 {
            let lower = 0.16 + step as f32 * 0.008;
            let upper = 0.38 + step as f32 * 0.005;
            let mut b = CellBuf::new();
            b.reset(1, 4);
            stacked(&[&[lower], &[upper]], &RasterTable::FULL, &mut b);

            // The inter-band boundary lies in the bottom cell; the stack's
            // partial top lies two cells above it.
            assert_eq!(b.row(3)[0].fg, 0);
            assert_eq!(b.row(3)[0].bg, Some(1));
            let boundary_eighths = ((lower * 4.0 * 8.0).round() as usize).clamp(1, 8);
            assert_eq!(
                b.row(3)[0].ch,
                RasterTable::FULL.ramp8[boundary_eighths - 1]
            );
            assert_eq!(b.row(1)[0].fg, 1);
            assert_eq!(b.row(1)[0].bg, None);
            let total_top = lower + upper;
            let top_eighths = (((total_top - 0.5) * 4.0 * 8.0).round() as usize).clamp(1, 8);
            assert_eq!(b.row(1)[0].ch, RasterTable::FULL.ramp8[top_eighths - 1]);
        }

        let mut b = CellBuf::new();
        b.reset(1, 4);
        stacked(&[&[0.375], &[0.25]], &RasterTable::FULL, &mut b);
        assert_eq!(b.row(2)[0].ch, '▄');
        assert_eq!(b.row(2)[0].fg, 0);
        assert_eq!(b.row(2)[0].bg, Some(1));
        assert_eq!(b.row(1)[0].ch, '▄');
        assert_eq!(b.row(1)[0].fg, 1);
        assert_eq!(b.row(1)[0].bg, None);
        assert_eq!(b.row(0)[0].ch, ' ');

        b.reset(1, 1);
        stacked(&[&[0.25], &[0.25]], &RasterTable::FULL, &mut b);
        assert_eq!(b.row(0)[0].ch, '▄');
        assert_eq!(b.row(0)[0].fg, 0);
        assert_eq!(b.row(0)[0].bg, None);
    }

    #[test]
    fn heatmap_packs_two_instances_per_row() {
        let rows = [&[0.0][..], &[0.5][..], &[1.0][..]];
        let mut b = CellBuf::new();
        heatmap(&rows, &RasterTable::FULL, &mut b);
        assert_eq!(b.height(), 2);
        assert_eq!(b.row(1)[0].bg, None);

        let rows = [&[f32::NAN][..], &[0.5][..], &[1.0][..], &[f32::NAN][..]];
        heatmap(&rows, &RasterTable::FULL, &mut b);
        assert_eq!(b.row(0)[0].ch, RasterTable::FULL.half_lower);
        assert_eq!(b.row(0)[0].fg, 4);
        assert_eq!(b.row(0)[0].bg, None);
        assert_eq!(b.row(1)[0].ch, RasterTable::FULL.half_upper);
        assert_eq!(b.row(1)[0].fg, 7);
        assert_eq!(b.row(1)[0].bg, None);

        let rows = [&[f32::NAN][..], &[f32::NAN][..]];
        heatmap(&rows, &RasterTable::FULL, &mut b);
        assert_eq!(b.row(0)[0].ch, ' ');
    }

    #[test]
    fn mirrored_grows_away_from_axis() {
        for table in [&RasterTable::FULL, &RasterTable::ASCII] {
            let mut b = CellBuf::new();
            b.reset(1, 4);
            mirrored(&[0.25, 0.25], &[0.25, 0.25], table, &mut b);
            assert_eq!(b.row(0)[0].ch, table.blank);
            assert_ne!(b.row(1)[0].ch, table.blank);
            assert_ne!(b.row(2)[0].ch, table.blank);
            assert_eq!(b.row(3)[0].ch, table.blank);
        }
    }

    #[test]
    fn nan_is_an_empty_column_and_over_supply_drops_oldest() {
        let mut b = CellBuf::new();
        b.reset(1, 2);
        area(&[f32::NAN, 1.0], &RasterTable::FULL, &mut b);
        for row in 0..b.height() {
            assert_eq!((b.row(row)[0].ch as u32 - 0x2800) & 0x47, 0);
        }
        area(&[0.0, 0.0, 1.0, 1.0], &RasterTable::FULL, &mut b);
        let mut expected = CellBuf::new();
        expected.reset(1, 2);
        area(&[1.0, 1.0], &RasterTable::FULL, &mut expected);
        assert_eq!(chars(&b), chars(&expected));
    }

    #[test]
    fn redraw_reuses_the_buffer() {
        let mut b = CellBuf::new();
        b.reset(4, 3);
        area(&[1.0; 8], &RasterTable::FULL, &mut b);
        let ptr = b.cells.as_ptr();
        let cap = b.cells.capacity();
        area(&[0.5; 8], &RasterTable::FULL, &mut b);
        assert_eq!(b.cells.as_ptr(), ptr);
        assert_eq!(b.cells.capacity(), cap);
    }

    #[test]
    fn every_table_glyph_is_one_cell_and_levels_map() {
        for table in [&RasterTable::FULL, &RasterTable::BASIC, &RasterTable::ASCII] {
            assert!(glyphs_of(table).all(|ch| UnicodeWidthChar::width(ch) == Some(1)));
        }
        assert!(glyphs_of(&RasterTable::ASCII).all(|ch| ch.is_ascii()));
        assert_eq!(
            *RasterTable::for_level(UnicodeLevel::Full),
            RasterTable::FULL
        );
        assert_eq!(
            *RasterTable::for_level(UnicodeLevel::Basic),
            RasterTable::BASIC
        );
        assert_eq!(
            *RasterTable::for_level(UnicodeLevel::Ascii),
            RasterTable::ASCII
        );
        assert!(!RasterTable::for_level(UnicodeLevel::Ascii).braille);
        assert_eq!(RasterTable::BASIC, RasterTable::FULL);
    }
}
