//! `superzej status` — home-tab inventory + key hints, then an interactive shell.

use crate::commands::list;
use crate::config::Config;
use crate::util;
use anyhow::Result;

pub fn run(cfg: &Config) -> Result<()> {
    println!("\x1b[1msuperzej\x1b[0m — terminal-native worktree IDE\n");
    list::run(cfg, false)?;
    println!(
        "\n  Keys:  Alt-W new workspace   Alt-w new worktree pane   Alt-d dashboard\n         \
         Alt-g lazygit   Alt-y yazi   Alt-e editor   Alt-/ diff\n         \
         Alt-X close pane + remove worktree\n"
    );
    util::exec_shell();
}
