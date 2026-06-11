//! `superzej files` — toggle the bottom file-manager drawer (yazi by default).
//!
//! A spawn-on-open / close-on-dismiss bottom-anchored floating pane: opening
//! spawns a fresh yazi rooted in the focused worktree; dismissing (Ctrl+Alt+f
//! again, or `q` inside yazi) closes it. There is no persistent hidden pane and
//! no `next_swap_layout` reconcile — the drawer rides entirely outside the
//! chrome layout.
//!
//! Open/closed state is remembered **per worktree** under `~/.superzej/drawer/`
//! and restored when a worktree tab (re)loads (the statusbar pokes
//! `superzej files --restore --tab <name>` on becoming the active worktree tab),
//! so a drawer left open reappears across tab close and session restart.
//!
//! The targeted close is delegated to the statusbar plugin (the zellij CLI can
//! only close the *focused* pane): `superzej files --close` pipes it the
//! `superzej_close_files` message and it removes the pane by id.

use crate::config::Config;
use crate::db::{self, Db};
use crate::{commands, msg, util, yazi, zellij};
use anyhow::Result;
use std::path::PathBuf;

/// The drawer pane's name — how the statusbar finds it and how `--close`/restore
/// presence checks identify it.
pub const PANE_NAME: &str = "superzej-files";

pub fn run(
    cfg: &Config,
    reveal: Option<String>,
    worktree: Option<String>,
    tab: Option<String>,
    session: Option<String>,
    close: bool,
    restore: bool,
) -> Result<()> {
    // Plugin-spawned (the statusbar restore pipe): target the right session for
    // `zellij action` + the DB lookup, and satisfy in_zellij(). Mirrors new_tab.
    if let Some(s) = &session {
        std::env::set_var("ZELLIJ_SESSION_NAME", s);
    }
    if !zellij::in_zellij() {
        msg::info("(not in zellij) the file drawer is only available in a session");
        return Ok(());
    }

    // Resolve the worktree this drawer belongs to: an explicit --worktree, else
    // via --tab (the statusbar's restore path — look it up in the DB), else the
    // cwd-based resolution every other command uses.
    let wt: PathBuf = match (worktree, &tab) {
        (Some(w), _) => PathBuf::from(w),
        (None, Some(t)) => {
            resolve_tab_worktree(t).unwrap_or_else(|| commands::resolve_worktree(None))
        }
        (None, None) => commands::resolve_worktree(None),
    };
    let key = util::slugify(&wt.to_string_lossy());

    // Keep the bundled config + accent-derived theme fresh on every invocation.
    let cfg_home = yazi::ensure_config(cfg);

    let present = zellij::pane_named_in_focused_tab(PANE_NAME);

    match decide(close, present, restore, persisted_open(&key)) {
        Action::Close => {
            persist(&key, false);
            zellij::pipe_plugin(
                &commands::panels::plugin_url("statusbar.wasm"),
                "superzej_close_files",
                "1",
            );
        }
        Action::Noop => {}
        Action::Spawn => {
            // Run via the login shell so the private YAZI_CONFIG_HOME and the
            // worktree marker are exported for the file manager (and any
            // `superzej` it shells out to — e.g. the `q` close keybind —
            // resolves this worktree).
            let fm = yazi::bin(cfg);
            let reveal = reveal.unwrap_or_default();
            let cmd = drawer_command(
                cfg,
                cfg_home.as_deref(),
                &wt,
                &fm,
                &reveal,
                util::have("systemd-run"),
            );
            let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
            zellij::new_drawer(&wt, PANE_NAME, &cfg.drawer.height, &cfg.drawer.width, &refs);
            persist(&key, true);
        }
    }
    // No launcher-pane cleanup here: the keybind/menu that invoke `files` use
    // `Run … { close_on_exit true }`, so the launcher self-closes when we exit.
    // Calling `close-pane` would instead close the just-spawned (focused)
    // floating drawer. The restore path has no pane of its own either.
    Ok(())
}

/// What an invocation should do with the drawer.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Close,
    Noop,
    Spawn,
}

