//! Where the merge gate's commands run (THE-2200): `[merge_queue]
//! gate_sandbox = "sealed"` runs
//! `gate_setup_command`, `gate_command` and `regenerate_command` through the
//! repo's sealed sandbox.
//!
//! A sealed gate never falls back to the host: when the sealed spec cannot be
//! resolved (no runtime, a profile below the sealed floor, an unreadable
//! config) the gate is an infrastructure hold (`GateVerdict::Error`), never a
//! host run and never branch blame.
//!
//! What a sealed gate may touch:
//! - writable: the gate checkout (`spec.worktree`) and its `CARGO_TARGET_DIR`
//!   only;
//! - read-only: the repo's git common dir (the checkout's `.git` points into
//!   it), and every mount the profile brings (caches included);
//! - never mounted: the repo's main checkout and its `.git` carve-outs.
//!
//! Only a local bwrap sandbox with network `none` and no host daemon socket
//! qualifies: an OCI backend would exec into a container built for the main
//! checkout, a remote placement would run the gate on another host, and a
//! bound socket (the Nix daemon, a `[sandbox] mounts` socket or device, or a
//! mounted `/run`, `/var/run` or `/dev` tree) reaches a host service that
//! `--unshare-net` does not block.
//!
//! The guarded entry masks the daemon socket, state run dir, runtime dir and
//! Lead control dir; the sealed floor hides `$HOME`. The sealed command
//! inherits only an allowlisted env ([`scrub_sealed_env`]).

use std::path::{Path, PathBuf};

use thegn_core::config::{Config, GateSandbox, MergeQueueConfig, Network};
use thegn_core::sandbox::{Mount, SandboxSpec, SecretEnvFile};

/// The resolve error when the floor gate refuses without a reason of its own.
const NEEDS_SEALED: &str = "the merge gate needs a sealed sandbox for this repo";

/// Set on an unattended drain child (the land was approved as isolated): the
/// gate it runs must be sealed, whatever the reloaded config says.
pub(crate) const REQUIRE_SEALED_ENV: &str = "THEGN_GATE_REQUIRE_SEALED";

/// How a gate command is executed.
pub(crate) enum GateExec {
    /// On the host (the `off` default).
    Host,
    /// Inside this sealed sandbox.
    Sealed(Box<SandboxSpec>),
}

impl GateExec {
    /// The first line of a gate log for this mode (sealed runs only; the host
    /// gate keeps `gate::HOST_EXECUTION_LOG_HEADER`).
    pub(crate) fn sealed_log_header(spec: &SandboxSpec) -> String {
        format!("sandboxed gate (network: {})\n", spec.network.as_str())
    }
}

/// Resolve the gate execution for `repo_root` from the full config: the
/// repo's effective `[merge_queue] gate_sandbox` (a repo overlay can only
/// raise it) decides the mode; `sealed` must pass the same sealed floor an
/// unattended validation does.
pub(crate) fn resolve(full: &Config, repo_root: &Path) -> Result<GateExec, String> {
    match full.repo_merge_queue(repo_root).gate_sandbox {
        GateSandbox::Off => Err(needs_sealed()),
        GateSandbox::Sealed => sealed_spec(full, repo_root),
    }
}

fn needs_sealed() -> String {
    format!("{NEEDS_SEALED}; host execution cannot produce a trusted gate result")
}

/// The sealed spec for `repo_root`, or the refusal. Blocking (it ensures the
/// sandbox and runs a preflight): call off the event loop.
fn sealed_spec(full: &Config, repo_root: &Path) -> Result<GateExec, String> {
    use crate::agent_run::AgentDispatch;
    use crate::daemon::stage_executor::{floor_refusal, host_validation_isolated};
    let Some(root) = repo_root.to_str() else {
        return Err(format!("{NEEDS_SEALED} (repo path is not UTF-8)"));
    };
    let gate = crate::agent_run::agent_floor_gate(
        full,
        root,
        true,
        thegn_core::config::IsolationFloor::SharedKernel,
        thegn_core::config::OnFloorMiss::Fail,
    );
    match gate {
        AgentDispatch::Run(Some(spec)) if host_validation_isolated(&spec) => {
            match gate_backend_refusal(&spec) {
                None => match spec.mounts.iter().find(|m| {
                    m.host != "/dev/null"
                        && (reaches_host_services(&m.host)
                            || std::fs::metadata(&m.host)
                                .is_ok_and(|md| !md.is_dir() && !md.is_file()))
                }) {
                    Some(m) => Err(format!(
                        "the sealed merge gate refuses mount '{}': it is, or contains, a host socket or device, which reaches a host service",
                        m.dest
                    )),
                    None => Ok(GateExec::Sealed(Box::new(spec))),
                },
                Some(why) => Err(why),
            }
        }
        other => Err(floor_refusal(other).unwrap_or_else(|| NEEDS_SEALED.to_owned())),
    }
}

