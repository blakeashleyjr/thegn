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
/// Always uses the host (non-OCI) form; callers that know the sandbox context
/// should use `compose_spec` instead.
pub fn resolve_command(cfg: &Config, choice: &str) -> String {
    if choice == SHELL {
        return shell_inner(false);
    }
    if let Some(c) = cfg.agent_command(choice) {
        return c.to_string();
    }
    if let Some(c) = cfg.tool_command(choice) {
        return c.to_string();
    }
    // Unknown label — drop to a shell rather than spawning a dead pane.
    shell_inner(false)
}

/// The `inner` program string for a plain shell pane (what `enter_argv` wraps).
///
/// `in_oci` must be `true` when the inner command will run inside an OCI
/// container (podman/docker).  In that case the host's absolute `$SHELL` path
/// (e.g. `/run/current-system/sw/bin/zsh`) is meaningless — and even using the
/// basename fails if the container image doesn't have that shell installed (e.g.
/// a bare Debian image has bash but not zsh).  We emit a POSIX sh snippet that
/// walks a preference chain at runtime inside the container and execs the first
/// one that exists; `/bin/sh` is always the last resort.
///
/// On the host fallback path `in_oci = false` keeps the existing behaviour:
/// use `$SHELL` verbatim so NixOS users get the right store-path shell.
fn shell_inner(in_oci: bool) -> String {
    if in_oci {
        // Preference order: honour the host shell name if it's a known shell,
        // then try zsh/bash/fish/sh in that order.  The outer /bin/sh -lc
        // already provides a POSIX execution context, so this snippet is safe.
        let host_shell = std::env::var("SHELL").unwrap_or_default();
        let preferred = std::path::Path::new(&host_shell)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        // Build a chain that tries the host-preferred shell first (if it's a
        // known name) then falls through to bash → sh.
        let mut chain: Vec<&str> = Vec::new();
        if matches!(preferred, "zsh" | "bash" | "fish" | "dash" | "ksh" | "mksh") {
            chain.push(preferred);
        }
        for s in &["zsh", "bash", "sh"] {
            if !chain.contains(s) {
                chain.push(s);
            }
        }
        // Emit: for s in zsh bash sh; do command -v "$s" >/dev/null 2>&1 && exec "$s" -l; done
        let checks: String = chain
            .iter()
            .map(|s| format!("command -v {s} >/dev/null 2>&1 && exec {s} -l; "))
            .collect();
        // The trailing /bin/sh -l is the unconditional fallback — it exists in
        // every POSIX container.
        format!("{checks}exec /bin/sh -l")
    } else {
        let host_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        format!("{host_shell} -l")
    }
}

/// Like [`shell_inner`] but uses an explicit override from the sandbox config.
fn shell_inner_override(shell_override: &str) -> String {
    format!("{shell_override} -l")
}

/// A fully-resolved launch: the argv to spawn (sandbox/transport-wrapped when a
/// sandbox is configured, else a bare `$SHELL -lc <cmd>`), the cwd, and the env
/// the agent pane expects. Pure data so `run.rs` just spawns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// The effective containment backend used for this launch (`host` after fallback).
    pub backend: String,
    /// Human-visible notes when auto sandbox resolution fell through to another backend.
    pub warnings: Vec<String>,
}

impl LaunchSpec {
    pub fn warning_summary(&self) -> Option<String> {
        (!self.warnings.is_empty()).then(|| self.warnings.join("; "))
    }
}

/// The settled sandbox for a worktree: the resolved+ensured spec, or `None`
/// for the host fallback. `backend_label` is what the DB records ("host" when
/// no sandbox stuck); `warnings` are the human-visible fallback notes that
/// ride into [`LaunchSpec::warnings`].
#[derive(Debug, Clone)]
pub struct SandboxOutcome {
    pub spec: Option<sandbox::SandboxSpec>,
    pub backend_label: String,
    pub warnings: Vec<String>,
}

