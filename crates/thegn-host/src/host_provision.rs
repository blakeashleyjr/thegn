//! Per-worktree provisioning for HOST-backed envs: after `ensure_ready` brings
//! the machine to Ready, this applier runs the repo's toolchain tier + the
//! personal layer ([`thegn_core::envplan`] steps) INSIDE the worktree's
//! sandbox container, over the host's control channel — `podman exec` for
//! shell steps, tar-over-exec for file-carrying steps. The sprites applier in
//! `agent.rs` stays untouched; this is its generic-exec sibling.
//!
//! Everything here BLOCKS (subprocess over the multiplexed control channel)
//! and runs inside the same `spawn_blocking` context as `ensure_ready`.
//! A failed tool install or dotfile push warns and continues (the pane still
//! opens on the base image), mirroring the sprites pipeline's fatal-vs-best-
//! effort split where only workspace/clone are fatal — and neither exists here
//! (the worktree is already bind-mounted). Delivery attestation failures fail
//! their step and withhold the provision marker.
//! A process-shared lease allows one provisioning transfer per environment and container at a time.

use std::time::Duration;

use sha2::{Digest, Sha256};
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

/// Held for one `provision_worktree_on_host` run: one transfer per (env, container) at a time (THE-441).
struct ContainerLease {
    _file: std::fs::File,
}

fn container_lease_path(
    lock_dir: &std::path::Path,
    env_name: &str,
    container: &str,
) -> std::path::PathBuf {
    let mut digest = Sha256::new();
    digest.update(b"thegn-host-provision-lease-v1\0");
    digest.update(env_name.as_bytes());
    digest.update(b"\0");
    digest.update(container.as_bytes());
    lock_dir.join(format!("{:x}.lock", digest.finalize()))
}

