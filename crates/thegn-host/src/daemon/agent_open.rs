//! Launching a configured agent from the control API — through the *same*
//! machinery an interactive pane goes through.
//!
//! `sessions.open` has always taken a raw argv, which is honest but useless for
//! this: the caller would have to reconstruct, by hand and correctly, everything
//! thegn does around an agent. That list is long and unobvious — the worktree's
//! sandbox (bwrap/podman/ssh), the bundle and identity environment, the
//! provider's credential directory (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), the
//! build-cache mounts, the devshell entry, the resource cap, the
//! `worktrees.agent` binding — and getting any of it wrong produces an agent
//! that starts and then fails in a way that looks like the agent's fault.
//!
//! It is not hypothetical. An agent spawned through the raw-argv path died with
//! `401 OAuth access token has been revoked`, because the daemon's inherited
//! environment is not the caller's and nothing had composed the credentials.
//!
//! So this module does not reimplement any of it. It resolves the command and
//! then calls [`crate::agent::launch_spec_full`] — literally the function the
//! new-worktree wizard calls — which needs no `Session` and no `Panes`, opens
//! its own DB handle, and returns an argv that is already sandbox-wrapped and
//! already CPU-capped. A daemon-launched agent and a TUI-launched agent are the
//! same thing by construction, not by parallel maintenance.
//!
//! Blocking (SQLite, sandbox preparation, a bounded direnv warm), so callers run
//! it under `spawn_blocking`. That makes an agent-spawning `sessions.open`
//! measurably slower than a raw-argv one — seconds, not milliseconds, on a cold
//! worktree. That is the price of parity and it is worth paying.

use anyhow::{Context, Result, bail};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::WorkspaceStore;
use thegn_svc::control::{AgentLaunch, OpenSpec};

use crate::agent::{LaunchExtras, LaunchSpec};

/// Resolve an agent launch into the same `LaunchSpec` an interactive pane gets.
pub(crate) fn resolve(
    cfg: &Config,
    db: &Db,
    spec: &OpenSpec,
    launch: &AgentLaunch,
) -> Result<LaunchSpec> {
    let worktree = spec
        .worktree
        .clone()
        .or_else(|| spec.cwd.clone())
        .filter(|w| !w.is_empty())
        .context("an agent launch needs `worktree` or `cwd`")?;

    let agent = launch.agent.trim();
    if agent.is_empty() {
        bail!("an agent launch needs an agent name");
    }

    // Headless when a task was given, interactive otherwise — the shape a
    // caller almost always wants, overridable when it doesn't.
    let headless = launch.headless.unwrap_or(!launch.prompt.trim().is_empty());
    let cmd = command_for(
        cfg,
        agent,
        &launch.prompt,
        headless,
        launch.resume.as_deref().filter(|s| !s.is_empty()),
    )?;

    // The branch is the worktree's registered one; a worktree thegn does not
    // know about still launches, just without the branch in its environment.
    let branch = db
        .worktrees()
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|r| r.worktree == worktree)
                .map(|r| r.branch)
        })
        .filter(|b| !b.is_empty());

    // The one call that does everything: sandbox preparation, bundle/identity
    // env, credential directories, build-cache mounts, the CPU cap, and the
    // `worktrees.agent` record.
    let spec = crate::agent::launch_spec_full(
        cfg,
        &worktree,
        branch.as_deref(),
        agent,
        // Warm direnv synchronously: we are already off the loop, and a cold
        // worktree whose devshell has not been resolved would otherwise launch
        // the agent into an environment missing its toolchain.
        true,
        // Daemon-owned: drop bwrap's `--die-with-parent` so the session
        // survives the compositor, which is the entire point of spawning here.
        true,
        LaunchExtras {
            cmd_override: Some(&cmd),
            prompt: Some(launch.prompt.as_str()).filter(|p| !p.is_empty()),
            ..LaunchExtras::default()
        },
    )?;

    if launch.bind_worktree {
        // Not just bookkeeping: this is what tells the sidebar's activity model
        // that the worktree carries an agent, so its needs-you dot stops being
        // suppressed by the `red_requires_agent` rule.
        if let Err(e) = db.set_worktree_agent(&worktree, agent) {
            tracing::warn!(target: "thegn::daemon", "binding {agent} to {worktree} failed: {e}");
        }
    }

    Ok(spec)
}

