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
    resolve_inner(cfg, db, spec, launch, None)
}

/// Resolve `tools.run` exclusively through a fresh trusted `[[tools]]` entry.
/// An agent or bare harness with the same name is intentionally invisible.
pub(crate) fn resolve_tool(
    cfg: &Config,
    db: &Db,
    worktree: &str,
    name: &str,
) -> Result<LaunchSpec> {
    let command = configured_tool_command(cfg, name)?;
    let branch = db
        .worktrees()
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|row| row.worktree == worktree)
                .map(|row| row.branch)
        })
        .filter(|branch| !branch.is_empty());
    crate::agent::launch_spec_full(
        cfg,
        worktree,
        branch.as_deref(),
        name,
        true,
        true,
        LaunchExtras {
            cmd_override: Some(command),
            suppress_agent_record: true,
            ..LaunchExtras::default()
        },
    )
}

fn configured_tool_command<'a>(cfg: &'a Config, name: &str) -> Result<&'a str> {
    cfg.tool_command(name)
        .with_context(|| format!("unknown configured tool `{name}`"))
}

pub(crate) fn ensure_configured_agent(cfg: &Config, name: &str) -> Result<()> {
    anyhow::ensure!(
        cfg.agent_command(name).is_some(),
        "unknown configured agent `{name}`"
    );
    Ok(())
}

/// Resolve a native fork while preserving the harness selected by the source
/// row. The command is produced by the core harness seam; this function only
/// composes the configured agent's current sandbox and credentials around it.
pub(crate) fn resolve_fork(
    cfg: &Config,
    db: &Db,
    spec: &OpenSpec,
    launch: &AgentLaunch,
    source_harness: &str,
    source_command: &str,
) -> Result<LaunchSpec> {
    validate_fork_harness(cfg, &launch.agent, source_harness)?;
    resolve_inner(cfg, db, spec, launch, Some(source_command))
}

fn validate_fork_harness(cfg: &Config, agent: &str, source_harness: &str) -> Result<()> {
    let configured = harness_for_agent(cfg, agent)
        .with_context(|| format!("unknown agent `{agent}` — cannot fork"))?;
    if configured.id() != source_harness {
        bail!(
            "agent `{agent}` is configured for harness `{}`, but the source is `{source_harness}`",
            configured.id()
        );
    }
    Ok(())
}