/// Decide the action from the inputs, kept pure so every combination is tested:
/// `--close` (or a plain toggle while already present) closes; an
/// already-present drawer or a restore for a worktree left closed is a no-op;
/// otherwise spawn.
fn decide(close: bool, present: bool, restore: bool, persisted_open: bool) -> Action {
    if close || (present && !restore) {
        Action::Close
    } else if present || (restore && !persisted_open) {
        Action::Noop
    } else {
        Action::Spawn
    }
}

/// Build the command argv for the drawer pane. The inner shell exports the yazi
/// config/worktree env, then optional systemd scope containment wraps that process
/// tree so image preview helpers cannot escape the configured limits.
fn drawer_command(
    cfg: &Config,
    cfg_home: Option<&std::path::Path>,
    worktree: &std::path::Path,
    fm: &str,
    reveal: &str,
    systemd_available: bool,
) -> Vec<String> {
    let inner = spawn_inner(cfg_home, worktree, fm);
    let mut cmd = vec![util::shell(), "-lc".into(), inner, "_".into()];
    if !reveal.is_empty() {
        cmd.push(reveal.into());
    }
    contain_drawer_argv(cfg, cmd, systemd_available)
}

fn contain_drawer_argv(cfg: &Config, cmd: Vec<String>, systemd_available: bool) -> Vec<String> {
    if !cfg.drawer.contain || !systemd_available {
        return cmd;
    }

    let mut wrapped = vec![
        "systemd-run".into(),
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
    ];
    for (key, value) in [
        ("MemoryMax", cfg.drawer.memory_max.trim()),
        ("MemorySwapMax", cfg.drawer.memory_swap_max.trim()),
        ("CPUQuota", cfg.drawer.cpu_quota.trim()),
    ] {
        if !value.is_empty() {
            wrapped.push("-p".into());
            wrapped.push(format!("{key}={value}"));
        }
    }
    wrapped.push("--".into());
    wrapped.extend(cmd);
    wrapped
}

fn spawn_inner(cfg_home: Option<&std::path::Path>, worktree: &std::path::Path, fm: &str) -> String {
    let mut inner = String::new();
    if let Some(home) = cfg_home {
        inner.push_str(&format!(
            "export YAZI_CONFIG_HOME={}; ",
            sh_quote(&home.to_string_lossy())
        ));
    }
    inner.push_str(&format!(
        "export SUPERZEJ_WORKTREE={}; ",
        sh_quote(&worktree.to_string_lossy())
    ));
    inner.push_str(&format!("exec {fm} \"$@\""));
    inner
}

/// Resolve a worktree path from a tab name via the DB (mirrors `resolve.rs`,
/// including the "{base} ·N" extra-tab fallback).
fn resolve_tab_worktree(tab: &str) -> Option<PathBuf> {
    let session = db::session();
    let db = Db::open().ok()?;
    if let Ok(Some(p)) = db.worktree_for_tab(&session, tab) {
        return Some(PathBuf::from(p));
    }
    let base = commands::new_tab::strip_page_suffix(tab);
    if base != tab {
        if let Ok(Some(p)) = db.worktree_for_tab(&session, base) {
            return Some(PathBuf::from(p));
        }
    }
    None
}

/// Per-worktree open-state directory: `<superzej-dir>/drawer/` (honors SUPERZEJ_DIR).
fn drawer_dir() -> PathBuf {
    util::superzej_dir().join("drawer")
}

/// Record whether the drawer is open for `key` (a slugified worktree path).
fn persist(key: &str, open: bool) {
    persist_in(&drawer_dir(), key, open);
}

/// Whether the drawer was last left open for `key`.
fn persisted_open(key: &str) -> bool {
    persisted_open_in(&drawer_dir(), key)
}

/// Dir-parameterized persistence (testable without touching the real state dir).
fn persist_in(dir: &std::path::Path, key: &str, open: bool) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(key), if open { "true" } else { "false" });
}

