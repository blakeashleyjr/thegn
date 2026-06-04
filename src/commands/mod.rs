pub mod attach;
pub mod close_pane;
pub mod dashboard;
pub mod launch;
pub mod list;
pub mod new_pane;
pub mod new_workspace;
pub mod pick_agent;
pub mod recent;
pub mod repos;
pub mod status;
pub mod tool;

use crate::util;
use std::process::Command;

/// Yes/no confirmation (gum if present, else a y/N stdin prompt).
pub fn confirm(message: &str) -> bool {
    if util::have("gum") {
        return Command::new("gum")
            .args(["confirm", message])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    eprint!("{message} [y/N] ");
    use std::io::{BufRead, Write};
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES")
}
