//! `superzej sidebar` / `superzej panel` — show or toggle the two WASM plugin
//! surfaces (left session sidebar, right diff/PR panel).
//!
//! Visibility is owned by the **statusbar** plugin — the one chrome surface
//! that is never hidden and always full-width. A suppressed plugin can't
//! reliably re-show *itself* (nor observe the terminal width while suppressed),
//! but the statusbar can hide/show another pane and reapply the layout, so it
//! drives the narrow-terminal auto-collapse and the manual toggles alike. The
//! pane is suppressed, not closed — instant, the tiled layout reflows so the
//! center column absorbs the space, and re-showing restores the same slot.
//!
//! The `Ctrl Alt s` / `Ctrl Alt p` keybinds send the same per-surface pipe to
//! the statusbar via `MessagePlugin` (no spawned command pane, no flash); this
//! command is the equivalent path for the menu and the CLI.

use crate::{msg, zellij};
use anyhow::Result;

/// `file:` URL of an installed superzej plugin (`~/.local/share/superzej/<name>`),
/// matching the session layout so pipes reach the running instances.
pub fn plugin_url(name: &str) -> String {
    format!("file:~/.local/share/superzej/{}", name)
}

/// Pipe a visibility command for `surface` ("sidebar"/"panel") to the statusbar
/// controller. `toggle` flips it; otherwise it requests a show.
fn surface(surface: &str, toggle: bool) -> Result<()> {
    if !zellij::in_zellij() {
        msg::info(&format!(
            "(not in zellij) {surface} is only available in a session"
        ));
        return Ok(());
    }
    let url = plugin_url("statusbar.wasm");
    let pipe = if toggle {
        format!("superzej_toggle_{surface}")
    } else {
        format!("superzej_show_{surface}")
    };
    // Non-empty payload: the controller ignores payload-less CLI pipe messages
    // (the stdin-EOF dupe `zellij pipe` sends), so an empty payload is dropped.
    zellij::pipe_plugin(&url, &pipe, "1");
    Ok(())
}

pub fn sidebar(toggle: bool) -> Result<()> {
    surface("sidebar", toggle)
}

pub fn panel(toggle: bool) -> Result<()> {
    surface("panel", toggle)
}
