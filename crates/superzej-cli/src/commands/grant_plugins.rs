//! `superzej grant-plugins` — pre-grant zellij plugin permissions for the
//! sidebar + panel so users never see the first-load "Allow? (y/n)" prompt.
//!
//! zellij persists granted permissions in `$XDG_CACHE_HOME/zellij/permissions.kdl`,
//! keyed by the resolved plugin path. We rewrite our own plugin entries (so new
//! permissions propagate to existing installs) and leave any other entries
//! untouched. Run by install.sh and the home-manager activation step.

use crate::{msg, util, zellij};
use anyhow::Result;
use std::fs;

/// Permissions the plugins request in their `load()`.
const PERMS: &[&str] = &[
    "ReadApplicationState",
    "ChangeApplicationState",
    "RunCommands",
    "ReadCliPipes",
];

pub fn run() -> Result<()> {
    if seed()? {
        msg::info("pre-granted zellij permissions for superzej plugins");
    } else {
        msg::info("zellij plugin permissions already present");
    }
    Ok(())
}

/// Rewrite our plugin blocks in the zellij permission cache, leaving foreign
/// entries untouched. Returns `Ok(true)` if the file was changed, `Ok(false)`
/// if it already matched. Silent — safe to call on the session-launch path
/// (see `attach::cold_start`).
pub fn seed() -> Result<bool> {
    let base = util::plugin_dir();
    // superzej's private zellij cache (XDG_CACHE_HOME=~/.superzej/cache when we
    // launch zellij), NOT the system ~/.cache/zellij — keep them fully separate.
    let cache = zellij::cache_dir().join("zellij/permissions.kdl");
    let existing = fs::read_to_string(&cache).unwrap_or_default();

    let keys: Vec<String> = [
        "sidebar.wasm",
        "panel.wasm",
        "tabbar.wasm",
        "statusbar.wasm",
    ]
    .iter()
    .map(|w| base.join(w).to_string_lossy().into_owned())
    .collect();

    // Drop any existing blocks for our plugins (a block is `"<key>" {` through
    // the next bare `}`), keeping foreign entries as-is.
    let mut out = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        if !skipping && keys.iter().any(|k| line.starts_with(&format!("\"{k}\""))) {
            skipping = true;
            continue;
        }
        if skipping {
            if line.trim() == "}" {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    // Re-append fresh blocks with the current permission set.
    for key in &keys {
        out.push_str(&format!("\"{key}\" {{\n"));
        for p in PERMS {
            out.push_str(&format!("    {p}\n"));
        }
        out.push_str("}\n");
    }

    if out == existing {
        return Ok(false);
    }
    if let Some(dir) = cache.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&cache, out)?;
    Ok(true)
}