/// Whether a mount's host path is, lies under, or is an ancestor of a host
/// tree holding system service sockets or devices (`/run/docker.sock`,
/// `/run/podman`, the system bus, `/dev/*`). Compares the raw and the
/// canonical spelling (`/var/run` -> `/run`). `/run/current-system` (a
/// store symlink) is exempt.
fn reaches_host_services(host: &str) -> bool {
    const SERVICE_TREES: &[&str] = &["/run", "/var/run", "/dev"];
    let raw = Path::new(host);
    if raw.starts_with("/run/current-system") {
        return false;
    }
    let real = std::fs::canonicalize(raw).unwrap_or_else(|_| raw.to_path_buf());
    [raw, real.as_path()].iter().any(|p| {
        SERVICE_TREES
            .iter()
            .map(Path::new)
            .any(|tree| p.starts_with(tree) || tree.starts_with(p))
    })
}

/// Why `spec` cannot carry the sealed gate, or `None`. Only a local bwrap
/// sandbox honours the gate's re-rooted mounts and worktree (an OCI backend
/// execs into a container built for the main checkout; a remote placement runs
/// elsewhere), and the gate runs with no network. Pure.
fn gate_backend_refusal(spec: &SandboxSpec) -> Option<String> {
    const NIX_DAEMON: &str = "/nix/var/nix/daemon-socket";
    let nix = Path::new(NIX_DAEMON);
    if spec.env.iter().any(|(k, _)| k == "NIX_REMOTE")
        || spec.mounts.iter().any(|m| {
            let (h, d) = (Path::new(&m.host), Path::new(&m.dest));
            h.starts_with(nix) || d.starts_with(nix) || nix.starts_with(h) || nix.starts_with(d)
        })
    {
        return Some(
            "the sealed merge gate refuses a sandbox that reaches the host Nix daemon \
             ([sandbox] nix_daemon = true): daemon builds have network and write the host store"
                .into(),
        );
    }
    let profile = spec.backend.profile();
    if profile.family == thegn_core::sandbox::BackendFamily::Bwrap
        && spec.placement.is_local()
        && spec.network == Network::None
    {
        return None;
    }
    Some(format!(
        "the sealed merge gate needs a local bwrap sandbox with network none; \
         this repo resolves to backend '{}', network '{}'",
        profile.label,
        spec.network.as_str()
    ))
}

/// Resolve the gate execution from the repo's effective `[merge_queue]`
/// (`mq`, what the gate was handed). `off` never loads config; `sealed` loads
/// the process's effective config (the gate runs off-loop on its worker) and
/// resolves the sealed spec. Any error is an `Err`: the caller holds the gate,
/// it never runs on the host.
#[cfg(not(test))]
pub(crate) fn resolve_for_gate(
    repo_root: &Path,
    mq: &MergeQueueConfig,
) -> Result<GateExec, String> {
    resolve_mode(repo_root, mq.gate_sandbox)
}

/// Existing gate logic tests use the host executor as a test fixture. This
/// alternate entrypoint is absent from non-test builds; production callers
/// always use the fail-closed resolver above.
#[cfg(test)]
pub(crate) fn resolve_for_gate(
    repo_root: &Path,
    mq: &MergeQueueConfig,
) -> Result<GateExec, String> {
    resolve_for_gate_test_runner(repo_root, mq.gate_sandbox)
}

/// [`resolve_for_gate`] from the mode alone (the fold's `regenerate_command`
/// carries only the mode, not the whole `[merge_queue]`).
#[cfg(not(test))]
pub(crate) fn resolve_mode(repo_root: &Path, mode: GateSandbox) -> Result<GateExec, String> {
    resolve_for_gate_with(
        repo_root,
        mode,
        std::env::var_os(REQUIRE_SEALED_ENV).is_some(),
        load_process_config,
        sealed_spec,
    )
}

#[cfg(test)]
pub(crate) fn resolve_mode(repo_root: &Path, mode: GateSandbox) -> Result<GateExec, String> {
    resolve_for_gate_test_runner(repo_root, mode)
}

