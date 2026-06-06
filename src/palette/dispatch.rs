//! Turns a chosen `Action` into a real effect by calling the same command
//! functions the keybinds use. Runs *after* the iocraft render loop exits, so
//! the terminal is already restored and spawning panes / `zellij action`s is
//! safe (identical to how the legacy floating `menu` dispatched).

use super::item::Action;
use crate::cli::PrAction;
use crate::config::Config;
use crate::github::MergeMethod;
use crate::{commands, util, zellij};
use anyhow::Result;
use std::path::Path;

pub fn dispatch(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::NewWorkspace => commands::new_workspace::run(cfg, None, None, true),
        Action::NewWorktree => commands::new_worktree::run(cfg, None, None, false, None),
        Action::NewPanel => commands::new_panel::run(cfg, "right", false),
        Action::NewTab => commands::new_tab::run(None),
        Action::SwitchRepo => commands::launch::run(cfg),
        Action::Dashboard => commands::dashboard::run(cfg, false, false),
        Action::ToggleSidebar => commands::panels::sidebar(true),
        Action::TogglePanel => commands::panels::panel(true),
        Action::Tool(name) => commands::tool::run(cfg, &name, None, None),
        Action::CloseWorktree => commands::close_worktree::run(false, false),
        Action::PrOpen => commands::pr::run(PrAction::Open { worktree: None }),
        Action::PrCreate => commands::pr::run(PrAction::Create {
            worktree: None,
            title: None,
            body: None,
            base: None,
            draft: false,
            web: true,
            fill: false,
        }),
        Action::PrStatus => commands::panels::panel(false),
        Action::PrApprove => commands::pr::run(PrAction::Approve {
            worktree: None,
            body: None,
        }),
        Action::PrMerge => commands::pr::run(PrAction::Merge {
            worktree: None,
            method: MergeMethod::Squash,
            delete_branch: false,
            auto: false,
        }),
        Action::PrRerun => commands::pr::run(PrAction::RerunChecks { worktree: None }),
        Action::OpenFile(path) => {
            open_file(cfg, &path, None);
            Ok(())
        }
        Action::OpenFileAt(path, line) => {
            open_file(cfg, &path, Some(line));
            Ok(())
        }
        Action::GotoTab(name) => {
            zellij::go_to_tab_name(&name);
            Ok(())
        }
        Action::OpenRepo(path) => commands::new_workspace::run(cfg, Some(path), None, false),
        Action::Checkout(branch) => checkout(&branch),
    }
}

/// Open a file (optionally at a line) in the user's editor, floated in the
/// focused worktree. GUI editors are launched detached; terminal editors get a
/// `+LINE` jump where the file allows it.
fn open_file(cfg: &Config, file: &Path, line: Option<usize>) {
    let worktree = commands::resolve_worktree(None);
    let prog = editor_program(cfg);
    let file_str = file.to_string_lossy();
    let (cmd, detached) = editor_command(&prog, &file_str, line);
    if !zellij::in_zellij() {
        crate::msg::info(&format!("(not in zellij) would open: {cmd}"));
        return;
    }
    if detached {
        util::spawn_detached(&cmd, &worktree);
    } else {
        let sh = util::shell();
        zellij::new_float(&worktree, "editor", &[&sh, "-lc", &cmd]);
    }
}

/// Build the shell command to open `file` (optionally at `line`) and whether to
/// spawn it detached (GUI editors) vs floated in a pane (terminal editors).
/// Pure, so it's unit-testable without spawning anything.
fn editor_command(prog: &str, file: &str, line: Option<usize>) -> (String, bool) {
    if util::is_gui_editor(prog) {
        // VS Code / codium understand `--goto file:line`; others just open it.
        let cmd = match line {
            Some(l) if prog.contains("code") => format!("{prog} --goto {}:{l}", sh_quote(file)),
            _ => format!("{prog} {}", sh_quote(file)),
        };
        (cmd, true)
    } else {
        let cmd = match line {
            Some(l) => format!("{prog} +{l} {}", sh_quote(file)),
            None => format!("{prog} {}", sh_quote(file)),
        };
        (cmd, false)
    }
}

