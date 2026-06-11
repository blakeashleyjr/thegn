//! `superzej tool <name>` — open a per-worktree tool (lazygit/yazi/editor/diff)
//! as a floating pane scoped to the focused worktree.

use crate::config::Config;
use crate::db::Db;
use crate::remote::GitLoc;
use crate::{msg, repo, sandbox, util, yazi, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn run(cfg: &Config, name: &str, worktree: Option<String>, file: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let worktree: PathBuf = worktree
        .map(PathBuf::from)
        .or_else(|| std::env::var("SUPERZEJ_WORKTREE").ok().map(PathBuf::from))
        .or_else(|| repo::toplevel(&cwd))
        .unwrap_or(cwd);

    // The editor is special: resolve the user's real editor and route GUI
    // editors to a detached launch instead of a terminal pane. An optional
    // `--file` opens that path instead of the worktree directory.
    if name == "editor" {
        open_editor(cfg, &worktree, file.as_deref());
        // No launcher-pane cleanup: the keybind/menu invoke `tool` with
        // `Run … { floating true; close_on_exit true }`, so the floating
        // launcher self-closes when we exit. Calling `close-pane` here would
        // instead close the just-spawned (focused) editor float — the trap
        // documented in commands::files. The `--file` plugin path has no pane
        // of its own either.
        return Ok(());
    }

    let mut cmd = cfg
        .tool_command(name)
        .unwrap_or_else(|| msg::die(&format!("tool: unknown tool '{name}'")))
        .to_string();

    if name == "yazi" {
        cmd = yazi_tool_inner(yazi::ensure_config(cfg).as_deref(), &worktree, &cmd);
    }

    // 'diff' uses delta as pager when available for nicer output.
    if name == "diff" && util::have("delta") {
        cmd = "git -c core.pager=delta diff".to_string();
    }

    if zellij::in_zellij() {
        // Run the tool inside the worktree's sandbox so it shares the same git
        // env as the agent pane (no-op when the sandbox resolves to the host).
        let wt_s = worktree.to_string_lossy().into_owned();
        let loc = GitLoc::for_worktree(&worktree);
        let root = Db::open()
            .ok()
            .and_then(|db| db.repo_root_for(&wt_s).ok().flatten())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| repo::main_worktree(&worktree))
            .unwrap_or_else(|| worktree.clone());
        let sb = cfg.repo_sandbox(&root);
        let cname = sandbox::container_name(&wt_s);
        match sandbox::resolve(&sb, &loc, &cname) {
            Some(spec) if sandbox::ensure(&spec).is_ok() => {
                let argv = sandbox::enter_argv(&spec, &cmd);
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                zellij::new_float(&worktree, name, &refs);
            }
            _ => {
                let sh = util::shell();
                let inner = if name == "yazi" {
                    mem_contain_yazi(cfg, &cmd)
                } else {
                    mem_contain(cfg, &cmd)
                };
                zellij::new_float(&worktree, name, &[&sh, "-lc", &inner]);
            }
        }
        // No `close_pane()` here: the keybind/menu launcher is a floating,
        // `close_on_exit` pane (like `files`), so it self-closes when this
        // process exits. `close-pane` would close the just-spawned tool float
        // (the focused pane) instead — the "flashes and vanishes" bug.
    } else {
        msg::info(&format!(
            "(not in zellij) would run: {cmd}  [cwd={}]",
            Path::new(&worktree).display()
        ));
    }
    Ok(())
}

/// Launch the editor for `worktree`, opening `file` if given (else the worktree
/// directory). Honors an explicit `editor` tool command from config; otherwise
/// resolves `$VISUAL`/`$EDITOR` (the shipped default `${EDITOR:-vi} .` is POSIX
/// syntax that breaks under non-POSIX shells like fish, so it is treated as
/// "resolve from the environment"). GUI editors (vscode, zed, …) are spawned
/// detached so they don't sit in an empty terminal pane.
pub fn open_editor(cfg: &Config, worktree: &Path, file: Option<&str>) {
    let prog = editor_program(cfg);
    let target = file.unwrap_or(".");
    let cmd = format!("{prog} {}", sh_quote(target));
    if !zellij::in_zellij() {
        msg::info(&format!(
            "(not in zellij) would run: {cmd}  [cwd={}]",
            worktree.display()
        ));
        return;
    }
    if util::is_gui_editor(&prog) {
        util::spawn_detached(&cmd, worktree);
    } else {
        let sh = util::shell();
        zellij::new_float(worktree, "editor", &[&sh, "-lc", &cmd]);
    }
}

