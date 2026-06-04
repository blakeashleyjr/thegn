//! `superzej attach [session]` — launch/attach a superzej zellij session.
//!
//! zellij does not allow keybinds inside a layout, so we instead generate a
//! merged config (the user's config.kdl + superzej's keybinds) and start zellij
//! with `--config <merged>`. zellij merges keybinds blocks, so the user's
//! theme/options/keybinds are preserved and their read-only config.kdl is never
//! edited.

use crate::util;
use anyhow::Result;
use std::process::Command;

const KEYBINDS: &str = include_str!("../../layouts/keybinds.kdl");

pub fn run(session: Option<String>) -> Result<()> {
    let session = session.unwrap_or_else(|| "superzej".into());

    let exists = Command::new("zellij")
        .arg("list-sessions")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.split_whitespace().next() == Some(session.as_str()))
        })
        .unwrap_or(false);

    if exists {
        // Keybinds were applied when the session was created; just reattach.
        util::exec_command("zellij", &["attach", &session]);
    }

    // Layout: a name (installed under ~/.config/zellij/layouts) or a path set via
    // SUPERZEJ_LAYOUT (used by `just start` for the dev tree).
    let layout = std::env::var("SUPERZEJ_LAYOUT").unwrap_or_else(|_| "superzej".into());
    let config = generate_config()?;
    util::exec_command(
        "zellij",
        &[
            "--config", &config, "--layout", &layout, "attach", "--create", &session,
        ],
    );
}

/// Write `$XDG_STATE_HOME/superzej/zellij.kdl` = the user's config.kdl followed
/// by superzej's keybinds, and return its path. Regenerated each launch so it
/// tracks the user's config.
fn generate_config() -> Result<String> {
    let dir = util::xdg_state_home().join("superzej");
    std::fs::create_dir_all(&dir)?;
    let out = dir.join("zellij.kdl");

    let user_path = util::xdg_config_home().join("zellij/config.kdl");
    let user = std::fs::read_to_string(&user_path).unwrap_or_default();

    std::fs::write(&out, format!("{user}\n\n{KEYBINDS}\n"))?;
    Ok(out.to_string_lossy().into_owned())
}