/// The shell command for this agent, with the task substituted in.
///
/// Headless resolution reuses [`thegn_core::agent_task`], which is where the
/// merge and PR queues already get `claude -p {prompt} --permission-mode
/// acceptEdits` from — one place that knows each provider's headless form, and
/// one place that knows how to quote a prompt full of quotes and newlines for a
/// `sh -lc` body.
fn command_for(
    cfg: &Config,
    agent: &str,
    prompt: &str,
    headless: bool,
    resume: Option<&str>,
) -> Result<String> {
    use thegn_core::agent_task::{TaskVars, headless_command, resolve_agent, substitute_command};

    // Resume takes precedence over the normal launch form: the id is untrusted
    // (it crosses MCP/HTTP/CLI), so it is shape-validated and refused rather than
    // interpolated raw, then resolved through the harness's resume form.
    if let Some(id) = resume {
        if !thegn_core::harness::session_id_ok(id) {
            bail!("invalid resume session id `{id}`");
        }
        let harness = harness_for_agent(cfg, agent)
            .with_context(|| format!("unknown agent `{agent}` — cannot resume"))?;
        let cmd = harness
            .resume_command(id)
            .with_context(|| format!("agent `{agent}` does not support resume"))?;
        // A prompt alongside resume is handed over as an opening message, the
        // same way an interactive launch-with-task does.
        if prompt.trim().is_empty() {
            return Ok(cmd);
        }
        return Ok(format!("{cmd} {}", thegn_core::util::sh_quote(prompt)));
    }

    if !headless {
        let cmd = cfg
            .agent_command(agent)
            .or_else(|| cfg.tool_command(agent))
            .map(str::to_string)
            .or_else(|| bare_provider(agent).map(str::to_string))
            .with_context(|| format!("unknown agent `{agent}`"))?;
        if prompt.trim().is_empty() {
            return Ok(cmd);
        }
        // Interactive *with* a task: hand it over as a final argument, which is
        // how every agent CLI accepts an opening message.
        return Ok(format!("{cmd} {}", thegn_core::util::sh_quote(prompt)));
    }

    let template = resolve_agent(cfg, agent, "")
        .or_else(|| bare_provider(agent).map(|p| headless_command(p, p)))
        .with_context(|| format!("no headless form for agent `{agent}`"))?;
    substitute_command(&template, prompt, &TaskVars::new())
        .map_err(|e| anyhow::anyhow!("agent command template is invalid: {e}"))
}

/// A recognized harness id used as an agent name, for a caller that named
/// `claude` on a machine whose config calls it something else — or names no
/// `[[agents]]` at all.
///
/// A supervisor spawning a fleet says what it wants (`claude`); it has no way
/// to know the operator's entry names, and failing on that mismatch would be a
/// baffling error. Resolution is a closed **harness-registry** lookup: only a
/// launchable harness (one with an interactive/headless form) qualifies, so an
/// unknown name stays an error rather than becoming a guess at a command.
fn bare_provider(agent: &str) -> Option<&'static str> {
    thegn_core::harness::harness(agent)
        .filter(|h| h.headless_template().is_some() || h.home().is_some())
        .map(|h| h.id())
}

