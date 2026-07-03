//! Branded diagnostics. Once `crate::log::init` has installed the `tracing`
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
use std::sync::atomic::{AtomicBool, Ordering};

/// Set true while the native compositor owns the raw/alternate screen. When set,
/// `info`/`warn`/`error` route to `tracing` (a no-op if no subscriber is
/// installed) instead of `eprintln!` — a direct stderr write would paint over
/// the alt-screen frame (the same reason `log_trace::Role::Host` drops its
/// stderr layer). This matters even without `SUPERZEJ_LOG`, where no subscriber
/// is installed and `ready()` is false, so the plain fallback would otherwise
/// corrupt the frame (e.g. off-loop provisioning `msg::warn`s). `die` still
/// writes to stderr — a fatal must be seen even if it scars the frame on exit.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Toggle the alt-screen guard (see `TUI_ACTIVE`). The compositor sets it true
/// right after entering the alternate screen and false on teardown.
pub fn set_tui_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::Relaxed);
}

/// Whether the compositor owns the raw/alternate screen (see `TUI_ACTIVE`).
/// Callers that spawn subprocesses with inheritable stdio consult this to
/// capture their output instead of letting it paint over the frame.
pub fn tui_active() -> bool {
    TUI_ACTIVE.load(Ordering::Relaxed)
}

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
    if crate::log_trace::ready() || tui_active() {
        tracing::info!("{s}");
    } else {
        eprintln!("{} {s}", tag(theme::DIM));
    }
}

pub fn warn(s: &str) {
    if crate::log_trace::ready() || tui_active() {
        tracing::warn!("{s}");
    } else {
        eprintln!("{} {s}", tag(theme::AMBER));
    }
}

pub fn error(s: &str) {
    if crate::log_trace::ready() || tui_active() {
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
