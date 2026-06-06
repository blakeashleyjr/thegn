//! Session bootstrap. Each repo is its own zellij session; `new-workspace`
//! creates/switches one per repo. This module owns the cold-start exec (entering
//! a fresh superzej zellij session) and the managed-config seeding.
//!
//! superzej fully manages its zellij config: a default `config/zellij.kdl` is
//! copied to `~/.superzej/zellij.kdl` on first launch (and never overwritten
//! after), and zellij is started with `--config ~/.superzej/zellij.kdl`. The
//! user customizes that file for full control.

use crate::config::Config;
use crate::{commands, msg, util, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The default managed zellij config, seeded to ~/.superzej/zellij.kdl.
const DEFAULT_CONFIG: &str = include_str!("../../config/zellij.kdl");

/// `superzej attach [session]`:
///   - no session  -> run the launcher (pick a repo, then open it)
///   - a session    -> (re)attach to it, or cold-start it if not running
///
/// In the single-session model there's nothing to *switch to* from inside our
/// own session (repos are tabs, not sessions), so the in-session case is a
/// no-op; `attach` matters only from a plain terminal (re)entering the UI.
pub fn run(cfg: &Config, session: Option<String>) -> Result<()> {
    match session {
        None => commands::launch::run(cfg),
        Some(s) => {
            if zellij::in_superzej_session() {
                Ok(()) // already inside the one session — nothing to attach
            } else {
                // Keep the managed keybinds in sync with config before (re)launch.
                let _ = sync_managed_config(cfg);
                if session_exists(&s) {
                    // Reattach to the running session (managed config already applied).
                    exec_clean_attach(&s);
                } else {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| util::home());
                    cold_start(&s, &cwd);
                }
            }
        }
    }
}

/// Whether a zellij session named `s` is currently running (in superzej's
/// private socket namespace — never the system zellij's).
fn session_exists(s: &str) -> bool {
    zellij::command()
        .arg("list-sessions")
        .arg("-s")
        .arg("--no-formatting")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == s)
        })
        .unwrap_or(false)
}

/// The layout name (or dev path via SUPERZEJ_LAYOUT) for new sessions.
pub fn layout() -> String {
    std::env::var("SUPERZEJ_LAYOUT").unwrap_or_else(|_| "superzej".into())
}

/// Path to the managed config, seeding it from the default on first use.
///
/// `SUPERZEJ_CONFIG` overrides it with a path used verbatim (no seeding, no
/// "never overwrite" copy) — the dev counterpart to `SUPERZEJ_LAYOUT`, so
/// `just start` / `just start-term` run against the live `config/zellij.kdl`
/// instead of the once-seeded, never-overwritten `~/.superzej/zellij.kdl`.
pub fn config_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("SUPERZEJ_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let dir = util::superzej_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("zellij.kdl");
    if !path.exists() {
        std::fs::write(&path, DEFAULT_CONFIG)?;
    }
    Ok(path)
}

/// Re-render the generated keybind region of the managed `zellij.kdl` from the
/// effective registry (builtins + `[keybinds]`/`[[actions]]`), preserving every
/// other part of the file. Writes only when the content changes. No-op (with a
/// note) under the `SUPERZEJ_CONFIG` dev override, which is used verbatim.
pub fn sync_managed_config(cfg: &Config) -> Result<bool> {
    if std::env::var_os("SUPERZEJ_CONFIG").is_some() {
        msg::info("SUPERZEJ_CONFIG set; not rewriting the dev config — edit config/zellij.kdl");
        return Ok(false);
    }
    let path = config_path()?; // seeds the default on first use
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let generated = crate::keymap::render_keybinds_kdl(&crate::keymap::effective(cfg));
    let updated = crate::keymap::splice_managed_region(&existing, &generated);
    if updated != existing {
        std::fs::write(&path, &updated)?;
        tracing::debug!("rewrote managed keybinds in {}", path.display());
        return Ok(true);
    }
    Ok(false)
}

