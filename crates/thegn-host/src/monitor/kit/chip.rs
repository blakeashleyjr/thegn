use super::grid::{CellGrid, CellStyle, GridView};
use crate::{
    caps,
    compositor::Rect,
    seg::{StyleTok, Tok},
};
use thegn_core::numfmt::{NumFmt, Reading, Unit};

pub fn draw(
    grid: &mut CellGrid,
    rect: Rect,
    label: &str,
    reading: Reading,
    unit: Unit,
    fmt: NumFmt,
    state: thegn_core::theme::StateRole,
) {
    let mut v = GridView::new(grid, rect);
    if v.rect().rows == 0 {
        return;
    }
    let mut text = String::with_capacity(label.len() + unit.default_width() + 1);
    text.push_str(label);
    text.push(' ');
    let value = match reading {
        Reading::Unknown => caps::num_marks().unknown.to_owned(),
        Reading::Warming => caps::num_marks().warming.to_owned(),
        Reading::Denied => caps::num_marks().denied.to_owned(),
        Reading::NotApplicable => caps::num_marks().unknown.to_owned(),
        Reading::Value(_) => fmt.reading(reading, unit, v.rect().cols.saturating_sub(text.len())),
    };
    text.push_str(&value);
    v.put_str(
        0,
        0,
        &text,
        CellStyle {
            fg: StyleTok::from_tok(Tok::State(state)).unwrap_or(StyleTok::INHERIT),
            bg: StyleTok::INHERIT,
            ..CellStyle::default()
        },
    );
}
