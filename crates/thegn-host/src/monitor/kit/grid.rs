use crate::compositor::Rect;
use crate::seg::StyleTok;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const BOLD: u8 = 1;
pub const DIM: u8 = 2;
pub const ITALIC: u8 = 4;
pub const UNDER: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellStyle {
    pub ch: char,
    pub fg: StyleTok,
    pub bg: StyleTok,
    pub attr: u8,
}
impl Default for CellStyle {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: StyleTok::from_tok(crate::seg::Tok::Slot(crate::chrome::S::Text))
                .unwrap_or(StyleTok::INHERIT),
            bg: StyleTok::INHERIT,
            attr: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CellGrid {
    cols: usize,
    rows: usize,
    cells: Vec<CellStyle>,
}

impl CellGrid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut grid = Self::default();
        grid.resize(cols, rows);
        grid
    }
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let len = cols.saturating_mul(rows);
        if len > self.cells.len() {
            self.cells.resize(len, CellStyle::default());
        } else {
            self.cells.truncate(len);
        }
        self.cols = cols;
        self.rows = rows;
    }
    pub fn clear(&mut self) {
        self.cells.fill(CellStyle::default());
    }
    pub const fn cols(&self) -> usize {
        self.cols
    }
    pub const fn rows(&self) -> usize {
        self.rows
    }
    pub fn view(&mut self, rect: Rect) -> GridView<'_> {
        GridView::new(self, rect)
    }
    pub fn get(&self, x: usize, y: usize) -> Option<CellStyle> {
        (x < self.cols && y < self.rows).then(|| self.cells[y * self.cols + x])
    }
}

pub struct GridView<'a> {
    grid: &'a mut CellGrid,
    rect: Rect,
}
impl<'a> GridView<'a> {
    pub fn new(grid: &'a mut CellGrid, rect: Rect) -> Self {
        let x = rect.x.min(grid.cols);
        let y = rect.y.min(grid.rows);
        let cols = rect.cols.min(grid.cols.saturating_sub(x));
        let rows = rect.rows.min(grid.rows.saturating_sub(y));
        Self {
            grid,
            rect: Rect { x, y, cols, rows },
        }
    }
    pub const fn rect(&self) -> Rect {
        self.rect
    }
    pub fn put(&mut self, x: usize, y: usize, style: CellStyle) {
        if x < self.rect.cols && y < self.rect.rows && self.grid.cols > 0 {
            let gx = self.rect.x + x;
            let gy = self.rect.y + y;
            let i = gy * self.grid.cols + gx;
            let width = style.ch.width().unwrap_or(0);
            if width == 2 && x + 1 >= self.rect.cols {
                return;
            }
            clear_wide_pair(&mut self.grid.cells, self.grid.cols, gx, gy);
            if style.ch == '\0' {
                self.grid.cells[i] = style;
                return;
            }
            if width == 2 && x + 1 < self.rect.cols {
                clear_wide_pair(&mut self.grid.cells, self.grid.cols, gx + 1, gy);
                self.grid.cells[i] = style;
                self.grid.cells[i + 1] = CellStyle { ch: '\0', ..style };
            } else {
                self.grid.cells[i] = style;
            }
        }
    }
    pub fn put_str(&mut self, x: usize, y: usize, text: &str, style: CellStyle) -> usize {
        let mut col = x;
        for ch in text.chars() {
            let width = ch.width().unwrap_or(0);
            if width == 0 {
                continue;
            }
            if col.saturating_add(width) > self.rect.cols {
                break;
            }
            self.put(col, y, CellStyle { ch, ..style });
            col += width;
        }
        col.saturating_sub(x)
    }
    pub fn fill(&mut self, style: CellStyle) {
        for y in 0..self.rect.rows {
            for x in 0..self.rect.cols {
                self.put(x, y, style);
            }
        }
    }
    pub fn hline(&mut self, x: usize, y: usize, len: usize, style: CellStyle) {
        let count = len.min(self.rect.cols.saturating_sub(x.min(self.rect.cols)));
        for dx in 0..count {
            self.put(x + dx, y, style);
        }
    }
    pub fn vline(&mut self, x: usize, y: usize, len: usize, style: CellStyle) {
        let count = len.min(self.rect.rows.saturating_sub(y.min(self.rect.rows)));
        for dy in 0..count {
            self.put(x, y + dy, style);
        }
    }
    pub fn sub(self, rect: Rect) -> GridView<'a> {
        let x = rect.x.min(self.rect.cols);
        let y = rect.y.min(self.rect.rows);
        GridView::new(
            self.grid,
            Rect {
                x: self.rect.x.saturating_add(x),
                y: self.rect.y.saturating_add(y),
                cols: rect.cols.min(self.rect.cols.saturating_sub(x)),
                rows: rect.rows.min(self.rect.rows.saturating_sub(y)),
            },
        )
    }
}

pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn clear_wide_pair(cells: &mut [CellStyle], cols: usize, x: usize, y: usize) {
    let i = y * cols + x;
    if cells[i].ch == '\0' && x > 0 {
        cells[i - 1] = CellStyle::default();
    } else if cells[i].ch.width().unwrap_or(0) == 2 && x + 1 < cols {
        cells[i + 1] = CellStyle::default();
    }
}

/// Write a width-clipped string directly to the grid, adding the active
/// ellipsis when the text does not fit. Returns the number of columns written.
pub fn put_clipped(
    view: &mut GridView<'_>,
    x: usize,
    y: usize,
    text: &str,
    width: usize,
    style: CellStyle,
) -> usize {
    if display_width(text) <= width {
        return view.put_str(x, y, text, style);
    }
    let mark = crate::caps::glyph(crate::caps::Glyph::Ellipsis);
    let mark_width = display_width(mark);
    let text_width = width.saturating_sub(mark_width);
    let mut used = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > text_width {
            break;
        }
        view.put(x + used, y, CellStyle { ch, ..style });
        used += cw;
    }
    if mark_width <= width {
        used + view.put_str(x + used, y, mark, style)
    } else {
        used
    }
}
