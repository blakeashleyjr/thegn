//! The stdout *data* seam.
//!
//! thegn keeps a hard split: machine-readable results (JSON, TSV, resolved
//! paths, the values plugins parse) go to **stdout** via `out!`/`outln!`;
//! diagnostics go to **stderr** via `tracing`/`msg`. Clippy bans bare
//! `println!`/`print!` (see clippy.toml) so the two never get crossed — a stray
//! diagnostic `println!` corrupting the plugins' stdout contract is a real bug
//! class this seam exists to prevent.
#![allow(clippy::disallowed_macros)] // this module IS the sanctioned stdout path

use std::io::Write;

/// Write data to stdout with a trailing newline (drop-in for `println!`).
#[macro_export]
macro_rules! outln {
    () => { $crate::out::_line(std::format_args!("")) };
    ($($arg:tt)*) => { $crate::out::_line(std::format_args!($($arg)*)) };
}

/// Write data to stdout with no trailing newline (drop-in for `print!`).
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => { $crate::out::_raw(std::format_args!($($arg)*)) };
}

#[doc(hidden)]
pub fn _line(args: std::fmt::Arguments) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_fmt(args);
    let _ = o.write_all(b"\n");
}

#[doc(hidden)]
pub fn _raw(args: std::fmt::Arguments) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_fmt(args);
}
