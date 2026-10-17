use super::grid::{CellGrid, CellStyle, GridView};
use crate::{
    caps::{self, Glyph},
    compositor::Rect,
};
use thegn_core::numfmt::{NumFmt, Unit};

pub fn y_gutter(grid: &mut CellGrid, rect: Rect, max: f64, unit: Unit, fmt: NumFmt) {
    let mut v = GridView::new(grid, rect);
    if v.rect().cols == 0 || v.rect().rows == 0 {
        return;
    }
    let width = unit.default_width().min(v.rect().cols);
    let label_width = width.saturating_sub(1);
    let values = [
        (0, max),
        (v.rect().rows / 2, max / 2.0),
        (v.rect().rows - 1, 0.0),
    ];
    let mut taken = [usize::MAX; 3];
    let mut count = 0usize;
    for (index, (row, value)) in values.into_iter().enumerate() {
        if index == 1 && v.rect().rows <= 2 {
            continue;
        }
        if taken[..count].contains(&row) {
            continue;
        }
        taken[count] = row;
        count += 1;
        let label = fmt.fmt(value, unit, label_width);
        v.put_str(0, row, &label, CellStyle::default());
        if width > 0 {
            v.put(
                width - 1,
                row,
                CellStyle {
                    ch: caps::glyph(Glyph::AxisTick).chars().next().unwrap_or('|'),
                    ..CellStyle::default()
                },
            );
        }
    }
}

pub fn x_ticks(grid: &mut CellGrid, rect: Rect, labels: &[&str]) {
    let mut v = GridView::new(grid, rect);
    if v.rect().rows == 0 || v.rect().cols == 0 {
        return;
    }
    let cols = v.rect().cols;
    if cols == 1 {
        v.put(
            0,
            0,
            CellStyle {
                ch: caps::glyph(Glyph::NowMarker).chars().next().unwrap_or('|'),
                ..CellStyle::default()
            },
        );
        return;
    }
    let stride = cols / labels.len().max(1);
    let mut last_end = 0usize;
    for (i, label) in labels.iter().enumerate() {
        let x = (i * stride).min(cols.saturating_sub(1));
        if x > last_end {
            v.put(
                x,
                0,
                CellStyle {
                    ch: caps::glyph(Glyph::AxisTick).chars().next().unwrap_or('|'),
                    ..CellStyle::default()
                },
            );
        }
        let start = x.saturating_add(1);
        if start < cols.saturating_sub(1) && start > last_end {
            let limit = cols.saturating_sub(1);
            let mut used = 0usize;
            for ch in label.chars() {
                let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if start + used + char_width > limit {
                    break;
                }
                v.put(
                    start + used,
                    0,
                    CellStyle {
                        ch,
                        ..CellStyle::default()
                    },
                );
                used += char_width;
            }
            last_end = start.saturating_add(used).saturating_add(1);
        }
    }
    v.put(
        cols - 1,
        0,
        CellStyle {
            ch: caps::glyph(Glyph::NowMarker).chars().next().unwrap_or('|'),
            ..CellStyle::default()
        },
    );
}