#[cfg(test)]
fn resolve_for_gate_test_runner(repo_root: &Path, mode: GateSandbox) -> Result<GateExec, String> {
    if std::env::var_os(REQUIRE_SEALED_ENV).is_some() {
        return resolve_for_gate_with(repo_root, mode, true, load_process_config, sealed_spec);
    }
    match mode {
        GateSandbox::Off => Ok(GateExec::Host),
        GateSandbox::Sealed => {
            resolve_for_gate_with(repo_root, mode, false, load_process_config, sealed_spec)
        }
    }
}

/// The process's current admitted configuration (startup admission, the
/// channel clamp), as `config_source::fresh` loads it. Never the tolerant
/// layered loader: the gate's sandbox is authority-bearing.
fn load_process_config() -> Result<Config, String> {
    use crate::config_startup::ReloadOutcome;
    if crate::config_startup::store().is_none() {
        return Err("the sealed merge gate has no admitted configuration".into());
    }
    let admitted = match crate::config_startup::reload() {
        ReloadOutcome::Published(admitted) => admitted,
        // Another reload published first: use whatever is current now.
        ReloadOutcome::Superseded | ReloadOutcome::Unavailable => {
            crate::config_startup::display().ok_or("configuration store is unavailable")?
        }
        ReloadOutcome::Failed(error) => return Err(error.to_string()),
        ReloadOutcome::FailedCoalesced => {
            return Err(crate::config_startup::banner().unwrap_or_else(|| {
                "configuration reload failed; the last admitted generation is display-only".into()
            }));
        }
    };
    crate::config_startup::require_launchable(&admitted)?;
    let mut cfg = admitted.config().clone();
    // best-effort: the clamped-feature report is for `main`'s startup status
    // note; the gate only needs the clamped config.
    crate::channel_state::clamp_and_record(&mut cfg, crate::channel_state::current());
    Ok(cfg)
}

/// Inherited variables a sealed gate may see (cargo/just/nix toolchains).
const SEALED_ENV_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "COLORTERM",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "SOURCE_DATE_EPOCH",
    "IN_NIX_SHELL",
    "XDG_DATA_DIRS",
    "XDG_CONFIG_DIRS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "LD_LIBRARY_PATH",
    "LIBRARY_PATH",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "LIBCLANG_PATH",
    "BINDGEN_EXTRA_CLANG_ARGS",
    "PROTOC",
    "PROTOC_INCLUDE",
    "CC",
    "CXX",
    "AR",
    "LD",
    "RANLIB",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "RUSTC",
    "RUSTDOC",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUST_BACKTRACE",
    "RUST_MIN_STACK",
    "RUST_TEST_THREADS",
    "THEGN_GATE",
    "THEGN_WORKTREE",
    "THEGN_GATE_OID",
    "THEGN_SANDBOX",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_TERMINAL_PROMPT",
];
const SEALED_ENV_PREFIXES: &[&str] = &[
    "LC_",
    "CARGO_",
    "RUSTUP_",
    "NIX_",
    "PKG_CONFIG",
    "CC_",
    "CXX_",
    "AR_",
    "CFLAGS_",
    "CXXFLAGS_",
];
/// Never inherited, even when allowlisted or kept.
const SEALED_ENV_DENY: &[&str] = &[
    "NIX_CONFIG",
    "NIX_USER_CONF_FILES",
    "NIX_REMOTE",
    "NIX_DAEMON_SOCKET_PATH",
    "NIX_SSHOPTS",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_HTTP_PROXY",
    "THEGN_CONTROL_SOCKET",
];

fn sealed_env_allowed(key: &str, keep: &[&str]) -> bool {
    (SEALED_ENV_EXACT.contains(&key)
        || SEALED_ENV_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
        || keep.contains(&key))
        && !SEALED_ENV_DENY.contains(&key)
        && !thegn_core::log_redact::is_sensitive_key(key)
}

/// A sealed gate command's env: cleared, then only allowlisted inherited
/// or explicitly set variables (`keep`: the spec's own env keys).
pub(crate) fn scrub_sealed_env(command: &mut std::process::Command, keep: &[(String, String)]) {
    let keep: Vec<&str> = keep.iter().map(|(key, _)| key.as_str()).collect();
    let explicit: Vec<(std::ffi::OsString, std::ffi::OsString)> = command
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_os_string(), value.to_os_string())))
        .collect();
    command.env_clear();
    for (key, value) in std::env::vars_os().chain(explicit) {
        if key.to_str().is_some_and(|k| sealed_env_allowed(k, &keep)) {
            command.env(&key, &value);
        }
    }
}

