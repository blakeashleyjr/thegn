//! Per-worktree provisioning for HOST-backed envs: after `ensure_ready` brings
//! the machine to Ready, this applier runs the repo's toolchain tier + the
//! personal layer ([`thegn_core::envplan`] steps) INSIDE the worktree's
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

use thegn_core::config::Config;
use thegn_core::envplan::{self, EnvPlan, StepKind};
use thegn_core::host_config::HostBinding;
use thegn_core::store::WorkspaceStore;
use thegn_core::toolchain;
use thegn_svc::host::{HostRunner, OciRunner, oci_runner_for};

use crate::agent::{ProvisionState, ProvisionStepView};

/// Marker inside the container `$HOME`: the per-container pipeline ran.
/// (Volumes/images dedup the heavy parts; this only skips the cheap replay.)
const MARKER: &str = ".thegn-host-provisioned";
const MARKER_VERSION: &str = "2";
const TOOLCHAIN_PROVIDER_STEP: &str = "toolchain_provider";

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
    let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(worktree));
    let repo_root = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(std::path::Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let outcome = match crate::agent::prepare_sandbox_env(
        cfg,
        &repo_root,
        worktree,
        &loc,
        None,
        false,
        Some(env_name),
    ) {
        Ok(o) => o,
        Err(e) => {
            thegn_core::msg::warn(&format!(
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
            thegn_core::msg::warn(&format!("host worktree pipeline: {e}"));
            return None;
        }
    };
    if owned.connect().is_err() || owned.probe().is_err() {
        thegn_core::msg::warn("host worktree pipeline: host unreachable for exec");
        return None;
    }
    let runner: &OciRunner = &owned;

    // 3. Remote facts: container HOME + repo detection (the worktree may only
    //    exist on the host — detect over exec, parse with the pure core fn).
    let home = match runner.exec_in_container(&container, "printf %s \"$HOME\"", secs(30)) {
        Ok((true, out, _)) if !out.trim().is_empty() => out.trim().to_string(),
        _ => "/home/thegn".to_string(),
    };
    let marker_path = thegn_core::util::sh_quote(&format!("{home}/{MARKER}"));
    let marker_version = thegn_core::util::sh_quote(MARKER_VERSION);
    let marker_probe = format!(
        "test -f {marker_path} && grep -Fxq {marker_version} {marker_path} && echo HAVE || echo NEED"
    );
    let already = matches!(
        runner.exec_in_container(&container, &marker_probe, secs(30)),
        Ok((true, ref out, _)) if out.contains("HAVE")
    );
    let (req, detect_error) =
        match crate::mise_provider::detect_on_target(runner, &container, worktree) {
            Ok(req) => (req, None),
            Err(reason) => (envplan::detect_from_probe(""), Some(reason)),
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
    let mut plan = envplan::plan(&req, &opts);
    // `envplan` intentionally contains no vendor command. A ToolVersions tier
    // therefore gets a typed-in-practice provider step here, after target
    // detection and before personal files. A detection failure is retained as
    // a visible failed step instead of silently dropping the declared tier.
    if (plan.tier == envplan::Tier::ToolVersions && !req.toolchain_files.is_empty())
        || detect_error.is_some()
    {
        plan.steps.insert(
            1,
            envplan::ProvisionStep {
                id: TOOLCHAIN_PROVIDER_STEP.into(),
                label: "Install declared toolchain".into(),
                kind: StepKind::Exec(String::new()),
            },
        );
    }
    // Trust-gated devcontainer steps, appended after the toolchain tier and
    // covered by the same provision marker (so they run exactly once):
    //   1. feature installs (ordered), then
    //   2. one-time lifecycle commands (onCreate → updateContent → postCreate),
    //      which may depend on the features.
    // Both empty without a devcontainer.json (or until their category is approved).
    plan.steps
        .extend(crate::handlers::repo_trust::devcontainer_feature_steps(
            &repo_root, worktree,
        ));
    plan.steps
        .extend(crate::handlers::repo_trust::devcontainer_lifecycle_steps(
            &repo_root, worktree, worktree,
        ));

    // The pane-entry hook is derived purely — compute it even when the marker
    // short-circuits the replay (a fresh thegn process still needs it).
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
    let mut all_succeeded = true;
    for (i, step) in plan.steps.iter().enumerate() {
        views[i].state = ProvisionState::Active;
        progress(&views);
        let result: Result<(), String> = if step.id == TOOLCHAIN_PROVIDER_STEP {
            detect_error.clone().map_or_else(
                || {
                    crate::mise_provider::install_on_target(
                        cfg, worktree, worktree, &repo_root, runner, &container, &req,
                    )
                },
                Err,
            )
        } else {
            match &step.kind {
                StepKind::Exec(script) => {
                    let timeout = crate::agent::provision_step_timeout(&step.id);
                    match runner.exec_in_container(&container, script, timeout) {
                        Ok((true, _, _)) => Ok(()),
                        Ok((false, _, err)) => {
                            Err(err.lines().last().unwrap_or("failed").to_string())
                        }
                        Err(e) => Err(e),
                    }
                }
                StepKind::Dotfiles(names) => {
                    stage_and_push(runner, &container, &home, false, |staging| {
                        for name in names {
                            let rel = safe_relative_path(name)?;
                            let source = host_home.join(&rel);
                            let resolved = std::fs::canonicalize(&source)
                                .map_err(|e| format!("resolve {}: {e}", source.display()))?;
                            stage_file(staging, &resolved, &staging.join(rel), false)?;
                        }
                        Ok(())
                    })
                }
                StepKind::AgentConfigs(ids) => {
                    stage_and_push(runner, &container, &home, true, |staging| {
                        fill_agent_config_staging(staging, &host_home, ids)
                    })
                }
                StepKind::AtuinSync => {
                    stage_and_push(runner, &container, &home, false, |staging| {
                        fill_atuin_staging(staging, &host_home)
                    })
                }
                // Provider-only machinery: no meaning on a plain OCI host.
                StepKind::Checkpoint
                | StepKind::HomeClosurePush(_)
                | StepKind::LocalParity { .. }
                | StepKind::SnapshotRestore { .. } => {
                    tracing::debug!(target: "thegn::host", step = %step.id, "skipped (provider-only)");
                    Ok(())
                }
            }
        };
        match result {
            Ok(()) => views[i].state = ProvisionState::Done,
            Err(e) => {
                // Best-effort: surface, keep going — the pane still opens.
                all_succeeded = false;
                views[i].state = ProvisionState::Failed;
                views[i].detail = Some(e.clone());
                thegn_core::msg::warn(&format!(
                    "host worktree pipeline: step {} failed: {e}",
                    step.id
                ));
            }
        }
        progress(&views);
    }
    if all_succeeded {
        let marker_write = format!("printf '%s\\n' {marker_version} > {marker_path}");
        let _ = runner.exec_in_container(&container, &marker_write, secs(30)); // best-effort: marker write: a failed write just re-runs init on the next claim
    }
    init
}

/// The pane-entry init line for a synthesized devshell (SynthNix tier only —
/// repos with their own flake/devenv/direnv already have an entry path).
fn synth_init(req: &envplan::EnvRequirements, cfg: &Config, worktree: &str) -> Option<String> {
    if req.tier() != envplan::Tier::SynthNix
        || matches!(
            cfg.toolchain.mode,
            thegn_core::toolchain::ToolchainMode::Mise | thegn_core::toolchain::ToolchainMode::Off
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
        thegn_core::util::sh_quote(worktree),
    ))
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Stage files into a private, exclusive temp dir via `fill`, then tar-push
/// them into the container `$HOME`. An empty transaction is an error.
fn stage_and_push(
    runner: &OciRunner,
    container: &str,
    home: &str,
    allow_empty: bool,
    fill: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<(), String> {
    with_private_staging(fill, allow_empty, |staging| {
        runner.push_dir_to_container(container, staging, home, secs(120))
    })
}

fn fill_agent_config_staging(
    staging: &std::path::Path,
    host_home: &std::path::Path,
    ids: &[String],
) -> Result<(), String> {
    for id in ids {
        let (files, dirs) = envplan::agent_config_paths(id);
        for rel in dirs {
            let src = host_home.join(&rel);
            if !src.is_dir() {
                continue;
            }
            for (abs, inner, exec) in crate::agent_configs::collect_agent_config_files(&src) {
                let base = safe_relative_path(&rel)?;
                let inner = safe_relative_path(&inner)?;
                let relative_path = base.join(&inner);
                if is_agent_secret_path(&relative_path) {
                    continue;
                }
                // Agent config collection intentionally follows links (for
                // NixOS hooks); resolve that policy explicitly, then use a
                // no-follow open on the resolved regular file.
                let Some(resolved) = resolve_optional_agent_config_file(&abs)? else {
                    continue;
                };
                let destination = staging.join(&relative_path);
                if is_sanitized_agent_config_path(&relative_path) {
                    stage_sanitized_agent_config(staging, &resolved, &destination)?;
                } else {
                    stage_file(staging, &resolved, &destination, exec)?;
                }
            }
        }
        for rel in files {
            let rel = safe_relative_path(&rel)?;
            if is_agent_secret_path(&rel) {
                continue;
            }
            let source = host_home.join(&rel);
            let Some(resolved) = resolve_optional_agent_config_file(&source)? else {
                continue;
            };
            let destination = staging.join(&rel);
            if is_sanitized_agent_config_path(&rel) {
                stage_sanitized_agent_config(staging, &resolved, &destination)?;
            } else {
                stage_file(staging, &resolved, &destination, false)?;
            }
        }
    }
    Ok(())
}

fn resolve_optional_agent_config_file(
    path: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, String> {
    match std::fs::canonicalize(path) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("resolve {}: {error}", path.display())),
    }
}

fn fill_atuin_staging(
    staging: &std::path::Path,
    host_home: &std::path::Path,
) -> Result<(), String> {
    // Atuin runs on defaults without config.toml; its encryption key is required.
    let config = safe_relative_path(".config/atuin/config.toml")?;
    let config_source = host_home.join(&config);
    match std::fs::canonicalize(&config_source) {
        Ok(resolved) => stage_file(staging, &resolved, &staging.join(config), false)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("resolve {}: {error}", config_source.display())),
    }

    let key = safe_relative_path(".local/share/atuin/key")?;
    let key_source = host_home.join(&key);
    let resolved_key = std::fs::canonicalize(&key_source)
        .map_err(|error| format!("resolve {}: {error}", key_source.display()))?;
    stage_file(staging, &resolved_key, &staging.join(key), false)?;

    // Session exists only while logged in.
    let session = safe_relative_path(".local/share/atuin/session")?;
    let session_source = host_home.join(&session);
    match std::fs::canonicalize(&session_source) {
        Ok(resolved) => stage_file(staging, &resolved, &staging.join(session), false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("resolve {}: {error}", session_source.display())),
    }
}

/// Agent config staging deliberately excludes credentials. Sealed panes only
/// receive the managed per-account credential directory created under thegn's
/// state; `docs/help/sandboxing.md` says ambient `~/.claude` / `~/.codex`
/// credentials are never mounted into sealed panes. Keep host credentials out
/// of this separate host-to-container provisioning path too.
fn is_agent_secret_path(path: &std::path::Path) -> bool {
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if components
        .iter()
        .take(components.len().saturating_sub(1))
        .any(|component| component == "secrets" || component == "credentials")
    {
        return true;
    }
    let Some(name) = components.last() else {
        return false;
    };
    name.contains("credential")
        || name.contains("creds")
        || name.contains("token")
        || name.contains("secret")
        || name.contains("apikey")
        || name.contains("api_key")
        || name.starts_with(".env")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name == "auth.json"
        || name.ends_with(".auth.json")
}

fn is_sanitized_agent_config_path(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("json" | "toml")
    )
}

/// Agent configs can contain credentials in nested MCP environments and
/// headers. Sanitize every staged JSON/TOML config recursively; malformed
/// files are skipped instead of copied verbatim.
fn stage_sanitized_agent_config(
    staging_root: &std::path::Path,
    src: &std::path::Path,
    dst: &std::path::Path,
) -> Result<(), String> {
    use std::io::Write;

    let mut source = match thegn_core::fsperm::open_regular_file_nofollow(src) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read {}: {error}", src.display())),
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut source, &mut bytes)
        .map_err(|e| format!("read {}: {e}", src.display()))?;
    let extension = dst
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let sanitized = if extension == "json" {
        let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Ok(());
        };
        sanitize_json_config(&mut value);
        serde_json::to_vec(&value).map_err(|e| format!("serialize {}: {e}", src.display()))?
    } else {
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(());
        };
        let Ok(mut value) = text.parse::<toml::Value>() else {
            return Ok(());
        };
        sanitize_toml_config(&mut value);
        toml::to_string(&value)
            .map_err(|e| format!("serialize {}: {e}", src.display()))?
            .into_bytes()
    };
    create_staging_parents(staging_root, dst)?;
    let mut output = thegn_core::fsperm::create_owner_only_exclusive(dst)
        .map_err(|e| format!("create staged file {}: {e}", dst.display()))?;
    output
        .write_all(&sanitized)
        .and_then(|()| output.sync_all())
        .map_err(|e| format!("write staged file {}: {e}", dst.display()))
}