/// The editor program (with any flags, but no target): an explicit config
/// override, or the resolved `$VISUAL`/`$EDITOR`. A trailing ` .` in a configured
/// command is dropped so the caller can supply its own target (a file or `.`).
fn editor_program(cfg: &Config) -> String {
    let configured = cfg.tool_command("editor").unwrap_or_default().trim();
    if configured.is_empty() || configured.contains("${EDITOR") {
        util::editor()
    } else {
        configured
            .strip_suffix(" .")
            .unwrap_or(configured)
            .trim()
            .to_string()
    }
}

/// Single-quote a shell argument so paths with spaces/specials survive `-lc`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn yazi_tool_inner(cfg_home: Option<&std::path::Path>, worktree: &Path, cmd: &str) -> String {
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
    inner.push_str(&format!("exec {cmd}"));
    inner
}

/// Wrap a host tool command in a memory-capped transient systemd scope so a
/// runaway child is OOM-killed inside its own cgroup instead of triggering a
/// global OOM that kills the terminal. Scope teardown on exit also reaps orphans.
/// Falls back to the bare command when containment is disabled (empty
/// `tool_mem_max`) or `systemd-run` is unavailable (non-systemd hosts).
fn mem_contain(cfg: &Config, cmd: &str) -> String {
    let lim = &cfg.limits;
    if lim.tool_mem_max.trim().is_empty() || !util::have("systemd-run") {
        return cmd.to_string();
    }
    format!(
        "systemd-run --user --scope --quiet \
         -p MemoryMax={} -p MemorySwapMax={} -- {cmd}",
        lim.tool_mem_max, lim.tool_mem_swap_max
    )
}

/// Use drawer-specific safety limits for `superzej tool yazi`, because it has the
/// same image-preview helper risk as the files drawer.
fn mem_contain_yazi(cfg: &Config, cmd: &str) -> String {
    mem_contain_yazi_with(cfg, cmd, util::have("systemd-run"))
}

fn mem_contain_yazi_with(cfg: &Config, cmd: &str, systemd_available: bool) -> String {
    if !cfg.drawer.contain || !systemd_available {
        return cmd.to_string();
    }
    let mut parts = vec!["systemd-run --user --scope --quiet --collect".to_string()];
    for (key, value) in [
        ("MemoryMax", cfg.drawer.memory_max.trim()),
        ("MemorySwapMax", cfg.drawer.memory_swap_max.trim()),
        ("CPUQuota", cfg.drawer.cpu_quota.trim()),
    ] {
        if !value.is_empty() {
            parts.push(format!("-p {key}={value}"));
        }
    }
    parts.push(format!("-- {} -lc {}", util::shell(), sh_quote(cmd)));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yazi_tool_inner_exports_private_config_and_worktree() {
        let inner = yazi_tool_inner(Some(Path::new("/cfg/yazi")), Path::new("/wt"), "yazi");

        assert!(inner.contains("export YAZI_CONFIG_HOME='/cfg/yazi';"));
        assert!(inner.contains("export SUPERZEJ_WORKTREE='/wt';"));
        assert!(inner.ends_with("exec yazi"));
    }

    #[test]
    fn yazi_tool_containment_uses_drawer_limits_and_child_shell() {
        let cfg = Config::default();
        let wrapped = mem_contain_yazi_with(&cfg, "export X=1; exec yazi", true);

        assert!(wrapped.starts_with("systemd-run --user --scope --quiet --collect"));
        assert!(wrapped.contains("-p MemoryMax=2G"));
        assert!(wrapped.contains("-p MemorySwapMax=512M"));
        assert!(wrapped.contains("-p CPUQuota=200%"));
        assert!(wrapped.contains(" -lc 'export X=1; exec yazi'"));
    }

    #[test]
    fn yazi_tool_containment_omits_empty_properties() {
        let mut cfg = Config::default();
        cfg.drawer.memory_swap_max.clear();
        cfg.drawer.cpu_quota.clear();
        let wrapped = mem_contain_yazi_with(&cfg, "exec yazi", true);

        assert!(wrapped.contains("-p MemoryMax=2G"));
        assert!(!wrapped.contains("MemorySwapMax="));
        assert!(!wrapped.contains("CPUQuota="));
    }

    #[test]
    fn yazi_tool_containment_can_be_disabled() {
        let mut cfg = Config::default();
        cfg.drawer.contain = false;

        assert_eq!(mem_contain_yazi_with(&cfg, "exec yazi", true), "exec yazi");
        assert_eq!(
            mem_contain_yazi_with(&Config::default(), "exec yazi", false),
            "exec yazi"
        );
    }
}