#[allow(deprecated)]
fn acquire_container_lease_in(
    lock_dir: &std::path::Path,
    env_name: &str,
    container: &str,
) -> std::io::Result<Option<ContainerLease>> {
    thegn_core::fsperm::ensure_private_dir(lock_dir)?;
    let path = container_lease_path(lock_dir, env_name, container);
    let file = match thegn_core::fsperm::open_regular_file_nofollow(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match thegn_core::fsperm::create_owner_only_exclusive(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    thegn_core::fsperm::open_regular_file_nofollow(&path)?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(ContainerLease { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn acquire_container_lease(
    env_name: &str,
    container: &str,
) -> std::io::Result<Option<ContainerLease>> {
    let lock_dir = thegn_core::util::thegn_dir().join("host-provision-leases");
    acquire_container_lease_in(&lock_dir, env_name, container)
}

fn container_home(probe: Result<(bool, String, String), String>) -> Result<String, String> {
    let (ok, out, stderr) =
        probe.map_err(|error| format!("container HOME probe failed: {error}"))?;
    if !ok {
        let last_stderr = stderr.lines().last().unwrap_or("(empty stderr)");
        return Err(format!("container HOME probe failed: {last_stderr}"));
    }
    let home = out.strip_suffix('\n').unwrap_or(&out).to_string();
    let normalized = home == "/"
        || (home.starts_with('/')
            && !home.chars().any(char::is_control)
            && home
                .split('/')
                .skip(1)
                .all(|segment| !segment.is_empty() && segment != "." && segment != ".."));
    if !normalized || home.chars().any(char::is_control) {
        return Err(format!(
            "container HOME is not a normalized absolute path: {home:?}"
        ));
    }
    Ok(home)
}

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
    let loc = match thegn_core::remote::GitLoc::for_worktree_op(
        std::path::Path::new(worktree),
        "host worktree pipeline",
    ) {
        Ok(loc) => loc,
        Err(error) => {
            thegn_core::msg::warn(&format!("host worktree pipeline: {error}"));
            return None;
        }
    };
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
                "host worktree pipeline: sandbox prepare failed for {worktree}: {}",
                crate::sandbox_failure::failure_detail(&e)
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
    let sealed = spec.seal_home.is_some();

    let _lease = match acquire_container_lease(env_name, &container) {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            thegn_core::msg::warn(&format!(
                "host worktree pipeline: another provisioning of container {container} is already running; this open skips it (the next open re-checks the provision marker)"
            ));
            return None;
        }
        Err(error) => {
            thegn_core::msg::warn(&format!(
                "host worktree pipeline: provisioning lease for container {container} unavailable: {error}; provisioning skipped"
            ));
            return None;
        }
    };

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
    let home =
        container_home(runner.exec_in_container(&container, "printf %s \"$HOME\"", secs(30)));
    let (req, toolchain_digest, detect_error) =
        match crate::mise_provider::detect_on_target(runner, &container, worktree) {
            Ok((req, digest)) => (req, digest, None),
            Err(reason) => (envplan::detect_from_probe(""), None, Some(reason)),
        };

    // 4. The plan: toolchain tier + personal layer; no clone/workspace (the
    //    worktree is already there), no provider-only steps.
    let home_cfg = &cfg.sandbox.home;
    let host_home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let (dotfiles, _roots) =
        crate::agent_home::resolve_personal_dotfiles(&host_home, home_cfg, env_name);
    // Warned below, only once provisioning actually runs (not on every open).
    let host_dotfiles = if sealed { dotfiles.clone() } else { Vec::new() };
    let dropped_dotfiles = host_dotfiles.len();
    let (dotfiles, atuin) = personal_host_layer(sealed, dotfiles, home_cfg.atuin);
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
        atuin,
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
    // re-run when the plan generation changes (including once after upgrade):
    //   1. feature installs (ordered), then
    //   2. one-time lifecycle commands (onCreate → updateContent → postCreate),
    //      which may depend on the features. Lifecycle commands may therefore
    //      run again on a live container after a plan change.
    // Both empty without a devcontainer.json (or until their category is approved).
    plan.steps
        .extend(crate::handlers::repo_trust::devcontainer_feature_steps(
            &repo_root, worktree,
        ));
    plan.steps
        .extend(crate::handlers::repo_trust::devcontainer_lifecycle_steps(
            &repo_root, worktree, worktree,
        ));

    with_sealed_scrub(
        &mut plan,
        sealed,
        home.as_ref().map(String::as_str).map_err(String::as_str),
        &host_dotfile_digests(&host_home, &host_dotfiles),
    );

    let marker_generation = provision_generation(&plan, toolchain_digest.as_deref());
    let marker_value = thegn_core::util::sh_quote(&marker_generation);
    let already = if let Ok(home) = &home {
        let marker_path = thegn_core::util::sh_quote(&format!("{home}/{MARKER}"));
        let marker_probe = format!(
            "test -f {marker_path} && grep -Fxq {marker_value} {marker_path} && echo HAVE || echo NEED"
        );
        matches!(
            runner.exec_in_container(&container, &marker_probe, secs(30)),
            Ok((true, ref out, _)) if out.contains("HAVE")
        )
    } else {
        false
    };

    // The pane-entry hook is derived purely — compute it even when the marker
    // short-circuits the replay (a fresh thegn process still needs it).
    let init = synth_init(&req, cfg, worktree);
    if already {
        return init;
    }
    if sealed && (home_cfg.atuin || dropped_dotfiles > 0) {
        thegn_core::msg::warn(&format!(
            "host worktree pipeline: sealed profile: [sandbox.home] atuin ({}) and dotfiles ({dropped_dotfiles}) are not applied (they are credentials/host $HOME)",
            if home_cfg.atuin {
                "configured"
            } else {
                "not configured"
            },
        ));
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
    let mut home_error_reported = false;
    for (i, step) in plan.steps.iter().enumerate() {
        views[i].state = ProvisionState::Active;
        progress(&views);
        let result: Result<(), String> = if let Some(refusal) =
            host_credential_step_refusal(&step.kind, sealed)
        {
            Err(refusal)
        } else if step.id == TOOLCHAIN_PROVIDER_STEP {
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
                        Ok((true, _, err)) => {
                            if step.id == SEALED_SCRUB_STEP && !err.trim().is_empty() {
                                tracing::debug!(target: "thegn::host", stderr = %err.trim(), "sealed scrub: skipped paths");
                            }
                            Ok(())
                        }
                        Ok((false, _, err)) => {
                            if step.id == SEALED_SCRUB_STEP && home.is_err() {
                                home_error_reported = true;
                            }
                            Err(err.lines().last().unwrap_or("failed").to_string())
                        }
                        Err(e) => Err(e),
                    }
                }
                StepKind::Dotfiles(names) => match home.as_deref() {
                    Ok(home) => stage_and_push(runner, &container, home, false, |staging| {
                        for name in names {
                            let rel = safe_relative_path(name)?;
                            let source = host_home.join(&rel);
                            let resolved = std::fs::canonicalize(&source)
                                .map_err(|e| format!("resolve {}: {e}", source.display()))?;
                            stage_file(staging, &resolved, &staging.join(rel), false)?;
                        }
                        Ok(())
                    }),
                    Err(reason) => {
                        home_error_reported = true;
                        Err(format!("container HOME unusable: {reason}"))
                    }
                },
                StepKind::AgentConfigs(_) => match home.as_deref() {
                    Ok(home) => stage_and_push(runner, &container, home, true, |staging| {
                        fill_agent_config_staging(staging)
                    }),
                    Err(reason) => {
                        home_error_reported = true;
                        Err(format!("container HOME unusable: {reason}"))
                    }
                },
                StepKind::AtuinSync => match home.as_deref() {
                    Ok(home) => stage_and_push(runner, &container, home, false, |staging| {
                        fill_atuin_staging(staging, &host_home)
                    }),
                    Err(reason) => {
                        home_error_reported = true;
                        Err(format!("container HOME unusable: {reason}"))
                    }
                },
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
    if all_succeeded && let Ok(home) = &home {
        let marker_path = thegn_core::util::sh_quote(&format!("{home}/{MARKER}"));
        let marker_write = format!("printf '%s\\n' {marker_value} > {marker_path}");
        let _ = runner.exec_in_container(&container, &marker_write, secs(30)); // best-effort: marker write: a failed write just re-runs init on the next claim
    } else if let Err(reason) = &home
        && !home_error_reported
    {
        thegn_core::msg::warn(&format!(
            "host worktree pipeline: container HOME unusable: {reason}; provisioning marker not recorded"
        ));
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
    let ready = toolchain::synth_ready_test(&packages);
    // Best-effort eval: a cold/missing devshell leaves the base-image shell.
    Some(format!(
        "if command -v nix >/dev/null 2>&1 && {ready}; then \
           eval \"$(cd {} && nix print-dev-env path:{dir} 2>/dev/null)\" || true; \
         fi",
        thegn_core::util::sh_quote(worktree),
    ))
}

const SEALED_HOME_REFUSAL: &str = "sealed profile: host $HOME files (atuin key/session, dotfiles) are not copied into the container";

/// A sealed profile promises host `$HOME` is not visible: drop every
/// host-`$HOME`-derived layer (dotfiles, atuin key/session) from the plan.
fn personal_host_layer(sealed: bool, dotfiles: Vec<String>, atuin: bool) -> (Vec<String>, bool) {
    if sealed {
        (Vec::new(), false)
    } else {
        (dotfiles, atuin)
    }
}

const SEALED_SCRUB_STEP: &str = "sealed_scrub";

/// One-time marker (inside the container home) written only after a fully
/// verified scrub, so the scrub runs once per container.
const SEALED_SCRUB_DONE: &str = ".thegn-sealed-scrub-v1";

/// Always single-quote: the script and its tests agree on one exact form.
fn sh_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `(relative name, sha256 hex)` for each host dotfile an older provision could
/// have carried into a sealed container. A file that cannot be read or hashed
/// is left out: nothing can be proven to match it, so it is never removed.
fn host_dotfile_digests(host_home: &std::path::Path, names: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in names {
        let digest = safe_relative_path(name)
            .and_then(|rel| {
                let source = host_home.join(rel);
                let resolved = std::fs::canonicalize(&source)
                    .map_err(|e| format!("resolve {}: {e}", source.display()))?;
                std::fs::read(&resolved).map_err(|e| format!("read {}: {e}", resolved.display()))
            })
            .map(|bytes| {
                Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            });
        match digest {
            Ok(hex) => out.push((name.clone(), hex)),
            Err(reason) => {
                tracing::debug!(target: "thegn::host", name = %name, %reason, "sealed scrub: dotfile not hashed, left alone");
            }
        }
    }
    out
}

/// Shell script removing host-`$HOME`-derived credential files that an older
/// provision may have carried into a sealed container.
///
/// * Paths are rooted at the validated container `home`, never a raw `$HOME`;
///   an unusable home fails the script before anything is touched.
/// * Nothing is removed through a symlinked ancestor; a symlinked or directory
///   target is not followed (a final symlink is unlinked itself, a directory
///   is left alone). Skips are logged to stderr.
/// * Nothing is removed that is, or sits under, a mount point (read from
///   `/proc/self/mountinfo`): a bind-mounted provider home is the host's own
///   file. If mount status cannot be read, the atuin paths are still scrubbed
///   and every dotfile is skipped.
/// * A dotfile is removed only when its sha256 equals the host file's digest
///   (computed host-side), so an image's own `.bashrc` is never deleted. With
///   no `sha256sum` in the container the file is removed as before.
/// * Each removal is verified; any survivor makes the script exit non-zero so
///   the provision marker is withheld and the step retries.
/// * Runs once per container: the done marker is written only after success.
fn sealed_scrub_script(home: &str, dotfiles: &[(String, String)]) -> String {
    sealed_scrub_script_with(home, dotfiles, "/proc/self/mountinfo")
}

fn sealed_scrub_script_with(home: &str, dotfiles: &[(String, String)], mountinfo: &str) -> String {
    let mut named: Vec<&(String, String)> = Vec::new();
    for entry in dotfiles {
        if safe_relative_path(&entry.0).is_ok() {
            named.push(entry);
        } else {
            tracing::debug!(target: "thegn::host", name = %entry.0, "sealed scrub: unsafe dotfile name skipped");
        }
    }
    let mut lines: Vec<String> = vec![
        format!("H={}", sh_single(home)),
        "case \"$H\" in /?*) ;; *) echo \"sealed scrub: unusable container home\" >&2; exit 1;; esac".into(),
        format!("DONE=\"$H/{SEALED_SCRUB_DONE}\""),
        "[ -e \"$DONE\" ] && exit 0".into(),
        format!("MI={}", sh_single(mountinfo)),
        "MOK=0; [ -r \"$MI\" ] && MOK=1".into(),
        "HAS=0; command -v sha256sum >/dev/null 2>&1 && HAS=1".into(),
        "rc=0".into(),
        "mounted() {".into(),
        "  e=$(printf '%s' \"$1\" | sed -e 's/\\\\/\\\\134/g' -e 's/ /\\\\040/g')".into(),
        "  while read -r _a _b _c _d mp _rest; do".into(),
        "    [ \"$mp\" = \"$e\" ] && return 0".into(),
        "  done < \"$MI\"".into(),
        "  return 1".into(),
        "}".into(),
        "scrub() {".into(),
        "  p=\"$H/$1\"; d=\"$H\"; rest=\"$1\"".into(),
        "  while :; do".into(),
        "    case \"$rest\" in */*) ;; *) break;; esac".into(),
        "    d=\"$d/${rest%%/*}\"; rest=\"${rest#*/}\"".into(),
        "    if [ -L \"$d\" ]; then echo \"sealed scrub: skip $1 (symlinked ancestor)\" >&2; return 0; fi".into(),
        "    if [ \"$MOK\" = 1 ] && mounted \"$d\"; then echo \"sealed scrub: skip $1 (under a mount)\" >&2; return 0; fi".into(),
        "  done".into(),
        "  if [ \"$MOK\" = 1 ] && mounted \"$p\"; then echo \"sealed scrub: skip $1 (mount point)\" >&2; return 0; fi".into(),
        "  if [ -d \"$p\" ] && [ ! -L \"$p\" ]; then echo \"sealed scrub: skip $1 (directory)\" >&2; return 0; fi".into(),
        "  rm -f -- \"$p\"".into(),
        "  if [ -L \"$p\" ] || [ -e \"$p\" ]; then echo \"sealed scrub: could not remove $1\" >&2; rc=1; fi".into(),
        "}".into(),
        "scrub_dot() {".into(),
        "  if [ \"$MOK\" != 1 ]; then echo \"sealed scrub: skip $1 (mount status unknown)\" >&2; return 0; fi".into(),
        "  p=\"$H/$1\"".into(),
        "  if [ -e \"$p\" ] || [ -L \"$p\" ]; then".into(),
        "    if [ \"$HAS\" = 1 ]; then".into(),
        "      got=$(sha256sum \"$p\" 2>/dev/null); got=\"${got%% *}\"".into(),
        "      if [ \"$got\" != \"$2\" ]; then echo \"sealed scrub: keep $1 (content differs from the host file)\" >&2; return 0; fi".into(),
        "    else".into(),
        "      echo \"sealed scrub: no sha256sum; removing $1 unverified\" >&2".into(),
        "    fi".into(),
        "  fi".into(),
        "  scrub \"$1\"".into(),
        "}".into(),
    ];
    for rel in [".local/share/atuin/key", ".local/share/atuin/session"] {
        lines.push(format!("scrub {}", sh_single(rel)));
    }
    for (rel, digest) in named {
        lines.push(format!(
            "scrub_dot {} {}",
            sh_single(rel),
            sh_single(digest)
        ));
    }
    lines.push("[ \"$rc\" = 0 ] || exit 1".into());
    lines.push("touch \"$DONE\" || exit 1".into());
    lines.push("exit 0".into());
    lines.join("\n")
}

/// For a sealed profile, prepend a step that removes stale host credentials
/// (atuin key/session, carried dotfiles whose content still equals the host
/// file) from containers provisioned before they were refused. The step runs
/// on every provision plan but the script is a no-op once its one-time marker
/// exists. An unusable container home makes the step fail visibly instead of
/// scrubbing relative to an unknown root.
fn with_sealed_scrub(
    plan: &mut EnvPlan,
    sealed: bool,
    home: Result<&str, &str>,
    dotfiles: &[(String, String)],
) {
    if sealed {
        let script = match home {
            Ok(home) => sealed_scrub_script(home, dotfiles),
            Err(reason) => format!(
                "echo {} >&2\nexit 1",
                sh_single(&format!("sealed scrub: {reason}"))
            ),
        };
        plan.steps.insert(
            0,
            envplan::ProvisionStep {
                id: SEALED_SCRUB_STEP.into(),
                label: "Remove stale host credentials".into(),
                kind: StepKind::Exec(script),
            },
        );
    }
}

/// Defence in depth: refuse a host-`$HOME` step even if one is planned.
fn host_credential_step_refusal(kind: &StepKind, sealed: bool) -> Option<String> {
    (sealed && matches!(kind, StepKind::Dotfiles(_) | StepKind::AtuinSync))
        .then(|| SEALED_HOME_REFUSAL.to_string())
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn provision_generation(plan: &EnvPlan, toolchain_digest: Option<&str>) -> String {
    let mut hash = Sha256::new();
    hash.update(b"thegn.host-provision.v1\0");
    for step in &plan.steps {
        hash.update(step.id.as_bytes());
        hash.update([0_u8]);
        if step.id == SEALED_SCRUB_STEP {
            // A constant tag, not the script: editing `[sandbox.home] dotfiles`
            // (never applied when sealed) must not reprovision every container.
            hash.update(b"sealed_scrub:v2");
        } else {
            hash.update(format!("{:?}", step.kind).as_bytes());
        }
        hash.update([0_u8]);
    }
    if let Some(digest) = toolchain_digest {
        hash.update(b"toolchain-config\0");
        hash.update(digest.as_bytes());
        hash.update([0_u8]);
    }
    let digest = hash.finalize();
    let hex = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{MARKER_VERSION}:{hex}")
}

/// Stage files into a private, exclusive temp dir via `fill`, then tar-push
/// them into the container `$HOME`, then attest every extracted regular file
/// is owned by the container user and remains owner-only. An empty transaction
/// is an error unless explicitly allowed.
fn stage_and_push(
    runner: &OciRunner,
    container: &str,
    home: &str,
    allow_empty: bool,
    fill: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<(), String> {
    with_private_staging(fill, allow_empty, |staging| {
        let rels = staged_files(staging)?;
        runner.push_dir_to_container(container, staging, home, secs(120))?;
        attest_delivery(runner, container, home, &rels)
    })
}

fn staged_files(root: &std::path::Path) -> Result<Vec<String>, String> {
    fn walk(
        root: &std::path::Path,
        current: &std::path::Path,
        files: &mut Vec<String>,
    ) -> Result<(), String> {
        let entries = std::fs::read_dir(current)
            .map_err(|error| format!("inspect staging directory {}: {error}", current.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("inspect staging entry: {error}"))?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("inspect staging entry {}: {error}", path.display()))?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                walk(root, &path, files)?;
            } else if file_type.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| format!("staging entry escaped root: {}", path.display()))?;
                files.push(relative.to_string_lossy().replace('\\', "/"));
            } else {
                return Err(format!(
                    "staging entry is not a regular file or directory: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn attestation_script(home: &str, rels: &[String]) -> String {
    let stats = rels
        .iter()
        .map(|rel| {
            let path = format!("{home}/{rel}");
            format!(
                "stat -c '%F|%a|%u' -- {}",
                thegn_core::util::sh_quote(&path)
            )
        })
        .collect::<Vec<_>>();
    if stats.is_empty() {
        "id -u".to_string()
    } else {
        format!("id -u && {}", stats.join(" && "))
    }
}

fn check_delivery(stdout: &str, rels: &[String]) -> Result<(), String> {
    let lines = stdout.lines().collect::<Vec<_>>();
    let first_path = rels.first().map(String::as_str).unwrap_or("<uid>");
    let uid = lines
        .first()
        .ok_or_else(|| format!("delivery attestation missing container uid for {first_path}"))?
        .parse::<u32>()
        .map_err(|_| format!("delivery attestation has invalid container uid for {first_path}"))?;
    if lines.len() != rels.len() + 1 {
        let path = rels
            .get(lines.len().saturating_sub(1))
            .or_else(|| rels.last())
            .map(String::as_str)
            .unwrap_or("<uid>");
        return Err(format!(
            "delivery attestation line count mismatch at {path}: expected {}, got {}",
            rels.len() + 1,
            lines.len()
        ));
    }
    for (index, rel) in rels.iter().enumerate() {
        let line = lines[index + 1];
        let fields = line.split('|').collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err(format!("delivery attestation has invalid shape for {rel}"));
        }
        if !matches!(fields[0], "regular file" | "regular empty file") {
            return Err(format!("delivery attestation found {} at {rel}", fields[0]));
        }
        let mode = u32::from_str_radix(fields[1], 8)
            .map_err(|_| format!("delivery attestation has invalid mode for {rel}"))?;
        if mode & 0o077 != 0 {
            return Err(format!(
                "delivery attestation permissions are not owner-only for {rel}"
            ));
        }
        let file_uid = fields[2]
            .parse::<u32>()
            .map_err(|_| format!("delivery attestation has invalid owner uid for {rel}"))?;
        if file_uid != uid {
            return Err(format!(
                "delivery attestation owner uid does not match container uid for {rel}"
            ));
        }
    }
    Ok(())
}

fn attest_delivery(
    runner: &OciRunner,
    container: &str,
    home: &str,
    rels: &[String],
) -> Result<(), String> {
    let script = attestation_script(home, rels);
    match runner.exec_in_container(container, &script, secs(30)) {
        Ok((true, out, _)) => check_delivery(&out, rels),
        Ok((false, _, err)) => Err(format!(
            "delivery attestation requires `stat -c '%F|%a|%u'` and successful file checks: {}",
            err.lines().last().unwrap_or("failed")
        )),
        Err(error) => Err(error),
    }
}

/// agent config staging is disabled pending an explicit policy decision; see the follow-up issue
fn fill_agent_config_staging(_staging: &std::path::Path) -> Result<(), String> {
    Ok(())
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

    fn personal_plan(dotfiles: Vec<String>, atuin: bool) -> EnvPlan {
        envplan::plan(
            &envplan::detect_from_probe(""),
            &envplan::PlanOpts {
                workdir: "/wt/x".into(),
                strategy: thegn_core::config::ShellStrategy::Portable,
                tools: vec!["atuin".into()],
                dotfiles,
                atuin,
                ..envplan::PlanOpts::default()
            },
        )
    }

    #[test]
    fn sealed_profile_plan_has_no_atuin_or_dotfile_steps() {
        let dotfiles = vec![".zshrc".to_string()];
        let (d, a) = personal_host_layer(true, dotfiles.clone(), true);
        assert!(d.is_empty() && !a);
        let plan = personal_plan(d, a);
        assert!(plan.steps.iter().all(|s| {
            !matches!(s.kind, StepKind::AtuinSync | StepKind::Dotfiles(_))
                && s.id != "atuin_sync"
                && s.id != "dotfiles"
        }));
        // Control: unsealed keeps both, so the assertion above is not vacuous.
        let (d, a) = personal_host_layer(false, dotfiles.clone(), true);
        assert_eq!((d.clone(), a), (dotfiles, true));
        let plan = personal_plan(d, a);
        assert!(plan.steps.iter().any(|s| s.id == "atuin_sync"));
        assert!(plan.steps.iter().any(|s| s.id == "dotfiles"));
    }

    fn hex_of(content: &str) -> String {
        Sha256::digest(content.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A carried dotfile as the host side would describe it: name + the sha256
    /// of the host file's content.
    fn entry(name: &str, host_content: &str) -> (String, String) {
        (name.to_string(), hex_of(host_content))
    }

    /// A readable fixture standing in for `/proc/self/mountinfo`; the tempdir
    /// is returned so it outlives the script run.
    fn mountinfo_fixture(body: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mountinfo");
        std::fs::write(&path, body).unwrap();
        let path = path.to_str().unwrap().to_string();
        (dir, path)
    }

    #[test]
    fn sealed_scrub_script_removes_atuin_credentials_and_listed_dotfiles() {
        let script = sealed_scrub_script(
            "/home/u",
            &[
                entry(".zshrc", "z"),
                entry(".config/git/config", "g"),
                entry(".my rc", "m"),
                entry("../evil", "e"),
                entry("/etc/passwd", "p"),
            ],
        );
        for want in [
            "H='/home/u'",
            "scrub '.local/share/atuin/key'",
            "scrub '.local/share/atuin/session'",
            "rm -f -- \"$p\"",
        ] {
            assert!(script.contains(want), "missing {want}: {script}");
        }
        for (name, content) in [
            (".zshrc", "z"),
            (".config/git/config", "g"),
            (".my rc", "m"),
        ] {
            let want = format!("scrub_dot '{name}' '{}'", hex_of(content));
            assert!(script.contains(&want), "missing {want}: {script}");
        }
        // The atuin config is not a credential and is never removed.
        assert!(!script.contains("config.toml"));
        assert!(!script.contains("evil") && !script.contains("/etc/passwd"));
        assert!(!script.contains(" -r") && !script.contains("-rf"));
        assert!(!script.contains("thegn-host-provisioned"));
        assert!(!script.contains("\"$HOME\"") && !script.trim_end().ends_with("true"));
        assert!(script.ends_with("exit 0"));
    }

    #[test]
    fn sealed_scrub_step_is_first_only_when_sealed() {
        let base = personal_plan(vec![], false);
        let mut p = base.clone();
        with_sealed_scrub(&mut p, true, Ok("/home/u"), &[entry(".zshrc", "z")]);
        assert_eq!(p.steps[0].id, "sealed_scrub");
        assert!(matches!(p.steps[0].kind, StepKind::Exec(ref s) if s.contains(".zshrc")));
        assert_eq!(p.steps.len(), base.steps.len() + 1);
        let mut q = base.clone();
        with_sealed_scrub(&mut q, false, Ok("/home/u"), &[entry(".zshrc", "z")]);
        let ids = |plan: &EnvPlan| plan.steps.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&q), ids(&base));
    }

    #[test]
    fn sealed_scrub_changes_the_provision_generation_by_tag_only() {
        let without = personal_plan(vec![], false);
        let mut with = without.clone();
        with_sealed_scrub(&mut with, true, Ok("/home/u"), &[]);
        assert_ne!(
            provision_generation(&with, None),
            provision_generation(&without, None)
        );
        // Neither the home, the dotfile names nor their digests move it.
        let mut other = without.clone();
        with_sealed_scrub(
            &mut other,
            true,
            Ok("/home/other"),
            &[entry(".zshrc", "z"), entry(".netrc", "n")],
        );
        assert_eq!(
            provision_generation(&with, None),
            provision_generation(&other, None)
        );
    }

    #[test]
    fn sealed_scrub_hashes_host_dotfiles_and_skips_unreadable_ones() {
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join(".zshrc"), "host-zsh").unwrap();
        let got = host_dotfile_digests(
            host.path(),
            &[".zshrc".into(), ".missing".into(), "../evil".into()],
        );
        assert_eq!(got, vec![(".zshrc".to_string(), hex_of("host-zsh"))]);
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test drives the scrub script in a real shell, off any UI loop"
    )]
    fn run_scrub(script: &str, state: &std::path::Path) -> std::process::ExitStatus {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .envs(thegn_core::testenv::state_env_all(state))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .status()
            .unwrap()
    }

    fn write_file(root: &std::path::Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    fn touch_file(root: &std::path::Path, rel: &str) {
        write_file(root, rel, "x");
    }

    #[test]
    fn sealed_scrub_script_actually_deletes_files() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (_mi, mountinfo) = mountinfo_fixture("");
        let home = dir.path().to_str().unwrap();
        for rel in [
            ".local/share/atuin/key",
            ".local/share/atuin/session",
            ".config/atuin/config.toml",
            ".zshrc",
            "keep.txt",
        ] {
            touch_file(dir.path(), rel);
        }
        // Content equals the host file's ("x"), so the carried copy is removed.
        let script = sealed_scrub_script_with(home, &[entry(".zshrc", "x")], &mountinfo);
        assert!(run_scrub(&script, dir.path()).success());
        for rel in [
            ".local/share/atuin/key",
            ".local/share/atuin/session",
            ".zshrc",
        ] {
            assert!(!dir.path().join(rel).exists(), "{rel} remains");
        }
        assert!(dir.path().join("keep.txt").exists());
        assert!(
            dir.path().join(".config/atuin/config.toml").exists(),
            "atuin config is not a credential"
        );
        assert!(dir.path().join(SEALED_SCRUB_DONE).exists());
        // Once per container: a dotfile recreated later survives the next run.
        touch_file(dir.path(), ".zshrc");
        assert!(run_scrub(&script, dir.path()).success());
        assert!(dir.path().join(".zshrc").exists());
    }

    #[test]
    fn sealed_scrub_keeps_a_dotfile_whose_content_differs_from_the_host_file() {
        if !std::path::Path::new("/bin/sh").exists() || !have_sha256sum() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (_mi, mountinfo) = mountinfo_fixture("");
        write_file(dir.path(), ".bashrc", "image-owned bashrc");
        write_file(dir.path(), ".zshrc", "carried from host");
        touch_file(dir.path(), ".local/share/atuin/key");
        let script = sealed_scrub_script_with(
            dir.path().to_str().unwrap(),
            &[
                entry(".bashrc", "the host bashrc"),
                entry(".zshrc", "carried from host"),
            ],
            &mountinfo,
        );
        assert!(run_scrub(&script, dir.path()).success());
        assert!(dir.path().join(".bashrc").exists(), "image .bashrc kept");
        assert!(!dir.path().join(".zshrc").exists(), "carried copy removed");
        assert!(!dir.path().join(".local/share/atuin/key").exists());
    }

    fn have_sha256sum() -> bool {
        run_scrub(
            "command -v sha256sum >/dev/null 2>&1",
            std::path::Path::new("/"),
        )
        .success()
    }

    #[test]
    fn sealed_scrub_never_removes_a_mount_point_or_anything_under_one() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        write_file(dir.path(), ".codex/AGENTS.md", "x");
        write_file(dir.path(), ".single", "x");
        write_file(dir.path(), ".local/share/atuin/key", "x");
        write_file(dir.path(), ".plain", "x");
        // A directory bind mount, a single-file mount, a mount under the atuin
        // dir, and one with a space (escaped as \040 in mountinfo).
        let body = format!(
            "36 25 0:30 / {root}/.codex rw - tmpfs tmpfs rw\n\
             37 25 0:31 / {root}/.single rw - tmpfs tmpfs rw\n\
             38 25 0:32 / {root}/.local/share/atuin rw - tmpfs tmpfs rw\n"
        );
        let (_mi, mountinfo) = mountinfo_fixture(&body);
        let script = sealed_scrub_script_with(
            root,
            &[
                entry(".codex/AGENTS.md", "x"),
                entry(".single", "x"),
                entry(".plain", "x"),
            ],
            &mountinfo,
        );
        assert!(run_scrub(&script, dir.path()).success());
        assert!(
            dir.path().join(".codex/AGENTS.md").exists(),
            "under a mount"
        );
        assert!(dir.path().join(".single").exists(), "is a mount point");
        assert!(
            dir.path().join(".local/share/atuin/key").exists(),
            "atuin credential under a mount is host data"
        );
        assert!(
            !dir.path().join(".plain").exists(),
            "unmounted file removed"
        );
    }

    #[test]
    fn sealed_scrub_handles_escaped_mount_points() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        write_file(dir.path(), ".my dir/rc", "x");
        let body = format!("36 25 0:30 / {root}/.my\\040dir rw - tmpfs tmpfs rw\n");
        let (_mi, mountinfo) = mountinfo_fixture(&body);
        let script = sealed_scrub_script_with(root, &[entry(".my dir/rc", "x")], &mountinfo);
        assert!(run_scrub(&script, dir.path()).success());
        assert!(dir.path().join(".my dir/rc").exists());
    }

    #[test]
    fn sealed_scrub_without_mount_info_scrubs_atuin_but_skips_dotfiles() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        touch_file(dir.path(), ".zshrc");
        touch_file(dir.path(), ".local/share/atuin/key");
        let missing = dir.path().join("no-such-mountinfo");
        let script = sealed_scrub_script_with(
            dir.path().to_str().unwrap(),
            &[entry(".zshrc", "x")],
            missing.to_str().unwrap(),
        );
        assert!(run_scrub(&script, dir.path()).success());
        assert!(dir.path().join(".zshrc").exists(), "dotfile skipped");
        assert!(!dir.path().join(".local/share/atuin/key").exists());
    }

    #[test]
    fn sealed_scrub_refuses_an_empty_or_unusable_home() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for home in ["", "/"] {
            let script = sealed_scrub_script(home, &[entry(".zshrc", "x")]);
            assert!(!run_scrub(&script, dir.path()).success(), "home {home:?}");
        }
        // The step built from a failed home probe fails and writes no marker.
        let mut plan = personal_plan(vec![], false);
        with_sealed_scrub(&mut plan, true, Err("container HOME probe failed: x"), &[]);
        let StepKind::Exec(script) = &plan.steps[0].kind else {
            panic!("exec step")
        };
        assert!(!run_scrub(script, dir.path()).success());
        assert!(!dir.path().join(SEALED_SCRUB_DONE).exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn sealed_scrub_never_removes_through_a_symlinked_ancestor() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        let (_mi, mountinfo) = mountinfo_fixture("");
        touch_file(sibling.path(), "git/config");
        let link = format!(
            "ln -s {} {}",
            sh_single(sibling.path().to_str().unwrap()),
            sh_single(dir.path().join(".config").to_str().unwrap())
        );
        assert!(run_scrub(&link, dir.path()).success());
        let script = sealed_scrub_script_with(
            dir.path().to_str().unwrap(),
            &[entry(".config/git/config", "x")],
            &mountinfo,
        );
        assert!(run_scrub(&script, dir.path()).success());
        assert!(sibling.path().join("git/config").exists());
    }

    #[test]
    fn sealed_scrub_fails_when_a_target_cannot_be_removed() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (_mi, mountinfo) = mountinfo_fixture("");
        touch_file(dir.path(), ".local/share/atuin/key");
        let parent = dir.path().join(".local/share/atuin");
        let chmod = |mode: &str| {
            run_scrub(
                &format!("chmod {mode} {}", sh_single(parent.to_str().unwrap())),
                dir.path(),
            )
        };
        assert!(chmod("500").success());
        let script = sealed_scrub_script_with(dir.path().to_str().unwrap(), &[], &mountinfo);
        let status = run_scrub(&script, dir.path());
        let still_there = dir.path().join(".local/share/atuin/key").exists();
        assert!(chmod("700").success());
        // Root bypasses directory permissions; only assert when removal was blocked.
        if still_there {
            assert!(!status.success());
            assert!(!dir.path().join(SEALED_SCRUB_DONE).exists());
        }
    }

    #[test]
    fn sealed_profile_refuses_credential_steps_even_if_planned() {
        let files = StepKind::Dotfiles(vec![".zshrc".into()]);
        for kind in [&StepKind::AtuinSync, &files] {
            let refusal = host_credential_step_refusal(kind, true).expect("refused");
            assert!(refusal.contains("sealed"));
            assert!(host_credential_step_refusal(kind, false).is_none());
        }
        assert!(host_credential_step_refusal(&StepKind::Exec(String::new()), true).is_none());
        assert!(host_credential_step_refusal(&StepKind::AgentConfigs(Vec::new()), true).is_none());
    }

    #[test]
    fn container_lease_is_exclusive_per_env_and_container() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_container_lease_in(dir.path(), "box", "tg-a").unwrap();
        assert!(first.is_some());
        assert!(
            acquire_container_lease_in(dir.path(), "box", "tg-a")
                .unwrap()
                .is_none()
        );
        assert!(
            acquire_container_lease_in(dir.path(), "box", "tg-b")
                .unwrap()
                .is_some()
        );
        assert!(
            acquire_container_lease_in(dir.path(), "other", "tg-a")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn container_lease_is_freed_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let lease = acquire_container_lease_in(dir.path(), "box", "tg-a").unwrap();
        assert!(lease.is_some());
        drop(lease);
        assert!(
            acquire_container_lease_in(dir.path(), "box", "tg-a")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn container_lease_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let lease = acquire_container_lease_in(dir.path(), "box", "tg-a").unwrap();
        assert!(lease.is_some());
        if let Some(mode) =
            thegn_core::fsperm::mode_bits(&container_lease_path(dir.path(), "box", "tg-a")).unwrap()
        {
            assert_eq!(
                mode & 0o077,
                0,
                "lease file permissions must exclude group/other"
            );
        }
    }

    #[test]
    fn provision_generation_changes_with_the_plan() {
        let make_plan = |name: &str, label: &str, extra: bool| EnvPlan {
            steps: {
                let mut steps = vec![envplan::ProvisionStep {
                    id: "dotfiles".into(),
                    label: label.into(),
                    kind: StepKind::Dotfiles(vec![name.into()]),
                }];
                if extra {
                    steps.push(envplan::ProvisionStep {
                        id: "setup".into(),
                        label: "Setup".into(),
                        kind: StepKind::Exec("true".into()),
                    });
                }
                steps
            },
            tier: envplan::Tier::Bare,
        };

        let first = make_plan(".gitconfig", "Dotfiles", false);
        let same = make_plan(".gitconfig", "Dotfiles", false);
        let renamed = make_plan(".zshrc", "Dotfiles", false);
        let relabeled = make_plan(".gitconfig", "Personal files", false);
        let added_step = make_plan(".gitconfig", "Dotfiles", true);
        let generation = provision_generation(&first, Some("toolchain-v1"));

        assert!(generation.starts_with("2:"), "{generation}");
        assert_eq!(
            generation,
            provision_generation(&same, Some("toolchain-v1"))
        );
        assert_eq!(
            generation,
            provision_generation(&relabeled, Some("toolchain-v1"))
        );
        assert_ne!(
            generation,
            provision_generation(&renamed, Some("toolchain-v1"))
        );
        assert_ne!(
            generation,
            provision_generation(&added_step, Some("toolchain-v1"))
        );
        assert_ne!(
            generation,
            provision_generation(&first, Some("toolchain-v2")),
            "changing declared toolchain contents invalidates the provision marker"
        );
    }

    #[test]
    fn check_delivery_accepts_owner_only_regular_files() {
        let rels = vec!["a".into(), "d/empty".into()];
        assert!(
            check_delivery(
                "1000\nregular file|600|1000\nregular empty file|600|1000\n",
                &rels
            )
            .is_ok()
        );
    }

    #[test]
    fn check_delivery_refuses_bad_shapes() {
        let rels = vec!["a".into(), "b".into()];
        let cases = [
            (
                "1000\nregular file|640|1000\nregular file|600|1000\n",
                Some("a"),
            ),
            (
                "1000\nregular file|604|1000\nregular file|600|1000\n",
                Some("a"),
            ),
            (
                "1000\nregular file|600|0\nregular file|600|1000\n",
                Some("a"),
            ),
            (
                "1000\nsymbolic link|600|1000\nregular file|600|1000\n",
                Some("a"),
            ),
            (
                "1000\ndirectory|700|1000\nregular file|600|1000\n",
                Some("a"),
            ),
            ("1000\nregular file|600|1000\n", Some("b")),
            (
                "1000\nregular file|600|1000\nregular file|600|1000\nextra\n",
                Some("b"),
            ),
            (
                "not-a-uid\nregular file|600|1000\nregular file|600|1000\n",
                Some("a"),
            ),
            (
                "1000\nregular file|not-octal|1000\nregular file|600|1000\n",
                Some("a"),
            ),
            ("", Some("a")),
        ];
        for (stdout, expected_path) in cases {
            let error = check_delivery(stdout, &rels).unwrap_err();
            if let Some(path) = expected_path {
                assert!(error.contains(path), "{error:?} did not name {path:?}");
            }
        }
    }

    #[test]
    fn staged_files_lists_regular_files_and_refuses_symlinks() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        std::fs::write(root.path().join("a"), b"a").unwrap();
        std::fs::write(root.path().join("d/b"), b"b").unwrap();
        assert_eq!(
            staged_files(root.path()).unwrap(),
            vec![String::from("a"), String::from("d/b")]
        );

        if crate::platform::test_symlink_supported() {
            let target = root.path().join("a");
            let link = root.path().join("link");
            crate::platform::test_symlink(&target, &link).unwrap();
            assert!(staged_files(root.path()).is_err());
        }
    }

    #[test]
    fn attestation_script_quotes_every_path() {
        let rels = vec!["x'y".into(), "nested/file".into()];
        let script = attestation_script("/home/user space", &rels);
        assert!(script.starts_with("id -u && stat -c '%F|%a|%u' -- "));
        assert!(script.contains(&thegn_core::util::sh_quote("/home/user space/x'y")));
        assert!(script.contains(&thegn_core::util::sh_quote("/home/user space/nested/file")));
    }

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
    fn synth_init_requires_the_ready_marker() {
        let synth = envplan::detect_from_probe("LANG_PYTHON=1\n");
        let init =
            synth_init(&synth, &Config::default(), "/wt/x").expect("synth tier gets an entry");
        assert!(init.contains(".thegn-ready"), "{init}");
        assert!(!init.contains("[ -d "), "{init}");
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
    fn agent_config_staging_is_disabled_and_allows_the_provision_marker() {
        let transfer_called = std::cell::Cell::new(false);
        let staging_result = with_private_staging(fill_agent_config_staging, true, |_| {
            transfer_called.set(true);
            Ok(())
        });

        assert!(staging_result.is_ok());
        assert!(!transfer_called.get());
        // A successful no-op step leaves the provision pipeline eligible to
        // write its marker because the marker is gated only on step failures.
        let all_succeeded = staging_result.is_ok();
        assert!(all_succeeded);
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

    #[test]
    fn container_home_accepts_only_absolute_normalized_paths() {
        for (raw, expected) in [
            ("/home/thegn", "/home/thegn"),
            ("/root", "/root"),
            ("/", "/"),
            ("/home/a b", "/home/a b"),
            ("/home/x;$(y)", "/home/x;$(y)"),
            ("/home/thegn\n", "/home/thegn"),
        ] {
            assert_eq!(
                container_home(Ok((true, raw.to_string(), String::new()))).unwrap(),
                expected
            );
        }
        for raw in [
            "",
            "relative",
            "/home//x",
            "/home/x/",
            "/home/./x",
            "/home/../etc",
            "/home/x\ty",
            "/home/x\n/y",
        ] {
            assert!(
                container_home(Ok((true, raw.to_string(), String::new()))).is_err(),
                "accepted {raw:?}"
            );
        }
    }

    #[test]
    fn container_home_probe_failure_is_an_error_not_a_fallback() {
        for probe in [
            Err("ssh down".into()),
            Ok((false, String::new(), "boom".into())),
            Ok((true, String::new(), String::new())),
        ] {
            let result = container_home(probe);
            assert!(result.is_err());
            assert_ne!(result.as_deref().unwrap_or_default(), "/home/thegn");
        }
    }
}
