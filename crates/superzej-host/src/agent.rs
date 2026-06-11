//! Native agent launching. The zellij-era `superzej pick-agent` ran inside a
//! freshly-created worktree pane, showed an fzf/gum picker, then `exec`'d the
//! choice so the selection became the pane's own process. The native host owns
//! the screen (raw mode), so the picker is the in-process command palette and
//! the pane *is* the spawned process — we compose the sandbox-wrapped argv +
//! env here and hand it to `Panes::spawn_argv_env` rather than exec-replacing.
//!
//! This module is the testable seam: `choices`, `resolve_command`, and
//! `launch_spec` are pure over `Config`/`Db`, so the wiring in `run.rs` stays a
//! thin call.

use std::path::{Path, PathBuf};
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::remote::GitLoc;
use superzej_core::{repo, sandbox};

/// The literal shell sentinel — distinct from any configured agent/tool name.
const SHELL: &str = "shell";

/// What the agent picker offers for a worktree: every configured agent, then
/// every tool, then a literal `shell`. Order matches the zellij `pick_agent`.
pub fn choices(cfg: &Config) -> Vec<String> {
    let mut labels: Vec<String> = cfg.agents.iter().map(|a| a.name.clone()).collect();
    labels.extend(cfg.tools.iter().map(|t| t.name.clone()));
    if !labels.iter().any(|l| l == SHELL) {
        labels.push(SHELL.into());
    }
    labels
}

/// Resolve a picker label to the command string to run inside the worktree.
/// `shell` (and any unknown label) resolves to the interactive login shell.
pub fn resolve_command(cfg: &Config, choice: &str) -> String {
    if choice == SHELL {
        return shell_inner();
    }
    if let Some(c) = cfg.agent_command(choice) {
        return c.to_string();
    }
    if let Some(c) = cfg.tool_command(choice) {
        return c.to_string();
    }
    // Unknown label — drop to a shell rather than spawning a dead pane.
    shell_inner()
}

/// The `inner` program string for a plain shell pane (what `enter_argv` wraps).
fn shell_inner() -> String {
    "${SHELL:-/bin/sh} -l".to_string()
}

/// A fully-resolved launch: the argv to spawn (sandbox/transport-wrapped when a
/// sandbox is configured, else a bare `$SHELL -lc <cmd>`), the cwd, and the env
/// the agent pane expects. Pure data so `run.rs` just spawns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