/// Resolve and `ensure` the sandbox for `worktree` — the BLOCKING half of a
/// launch (container inspect/image pull/start can take seconds-to-minutes), so
/// callers must keep it off the event loop. No DB access: `backend_choice` is
/// the persisted/explicit backend label (empty or "auto" walks the chain).
///
/// Wraps in the worktree's sandbox/container (and/or the mosh/ssh transport
/// for a remote worktree). Auto walks the configured chain, collecting
/// human-visible fallback warnings; an explicit choice (config or
/// `backend_choice`) must not silently fall back — it errors instead. Host is
/// the last fallback for the auto chain only.
pub fn prepare_sandbox(
    cfg: &Config,
    repo_root: &Path,
    worktree: &str,
    loc: &GitLoc,
    backend_choice: Option<&str>,
) -> anyhow::Result<SandboxOutcome> {
    let mut sb = cfg.repo_sandbox(repo_root);
    let mut explicit_backend =
        sandbox::Backend::from_config(sb.backend).filter(|b| *b != sandbox::Backend::None);
    if let Some(saved) = backend_choice.map(str::trim)
        && !saved.is_empty()
        && saved != "auto"
        && let Ok(b) = superzej_core::config::SandboxBackend::from_str_validated(saved)
    {
        explicit_backend =
            sandbox::Backend::from_config(b).filter(|b| *b != sandbox::Backend::None);
        sb.backend = b;
    }
    let explicit_choice = explicit_backend.is_some();
    let auto_choice = sb.backend == superzej_core::config::SandboxBackend::Auto;
    let mut warnings = Vec::new();
    let cname = sandbox::container_name(worktree);
    for candidate in sandbox_candidates(&sb) {
        if let Some(spec) = sandbox::resolve(&candidate, loc, &cname) {
            if spec.backend == sandbox::Backend::None {
                break;
            }
            if let Some(expected) = explicit_backend
                && spec.backend != expected
            {
                anyhow::bail!(
                    "explicit sandbox backend '{}' resolved to '{}' for {worktree}; refusing fallback",
                    sb.backend,
                    spec.backend.label()
                );
            }
            match sandbox::ensure(&spec) {
                Ok(()) => {
                    return Ok(SandboxOutcome {
                        backend_label: spec.backend.label().to_string(),
                        spec: Some(spec),
                        warnings,
                    });
                }
                Err(e) if explicit_choice => {
                    anyhow::bail!(
                        "sandbox {} failed for {worktree}: {e}",
                        spec.backend.label()
                    );
                }
                Err(e) => {
                    warnings.push(format!("sandbox {} failed: {e}", spec.backend.label()));
                    superzej_core::msg::warn(&format!(
                        "sandbox {} failed for {worktree}: {e}; trying next backend",
                        spec.backend.label()
                    ));
                }
            }
        } else if candidate.backend == superzej_core::config::SandboxBackend::None {
            break;
        } else if explicit_choice {
            anyhow::bail!(
                "sandbox backend '{}' could not be resolved for {worktree}",
                candidate.backend
            );
        } else if candidate.backend != superzej_core::config::SandboxBackend::Auto {
            warnings.push(format!("sandbox {} unavailable", candidate.backend));
        }
    }
    if explicit_choice {
        anyhow::bail!(
            "explicit sandbox backend '{}' did not produce a runnable sandbox for {worktree}",
            sb.backend
        );
    }
    if auto_choice && warnings.is_empty() {
        warnings.push("sandbox auto selected host".to_string());
    } else if auto_choice {
        warnings.push("running on host after sandbox fallback".to_string());
    }
    Ok(SandboxOutcome {
        spec: None,
        backend_label: "host".to_string(),
        warnings,
    })
}

