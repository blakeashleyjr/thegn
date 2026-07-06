//! Per-worktree provisioning for HOST-backed envs: after `ensure_ready` brings
//! the machine to Ready, this applier runs the repo's toolchain tier + the
//! personal layer ([`superzej_core::envplan`] steps) INSIDE the worktree's
//! sandbox container, over the host's control channel — `podman exec` for
//! shell steps, tar-over-exec for file-carrying steps. The sprites applier in
//! `agent.rs` stays untouched; this is its generic-exec sibling.
//!
//! Everything here BLOCKS (subprocess over the multiplexed control channel)
//! and runs inside the same `spawn_blocking` context as `ensure_ready`.
//! All steps are BEST-EFFORT: a failed tool install or dotfile push warns and
//! continues (the pane still opens on the base image), mirroring the sprites
//! pipeline's fatal-vs-best-effort split where only workspace/clone are fatal
//! — and neither exists here (the worktree is already bind-mounted).

use std::time::Duration;

use superzej_core::config::Config;
use superzej_core::envplan::{self, EnvPlan, StepKind};
use superzej_core::host_config::HostBinding;
use superzej_core::store::WorkspaceStore;
use superzej_core::toolchain;
use superzej_svc::host::{HostRunner, OciRunner, oci_runner_for};

use crate::agent::{ProvisionState, ProvisionStepView, SandboxScope};

/// Marker inside the container `$HOME`: the per-container pipeline ran.
/// (Volumes/images dedup the heavy parts; this only skips the cheap replay.)
const MARKER: &str = ".superzej-host-provisioned";