/// Switch the focused worktree to an existing branch (best-effort; fails if the
/// branch is checked out in another worktree, which the model expects).
fn checkout(branch: &str) -> Result<()> {
    let worktree = commands::resolve_worktree(None);
    let argv = checkout_argv(&worktree.to_string_lossy(), branch);
    let status = std::process::Command::new("git").args(&argv).status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            crate::msg::warn(&format!(
                "could not switch to '{branch}' (it may be checked out in another worktree)"
            ));
            Ok(())
        }
    }
}

/// The `git` argv to switch `worktree` to `branch`.
fn checkout_argv(worktree: &str, branch: &str) -> Vec<String> {
    vec!["-C".into(), worktree.into(), "switch".into(), branch.into()]
}

/// The editor program with flags but no target (mirrors `commands::tool`'s
/// resolution: an explicit config `editor` tool, else `$VISUAL`/`$EDITOR`).
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

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, NamedCommand};

    #[test]
    fn terminal_editor_command_jumps_to_line() {
        let (cmd, detached) = editor_command("nvim", "/a/b.rs", Some(42));
        assert_eq!(cmd, "nvim +42 '/a/b.rs'");
        assert!(!detached);
        let (cmd, _) = editor_command("nvim", "/a/b.rs", None);
        assert_eq!(cmd, "nvim '/a/b.rs'");
    }

    #[test]
    fn gui_editor_is_detached_and_uses_goto() {
        let (cmd, detached) = editor_command("code", "/a/b.rs", Some(7));
        assert_eq!(cmd, "code --goto '/a/b.rs':7");
        assert!(detached);
        // A non-code GUI editor opens the file without a line jump.
        let (cmd, detached) = editor_command("zed", "/a/b.rs", Some(7));
        assert_eq!(cmd, "zed '/a/b.rs'");
        assert!(detached);
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_quote("plain"), "'plain'");
    }

    #[test]
    fn checkout_argv_is_worktree_scoped() {
        assert_eq!(
            checkout_argv("/w", "feature/x"),
            vec!["-C", "/w", "switch", "feature/x"]
        );
    }

    #[test]
    fn safe_actions_dispatch_without_blocking() {
        crate::palette::testutil::sandbox();
        // A configured "diff" tool, so `Tool("diff")` resolves instead of dying.
        let cfg = Config {
            tools: vec![NamedCommand {
                name: "diff".into(),
                command: "git diff".into(),
            }],
            ..Config::default()
        };
        // Every action here either guards on `in_zellij()` (false in tests, so it
        // just logs) or is a fast-failing zellij no-op without a session. The
        // picker/gh/confirm arms are intentionally excluded (they'd read a tty),
        // and Checkout is excluded (it would mutate the real repo's branch).
        // NewTab is excluded: it calls `msg::die` (process exit) outside a
        // session rather than logging, which would abort the whole test binary.
        let actions = [
            Action::NewPanel,
            Action::ToggleSidebar,
            Action::TogglePanel,
            Action::PrStatus,
            Action::Tool("diff".into()),
            Action::OpenFile("/tmp/sz-nope.rs".into()),
            Action::OpenFileAt("/tmp/sz-nope.rs".into(), 12),
            Action::GotoTab("no/such-tab".into()),
        ];
        for action in actions {
            dispatch(&cfg, action).unwrap();
        }
    }

    #[test]
    fn editor_program_prefers_config_then_env() {
        let cfg = Config {
            tools: vec![NamedCommand {
                name: "editor".into(),
                command: "hx .".into(),
            }],
            ..Config::default()
        };
        // Trailing " ." is stripped so the caller can supply its own target.
        assert_eq!(editor_program(&cfg), "hx");

        // The shipped POSIX default `${EDITOR:-vi} .` means "resolve from env".
        let cfg = Config {
            tools: vec![NamedCommand {
                name: "editor".into(),
                command: "${EDITOR:-vi} .".into(),
            }],
            ..Config::default()
        };
        std::env::set_var("VISUAL", "myed");
        assert_eq!(editor_program(&cfg), "myed");
        std::env::remove_var("VISUAL");
    }
}