fn persisted_open_in(dir: &std::path::Path, key: &str) -> bool {
    std::fs::read_to_string(dir.join(key))
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// Single-quote a shell argument so paths with spaces/specials survive `-lc`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sz-files-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn cfg_for_run(config_home: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.drawer.command = "true".into();
        cfg.drawer.config_home = config_home.to_string_lossy().into_owned();
        cfg.drawer.contain = false;
        cfg
    }

    #[test]
    fn run_spawn_and_close_persist_state_in_session() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tmpdir();
        let wt = dir.join("repo");
        std::fs::create_dir_all(&wt).unwrap();
        let state = dir.join("state");
        let cfg = cfg_for_run(&dir.join("yazi"));
        std::env::set_var("SUPERZEJ_DIR", &state);
        std::env::set_var("ZELLIJ_SESSION_NAME", "sz-test-no-session");

        run(
            &cfg,
            Some("README.md".into()),
            Some(wt.to_string_lossy().into_owned()),
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let key = util::slugify(&wt.to_string_lossy());
        assert!(persisted_open_in(&state.join("drawer"), &key));

        run(
            &cfg,
            None,
            Some(wt.to_string_lossy().into_owned()),
            None,
            None,
            true,
            false,
        )
        .unwrap();
        assert!(!persisted_open_in(&state.join("drawer"), &key));

        std::env::remove_var("ZELLIJ_SESSION_NAME");
        std::env::remove_var("SUPERZEJ_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_restore_closed_worktree_is_noop() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tmpdir();
        let wt = dir.join("repo");
        std::fs::create_dir_all(&wt).unwrap();
        let state = dir.join("state");
        let cfg = cfg_for_run(&dir.join("yazi"));
        let key = util::slugify(&wt.to_string_lossy());
        persist_in(&state.join("drawer"), &key, false);
        std::env::set_var("SUPERZEJ_DIR", &state);
        std::env::set_var("ZELLIJ_SESSION_NAME", "sz-test-no-session");

        run(
            &cfg,
            None,
            Some(wt.to_string_lossy().into_owned()),
            None,
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!persisted_open_in(&state.join("drawer"), &key));

        std::env::remove_var("ZELLIJ_SESSION_NAME");
        std::env::remove_var("SUPERZEJ_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_tab_worktree_missing_tab_returns_none() {
        assert!(resolve_tab_worktree("definitely-missing-tab").is_none());
    }

    #[test]
    fn decide_close_wins() {
        // --close always closes, regardless of presence/restore/persistence.
        for &present in &[false, true] {
            for &restore in &[false, true] {
                for &persisted in &[false, true] {
                    assert_eq!(
                        decide(true, present, restore, persisted),
                        Action::Close,
                        "close=true present={present} restore={restore} persisted={persisted}"
                    );
                }
            }
        }
    }

    #[test]
    fn decide_toggle_open_closes_when_present() {
        // Interactive toggle (no --restore) while present -> close.
        assert_eq!(decide(false, true, false, false), Action::Close);
        assert_eq!(decide(false, true, false, true), Action::Close);
    }

    #[test]
    fn decide_restore_never_closes() {
        // A restore that finds the drawer already present is a no-op, not a close.
        assert_eq!(decide(false, true, true, true), Action::Noop);
        assert_eq!(decide(false, true, true, false), Action::Noop);
    }

    #[test]
    fn decide_spawns_when_absent_and_wanted() {
        // Interactive open of a closed drawer.
        assert_eq!(decide(false, false, false, false), Action::Spawn);
        assert_eq!(decide(false, false, false, true), Action::Spawn);
        // Restore of a worktree that was left open.
        assert_eq!(decide(false, false, true, true), Action::Spawn);
    }

    #[test]
    fn decide_restore_noops_when_left_closed() {
        // Restore must not open a drawer the user had closed.
        assert_eq!(decide(false, false, true, false), Action::Noop);
    }

    #[test]
    fn spawn_inner_exports_config_home_and_worktree() {
        let inner = spawn_inner(Some(Path::new("/cfg/yazi")), Path::new("/wt"), "yazi");
        assert!(inner.contains("export YAZI_CONFIG_HOME='/cfg/yazi';"));
        assert!(inner.contains("export SUPERZEJ_WORKTREE='/wt';"));
        assert!(inner.trim_end().ends_with("exec yazi \"$@\""));
    }

    #[test]
    fn spawn_inner_omits_config_home_for_system() {
        let inner = spawn_inner(None, Path::new("/wt"), "yazi");
        assert!(!inner.contains("YAZI_CONFIG_HOME"));
        assert!(inner.contains("export SUPERZEJ_WORKTREE='/wt';"));
    }

    #[test]
    fn spawn_inner_leaves_command_with_args_unquoted() {
        // A configured command with flags must reach the shell intact.
        let inner = spawn_inner(None, Path::new("/wt"), "ranger --cmd=foo");
        assert!(inner.contains("exec ranger --cmd=foo \"$@\""));
    }

    #[test]
    fn drawer_command_wraps_yazi_in_systemd_scope_with_limits() {
        let cfg = Config::default();
        let cmd = drawer_command(
            &cfg,
            Some(Path::new("/cfg/yazi")),
            Path::new("/wt"),
            "yazi",
            "images/logo.png",
            true,
        );

        assert_eq!(cmd[0], "systemd-run");
        assert!(cmd.contains(&"--user".to_string()));
        assert!(cmd.contains(&"--scope".to_string()));
        assert!(cmd.contains(&"--collect".to_string()));
        assert!(cmd.contains(&"MemoryMax=2G".to_string()));
        assert!(cmd.contains(&"MemorySwapMax=512M".to_string()));
        assert!(cmd.contains(&"CPUQuota=200%".to_string()));
        assert!(
            cmd.iter()
                .any(|arg| arg.contains("YAZI_CONFIG_HOME='/cfg/yazi'"))
        );
        assert_eq!(cmd.last().map(String::as_str), Some("images/logo.png"));
    }

    #[test]
    fn drawer_command_omits_empty_limit_properties() {
        let mut cfg = Config::default();
        cfg.drawer.memory_swap_max.clear();
        cfg.drawer.cpu_quota.clear();
        let cmd = drawer_command(&cfg, None, Path::new("/wt"), "yazi", "", true);

        assert!(cmd.contains(&"MemoryMax=2G".to_string()));
        assert!(!cmd.iter().any(|arg| arg.starts_with("MemorySwapMax=")));
        assert!(!cmd.iter().any(|arg| arg.starts_with("CPUQuota=")));
    }

    #[test]
    fn drawer_command_leaves_argv_unwrapped_when_containment_disabled() {
        let mut cfg = Config::default();
        cfg.drawer.contain = false;
        let cmd = drawer_command(&cfg, None, Path::new("/wt"), "yazi", "", true);

        assert_ne!(cmd[0], "systemd-run");
        assert!(cmd.iter().any(|arg| arg.contains("exec yazi")));
    }

    #[test]
    fn drawer_command_leaves_argv_unwrapped_without_systemd_run() {
        let cfg = Config::default();
        let cmd = drawer_command(&cfg, None, Path::new("/wt"), "yazi", "", false);

        assert_ne!(cmd[0], "systemd-run");
        assert!(cmd.iter().any(|arg| arg.contains("exec yazi")));
    }

    #[test]
    fn sh_quote_escapes_quotes_and_spaces() {
        assert_eq!(sh_quote("/a b"), "'/a b'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_quote("plain"), "'plain'");
    }

    #[test]
    fn persist_round_trips_per_key() {
        let dir = tmpdir();
        assert!(!persisted_open_in(&dir, "k")); // missing -> closed
        persist_in(&dir, "k", true);
        assert!(persisted_open_in(&dir, "k"));
        persist_in(&dir, "k", false);
        assert!(!persisted_open_in(&dir, "k"));
        // keys are independent (per-worktree).
        persist_in(&dir, "other", true);
        assert!(persisted_open_in(&dir, "other"));
        assert!(!persisted_open_in(&dir, "k"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_open_treats_garbage_as_closed() {
        let dir = tmpdir();
        std::fs::write(dir.join("k"), "yeah\n").unwrap();
        assert!(!persisted_open_in(&dir, "k"));
        std::fs::write(dir.join("k"), "true\n").unwrap(); // trailing newline ok
        assert!(persisted_open_in(&dir, "k"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
