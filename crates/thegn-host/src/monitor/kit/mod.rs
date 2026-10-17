//! F5a monitor widgets. F6b is the first production caller; this tree is
//! currently exercised by its unit tests.
#![allow(dead_code)]

pub(crate) mod axis;
pub(crate) mod chip;
pub(crate) mod emit;
pub(crate) mod grid;
pub(crate) mod legend;
pub(crate) mod meter;
pub(crate) mod panel;
pub(crate) mod table;

#[cfg(test)]
mod tests;
