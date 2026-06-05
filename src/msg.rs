//! Minimal stderr logging (coloured when stderr is a tty).

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
    eprintln!("{} {s}", tag(theme::DIM));
}

pub fn warn(s: &str) {
    eprintln!("{} {s}", tag(theme::AMBER));
}

pub fn error(s: &str) {
    eprintln!("{} {s}", tag(theme::RED));
}

/// Print an error and exit non-zero.
pub fn die(s: &str) -> ! {
    error(s);
    std::process::exit(1);
}