/// Pure composition of the final [`LaunchSpec`] from a settled sandbox: argv
/// (sandbox-wrapped, or a bare login shell on the host fallback), cwd, env,
/// plus the effective backend label and any fallback warnings.
pub fn compose_spec(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    loc: &GitLoc,
    sb: &SandboxOutcome,
) -> LaunchSpec {
    // If the sandbox config has an explicit shell override, use it for shell
    // panes. Empty string = resolve from host $SHELL (the default).
    let sb_shell = cfg
        .repo_sandbox(Path::new(worktree))
        .shell
        .trim()
        .to_string();
    // When running inside an OCI container the host's absolute $SHELL path
    // (e.g. /run/current-system/sw/bin/zsh) does not exist in the container
    // filesystem.  Pass in_oci=true so shell_inner() uses only the basename.
    let in_oci = sb.spec.as_ref().map_or(false, |s| s.backend.is_oci());
    let cmd = if choice == "shell" && !sb_shell.is_empty() {
        shell_inner_override(&sb_shell)
    } else if choice == "shell" {
        shell_inner(in_oci)
    } else {
        resolve_command(cfg, choice)
    };
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
    let argv = match &sb.spec {
        Some(spec) => sandbox::enter_argv(spec, &cmd),
        // Host fallback: run the command through a login shell so PATH/env expand.
        None => vec![superzej_core::util::shell(), "-lc".to_string(), cmd],
    };
    LaunchSpec {
        argv,
        cwd,
        env,
        backend: sb.backend_label.clone(),
        warnings: sb.warnings.clone(),
    }
}