/// Run the host-backed per-worktree pipeline. Returns the pane-entry init
/// line when the repo landed on the synthesized-devshell tier (`None` when
/// the repo brings its own entry — flake/direnv/devenv — or nothing applies).
pub(crate) fn provision_worktree_on_host(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
    binding: &HostBinding,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
) -> Option<String> {
    // 1. Ensure the worktree's sandbox container exists (idempotent; the
    //    Ready-host spec injection pins its image + warm volumes).
    let loc = superzej_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    let repo_root = superzej_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| superzej_core::repo::main_worktree(std::path::Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let outcome = match crate::agent::prepare_sandbox_env(
        cfg,
        &repo_root,
        worktree,
        &loc,
        None,
        false,
        SandboxScope::Shell,
        Some(env_name),
    ) {
        Ok(o) => o,
        Err(e) => {
            superzej_core::msg::warn(&format!(
                "host worktree pipeline: sandbox prepare failed for {worktree}: {e}"
            ));
            return None;
        }
    };
    let Some(spec) = outcome.spec else {
        return None; // host-shell fallback: no container to provision into
    };
    if !spec.backend.is_oci() {
        return None; // bwrap/none: the pipeline targets OCI containers only
    }
    let container = spec.name.clone();

    // 2. A probed CONCRETE runner (container execs + tar push are inherent
    //    OciRunner methods; probe fills the runtime kind, one cheap exec).
    let mut owned: OciRunner = match oci_runner_for(&binding.reach) {
        Ok(r) => r,
        Err(e) => {
            superzej_core::msg::warn(&format!("host worktree pipeline: {e}"));
            return None;
        }
    };
    if owned.connect().is_err() || owned.probe().is_err() {
        superzej_core::msg::warn("host worktree pipeline: host unreachable for exec");
        return None;
    }
    let runner: &OciRunner = &owned;

    // 3. Remote facts: container HOME + repo detection (the worktree may only
    //    exist on the host — detect over exec, parse with the pure core fn).
    let home = match runner.exec_in_container(&container, "printf %s \"$HOME\"", secs(30)) {
        Ok((true, out, _)) if !out.trim().is_empty() => out.trim().to_string(),
        _ => "/home/superzej".to_string(),
    };
    let marker_probe = format!("test -f {home}/{MARKER} && echo HAVE || echo NEED");
    let already = matches!(
        runner.exec_in_container(&container, &marker_probe, secs(30)),
        Ok((true, ref out, _)) if out.contains("HAVE")
    );
    let detect_cmd = format!(
        "cd {} 2>/dev/null; {}",
        superzej_core::util::sh_quote(worktree),
        envplan::DETECT_PROBE_SCRIPT
    );
    let req = match runner.exec_in_container(&container, &detect_cmd, secs(60)) {
        Ok((true, out, _)) => envplan::detect_from_probe(&out),
        _ => envplan::detect_from_probe(""),
    };

    // 4. The plan: toolchain tier + personal layer; no clone/workspace (the
    //    worktree is already there), no provider-only steps.
    let home_cfg = &cfg.sandbox.home;
    let host_home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let (dotfiles, _roots) =
        crate::agent_home::resolve_personal_dotfiles(&host_home, home_cfg, env_name);
    let opts = envplan::PlanOpts {
        workdir: worktree.to_string(),
        origin: None,
        dotfiles,
        tools: home_cfg.tools.clone(),
        dotfiles_repo: home_cfg.dotfiles_repo.clone(),
        setup: crate::agent_home::resolve_setup(home_cfg),
        agents: crate::agent::provisioned_agent_kinds(cfg),
        allow_nix: true,
        checkpoint: false,
        strategy: home_cfg.strategy,
        atuin: home_cfg.atuin,
        toolchain: cfg.toolchain.clone(),
        ..envplan::PlanOpts::default()
    };
    let plan = envplan::plan(&req, &opts);

    // The pane-entry hook is derived purely — compute it even when the marker
    // short-circuits the replay (a fresh szhost process still needs it).
    let init = synth_init(&req, cfg, worktree);
    if already {
        return init;
    }

    // 5. Apply, streaming the splash.
    let mut views: Vec<ProvisionStepView> = plan
        .steps
        .iter()
        .map(|s| ProvisionStepView {
            label: s.label.clone(),
            state: ProvisionState::Pending,
            detail: None,
        })
        .collect();
    progress(&views);
    for (i, step) in plan.steps.iter().enumerate() {
        views[i].state = ProvisionState::Active;
        progress(&views);
        let result: Result<(), String> = match &step.kind {
            StepKind::Exec(script) => {
                let timeout = crate::agent::provision_step_timeout(&step.id);
                match runner.exec_in_container(&container, script, timeout) {
                    Ok((true, _, _)) => Ok(()),
                    Ok((false, _, err)) => Err(err.lines().last().unwrap_or("failed").to_string()),
                    Err(e) => Err(e),
                }
            }
            StepKind::Dotfiles(names) => stage_and_push(runner, &container, &home, |staging| {
                for name in names {
                    let src = host_home.join(name);
                    if let Ok(data) = std::fs::read(&src) {
                        let dst = staging.join(name);
                        if let Some(parent) = dst.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(dst, data);
                    }
                }
            }),
            StepKind::AgentConfigs(ids) => stage_and_push(runner, &container, &home, |staging| {
                for id in ids {
                    let (dirs, files) = envplan::agent_config_paths(id);
                    for rel in dirs {
                        let src = host_home.join(&rel);
                        if !src.is_dir() {
                            continue;
                        }
                        for (abs, inner) in crate::agent_configs::collect_agent_config_files(&src) {
                            let dst = staging.join(&rel).join(&inner);
                            if let Some(parent) = dst.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = std::fs::copy(&abs, &dst);
                        }
                    }
                    for rel in files {
                        let src = host_home.join(&rel);
                        if let Ok(data) = std::fs::read(&src) {
                            let dst = staging.join(&rel);
                            if let Some(parent) = dst.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = std::fs::write(dst, data);
                        }
                    }
                }
            }),
            StepKind::AtuinSync => stage_and_push(runner, &container, &home, |staging| {
                for rel in [
                    ".config/atuin/config.toml",
                    ".local/share/atuin/key",
                    ".local/share/atuin/session",
                ] {
                    let src = host_home.join(rel);
                    if let Ok(data) = std::fs::read(&src) {
                        let dst = staging.join(rel);
                        if let Some(parent) = dst.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(dst, data);
                    }
                }
            }),
            // Provider-only machinery: no meaning on a plain OCI host.
            StepKind::Checkpoint
            | StepKind::HomeClosurePush(_)
            | StepKind::DevShellClosurePush
            | StepKind::LocalParity { .. }
            | StepKind::ManagedPi => {
                tracing::debug!(target: "szhost::host", step = %step.id, "skipped (provider-only)");
                Ok(())
            }
        };
        match result {
            Ok(()) => views[i].state = ProvisionState::Done,
            Err(e) => {
                // Best-effort: surface, keep going — the pane still opens.
                views[i].state = ProvisionState::Failed;
                views[i].detail = Some(e.clone());
                superzej_core::msg::warn(&format!(
                    "host worktree pipeline: step {} failed: {e}",
                    step.id
                ));
            }
        }
        progress(&views);
    }
    let _ = runner.exec_in_container(&container, &format!("touch {home}/{MARKER}"), secs(30));
    init
}

/// The pane-entry init line for a synthesized devshell (SynthNix tier only —
/// repos with their own flake/devenv/direnv already have an entry path).
fn synth_init(req: &envplan::EnvRequirements, cfg: &Config, worktree: &str) -> Option<String> {
    if req.tier() != envplan::Tier::SynthNix
        || matches!(
            cfg.toolchain.mode,
            superzej_core::toolchain::ToolchainMode::Mise
                | superzej_core::toolchain::ToolchainMode::Off
        )
    {
        return None;
    }
    let packages = toolchain::packages_for(&req.languages, &cfg.toolchain);
    if packages.is_empty() {
        return None;
    }
    let dir = toolchain::synth_dir(&packages);
    // Best-effort eval: a cold/missing devshell leaves the base-image shell.
    Some(format!(
        "if command -v nix >/dev/null 2>&1 && [ -d {dir} ]; then \
           eval \"$(cd {} && nix print-dev-env path:{dir} 2>/dev/null)\" || true; \
         fi",
        superzej_core::util::sh_quote(worktree),
    ))
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Stage files into a temp dir via `fill`, then tar-push them into the
/// container `$HOME`. Empty staging ⇒ no-op success.
fn stage_and_push(
    runner: &OciRunner,
    container: &str,
    home: &str,
    fill: impl FnOnce(&std::path::Path),
) -> Result<(), String> {
    let staging = std::env::temp_dir().join(format!(
        "sz-hostwt-{}-{}",
        std::process::id(),
        superzej_core::util::short_hash(container, 6)
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    fill(&staging);
    let empty = std::fs::read_dir(&staging)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true);
    let result = if empty {
        Ok(())
    } else {
        runner.push_dir_to_container(container, &staging, home, secs(120))
    };
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Report the plan a worktree WOULD run (doctor / tests): tier + step ids.
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) fn plan_summary(req: &envplan::EnvRequirements, cfg: &Config) -> (String, Vec<String>) {
    let opts = envplan::PlanOpts {
        toolchain: cfg.toolchain.clone(),
        ..envplan::PlanOpts::default()
    };
    let plan: EnvPlan = envplan::plan(req, &opts);
    (
        format!("{:?}", plan.tier),
        plan.steps.iter().map(|s| s.id.clone()).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synth_init_only_for_synthnix_and_respects_mode() {
        let cfg = Config::default();
        let synth = envplan::detect_from_probe("LANG_PYTHON=1\nLANG_DENO=1\n");
        let init = synth_init(&synth, &cfg, "/wt/x").expect("synth tier gets an entry");
        assert!(init.contains("nix print-dev-env"));
        assert!(init.contains("/.superzej/synth/"));

        // A repo with its own flake brings its own entry.
        let own = envplan::detect_from_probe("FLAKE_DEVSHELL=1\nLANG_PYTHON=1\n");
        assert!(synth_init(&own, &cfg, "/wt/x").is_none());

        // mise/off modes have no nix devshell to enter.
        let mut mise_cfg = Config::default();
        mise_cfg.toolchain.mode = superzej_core::toolchain::ToolchainMode::Mise;
        assert!(synth_init(&synth, &mise_cfg, "/wt/x").is_none());
    }

    #[test]
    fn plan_summary_shapes() {
        let cfg = Config::default();
        let (tier, steps) = plan_summary(&envplan::detect_from_probe("LANG_JVM=1\n"), &cfg);
        assert_eq!(tier, "SynthNix");
        assert!(steps.iter().any(|s| s == "toolchain"), "{steps:?}");
        assert!(
            !steps.iter().any(|s| s == "clone"),
            "no clone for existing worktrees"
        );
    }
}
