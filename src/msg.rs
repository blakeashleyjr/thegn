//! Minimal stderr logging (coloured when stderr is a tty).

use std::io::IsTerminal;

fn color(code: &str) -> &str {
    if std::io::stderr().is_terminal() {
        code
    } else {
        ""
    }
}

pub fn info(s: &str) {
    eprintln!("{}superzej:{} {s}", color("\x1b[2m"), color("\x1b[0m"));
}

pub fn warn(s: &str) {
    eprintln!("{}superzej:{} {s}", color("\x1b[33m"), color("\x1b[0m"));
}

pub fn error(s: &str) {
    eprintln!("{}superzej:{} {s}", color("\x1b[31m"), color("\x1b[0m"));
}

/// Print an error and exit non-zero.
pub fn die(s: &str) -> ! {
    error(s);
    std::process::exit(1);
}
