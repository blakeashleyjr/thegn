//! F5a monitor widgets. F6b is the first production caller; this tree is
//! currently exercised by its unit tests.
#![allow(dead_code)]

pub(crate) mod axis;
pub(crate) mod chip;
pub(crate) mod emit;
pub(crate) mod grid;
pub(crate) mod history_rect;
pub(crate) mod legend;
pub(crate) mod meter;
pub(crate) mod panel;
pub(crate) mod raster;
pub(crate) mod table;

pub(crate) fn edge_fmt(fmt: thegn_core::numfmt::NumFmt) -> thegn_core::numfmt::NumFmt {
    thegn_core::numfmt::NumFmt {
        marks: crate::caps::num_marks(),
        ..fmt
    }
}

#[cfg(test)]
mod tests;