/// [`resolve_for_gate`] with the config loader and sealed resolver injected
/// (tests prove `off` never loads config). `mode` is authoritative: a
/// `sealed` gate stays sealed even if the reloaded config reads `off`.
/// Candidate-derived commands always require a sealed runner; unattended
/// land also requires it when `require_sealed` is set.
fn resolve_for_gate_with(
    repo_root: &Path,
    mode: GateSandbox,
    require_sealed: bool,
    load: impl FnOnce() -> Result<Config, String>,
    sealed: impl FnOnce(&Config, &Path) -> Result<GateExec, String>,
) -> Result<GateExec, String> {
    if mode == GateSandbox::Off {
        return Err(if require_sealed {
            "this unattended land requires the sealed merge gate, but \
             [merge_queue] gate_sandbox is off for this repo"
                .into()
        } else {
            needs_sealed()
        });
    }
    match mode {
        GateSandbox::Off => unreachable!("off refused above"),
        GateSandbox::Sealed => {
            let full = load()?;
            sealed(&full, repo_root)
        }
    }
}

/// Whether `path` is `root` or lies beneath it (component-wise).
fn under(path: &Path, root: &Path) -> bool {
    !root.as_os_str().is_empty() && path.starts_with(root)
}

/// Every spelling of `p` the filter compares: the raw path plus its
/// canonical form (symlinks resolved, e.g. `/home` vs `/var/home`), deduped.
/// A path that cannot be canonicalized (missing, unreadable) is raw only.
fn spellings(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf()];
    if let Ok(canonical) = std::fs::canonicalize(p)
        && canonical != p
    {
        out.push(canonical);
    }
    out
}

/// Whether any spelling in `a` is any spelling in `b` or lies beneath it.
fn under_any(a: &[PathBuf], b: &[PathBuf]) -> bool {
    a.iter().any(|a| b.iter().any(|b| under(a, b)))
}

