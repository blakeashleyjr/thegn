//! Branded diagnostics. Once [`crate::log::init`] has installed the `tracing`
//! subscriber, `info`/`warn`/`error` route through it (level-filtered, mirrored
//! to the log file with the same `✦ superzej` look). Before that — and always
//! for `die` — they print straight to stderr so early config errors and fatals
//! are never lost.
//!
//! Most code should keep calling these; new code wanting per-module filtering or
//! `debug!`/`trace!` can use `tracing` macros directly.
#![allow(clippy::disallowed_macros)] // this module is the stderr fallback

use crate::theme;
use std::io::IsTerminal;

/// A "✦ superzej" prefix in the given hue (faint star + faint name), tty-gated.
fn tag(hue: &str) -> String {
    if std::io::stderr().is_terminal() {
        format!(
            "\x1b[38;2;{}m\u{2726}\x1b[0m \x1b[38;2;{hue}msuperzej\x1b[0m",
            theme::MAGENTA
        )
    } else {
        "superzej:".into()
    }
}

pub fn info(s: &str) {
    if crate::log_trace::ready() {
        tracing::info!("{s}");
    } else {
        eprintln!("{} {s}", tag(theme::DIM));
    }
}

pub fn warn(s: &str) {
    if crate::log_trace::ready() {
        tracing::warn!("{s}");
    } else {
        eprintln!("{} {s}", tag(theme::AMBER));
    }
}

pub fn error(s: &str) {
    if crate::log_trace::ready() {
        tracing::error!("{s}");
    } else {
        eprintln!("{} {s}", tag(theme::RED));
    }
}

/// Print an error and exit non-zero. Always goes straight to stderr (branded),
/// regardless of the subscriber or level filter — a fatal must be seen.
pub fn die(s: &str) -> ! {
    eprintln!("{} {s}", tag(theme::RED));
    std::process::exit(1);
}