/// Remove any inherited zellij env so a fresh `zellij` invocation doesn't think
/// it's nested (these vars leak into every child of a pane), and mark this as a
/// superzej-managed session so future invocations recognize our world.
fn prepare_env(session: &str) {
    std::env::remove_var("ZELLIJ");
    std::env::remove_var("ZELLIJ_SESSION_NAME");
    std::env::remove_var("ZELLIJ_PANE_ID");
    std::env::set_var("SUPERZEJ_SESSION", session);
    // Pin zellij to superzej's private socket + cache namespace. The exec'd
    // zellij server inherits these, so every pane, plugin `run_command`, and
    // in-session `superzej` call lands in the same isolated world — and a
    // system `zellij` (different socket dir) can neither see nor disturb it.
    zellij::export_private_env();
}

/// Reattach to an existing superzej session from a clean environment.
fn exec_clean_attach(session: &str) -> ! {
    prepare_env(session);
    let config = config_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Export it so any *new* session spawned from within (switch-session can't
    // take --config) still loads our managed config. See cold_start.
    std::env::set_var("ZELLIJ_CONFIG_FILE", &config);
    // Seed the plugin permission cache so the sidebar/panel/tabbar/statusbar
    // are pre-approved on load — the prompt renders inside fixed plugin panes
    // and is effectively un-approvable. Best-effort; idempotent.
    let _ = commands::grant_plugins::seed();
    spawn_watch_daemon(session);
    util::exec_command(&zellij::bin(), &["--config", &config, "attach", session]);
}

/// Spawn the per-session `watch` daemon (live panel refresh) detached, before we
/// exec into zellij. It inherits the private socket/cache env set by
/// `prepare_env`, and its own pid lockfile makes this idempotent — so spawning
/// from both cold-start and reattach is safe.
fn spawn_watch_daemon(session: &str) {
    if let Ok(exe) = std::env::current_exe() {
        util::spawn_daemon(&exe.to_string_lossy(), &["watch", "--session", session]);
    }
}

/// Start a fresh superzej zellij session, rooted at `cwd` (the repo root, so the
/// home tab resolves the right repo). Strips inherited zellij env first so it
/// never nests into / hijacks a foreign session. Replaces this process.
pub fn cold_start(session: &str, cwd: &Path) -> ! {
    prepare_env(session);
    // Dev escape hatch (`SUPERZEJ_FRESH`, set by `just start-term`): force-kill
    // and delete any existing session of this name first, so each launch is a
    // truly fresh session that picks up the latest layout/config. Never set in
    // production, so real `sj` invocations attach/resurrect as usual.
    if std::env::var_os("SUPERZEJ_FRESH").is_some() {
        let _ = zellij::command()
            .args(["delete-session", session, "--force"])
            .status();
    }
    let config = match config_path() {
        Ok(c) => c.to_string_lossy().into_owned(),
        Err(e) => msg::die(&format!("could not write managed config: {e:#}")),
    };
    // Export the config path so sessions created *later* from within this one
    // inherit our theme/keybinds. `switch-session` (how `new-workspace` opens a
    // workspace once we're inside zellij) has no `--config` flag, so without this
    // a freshly-created workspace would boot with zellij's default config — no
    // superzej theme, no Super+Alt navigation. `--config` reads this same env
    // var, and the new session's server inherits it from this long-lived client.
    std::env::set_var("ZELLIJ_CONFIG_FILE", &config);
    let layout = layout();
    let _ = std::env::set_current_dir(cwd);
    // Seed the plugin permission cache before zellij reads it on plugin load,
    // so RunCommands is granted from the first instant (no un-approvable prompt
    // in the fixed plugin panes). Best-effort; idempotent.
    let _ = commands::grant_plugins::seed();
    spawn_watch_daemon(session);
    util::exec_command(
        &zellij::bin(),
        &[
            "--config", &config, "--layout", &layout, "attach", "--create", session,
        ],
    );
}