/// The sealed spec for one gate command: `spec` re-rooted at `checkout`.
/// The repo's own mounts (its main checkout, anything above it, its `.git`
/// carve-outs) are dropped, every remaining profile mount becomes read-only,
/// and only the checkout and the target dir (unless inside the checkout) are
/// writable; the git common dir is bound read-only, last. Pure.
pub(crate) fn gate_spec(
    spec: &SandboxSpec,
    checkout: &Path,
    target: Option<&Path>,
    common_git: &Path,
    extra_env: &[(String, String)],
) -> SandboxSpec {
    // Compare raw AND canonical spellings, so a mount naming the main
    // checkout through another path (`/home` vs `/var/home`, a symlink) is
    // dropped as well.
    let original = spellings(&spec.worktree);
    let common_spellings = spellings(common_git);
    let mut spec = spec.clone();
    spec.mounts.retain(|m| {
        let (h, d) = (spellings(Path::new(&m.host)), spellings(Path::new(&m.dest)));
        !(under_any(&h, &original)
            || under_any(&d, &original)
            || under_any(&original, &h)
            || under_any(&original, &d)
            || under_any(&h, &common_spellings)
            || under_any(&d, &common_spellings))
    });
    for m in &mut spec.mounts {
        m.ro = true;
    }
    spec.worktree = checkout.to_path_buf();
    let checkout_str = checkout.to_string_lossy().into_owned();
    spec.mounts.push(Mount {
        host: checkout_str.clone(),
        dest: checkout_str,
        ro: false,
        cache: false,
    });
    if let Some(target) = target
        && !under(target, checkout)
    {
        let target = target.to_string_lossy().into_owned();
        spec.mounts.push(Mount {
            host: target.clone(),
            dest: target,
            ro: false,
            cache: false,
        });
    }
    let common = common_git.to_string_lossy().into_owned();
    spec.mounts.push(Mount {
        host: common.clone(),
        dest: common,
        ro: true,
        cache: false,
    });
    spec.env.extend(extra_env.iter().cloned());
    if spec.network == Network::None {
        spec.env
            .push(("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()));
    }
    spec
}

/// The full argv for one sealed gate command: [`gate_spec`] through the
/// guarded sandbox entry (control-dir masks, secret env-file) and the
/// `thegn.slice` cap tagged as a gate scope. The returned env-file guard must
/// stay alive until the child is reaped.
pub(crate) fn sealed_argv(
    spec: &SandboxSpec,
    checkout: &Path,
    target: Option<&Path>,
    common_git: &Path,
    command: &str,
    extra_env: &[(String, String)],
) -> Result<(Vec<String>, Option<SecretEnvFile>), String> {
    let spec = gate_spec(spec, checkout, target, common_git, extra_env);
    let (inner, env_file) = thegn_core::sandbox::enter_argv_guarded_background(&spec, command)
        .map_err(|error| format!("the sealed merge gate cannot start: {error}"))?
        .into_parts();
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv_tagged(
        inner,
        &thegn_core::sandbox_cpucap::ScopeTag {
            kind: thegn_core::sandbox_cpucap::ScopeKind::Gate,
            worktree: checkout.to_string_lossy().into_owned(),
        },
    );
    if argv.is_empty() {
        return Err("the sealed merge gate cannot start: empty sandbox entry".to_owned());
    }
    Ok((argv, env_file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    #[test]
    fn host_service_trees_are_refused_but_the_system_profile_is_not() {
        for host in [
            "/run",
            "/var/run",
            "/run/docker",
            "/run/podman",
            "/dev",
            "/var",
            "/",
        ] {
            assert!(reaches_host_services(host), "{host} refused");
        }
        for host in [
            "/run/current-system",
            "/nix/store",
            "/usr",
            "/etc/static",
            "/home/u/.cargo/registry",
        ] {
            assert!(!reaches_host_services(host), "{host} allowed");
        }
    }

    fn env(spec: &SandboxSpec, key: &str) -> Option<String> {
        spec.env
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    fn gate_env() -> Vec<(String, String)> {
        vec![
            ("CARGO_TARGET_DIR".into(), "/state/gate/target".into()),
            ("THEGN_GATE".into(), "1".into()),
            ("THEGN_WORKTREE".into(), "/state/gate/wt".into()),
            ("THEGN_GATE_OID".into(), "a".repeat(40)),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ]
    }

    fn base() -> SandboxSpec {
        SandboxSpec {
            worktree: PathBuf::from("/repo"),
            network: Network::None,
            ..SandboxSpec::default()
        }
    }

    #[test]
    fn the_gate_checkout_is_the_sandbox_worktree() {
        let spec = gate_spec(
            &base(),
            Path::new("/state/gate/wt"),
            Some(Path::new("/state/gate/target")),
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(spec.worktree, PathBuf::from("/state/gate/wt"));
    }

    #[test]
    fn the_target_is_bound_rw_and_the_common_git_ro() {
        let spec = gate_spec(
            &base(),
            Path::new("/state/gate/wt"),
            Some(Path::new("/state/gate/target")),
            Path::new("/repo/.git"),
            &gate_env(),
        );
        let target = spec
            .mounts
            .iter()
            .find(|m| m.host == "/state/gate/target")
            .expect("target bind");
        assert!(!target.ro);
        assert_eq!(target.dest, "/state/gate/target");
        let common = spec
            .mounts
            .iter()
            .find(|m| m.host == "/repo/.git")
            .expect("common git bind");
        assert!(common.ro);
        assert_eq!(common.dest, "/repo/.git");
        // Nothing else became writable: the checkout and the target only.
        assert_eq!(spec.mounts.iter().filter(|m| !m.ro).count(), 2);
        assert!(
            spec.mounts
                .iter()
                .any(|m| !m.ro && m.host == "/state/gate/wt" && m.dest == "/state/gate/wt")
        );
    }

    #[test]
    fn a_target_under_the_checkout_gets_no_second_bind() {
        let spec = gate_spec(
            &base(),
            Path::new("/state/gate/wt"),
            Some(Path::new("/state/gate/wt/target")),
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert!(!spec.mounts.iter().any(|m| m.host.ends_with("target")));
        assert_eq!(
            spec.mounts.len(),
            2,
            "the checkout (rw) and the common git (ro) only"
        );
    }

    fn mount(path: &str, ro: bool, cache: bool) -> Mount {
        Mount {
            host: path.into(),
            dest: path.into(),
            ro,
            cache,
        }
    }

    #[test]
    fn the_main_checkout_git_carveouts_and_caches_are_not_writable() {
        let mut spec = base();
        spec.mounts = vec![
            mount("/repo", false, false),
            mount("/repo/.git/refs/heads/x", false, false),
            mount("/home/u/.cargo/registry", false, true),
        ];
        let gated = gate_spec(
            &spec,
            Path::new("/state/gate/wt"),
            Some(Path::new("/state/gate/target")),
            Path::new("/repo/.git"),
            &gate_env(),
        );
        let (last, rest) = gated.mounts.split_last().unwrap();
        assert_eq!((last.host.as_str(), last.ro), ("/repo/.git", true));
        assert!(
            rest.iter()
                .all(|m| !m.host.starts_with("/repo") && !m.dest.starts_with("/repo")),
            "{rest:?}"
        );
        let cargo = gated
            .mounts
            .iter()
            .find(|m| m.host == "/home/u/.cargo/registry")
            .expect("cache mount kept");
        assert!(cargo.ro);
        let mut writable: Vec<&str> = gated
            .mounts
            .iter()
            .filter(|m| !m.ro)
            .map(|m| m.host.as_str())
            .collect();
        writable.sort_unstable();
        assert_eq!(writable, ["/state/gate/target", "/state/gate/wt"]);

        // A read-only parent of the main checkout is dropped too.
        let mut nested = SandboxSpec {
            worktree: PathBuf::from("/srv/repo"),
            ..base()
        };
        nested.mounts = vec![mount("/srv", true, false), mount("/srv/repo", false, false)];
        let gated = gate_spec(
            &nested,
            Path::new("/state/gate/wt"),
            None,
            Path::new("/srv/repo/.git"),
            &gate_env(),
        );
        assert!(
            gated
                .mounts
                .iter()
                .all(|m| m.host != "/srv" && m.host != "/srv/repo"),
            "{:?}",
            gated.mounts
        );
        assert_eq!(gated.mounts.len(), 2);
    }

    #[test]
    fn a_mount_reaching_the_checkout_through_a_symlink_is_dropped() {
        // `link` -> `real`: the main checkout is `real`, and a profile mount
        // names it (and a path under it) through the symlink spelling.
        let temp = thegn_core::testenv::symlinked_tempdir();
        if temp.path() == temp.real_path() {
            return; // no unprivileged directory symlink on this platform
        }
        let real = temp.real_path();
        let link = temp.path();
        std::fs::create_dir(real.join("sub")).unwrap();
        let link_str = link.to_string_lossy().into_owned();
        let sub_str = link.join("sub").to_string_lossy().into_owned();
        let mut spec = SandboxSpec {
            worktree: real.to_path_buf(),
            ..base()
        };
        spec.mounts = vec![
            mount(&link_str, false, false),
            mount(&sub_str, true, false),
            mount("/home/u/.cargo/registry", false, true),
        ];
        let gated = gate_spec(
            &spec,
            Path::new("/state/gate/wt"),
            None,
            &real.join(".git"),
            &gate_env(),
        );
        assert!(
            gated
                .mounts
                .iter()
                .all(|m| m.host != link_str && m.host != sub_str),
            "{:?}",
            gated.mounts
        );
        assert!(
            gated
                .mounts
                .iter()
                .any(|m| m.host == "/home/u/.cargo/registry" && m.ro)
        );

        // The reverse spelling: the spec names the checkout through the
        // symlink and the mount names the real path.
        let mut aliased = SandboxSpec {
            worktree: link.to_path_buf(),
            ..base()
        };
        let real_str = real.to_string_lossy().into_owned();
        aliased.mounts = vec![mount(&real_str, false, false)];
        let gated = gate_spec(
            &aliased,
            Path::new("/state/gate/wt"),
            None,
            &link.join(".git"),
            &gate_env(),
        );
        assert!(
            gated.mounts.iter().all(|m| m.host != real_str),
            "{:?}",
            gated.mounts
        );
    }

    #[test]
    fn spellings_are_raw_plus_canonical_deduped() {
        let missing = Path::new("/definitely/not/a/real/path/for/thegn");
        assert_eq!(spellings(missing), vec![missing.to_path_buf()]);
        let temp = thegn_core::testenv::symlinked_tempdir();
        let real = spellings(temp.real_path());
        assert_eq!(real, vec![temp.real_path().to_path_buf()]);
        if temp.path() != temp.real_path() {
            assert_eq!(
                spellings(temp.path()),
                vec![temp.path().to_path_buf(), temp.real_path().to_path_buf()]
            );
        }
        assert!(under_any(
            &spellings(temp.path()),
            &[temp.real_path().to_path_buf()]
        ));
    }

    #[test]
    fn only_a_local_bwrap_sandbox_without_network_carries_the_gate() {
        use thegn_core::placement::{Placement, SshPlacement, TransportKind};
        use thegn_core::sandbox::Backend;
        let ok = SandboxSpec {
            backend: Backend::Bwrap,
            network: Network::None,
            ..SandboxSpec::default()
        };
        assert_eq!(gate_backend_refusal(&ok), None);
        let podman = SandboxSpec {
            backend: Backend::Podman,
            network: Network::None,
            ..SandboxSpec::default()
        };
        let why = gate_backend_refusal(&podman).expect("podman refused");
        assert!(why.contains("local bwrap"), "{why}");
        assert!(why.contains("network 'none'"), "{why}");
        let remote = SandboxSpec {
            backend: Backend::Bwrap,
            network: Network::None,
            placement: Placement::Ssh(SshPlacement::plain(
                "box".into(),
                22,
                false,
                TransportKind::Ssh,
            )),
            ..SandboxSpec::default()
        };
        assert!(gate_backend_refusal(&remote).is_some());
        let nat = SandboxSpec {
            backend: Backend::Bwrap,
            network: Network::Nat,
            ..SandboxSpec::default()
        };
        let why = gate_backend_refusal(&nat).expect("nat refused");
        assert!(why.contains("network 'nat'"), "{why}");
        let daemon_mount = SandboxSpec {
            backend: Backend::Bwrap,
            network: Network::None,
            mounts: vec![Mount {
                host: "/nix/var/nix/daemon-socket".into(),
                dest: "/nix/var/nix/daemon-socket".into(),
                ro: true,
                cache: false,
            }],
            ..SandboxSpec::default()
        };
        assert!(gate_backend_refusal(&daemon_mount).is_some());
        let daemon_env = SandboxSpec {
            backend: Backend::Bwrap,
            network: Network::None,
            env: vec![("NIX_REMOTE".into(), "daemon".into())],
            ..SandboxSpec::default()
        };
        assert!(gate_backend_refusal(&daemon_env).is_some());
    }

    #[test]
    fn the_sealed_env_scrub_drops_explicit_and_listed_credentials() {
        let mut command = std::process::Command::new("true");
        command
            .env("FOO_API_KEY", "x")
            .env("AWS_ACCESS_KEY_ID", "AKIA")
            .env("GIT_SSH_COMMAND", "ssh -i k")
            .env("RUSTC_WRAPPER", "sccache")
            .env("NIX_CONFIG", "access-tokens = x")
            .env("CARGO_REGISTRY_TOKEN", "t")
            .env("THEGN_GATE", "1")
            .env("CARGO_TARGET_DIR", "/t")
            .env("PATH", "/bin");
        scrub_sealed_env(
            &mut command,
            &[("GIT_CONFIG_GLOBAL".into(), "/dev/null".into())],
        );
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let value = |key: &str| {
            envs.iter()
                .find(|(k, _)| k == key)
                .and_then(|(_, v)| v.clone())
        };
        for key in [
            "FOO_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "GIT_SSH_COMMAND",
            "RUSTC_WRAPPER",
            "NIX_CONFIG",
            "CARGO_REGISTRY_TOKEN",
            "GH_TOKEN",
            "GIT_DIR",
        ] {
            assert_eq!(value(key), None, "{key} must not reach the sealed gate");
        }
        assert_eq!(value("THEGN_GATE").as_deref(), Some("1"));
        assert_eq!(value("CARGO_TARGET_DIR").as_deref(), Some("/t"));
        assert_eq!(value("PATH").as_deref(), Some("/bin"));
    }

    #[test]
    fn sealed_env_allowed_is_an_allowlist() {
        let keep = ["GIT_CONFIG_GLOBAL", "GH_TOKEN"];
        for key in [
            "PATH",
            "LC_ALL",
            "CARGO_HOME",
            "RUSTUP_TOOLCHAIN",
            "NIX_CFLAGS_COMPILE",
            "GIT_CONFIG_GLOBAL",
        ] {
            assert!(sealed_env_allowed(key, &keep), "{key} allowed");
        }
        for key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "NIX_CONFIG",
            "RUSTC_WRAPPER",
            "CARGO_REGISTRIES_X_TOKEN",
            "SSH_AUTH_SOCK",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "THEGN_CONTROL_SOCKET",
            "DATABASE_URL",
            "GH_TOKEN",
        ] {
            assert!(!sealed_env_allowed(key, &keep), "{key} denied");
        }
        assert!(!sealed_env_allowed("GIT_CONFIG_GLOBAL", &[]));
    }

    #[test]
    fn a_read_write_mount_covering_the_common_git_still_gets_a_ro_overlay() {
        let mut spec = base();
        spec.mounts.push(Mount {
            host: "/repo".into(),
            dest: "/repo".into(),
            ro: false,
            cache: false,
        });
        let spec = gate_spec(
            &spec,
            Path::new("/state/gate/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        let last = spec.mounts.last().unwrap();
        assert_eq!(last.host, "/repo/.git");
        assert!(last.ro);
    }

    #[test]
    fn cargo_net_offline_only_without_network() {
        let none = gate_spec(
            &base(),
            Path::new("/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(env(&none, "CARGO_NET_OFFLINE").as_deref(), Some("true"));
        let nat = gate_spec(
            &SandboxSpec {
                network: Network::Nat,
                ..base()
            },
            Path::new("/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(env(&nat, "CARGO_NET_OFFLINE"), None);
    }

    #[test]
    fn the_gate_env_reaches_the_sandbox() {
        let spec = gate_spec(
            &base(),
            Path::new("/wt"),
            None,
            Path::new("/repo/.git"),
            &gate_env(),
        );
        assert_eq!(env(&spec, "THEGN_GATE_OID"), Some("a".repeat(40)));
        assert_eq!(env(&spec, "THEGN_GATE").as_deref(), Some("1"));
        assert_eq!(
            env(&spec, "CARGO_TARGET_DIR").as_deref(),
            Some("/state/gate/target")
        );
        assert_eq!(env(&spec, "GIT_NO_REPLACE_OBJECTS").as_deref(), Some("1"));
    }

    #[test]
    fn the_sealed_log_header_names_the_network() {
        assert_eq!(
            GateExec::sealed_log_header(&base()),
            "sandboxed gate (network: none)\n"
        );
        let nat = SandboxSpec {
            network: Network::Nat,
            ..base()
        };
        assert_eq!(
            GateExec::sealed_log_header(&nat),
            "sandboxed gate (network: nat)\n"
        );
    }

    #[test]
    fn off_refuses_without_loading_config() {
        let loaded = Cell::new(false);
        let resolved = resolve_for_gate_with(
            Path::new("/nonexistent"),
            MergeQueueConfig::default().gate_sandbox,
            false,
            || {
                loaded.set(true);
                Ok(Config::default())
            },
            |_, _| panic!("off never resolves a sealed spec"),
        );
        assert!(resolved.as_ref().is_err_and(|why| {
            why.contains("host execution cannot produce a trusted gate result")
        }));
        assert!(!loaded.get());
    }

    #[test]
    fn a_required_seal_refuses_off_without_loading_config() {
        let loaded = Cell::new(false);
        let resolved = resolve_for_gate_with(
            Path::new("/nonexistent"),
            GateSandbox::Off,
            true,
            || {
                loaded.set(true);
                Ok(Config::default())
            },
            |_, _| panic!("off never resolves a sealed spec"),
        );
        let why = resolved
            .err()
            .expect("off refused when sealing is required");
        assert!(why.contains("requires the sealed merge gate"), "{why}");
        assert!(!loaded.get());
    }

    #[test]
    fn sealed_loads_config_and_never_falls_back_to_the_host() {
        let sealed = GateSandbox::Sealed;
        let loaded = Cell::new(false);
        // The reloaded config reads `off`, yet the gate's own `sealed` holds.
        let resolved = resolve_for_gate_with(
            Path::new("/nonexistent"),
            sealed,
            false,
            || {
                loaded.set(true);
                Ok(Config::default())
            },
            |_, _| Err("no runtime".into()),
        );
        assert!(loaded.get());
        assert_eq!(resolved.err().as_deref(), Some("no runtime"));

        let unreadable = resolve_for_gate_with(
            Path::new("/nonexistent"),
            sealed,
            false,
            || Err("bad toml".into()),
            |_, _| panic!("no spec without a config"),
        );
        assert_eq!(unreadable.err().as_deref(), Some("bad toml"));
    }

    #[test]
    fn resolve_follows_the_full_config_mode() {
        let root = Path::new("/nonexistent");
        assert!(resolve(&Config::default(), root).is_err_and(|why| {
            why.contains("host execution cannot produce a trusted gate result")
        }));
    }

    #[test]
    fn test_only_runner_seam_preserves_gate_logic_tests() {
        assert!(
            resolve_for_gate_test_runner(Path::new("/unused"), GateSandbox::Off)
                .is_ok_and(|exec| matches!(exec, GateExec::Host))
        );
        assert!(
            resolve_for_gate_with(
                Path::new("/unused"),
                GateSandbox::Off,
                false,
                || panic!("off never loads config"),
                |_, _| panic!("off never resolves a sealed spec"),
            )
            .is_err_and(|why| why.contains("host execution cannot produce a trusted gate result"))
        );
    }
}