/// The harness backing a named agent: an `[[agents]]`/`[[tools]]` entry's
/// explicit `provider` (or its command basename), else the agent name treated
/// as a bare harness id. `None` when nothing in the closed registry matches.
fn harness_for_agent(
    cfg: &Config,
    agent: &str,
) -> Option<&'static dyn thegn_core::harness::Harness> {
    if let Some(entry) = cfg
        .agents
        .iter()
        .chain(cfg.tools.iter())
        .find(|a| a.name == agent)
    {
        let id = entry.provider.clone().unwrap_or_else(|| {
            let prog = entry.command.split_whitespace().next().unwrap_or_default();
            let base = thegn_core::util::basename(prog);
            base.strip_suffix(".exe").unwrap_or(base).to_string()
        });
        return thegn_core::harness::harness(&id);
    }
    thegn_core::harness::harness(agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn a_headless_claude_launch_carries_the_prompt() {
        let cmd = command_for(&cfg(), "claude", "write a test", true, None).expect("resolves");
        assert!(cmd.starts_with("claude -p "), "got {cmd}");
        assert!(cmd.contains("write a test"));
        assert!(cmd.contains("--permission-mode acceptEdits"), "got {cmd}");
    }

    /// The reason quoting lives in `agent_task` and not here: this is what an
    /// agent's task actually looks like.
    #[test]
    fn a_prompt_full_of_shell_metacharacters_survives() {
        let nasty = "fix `foo`; rm -rf /\n\"quoted\" 'single' $HOME";
        let cmd = command_for(&cfg(), "claude", nasty, true, None).expect("resolves");
        // The dangerous parts are inside quotes, not free-standing commands.
        assert!(
            !cmd.contains("; rm -rf / "),
            "unquoted metacharacters: {cmd}"
        );
        assert!(cmd.contains("rm -rf /"), "the text itself is preserved");
    }

    #[test]
    fn an_interactive_launch_has_no_headless_flag() {
        let cmd = command_for(&cfg(), "claude", "", false, None).expect("resolves");
        assert_eq!(cmd, "claude");
        assert!(!cmd.contains("-p"));
    }

    #[test]
    fn an_interactive_launch_with_a_task_appends_it_quoted() {
        let cmd = command_for(&cfg(), "claude", "hello there", false, None).expect("resolves");
        assert!(cmd.starts_with("claude "), "got {cmd}");
        assert!(cmd.contains("hello there"));
    }

    #[test]
    fn an_unknown_agent_is_an_error_not_a_guess() {
        assert!(command_for(&cfg(), "nosuchagent", "", false, None).is_err());
        assert!(
            command_for(&cfg(), "nosuchagent", "do it", true, None).is_err(),
            "the headless path must not invent a command either"
        );
    }

    /// A supervisor says `claude`; the operator's config may name that entry
    /// anything, or configure no agents at all. A recognized provider id has
    /// to keep working, or spawning a fleet would depend on someone else's
    /// naming choices.
    #[test]
    fn a_provider_id_resolves_without_a_matching_config_entry() {
        let bare = Config::default();
        assert!(bare.agents.is_empty(), "the fixture has no [[agents]]");
        assert_eq!(
            command_for(&bare, "claude", "", false, None).expect("resolves"),
            "claude"
        );
        let headless = command_for(&bare, "codex", "write a test", true, None).expect("resolves");
        assert!(headless.starts_with("codex exec "), "got {headless}");
        assert!(headless.contains("write a test"));
    }

    #[test]
    fn resume_resolves_to_the_harness_resume_form() {
        // A valid id resolves to claude's resume command with the id quoted.
        let cmd = command_for(&cfg(), "claude", "", false, Some("0c1f-uuid")).expect("resolves");
        assert!(cmd.starts_with("claude --resume "), "got {cmd}");
        assert!(cmd.contains("0c1f-uuid"), "got {cmd}");
        // A prompt alongside resume is appended as an opening message.
        let cmd =
            command_for(&cfg(), "codex", "carry on", false, Some("sess-9")).expect("resolves");
        assert!(cmd.starts_with("codex resume "), "got {cmd}");
        assert!(
            cmd.contains("sess-9") && cmd.contains("carry on"),
            "got {cmd}"
        );
    }

    #[test]
    fn an_invalid_resume_id_is_refused_and_never_interpolated() {
        let err = command_for(&cfg(), "claude", "", false, Some("bad id; rm -rf /"))
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("invalid resume session id"),
            "got {err}"
        );
    }

    #[test]
    fn resume_is_refused_for_a_harness_without_resume_support() {
        // aider is a real harness with no RESUME cap → resume errors, cold works.
        let err =
            command_for(&cfg(), "aider", "", false, Some("sess-1")).expect_err("no resume support");
        assert!(
            err.to_string().contains("does not support resume"),
            "got {err}"
        );
    }

    #[test]
    fn a_worktreeless_spec_is_refused() {
        let spec = OpenSpec {
            argv: vec![],
            cwd: None,
            env: vec![],
            rows: 24,
            cols: 80,
            worktree: None,
            agent: None,
            adopt: false,
            already_capped: false,
        };
        let launch = AgentLaunch {
            agent: "claude".into(),
            prompt: String::new(),
            headless: None,
            bind_worktree: false,
            resume: None,
        };
        let db = Db::open_memory().expect("in-memory db");
        let err = resolve(&cfg(), &db, &spec, &launch).expect_err("should refuse");
        assert!(err.to_string().contains("worktree"), "got {err}");
    }
}