/// Compose the [`LaunchSpec`] for running `choice` in `worktree`. Records the
/// choice (and any sandbox backend) in the DB, mirroring the zellij path's
/// side effects so the dashboard/`--resume` keep working.
///
/// `branch` is the worktree's branch (for the pane env + title); `None` falls
/// back to the worktree basename.
pub fn launch_spec(cfg: &Config, worktree: &str, branch: Option<&str>, choice: &str) -> LaunchSpec {
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let cmd = resolve_command(cfg, choice);

    // Record the choice for the dashboard / `--resume` (keyed by worktree path).
    if let Ok(db) = Db::open() {
        let _ = db.set_worktree_agent(worktree, choice);
    }

    // The local repo root drives the per-repo sandbox overlay + slug. Prefer the
    // DB (carries remote worktrees with no local cwd), else climb from the path.
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));

    // Local worktrees run in their own dir; remote worktrees have no local dir
    // (the transport cd's on the remote), so the pane cwd stays unset.
    let cwd = (!loc.is_remote()).then(|| PathBuf::from(worktree));

    let env = vec![
        ("SUPERZEJ_WORKTREE".to_string(), worktree.to_string()),
        (
            "SUPERZEJ_BRANCH".to_string(),
            branch.unwrap_or_default().to_string(),
        ),
    ];

    // Wrap the chosen program in the worktree's sandbox/container (and/or the
    // mosh/ssh transport for a remote worktree). Auto walks the configured chain;
    // a persisted worktree choice (from the new-worktree picker) narrows the
    // candidates. Host is an explicit last fallback, labeled in the DB.
    let mut sb = cfg.repo_sandbox(&repo_root);
    if let Ok(db) = Db::open()
        && let Ok(Some(saved)) = db.worktree_sandbox(worktree)
        && !saved.trim().is_empty()
        && saved.trim() != "auto"
        && let Ok(b) = superzej_core::config::SandboxBackend::from_str_validated(&saved)
    {
        sb.backend = b;
    }
    let cname = sandbox::container_name(worktree);
    for candidate in sandbox_candidates(&sb) {
        if let Some(spec) = sandbox::resolve(&candidate, &loc, &cname) {
            if spec.backend == sandbox::Backend::None {
                break;
            }
            match sandbox::ensure(&spec) {
                Ok(()) => {
                    if let Ok(db) = Db::open() {
                        let _ = db.set_worktree_sandbox(worktree, spec.backend.label());
                    }
                    return LaunchSpec {
                        argv: sandbox::enter_argv(&spec, &cmd),
                        cwd,
                        env,
                    };
                }
                Err(e) => superzej_core::msg::warn(&format!(
                    "sandbox {} failed for {worktree}: {e}; trying next backend",
                    spec.backend.label()
                )),
            }
        } else if candidate.backend == superzej_core::config::SandboxBackend::None {
            break;
        }
    }

    if let Ok(db) = Db::open() {
        let _ = db.set_worktree_sandbox(worktree, "host");
    }
    // Host fallback: run the command through a login shell so PATH/env expand.
    LaunchSpec {
        argv: vec![superzej_core::util::shell(), "-lc".to_string(), cmd],
        cwd,
        env,
    }
}

fn sandbox_candidates(
    sb: &superzej_core::config::SandboxConfig,
) -> Vec<superzej_core::config::SandboxConfig> {
    if sb.backend != superzej_core::config::SandboxBackend::Auto {
        return vec![sb.clone()];
    }
    let mut out = Vec::new();
    for name in &sb.backend_chain {
        if let Ok(backend) = superzej_core::config::SandboxBackend::from_str_validated(name) {
            let mut c = sb.clone();
            c.backend = backend;
            out.push(c);
        }
    }
    if !out
        .iter()
        .any(|c| c.backend == superzej_core::config::SandboxBackend::None)
    {
        let mut c = sb.clone();
        c.backend = superzej_core::config::SandboxBackend::None;
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(agents: &[(&str, &str)], tools: &[(&str, &str)]) -> Config {
        let mut cfg = Config::default();
        let mk = |(n, c): &(&str, &str)| superzej_core::config::NamedCommand {
            name: n.to_string(),
            command: c.to_string(),
            hints: Vec::new(),
        };
        cfg.agents = agents.iter().map(mk).collect();
        cfg.tools = tools.iter().map(mk).collect();
        cfg
    }

    #[test]
    fn choices_lists_agents_then_tools_then_shell() {
        let cfg = cfg_with(&[("claude", "claude")], &[("lazygit", "lazygit")]);
        assert_eq!(choices(&cfg), vec!["claude", "lazygit", "shell"]);
    }

    #[test]
    fn choices_does_not_duplicate_an_explicit_shell() {
        let cfg = cfg_with(&[], &[("shell", "bash")]);
        assert_eq!(choices(&cfg), vec!["shell"]);
    }

    #[test]
    fn resolve_command_maps_agent_tool_and_shell() {
        let cfg = cfg_with(&[("claude", "claude --foo")], &[("lazygit", "lazygit")]);
        assert_eq!(resolve_command(&cfg, "claude"), "claude --foo");
        assert_eq!(resolve_command(&cfg, "lazygit"), "lazygit");
        assert_eq!(resolve_command(&cfg, "shell"), shell_inner());
        // Unknown label degrades to a shell.
        assert_eq!(resolve_command(&cfg, "nope"), shell_inner());
    }
}