fn is_config_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    matches!(normalized.as_str(), "env" | "headers")
        || [
            "apikey",
            "api_key",
            "token",
            "secret",
            "password",
            "credential",
        ]
        .iter()
        .any(|needle| normalized.contains(needle))
}

fn sanitize_json_config(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.retain(|key, _| !is_config_secret_key(key));
            for child in object.values_mut() {
                sanitize_json_config(child);
            }
        }
        serde_json::Value::Array(array) => {
            for child in array {
                sanitize_json_config(child);
            }
        }
        _ => {}
    }
}

fn sanitize_toml_config(value: &mut toml::Value) {
    match value {
        toml::Value::Table(table) => {
            table.retain(|key, _| !is_config_secret_key(key));
            for child in table.values_mut() {
                sanitize_toml_config(child);
            }
        }
        toml::Value::Array(array) => {
            for child in array {
                sanitize_toml_config(child);
            }
        }
        _ => {}
    }
}

fn with_private_staging(
    fill: impl FnOnce(&std::path::Path) -> Result<(), String>,
    allow_empty: bool,
    transfer: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<(), String> {
    let staging = tempfile::Builder::new()
        .prefix("tg-hostwt-")
        .tempdir()
        .map_err(|e| format!("create private staging directory: {e}"))?;
    let result = thegn_core::fsperm::restrict_dir_to_owner(staging.path())
        .map_err(|e| format!("restrict staging directory: {e}"))
        .and_then(|()| {
            fill(staging.path())?;
            let mut entries = std::fs::read_dir(staging.path())
                .map_err(|e| format!("inspect staging directory: {e}"))?;
            let has_entry = entries
                .next()
                .transpose()
                .map_err(|e| format!("inspect staging directory entry: {e}"))?
                .is_some();
            if !has_entry {
                return if allow_empty {
                    Ok(())
                } else {
                    Err("staging transaction produced no files".into())
                };
            }
            transfer(staging.path())
        });
    let cleanup = staging
        .close()
        .map_err(|e| format!("remove staging directory: {e}"));
    finish_staging_transaction(result, cleanup)
}

fn finish_staging_transaction(
    result: Result<(), String>,
    cleanup: Result<(), String>,
) -> Result<(), String> {
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(format!("{error}; {cleanup}")),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Accept only normalized relative paths: no roots, parent components, or
/// platform-specific separator tricks. Callers may use nested known config
/// paths, while user-configured dotfiles are additionally restricted to one
/// component in `agent_home`.
fn safe_relative_path(value: &str) -> Result<std::path::PathBuf, String> {
    use std::path::Component;
    if value.is_empty() || value.contains('\\') || value.chars().any(char::is_control) {
        return Err(format!("unsafe provisioning path {value:?}"));
    }
    let path = std::path::Path::new(value);
    let normalized = path
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_string_lossy().into_owned(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("/");
    if path.is_absolute()
        || normalized != value
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.components().count() == 0
    {
        return Err(format!("unsafe provisioning path {value:?}"));
    }
    Ok(path.to_path_buf())
}

/// Read an existing regular file and create its staged copy exclusively with
/// owner-only permissions. Agent hooks marked executable retain that bit only
/// for the in-container copy path that depends on it.
fn stage_file(
    staging_root: &std::path::Path,
    src: &std::path::Path,
    dst: &std::path::Path,
    executable: bool,
) -> Result<(), String> {
    use std::io::{Read, Write};
    let mut source = match thegn_core::fsperm::open_regular_file_nofollow(src) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read {}: {error}", src.display())),
    };
    let mut bytes = Vec::new();
    source
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", src.display()))?;
    create_staging_parents(staging_root, dst)?;
    let mut output = thegn_core::fsperm::create_owner_only_exclusive(dst)
        .map_err(|e| format!("create staged file {}: {e}", dst.display()))?;
    output
        .write_all(&bytes)
        .and_then(|()| output.sync_all())
        .map_err(|e| format!("write staged file {}: {e}", dst.display()))?;
    if executable {
        crate::platform::set_owner_executable(dst)
            .map_err(|e| format!("set staged mode {}: {e}", dst.display()))?;
    }
    Ok(())
}

/// Create each staged directory component without accepting a pre-existing
/// symlink as a directory. The private temporary root is the only parent.
fn create_staging_parents(
    staging_root: &std::path::Path,
    destination: &std::path::Path,
) -> Result<(), String> {
    use std::path::Component;
    let parent = destination
        .parent()
        .ok_or_else(|| format!("staging path has no parent: {}", destination.display()))?;
    let relative = parent
        .strip_prefix(staging_root)
        .map_err(|_| format!("staging path escaped its root: {}", destination.display()))?;
    let mut current = staging_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(format!("unsafe staging parent: {}", parent.display()));
        };
        current.push(name);
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&current)
                    .map_err(|e| format!("inspect staging directory {}: {e}", current.display()))?;
                if !metadata.file_type().is_dir() {
                    return Err(format!(
                        "staging path component is not a directory: {}",
                        current.display()
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "create staging directory {}: {error}",
                    current.display()
                ));
            }
        }
        thegn_core::fsperm::restrict_dir_to_owner(&current)
            .map_err(|e| format!("restrict staging directory {}: {e}", current.display()))?;
    }
    Ok(())
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
        assert!(init.contains("/.thegn/synth/"));

        // A repo with its own flake brings its own entry.
        let own = envplan::detect_from_probe("FLAKE_DEVSHELL=1\nLANG_PYTHON=1\n");
        assert!(synth_init(&own, &cfg, "/wt/x").is_none());

        // mise/off modes have no nix devshell to enter.
        let mut mise_cfg = Config::default();
        mise_cfg.toolchain.mode = thegn_core::toolchain::ToolchainMode::Mise;
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

    #[test]
    fn staging_transaction_is_private_and_cleans_up_after_fill_failure() {
        use std::cell::RefCell;

        let seen = RefCell::new(None);
        let result = with_private_staging(
            |staging| {
                *seen.borrow_mut() = Some(staging.to_path_buf());
                if let Some(mode) = thegn_core::fsperm::mode_bits(staging).unwrap() {
                    assert_eq!(mode, 0o700);
                }
                Err("injected read failure".into())
            },
            false,
            |_| panic!("a failed fill must never transfer"),
        );
        assert!(result.unwrap_err().contains("injected read failure"));
        assert!(!seen.borrow().as_ref().unwrap().exists());
    }

    #[test]
    fn staging_transaction_rejects_empty_output() {
        let transferred = std::cell::Cell::new(false);
        let result = with_private_staging(
            |_| Ok(()),
            false,
            |_| {
                transferred.set(true);
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("produced no files"));
        assert!(!transferred.get());
    }

    #[test]
    fn concurrent_staging_transactions_get_distinct_directories() {
        use std::sync::{Arc, Barrier, Mutex};

        let barrier = Arc::new(Barrier::new(2));
        let paths = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for contents in [&b"first"[..], &b"second"[..]] {
                let barrier = Arc::clone(&barrier);
                let paths = &paths;
                workers.push(scope.spawn(move || {
                    with_private_staging(
                        |staging| {
                            std::fs::write(staging.join("identity"), contents)
                                .map_err(|e| e.to_string())
                        },
                        false,
                        |staging| {
                            paths.lock().unwrap().push(staging.to_path_buf());
                            barrier.wait();
                            assert_eq!(std::fs::read(staging.join("identity")).unwrap(), contents);
                            Ok(())
                        },
                    )
                    .unwrap();
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        let paths = paths.into_inner().unwrap();
        assert_eq!(paths.len(), 2);
        assert_ne!(paths[0], paths[1]);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn staging_transaction_reports_cleanup_failure() {
        let error = finish_staging_transaction(
            Err("injected transfer failure".into()),
            Err("remove staging directory: injected cleanup failure".into()),
        )
        .unwrap_err();
        assert!(error.contains("injected transfer failure"));
        assert!(error.contains("injected cleanup failure"));
    }

    #[test]
    fn a_failed_staging_attempt_does_not_poison_the_retry() {
        let failed =
            with_private_staging(|_| Err("injected copy failure".into()), false, |_| Ok(()));
        assert!(failed.unwrap_err().contains("injected copy failure"));

        let retry = with_private_staging(
            |staging| stage_file_from_bytes_for_test(staging, "credential", b"retry data"),
            false,
            |staging| {
                assert_eq!(
                    std::fs::read(staging.join("credential")).unwrap(),
                    b"retry data"
                );
                Ok(())
            },
        );
        assert!(retry.is_ok());
    }

    fn stage_file_from_bytes_for_test(
        staging: &std::path::Path,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        use std::io::Write;
        let mut file = thegn_core::fsperm::create_owner_only_exclusive(&staging.join(name))
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())
    }

    #[test]
    fn stage_file_is_exclusive_owner_only_and_rejects_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("staging").join("secret");
        std::fs::write(&source, b"secret bytes").unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        stage_file(&staging, &source, &destination, false).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"secret bytes");
        if let Some(mode) = thegn_core::fsperm::mode_bits(&destination).unwrap() {
            assert_eq!(mode, 0o600);
        }
        assert!(stage_file(&staging, &source, &destination, false).is_err());
        let executable = staging.join("hook");
        stage_file(&staging, &source, &executable, true).unwrap();
        if let Some(mode) = thegn_core::fsperm::mode_bits(&executable).unwrap() {
            assert_eq!(mode, 0o700);
        }

        if crate::platform::test_symlink_supported() {
            let link = root.path().join("source-link");
            crate::platform::test_symlink(&source, &link).unwrap();
            assert!(
                stage_file(
                    &staging,
                    &link,
                    &root.path().join("staging/link-copy"),
                    false
                )
                .is_err()
            );
            let external = root.path().join("external");
            std::fs::create_dir(&external).unwrap();
            crate::platform::test_symlink(&external, &staging.join("linked-parent")).unwrap();
            assert!(
                stage_file(
                    &staging,
                    &source,
                    &staging.join("linked-parent/escaped"),
                    false
                )
                .is_err()
            );
            assert!(!external.join("escaped").exists());
        }
    }

    #[test]
    fn agent_config_staging_preserves_root_and_file_paths() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"theme":"dark","primaryApiKey":"sk-secret","customApiKey":"also-secret","refreshToken":"token-secret","oauthAccount":{"email":"person@example.com"},"mcpServers":{"x":{"command":"node","env":{"GITHUB_TOKEN":"nested-secret","SAFE_VALUE":"keep"}}},"projects":{"/p":{"mcpServers":{"y":{"headers":{"Authorization":"Bearer nested-secret","Accept":"application/json"}}}}}}"#,
        )
        .unwrap();
        std::fs::create_dir(home.path().join(".claude")).unwrap();
        std::fs::write(home.path().join(".claude/settings.json"), "{}\n").unwrap();
        std::fs::write(home.path().join(".claude/.credentials.json"), "secret\n").unwrap();
        std::fs::write(home.path().join(".claude/oauth_token.json"), "secret\n").unwrap();
        std::fs::create_dir(home.path().join(".codex")).unwrap();
        std::fs::write(home.path().join(".codex/auth.json"), "secret\n").unwrap();
        std::fs::create_dir(home.path().join(".gemini")).unwrap();
        std::fs::create_dir_all(home.path().join(".gemini/secrets")).unwrap();
        std::fs::create_dir_all(home.path().join(".gemini/credentials")).unwrap();
        std::fs::write(
            home.path().join(".gemini/secrets/foo.json"),
            r#"{"visible":"secret directory"}"#,
        )
        .unwrap();
        std::fs::write(home.path().join(".gemini/credentials/default"), "secret\n").unwrap();
        std::fs::write(
            home.path().join(".codex/config.toml"),
            "model = 'keep'\n\n[mcp_servers.x]\ncommand = 'node'\n\n[mcp_servers.x.env]\nGITHUB_TOKEN = 'nested-secret'\nSAFE_VALUE = 'keep'\n\n[mcp_servers.x.headers]\nAuthorization = 'Bearer nested-secret'\nAccept = 'application/json'\n",
        )
        .unwrap();
        std::fs::write(home.path().join(".gemini/oauth_creds.json"), "secret\n").unwrap();
        std::fs::write(home.path().join(".gemini/api_key.json"), "secret\n").unwrap();
        std::fs::write(home.path().join(".gemini/apikey.json"), "secret\n").unwrap();
        std::fs::write(home.path().join(".gemini/my-secret.json"), "secret\n").unwrap();
        std::fs::write(home.path().join(".gemini/.env.local"), "secret\n").unwrap();
        std::fs::write(home.path().join(".gemini/client.pem"), "secret\n").unwrap();
        std::fs::write(home.path().join(".gemini/client.key"), "secret\n").unwrap();

        fill_agent_config_staging(
            staging.path(),
            home.path(),
            &["claude".into(), "codex".into(), "gemini".into()],
        )
        .unwrap();

        let claude_config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(staging.path().join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(claude_config["theme"], "dark");
        assert_eq!(claude_config["oauthAccount"]["email"], "person@example.com");
        assert!(claude_config.get("primaryApiKey").is_none());
        assert!(claude_config.get("customApiKey").is_none());
        assert!(claude_config.get("refreshToken").is_none());
        assert!(claude_config["mcpServers"]["x"].get("env").is_none());
        assert!(
            claude_config["projects"]["/p"]["mcpServers"]["y"]
                .get("headers")
                .is_none()
        );
        assert_eq!(claude_config["mcpServers"]["x"]["command"], "node");
        assert_eq!(claude_config["mcpServers"]["x"]["SAFE_VALUE"], "keep");
        let codex_config: toml::Value = toml::from_str(
            &std::fs::read_to_string(staging.path().join(".codex/config.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(codex_config["model"].as_str(), Some("keep"));
        assert!(codex_config["mcp_servers"]["x"].get("env").is_none());
        assert!(codex_config["mcp_servers"]["x"].get("headers").is_none());
        assert_eq!(
            std::fs::read(staging.path().join(".claude/settings.json")).unwrap(),
            b"{}\n"
        );
        assert!(!staging.path().join(".claude/.credentials.json").exists());
        assert!(!staging.path().join(".claude/oauth_token.json").exists());
        assert!(!staging.path().join(".codex/auth.json").exists());
        assert!(!staging.path().join(".gemini/secrets/foo.json").exists());
        assert!(!staging.path().join(".gemini/credentials/default").exists());
        for secret in [
            ".gemini/oauth_creds.json",
            ".gemini/api_key.json",
            ".gemini/apikey.json",
            ".gemini/my-secret.json",
            ".gemini/.env.local",
            ".gemini/client.pem",
            ".gemini/client.key",
        ] {
            assert!(!staging.path().join(secret).exists(), "staged {secret}");
        }
    }

    #[test]
    fn malformed_claude_json_is_skipped_instead_of_staged_raw() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".claude.json"), "primaryApiKey: secret\n").unwrap();

        fill_agent_config_staging(staging.path(), home.path(), &["claude".into()]).unwrap();

        assert!(!staging.path().join(".claude.json").exists());
    }

    #[test]
    fn missing_agent_config_file_resolves_as_a_skip() {
        let home = tempfile::tempdir().unwrap();

        let resolved =
            resolve_optional_agent_config_file(&home.path().join("rewritten.json")).unwrap();

        assert!(resolved.is_none());
    }

    #[test]
    fn agent_config_vanishing_before_open_is_a_skip() {
        let root = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let source = root.path().join("vanished.json");
        std::fs::write(&source, r#"{"safe":"value"}"#).unwrap();
        let vanished = std::fs::canonicalize(&source).unwrap();
        std::fs::remove_file(&source).unwrap();
        let raw_destination = staging.path().join("raw.json");
        let sanitized_destination = staging.path().join("sanitized.json");

        assert!(stage_file(staging.path(), &vanished, &raw_destination, false).is_ok());
        assert!(
            stage_sanitized_agent_config(staging.path(), &vanished, &sanitized_destination).is_ok()
        );
        assert!(!raw_destination.exists());
        assert!(!sanitized_destination.exists());
    }

    #[test]
    fn top_level_agent_config_symlink_is_resolved_before_staging() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let target = home.path().join("claude-config-target");
        std::fs::write(&target, "{}\n").unwrap();
        crate::platform::test_symlink(&target, &home.path().join(".claude.json")).unwrap();

        fill_agent_config_staging(staging.path(), home.path(), &["claude".into()]).unwrap();

        assert_eq!(
            std::fs::read(staging.path().join(".claude.json")).unwrap(),
            b"{}\n"
        );
    }

    #[test]
    fn atuin_session_and_config_are_optional_but_key_is_required() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/atuin")).unwrap();
        std::fs::create_dir_all(home.path().join(".local/share/atuin")).unwrap();
        std::fs::write(
            home.path().join(".config/atuin/config.toml"),
            "sync = true\n",
        )
        .unwrap();
        std::fs::write(home.path().join(".local/share/atuin/key"), "secret\n").unwrap();

        fill_atuin_staging(staging.path(), home.path()).unwrap();
        assert!(staging.path().join(".config/atuin/config.toml").is_file());
        assert!(staging.path().join(".local/share/atuin/key").is_file());
        assert!(!staging.path().join(".local/share/atuin/session").exists());

        std::fs::remove_file(home.path().join(".local/share/atuin/key")).unwrap();
        let missing_key = tempfile::tempdir().unwrap();
        assert!(fill_atuin_staging(missing_key.path(), home.path()).is_err());
    }

    #[test]
    fn atuin_config_symlink_is_resolved_and_config_may_be_missing() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/atuin")).unwrap();
        std::fs::create_dir_all(home.path().join(".local/share/atuin")).unwrap();
        let config_target = home.path().join("atuin-config-target");
        std::fs::write(&config_target, "sync = true\n").unwrap();
        if crate::platform::test_symlink_supported() {
            crate::platform::test_symlink(
                &config_target,
                &home.path().join(".config/atuin/config.toml"),
            )
            .unwrap();
            std::fs::write(home.path().join(".local/share/atuin/key"), "key\n").unwrap();
            let staging = tempfile::tempdir().unwrap();

            fill_atuin_staging(staging.path(), home.path()).unwrap();

            assert_eq!(
                std::fs::read(staging.path().join(".config/atuin/config.toml")).unwrap(),
                b"sync = true\n"
            );
        }

        std::fs::remove_file(home.path().join(".config/atuin/config.toml")).unwrap_or_else(
            |error| {
                assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            },
        );
        std::fs::write(home.path().join(".local/share/atuin/key"), "key\n").unwrap();
        let missing_config = tempfile::tempdir().unwrap();

        fill_atuin_staging(missing_config.path(), home.path()).unwrap();

        assert!(
            !missing_config
                .path()
                .join(".config/atuin/config.toml")
                .exists()
        );
        assert!(
            missing_config
                .path()
                .join(".local/share/atuin/key")
                .is_file()
        );
    }

    #[test]
    fn empty_agent_config_staging_is_success_without_transfer() {
        let transferred = std::cell::Cell::new(false);
        let result = with_private_staging(
            |_| Ok(()),
            true,
            |_| {
                transferred.set(true);
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert!(!transferred.get());
    }

    #[test]
    fn provisioning_paths_must_be_normalized_relative_paths() {
        for bad in [
            "",
            "/tmp/file",
            "../file",
            "a/../file",
            "a/./file",
            "a\\file",
            "bad\nname",
        ] {
            assert!(safe_relative_path(bad).is_err(), "accepted {bad:?}");
        }
        assert_eq!(
            safe_relative_path(".config/app/file").unwrap(),
            std::path::PathBuf::from(".config/app/file")
        );
    }
}