fn resolve_inner(
    cfg: &Config,
    db: &Db,
    spec: &OpenSpec,
    launch: &AgentLaunch,
    fork_command: Option<&str>,
) -> Result<LaunchSpec> {
    // NOTE: `cfg` is already per-request fresh — `service.rs` re-loads the
    // layered config through `config_source::fresh()` (the daemon records its
    // real `--set`/`--config` source at startup) and falls back to the boot
    // snapshot when the file no longer loads. An `[[agents]]` entry added
    // after the daemon started is honoured by the next dispatch; this module
    // never keeps a second re-loader.
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
    let stage = launch
        .stage
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cmd = if launch.fork {
        let native_id = launch
            .native_session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .context("a fork launch needs a native session id")?;
        if let Some(command) = fork_command {
            command.to_owned()
        } else {
            fork_command_for(cfg, agent, native_id)?
        }
    } else {
        command_for(
            cfg,
            agent,
            &launch.prompt,
            headless,
            launch.resume.as_deref().filter(|s| !s.is_empty()),
            launch.continue_last,
            stage,
        )?
    };

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
            stage,
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
/// `pub(crate)` so the resurrection relaunch (`handlers::worktree_launch`)
/// composes the resume form through the same shape-validated path the daemon
/// uses — one place that knows each harness's resume form.
pub(crate) fn command_for(
    cfg: &Config,
    agent: &str,
    prompt: &str,
    headless: bool,
    resume: Option<&str>,
    continue_last: bool,
    stage: Option<&str>,
) -> Result<String> {
    use thegn_core::agent_task::{TaskVars, effective_agent, substitute_command};

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

    // The continue form is the transport-retry relaunch (THE-86): same shape
    // as resume, but id-free — the harness picks up its own latest session in
    // the worktree, and the prompt (the nudge) rides as the opening message.
    if continue_last {
        let harness = harness_for_agent(cfg, agent)
            .with_context(|| format!("unknown agent `{agent}` — cannot continue"))?;
        let cmd = harness
            .continue_command()
            .with_context(|| format!("agent `{agent}` does not support continue"))?;
        if prompt.trim().is_empty() {
            return Ok(cmd);
        }
        return Ok(format!("{cmd} {}", thegn_core::util::sh_quote(prompt)));
    }

    // The entry (or bare harness id) with the stage's model/env/permissions
    // layered on; the model lands on the command through the harness's flag.
    let eff = effective_agent(cfg, agent, stage).map_err(|e| anyhow::anyhow!("{e}"))?;

    if !headless {
        let cmd = eff
            .interactive_command()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if prompt.trim().is_empty() {
            return Ok(cmd);
        }
        // Interactive *with* a task: hand it over as a final argument, which is
        // how every agent CLI accepts an opening message.
        return Ok(format!("{cmd} {}", thegn_core::util::sh_quote(prompt)));
    }

    let template = eff
        .headless_template()
        .map_err(|e| anyhow::anyhow!("no headless form for agent `{agent}`: {e}"))?;
    substitute_command(&template, prompt, &TaskVars::new())
        .map_err(|e| anyhow::anyhow!("agent command template is invalid: {e}"))
}

/// The harness backing a named agent: an `[[agents]]`/`[[tools]]` entry's
/// explicit `provider` (or its command basename), else the agent name treated
/// as a bare harness id. `None` when nothing in the closed registry matches.
pub(crate) fn harness_for_agent(
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

/// Resolve the vendor-owned native fork command. The generic daemon never
/// interprets provider syntax; it only asks the harness seam for the operation.
pub(crate) fn fork_command_for(
    cfg: &Config,
    agent: &str,
    native_session_id: &str,
) -> Result<String> {
    if !thegn_core::harness::session_id_ok(native_session_id) {
        bail!("invalid fork session id {native_session_id}");
    }
    let harness = harness_for_agent(cfg, agent)
        .with_context(|| format!("unknown agent {agent} — cannot fork"))?;
    harness
        .fork_command(native_session_id)
        .filter(|command| !command.is_empty())
        .with_context(|| format!("agent {agent} does not support native session fork"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    fn named(name: &str, command: &str) -> thegn_core::config::NamedCommand {
        thegn_core::config::NamedCommand {
            drawer_scope: Default::default(),
            drawer_cwd: Default::default(),
            name: name.into(),
            command: command.into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
        }
    }

    #[test]
    fn action_names_are_resolved_only_in_their_declared_registry() {
        let mut c = cfg();
        c.agents.push(named("same", "claude"));
        assert!(configured_tool_command(&c, "same").is_err());
        ensure_configured_agent(&c, "same").expect("configured agent");

        let mut c = cfg();
        c.tools.push(named("same", "codex"));
        assert!(ensure_configured_agent(&c, "same").is_err());
        assert_eq!(configured_tool_command(&c, "same").unwrap(), "codex");

        assert!(configured_tool_command(&cfg(), "claude").is_err());
        assert!(ensure_configured_agent(&cfg(), "claude").is_err());
    }

    #[test]
    fn a_headless_claude_launch_carries_the_prompt() {
        let cmd = command_for(&cfg(), "claude", "write a test", true, None, false, None)
            .expect("resolves");
        assert!(cmd.starts_with("claude -p "), "got {cmd}");
        assert!(cmd.contains("write a test"));
        assert!(cmd.contains("--permission-mode acceptEdits"), "got {cmd}");
    }

    /// The reason quoting lives in `agent_task` and not here: this is what an
    /// agent's task actually looks like.
    #[test]
    fn a_prompt_full_of_shell_metacharacters_survives() {
        let nasty = "fix `foo`; rm -rf /\n\"quoted\" 'single' $HOME";
        let cmd = command_for(&cfg(), "claude", nasty, true, None, false, None).expect("resolves");
        // The dangerous parts are inside quotes, not free-standing commands.
        assert!(
            !cmd.contains("; rm -rf / "),
            "unquoted metacharacters: {cmd}"
        );
        assert!(cmd.contains("rm -rf /"), "the text itself is preserved");
    }

    /// A stage's `model` rides on the harness flag for both launch shapes, and
    /// an unknown stage is an error rather than a silent default tier.
    #[test]
    fn a_stage_model_lands_on_the_command() {
        let mut c = cfg();
        c.agents.push(thegn_core::config::NamedCommand {
            name: "worker".into(),
            command: "claude".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: Some("claude-sonnet-5".into()),
            env: Default::default(),
            permissions: Vec::new(),
            drawer_scope: None,
            drawer_cwd: None,
        });
        c.pipeline
            .stages
            .push(thegn_core::config_pipeline::PipelineStage {
                name: "review".into(),
                agent: "worker".into(),
                model: Some("claude-opus-5".into()),
                ..Default::default()
            });
        let plain = command_for(&c, "worker", "do it", true, None, false, None).expect("resolves");
        assert!(plain.ends_with("--model claude-sonnet-5"), "got {plain}");
        let staged = command_for(&c, "worker", "do it", true, None, false, Some("review"))
            .expect("resolves");
        assert!(staged.starts_with("claude -p "), "got {staged}");
        assert!(staged.ends_with("--model claude-opus-5"), "got {staged}");
        let interactive =
            command_for(&c, "worker", "", false, None, false, Some("review")).unwrap();
        assert_eq!(interactive, "claude --model claude-opus-5");
        let err = command_for(&c, "worker", "", false, None, false, Some("ghost")).unwrap_err();
        assert!(err.to_string().contains("stage"), "{err}");
    }

    /// `pi` is a launchable bare harness id: headless is `pi -p <prompt>`.
    #[test]
    fn a_bare_pi_launch_is_headless_via_dash_p() {
        let cmd = command_for(&cfg(), "pi", "reply OK", true, None, false, None).expect("resolves");
        assert!(cmd.starts_with("pi -p "), "got {cmd}");
        assert!(cmd.contains("reply OK"));
    }

    #[test]
    fn an_interactive_launch_has_no_headless_flag() {
        let cmd = command_for(&cfg(), "claude", "", false, None, false, None).expect("resolves");
        assert_eq!(cmd, "claude");
        assert!(!cmd.contains("-p"));
    }

    #[test]
    fn an_interactive_launch_with_a_task_appends_it_quoted() {
        let cmd = command_for(&cfg(), "claude", "hello there", false, None, false, None)
            .expect("resolves");
        assert!(cmd.starts_with("claude "), "got {cmd}");
        assert!(cmd.contains("hello there"));
    }

    #[test]
    fn an_unknown_agent_is_an_error_not_a_guess() {
        assert!(command_for(&cfg(), "nosuchagent", "", false, None, false, None).is_err());
        assert!(
            command_for(&cfg(), "nosuchagent", "do it", true, None, false, None).is_err(),
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
            command_for(&bare, "claude", "", false, None, false, None).expect("resolves"),
            "claude"
        );
        let headless =
            command_for(&bare, "codex", "write a test", true, None, false, None).expect("resolves");
        assert!(headless.starts_with("codex exec "), "got {headless}");
        assert!(headless.contains("write a test"));
    }

    #[test]
    fn resume_resolves_to_the_harness_resume_form() {
        // A valid id resolves to claude's resume command with the id quoted.
        let cmd = command_for(&cfg(), "claude", "", false, Some("0c1f-uuid"), false, None)
            .expect("resolves");
        assert!(cmd.starts_with("claude --resume "), "got {cmd}");
        assert!(cmd.contains("0c1f-uuid"), "got {cmd}");
        // A prompt alongside resume is appended as an opening message.
        let cmd = command_for(
            &cfg(),
            "codex",
            "carry on",
            false,
            Some("sess-9"),
            false,
            None,
        )
        .expect("resolves");
        assert!(cmd.starts_with("codex resume "), "got {cmd}");
        assert!(
            cmd.contains("sess-9") && cmd.contains("carry on"),
            "got {cmd}"
        );
    }

    #[test]
    fn an_invalid_resume_id_is_refused_and_never_interpolated() {
        let err = command_for(
            &cfg(),
            "claude",
            "",
            false,
            Some("bad id; rm -rf /"),
            false,
            None,
        )
        .expect_err("must refuse");
        assert!(
            err.to_string().contains("invalid resume session id"),
            "got {err}"
        );
    }

    #[test]
    fn resume_is_refused_for_a_harness_without_resume_support() {
        // aider is a real harness with no RESUME cap → resume errors, cold works.
        let err = command_for(&cfg(), "aider", "", false, Some("sess-1"), false, None)
            .expect_err("no resume support");
        assert!(
            err.to_string().contains("does not support resume"),
            "got {err}"
        );
    }

    /// The transport-retry relaunch form (THE-86): the harness's id-free
    /// continue command, with the nudge riding as the opening message — and
    /// no session id interpolated anywhere, because thegn holds none.
    #[test]
    fn continue_last_resolves_to_the_harness_continue_form() {
        let cmd = command_for(
            &cfg(),
            "claude",
            "continue the work",
            false,
            None,
            true,
            None,
        )
        .expect("resolves");
        assert!(cmd.starts_with("claude --continue "), "got {cmd}");
        assert!(cmd.contains("continue the work"), "got {cmd}");
        // Pi advertises the continue cap too (its first continue form).
        let cmd = command_for(&cfg(), "pi", "carry on", false, None, true, None).expect("resolves");
        assert!(cmd.starts_with("pi --continue "), "got {cmd}");
        // An empty prompt is the bare continue form.
        assert_eq!(
            command_for(&cfg(), "claude", "", false, None, true, None).expect("resolves"),
            "claude --continue"
        );
    }

    #[test]
    fn continue_last_is_refused_for_a_harness_without_a_continue_form() {
        // aider has no CONTINUE cap → the error names the agent, cold works.
        let err = command_for(&cfg(), "aider", "", false, None, true, None)
            .expect_err("no continue form");
        assert!(
            err.to_string().contains("aider")
                && err.to_string().contains("does not support continue"),
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
            automation_origin: None,
        };
        let launch = AgentLaunch {
            agent: "claude".into(),
            prompt: String::new(),
            headless: None,
            bind_worktree: false,
            resume: None,
            continue_last: false,
            stage: None,
            fork: false,
            native_session_id: None,
        };
        let db = Db::open_memory().expect("in-memory db");
        let err = resolve(&cfg(), &db, &spec, &launch).expect_err("should refuse");
        assert!(err.to_string().contains("worktree"), "got {err}");
    }

    #[test]
    fn native_fork_requires_the_configured_agent_provider_to_match() {
        let mut c = cfg();
        c.agents.push(thegn_core::config::NamedCommand {
            drawer_scope: Default::default(),
            drawer_cwd: Default::default(),
            name: "worker".into(),
            command: "codex".into(),
            hints: Vec::new(),
            provider: Some("codex".into()),
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
        });
        let mismatch = validate_fork_harness(&c, "worker", "claude").expect_err("must reject");
        assert!(
            mismatch
                .to_string()
                .contains("configured for harness `codex`")
        );
        validate_fork_harness(&c, "worker", "codex").expect("matching native harness");
    }

    #[test]
    fn matching_native_fork_preserves_source_command_and_fresh_launch_context() {
        let worktree = tempfile::tempdir().expect("isolated worktree");
        let state = tempfile::tempdir().expect("isolated state dir");
        let _env = crate::testenv::EnvVarGuard::set(&[(
            "XDG_STATE_HOME",
            state.path().to_str().expect("state path"),
        )]);
        let mut c = cfg();
        c.sandbox.enabled = false;
        let mut env = std::collections::BTreeMap::new();
        env.insert("THEGN_TEST_FORK_AGENT_ENV".into(), "fresh-context".into());
        c.agents.push(thegn_core::config::NamedCommand {
            drawer_scope: Default::default(),
            drawer_cwd: Default::default(),
            name: "worker".into(),
            command: "codex".into(),
            hints: Vec::new(),
            provider: Some("codex".into()),
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env,
            permissions: Vec::new(),
        });
        let db = Db::open_memory().expect("in-memory db");
        let spec = OpenSpec {
            cwd: Some(worktree.path().to_string_lossy().into_owned()),
            worktree: Some(worktree.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let launch = AgentLaunch {
            agent: "worker".into(),
            prompt: String::new(),
            headless: Some(false),
            bind_worktree: false,
            resume: None,
            continue_last: false,
            stage: None,
            fork: true,
            native_session_id: Some("native-codex".into()),
        };
        let source_command = "source-harness-command --native native-codex";
        let resolved = resolve_fork(&c, &db, &spec, &launch, "codex", source_command)
            .expect("matching provider resolves");

        assert!(
            resolved.argv.iter().any(|arg| arg.contains(source_command)),
            "the source harness command must survive launch composition: {:?}",
            resolved.argv
        );
        assert!(
            resolved
                .env
                .iter()
                .any(|(key, value)| key == "THEGN_TEST_FORK_AGENT_ENV" && value == "fresh-context"),
            "configured launch context must still be composed: {:?}",
            resolved.env
        );
    }
}