/// Compose the [`LaunchSpec`] for running `choice` in `worktree`. Records the
/// choice (and any sandbox backend) in the DB, mirroring the zellij path's
/// side effects so the dashboard/`--resume` keep working. Errors when an
/// explicit sandbox choice cannot be honored (no silent host fallback).
///
/// `branch` is the worktree's branch (for the pane env + title); `None` falls
/// back to the worktree basename.
pub fn launch_spec(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
) -> anyhow::Result<LaunchSpec> {
    let loc = GitLoc::for_worktree(Path::new(worktree));

    // Record the choice for the dashboard / `--resume` (keyed by worktree path).
    let saved_backend = match Db::open() {
        Ok(db) => {
            let _ = db.set_worktree_agent(worktree, choice);
            db.worktree_sandbox(worktree).ok().flatten()
        }
        Err(_) => None,
    };

    // The local repo root drives the per-repo sandbox overlay + slug. Prefer the
    // DB (carries remote worktrees with no local cwd), else climb from the path.
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));

    let outcome = prepare_sandbox(cfg, &repo_root, worktree, &loc, saved_backend.as_deref())?;
    if let Ok(db) = Db::open() {
        let _ = db.set_worktree_sandbox(worktree, &outcome.backend_label);
    }
    Ok(compose_spec(cfg, worktree, branch, choice, &loc, &outcome))
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
        assert_eq!(resolve_command(&cfg, "shell"), shell_inner(false));
        // Unknown label degrades to a shell.
        assert_eq!(resolve_command(&cfg, "nope"), shell_inner(false));
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_temp_state<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("sz-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::env::var_os("XDG_STATE_HOME");
        // SAFETY: guarded by ENV_LOCK; this module's DB-touching tests run inside this critical section.
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let out = f();
        match old {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn explicit_unavailable_sandbox_does_not_fall_back_to_host() {
        with_temp_state("explicit-no-host", || {
            let mut cfg = cfg_with(&[], &[]);
            cfg.sandbox.backend = superzej_core::config::SandboxBackend::Wsl;
            cfg.sandbox.backend_chain = vec!["host".to_string()];
            let worktree =
                std::env::temp_dir().join(format!("sz-agent-wsl-missing-{}", std::process::id()));
            let err = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell")
                .expect_err("explicit WSL sandbox must not degrade to host");
            let msg = err.to_string();
            assert!(
                msg.contains("explicit sandbox backend")
                    || msg.contains("refusing fallback")
                    || msg.contains("could not be resolved"),
                "{msg}"
            );
        });
    }

    #[test]
    fn auto_backend_chain_can_fall_back_to_host() {
        with_temp_state("auto-host", || {
            let mut cfg = cfg_with(&[], &[]);
            cfg.sandbox.backend = superzej_core::config::SandboxBackend::Auto;
            cfg.sandbox.backend_chain = vec!["host".to_string()];
            let worktree =
                std::env::temp_dir().join(format!("sz-agent-auto-host-{}", std::process::id()));
            let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
            assert_eq!(spec.backend, "host");
            assert!(spec.argv.join(" ").contains("sh"));
            assert_eq!(
                spec.warning_summary().as_deref(),
                Some("sandbox auto selected host")
            );
        });
    }

    #[test]
    fn auto_backend_fallthrough_carries_visible_warning() {
        with_temp_state("auto-fallthrough-warning", || {
            let mut cfg = cfg_with(&[], &[]);
            cfg.sandbox.backend = superzej_core::config::SandboxBackend::Auto;
            cfg.sandbox.backend_chain = vec!["wsl".to_string(), "host".to_string()];
            let worktree = std::env::temp_dir()
                .join(format!("sz-agent-auto-fallthrough-{}", std::process::id()));
            let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
            assert_eq!(spec.backend, "host");
            let warning = spec
                .warning_summary()
                .expect("host fallback should be visible");
            assert!(warning.contains("sandbox wsl unavailable"), "{warning}");
            assert!(
                warning.contains("running on host after sandbox fallback"),
                "{warning}"
            );
        });
    }

    #[test]
    fn compose_spec_host_fallback_is_login_shell() {
        let cfg = cfg_with(&[("claude", "claude --foo")], &[]);
        let loc = GitLoc::from_db("/wt/x", None);
        let host = SandboxOutcome {
            spec: None,
            backend_label: "host".into(),
            warnings: vec!["sandbox auto selected host".into()],
        };
        let spec = compose_spec(&cfg, "/wt/x", Some("sz/x"), "claude", &loc, &host);
        assert_eq!(
            spec.argv,
            vec![
                superzej_core::util::shell(),
                "-lc".to_string(),
                "claude --foo".to_string()
            ]
        );
        assert_eq!(spec.cwd, Some(PathBuf::from("/wt/x")));
        assert!(
            spec.env
                .contains(&("SUPERZEJ_WORKTREE".to_string(), "/wt/x".to_string()))
        );
        assert!(
            spec.env
                .contains(&("SUPERZEJ_BRANCH".to_string(), "sz/x".to_string()))
        );
        // The settled backend + warnings ride into the spec.
        assert_eq!(spec.backend, "host");
        assert_eq!(
            spec.warning_summary().as_deref(),
            Some("sandbox auto selected host")
        );
    }

    /// OCI shell panes emit a runtime probe chain so containers that don't have
    /// the host shell (e.g. a bare Debian image has bash but not zsh) still get
    /// a working login shell instead of "exec: zsh: not found".
    #[test]
    fn shell_inner_oci_emits_runtime_probe_chain() {
        let oci = shell_inner(true);
        // Must contain a POSIX command -v probe for each candidate shell.
        assert!(oci.contains("command -v"), "should probe for shell availability");
        // Must have an unconditional /bin/sh -l fallback at the end.
        assert!(oci.ends_with("exec /bin/sh -l"), "must end with /bin/sh fallback");
        // bash must always appear in the chain (present in every Debian image).
        assert!(oci.contains("bash"), "bash must be in the probe chain");
        // Non-OCI: must be a simple "<shell> -l" with the host path, not a chain.
        let host = shell_inner(false);
        assert!(!host.contains("command -v"), "host form must not emit a probe chain");
        assert!(host.ends_with(" -l"), "host form must end with -l");
    }

    #[test]
    fn prepare_sandbox_none_backend_falls_to_host() {
        let mut cfg = Config::default();
        cfg.sandbox.backend = superzej_core::config::SandboxBackend::None;
        let loc = GitLoc::from_db("/wt/x", None);
        let out = prepare_sandbox(&cfg, Path::new("/repo"), "/wt/x", &loc, None).unwrap();
        assert!(out.spec.is_none());
        assert_eq!(out.backend_label, "host");
        // An explicit "none" choice behaves the same as the configured backend.
        let out = prepare_sandbox(&cfg, Path::new("/repo"), "/wt/x", &loc, Some("none")).unwrap();
        assert!(out.spec.is_none());
    }
}
