//! Build- and hook-tooling caches for interactive panes.
//!
//! Two concerns, both driven by `[disk]`:
//!  * the env a pane needs for a shared `sccache` compile cache / `CARGO_TARGET_DIR`
//!    ([`build_env_vars`]), and
//!  * making the target and pre-commit hook FRAMEWORK caches
//!    (`prek`/`pre-commit`) — writable *inside* a sandbox that binds `$HOME`
//!    read-only ([`inject_cache_mounts`]). Without the latter, `git commit` hooks
//!    die with "Read-only file system" and agents fall back to `--no-verify`.
//!
//! Extracted from `agent.rs` (pinned at its god-file ratchet ceiling).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use thegn_core::config::{Config, SandboxCompilerCache};
use thegn_core::sandbox::{Backend, Mount, SandboxSpec};

const CACHE_ENDPOINT_VARS: &[&str] = &[
    "SCCACHE_SERVER_UDS",
    "SCCACHE_SERVER_PORT",
    "SCCACHE_ENDPOINT",
    "SCCACHE_REDIS",
    "SCCACHE_MEMCACHED",
];
const WRAPPER_VARS: &[&str] = &["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SandboxCacheDecision {
    pub active: bool,
    pub reason: String,
}

fn decision_path() -> std::path::PathBuf {
    thegn_core::util::thegn_dir().join("state/compiler-cache-last.json")
}

fn compiler_cache_state_dir() -> std::path::PathBuf {
    #[cfg(test)]
    {
        std::env::temp_dir().join(format!("thegn-compiler-cache-test-{}", std::process::id()))
    }
    #[cfg(not(test))]
    {
        thegn_core::util::thegn_dir().join("state/compiler-cache")
    }
}

fn fallback_log_path() -> std::path::PathBuf {
    compiler_cache_state_dir().join("fallback.log")
}

fn write_fail_soft_wrapper(sccache: &str) -> Option<String> {
    let timeout = thegn_core::util::which_path("timeout")?;
    let dir = compiler_cache_state_dir();
    std::fs::create_dir_all(&dir).ok()?;
    crate::platform::restrict_dir_owner_only_checked(&dir).ok()?;
    let path = dir.join("sccache-fail-soft");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = dir.join(format!(
        ".sccache-fail-soft.{}.{}",
        std::process::id(),
        stamp
    ));
    let log = fallback_log_path();
    let script = format!(
        "#!/bin/sh\ncompiler=$1\nshift\nunset SCCACHE_IGNORE_SERVER_IO_ERROR\n{} \"$compiler\" \"$@\"\nstatus=$?\n[ \"$status\" -eq 0 ] && exit 0\nif {} 2s {} \"$compiler\" --version >/dev/null 2>&1; then\n  exit \"$status\"\nfi\nprintf '%s\\n' \"thegn: sccache transport unavailable after exit $status; retrying compiler directly\" >&2\nprintf '%s\\n' \"sccache transport unavailable after exit $status; retried compiler directly\" >> {} 2>/dev/null || true\nexec \"$compiler\" \"$@\"\n",
        thegn_core::util::sh_quote(sccache),
        thegn_core::util::sh_quote(&timeout),
        thegn_core::util::sh_quote(sccache),
        thegn_core::util::sh_quote(&log.to_string_lossy()),
    );
    crate::platform::publish_private_executable(&temporary, &path, script.as_bytes()).ok()?;
    Some(path.to_string_lossy().into_owned())
}

pub(crate) fn record_sandbox_cache_decision(
    cfg: &Config,
    repo_root: &Path,
    backend: Backend,
    decision: &SandboxCacheDecision,
) {
    let path = decision_path();
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let value = serde_json::json!({
        "authority": "sandbox.compiler_cache",
        "mode": cfg.sandbox.compiler_cache.as_str(),
        "backend": backend.label(),
        "repo": repo_root,
        "active": decision.active,
        "reason": decision.reason,
        "recorded_at_unix": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&value) {
        let _ = std::fs::write(path, bytes); // best-effort: diagnostic state must never block a launch
    }
}

pub(crate) fn sandbox_cache_doctor_json(
    cfg: &Config,
    config_path: Option<std::path::PathBuf>,
    cli_overrides: &[String],
) -> serde_json::Value {
    let repo = std::env::current_dir().unwrap_or_default();
    let data_path = sandbox_sccache_dir(cfg, &repo);
    let host_binary = thegn_core::util::which_path("sccache");
    let host_available = host_binary.is_some();
    let devshell_wrapper = cfg
        .sandbox
        .inject_devshell
        .then(|| thegn_core::devenv::cached(&repo))
        .flatten()
        .and_then(|shell| {
            shell
                .vars
                .into_iter()
                .find(|(key, _)| key == "RUSTC_WRAPPER")
                .map(|(_, value)| value)
        });
    let data_exists = data_path.as_ref().is_some_and(|p| Path::new(p).is_dir());
    let data_read_only = data_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.permissions().readonly());
    let last_probe = std::fs::read(decision_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let last_fallback = std::fs::read_to_string(fallback_log_path())
        .ok()
        .and_then(|text| text.lines().last().map(str::to_string));
    let provenance = thegn_core::config_resolve::explain(
        &thegn_core::config::ProcessEnv,
        cli_overrides,
        config_path,
        "sandbox.compiler_cache",
    );
    serde_json::json!({
        "authority": "sandbox.compiler_cache",
        "mode": cfg.sandbox.compiler_cache.as_str(),
        "origin": provenance.origin.as_str(),
        "source_trace": provenance.trace.iter().map(|(layer, value)| serde_json::json!({
            "layer": layer.as_str(),
            "value": value,
        })).collect::<Vec<_>>(),
        "candidate_inputs": {
            "disk_sccache": cfg.disk.sccache,
            "devshell_wrapper": devshell_wrapper,
            "process_wrapper": std::env::var("RUSTC_WRAPPER")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            "note": "candidate inputs cannot authorize sandbox cache use",
        },
        "wrapper": {
            "host_available": host_available,
            "host_path": host_binary,
        },
        "data": {
            "host_path": data_path.clone(),
            "host_exists": data_exists,
            "host_read_only": data_read_only,
        },
        "transport": if cfg.sandbox.compiler_cache == SandboxCompilerCache::Auto {
            serde_json::json!({
                "kind": "private sandbox UDS",
                "endpoint": "/tmp/thegn-sccache.sock",
                "inherited_host_endpoint": false,
                "fail_soft": "app-owned wrapper retries only after a bounded compiler-version transport probe fails",
            })
        } else {
            serde_json::json!({
                "kind": "none",
                "endpoint": null,
                "inherited_host_endpoint": false,
                "fail_soft": "plain rustc",
            })
        },
        "contained": {
            "supported_backend": "bwrap",
            "grants": if cfg.sandbox.compiler_cache == SandboxCompilerCache::Auto {
                serde_json::json!([data_path, compiler_cache_state_dir()])
            } else {
                serde_json::json!([])
            },
            "last_launch_probe": last_probe,
            "note": "other backends are not proven and fall back to plain rustc",
        },
        "last_runtime_fallback": last_fallback,
        "remediation": if cfg.sandbox.compiler_cache == SandboxCompilerCache::Off {
            "set [sandbox].compiler_cache = \"auto\" to opt in; off already guarantees plain rustc"
        } else if !host_available {
            "install sccache on the host, or set [sandbox].compiler_cache = \"off\""
        } else {
            "inspect the last contained probe; unsupported or failed backends use plain rustc safely"
        },
    })
}

/// Resolve a configured build path: `~`/`~/…` expands to home; a relative path
/// resolves against the repo root (so a shared `target/` is per-repo).
pub(crate) fn resolve_build_path(raw: &str, repo_root: &Path) -> String {
    let expanded = thegn_core::util::expand_tilde(raw);
    let p = Path::new(&expanded);
    if p.is_absolute() {
        expanded
    } else {
        repo_root.join(p).to_string_lossy().into_owned()
    }
}

/// Where the shared `sccache` compile cache lives when `[disk] sccache` is on:
/// the configured `sccache_dir` (tilde/relative-resolved), or sccache's own
/// default `~/.cache/sccache`. `None` when sccache is disabled. Config-gated only
/// (the PATH check for the actual `RUSTC_WRAPPER` lives in [`build_env_vars`]), so
/// it's a pure function of config + `$HOME` — the single source of truth shared by
/// the pane env and the sandbox cache mount so they can never disagree.
pub(crate) fn resolved_sccache_dir(cfg: &Config, repo_root: &Path) -> Option<String> {
    if !cfg.disk.sccache {
        return None;
    }
    if cfg.disk.sccache_dir.is_empty() {
        let home = std::env::var("HOME").ok()?;
        Some(format!("{home}/.cache/sccache"))
    } else {
        Some(resolve_build_path(&cfg.disk.sccache_dir, repo_root))
    }
}

fn sandbox_sccache_dir(cfg: &Config, repo_root: &Path) -> Option<String> {
    if cfg.disk.sccache_dir.is_empty() {
        std::env::var("HOME")
            .ok()
            .map(|home| format!("{home}/.cache/sccache"))
    } else {
        Some(resolve_build_path(&cfg.disk.sccache_dir, repo_root))
    }
}

fn is_sccache_wrapper(value: &str) -> bool {
    Path::new(value.trim())
        .file_name()
        .is_some_and(|name| name == "sccache" || name == "sccache-fail-soft")
}

fn block(spec: &mut SandboxSpec, key: &str) {
    spec.env_overrides.remove(key);
    if !spec.env_block.iter().any(|present| present == key) {
        spec.env_block.push(key.to_string());
    }
}

fn effective_var<'a>(
    spec: &'a SandboxSpec,
    agent_env: &'a [(String, String)],
    key: &str,
) -> Option<&'a str> {
    agent_env
        .iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .or_else(|| spec.env_overrides.get(key).map(String::as_str))
        .or_else(|| {
            spec.env
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        })
}

fn strip_sccache(spec: &mut SandboxSpec, agent_env: &[(String, String)]) {
    for key in WRAPPER_VARS {
        if effective_var(spec, agent_env, key).is_some_and(is_sccache_wrapper) {
            block(spec, key);
        }
    }
    for key in CACHE_ENDPOINT_VARS {
        block(spec, key);
    }
    spec.env_overrides.remove("SCCACHE_DIR");
    block(spec, "SCCACHE_IGNORE_SERVER_IO_ERROR");
}

fn status_with_timeout(argv: &[String], timeout: Duration) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                std::thread::spawn(move || {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "detached reap runs off the compositor/event loop"
                    )]
                    let _ = child.wait();
                });
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
}

/// Apply the sole sandbox compiler-cache authority after all build, dev-shell,
/// bundle, and agent env contributors have been composed. A custom non-sccache
/// wrapper is deliberately left alone. `auto` is currently proven for local
/// bwrap only; every other backend degrades to plain rustc instead of guessing.
pub(crate) fn apply_sandbox_compiler_cache(
    spec: &mut SandboxSpec,
    cfg: &Config,
    repo_root: &Path,
    agent_env: &[(String, String)],
) -> SandboxCacheDecision {
    apply_sandbox_compiler_cache_with(spec, cfg, repo_root, agent_env, |probe| {
        status_with_timeout(probe, Duration::from_secs(4))
    })
}

fn apply_sandbox_compiler_cache_with<F>(
    spec: &mut SandboxSpec,
    cfg: &Config,
    repo_root: &Path,
    agent_env: &[(String, String)],
    probe: F,
) -> SandboxCacheDecision
where
    F: FnOnce(&[String]) -> bool,
{
    if cfg.sandbox.compiler_cache == SandboxCompilerCache::Off {
        strip_sccache(spec, agent_env);
        return SandboxCacheDecision {
            active: false,
            reason: "disabled by [sandbox].compiler_cache=off".to_string(),
        };
    }

    let wrapper = WRAPPER_VARS
        .iter()
        .find_map(|key| effective_var(spec, agent_env, key));
    if wrapper.is_some_and(|value| !is_sccache_wrapper(value)) {
        return SandboxCacheDecision {
            active: false,
            reason: "custom compiler wrapper preserved; sccache policy not applied".to_string(),
        };
    }
    let Some(binary) = thegn_core::util::which_path("sccache") else {
        strip_sccache(spec, agent_env);
        return SandboxCacheDecision {
            active: false,
            reason: "sccache is not available on the host; using rustc directly".to_string(),
        };
    };
    let Some(cache_dir) = sandbox_sccache_dir(cfg, repo_root) else {
        strip_sccache(spec, agent_env);
        return SandboxCacheDecision {
            active: false,
            reason: "sccache data directory could not be resolved; using rustc directly"
                .to_string(),
        };
    };
    if spec.backend != Backend::Bwrap || !spec.placement.is_local() {
        strip_sccache(spec, agent_env);
        return SandboxCacheDecision {
            active: false,
            reason: format!(
                "compiler-cache containment is not proven for {}; using rustc directly",
                spec.backend.label()
            ),
        };
    }
    let Some(fail_soft_wrapper) = write_fail_soft_wrapper(&binary) else {
        strip_sccache(spec, agent_env);
        return SandboxCacheDecision {
            active: false,
            reason: "fail-soft compiler wrapper could not be materialized; using rustc directly"
                .to_string(),
        };
    };

    let _ = std::fs::create_dir_all(&cache_dir); // best-effort: the contained write probe decides readiness
    let mount_count_before_cache_policy = spec.mounts.len();
    let mount = Mount {
        host: cache_dir.clone(),
        dest: cache_dir.clone(),
        ro: false,
        cache: true,
    };
    if thegn_core::sandbox_mounts::keep_cfg_mount(&spec.mounts, &mount) {
        spec.mounts.push(mount);
    }
    let wrapper_dir = compiler_cache_state_dir();
    let wrapper_dir = wrapper_dir.to_string_lossy().into_owned();
    let wrapper_mount = Mount {
        host: wrapper_dir.clone(),
        dest: wrapper_dir,
        ro: false,
        cache: false,
    };
    if thegn_core::sandbox_mounts::keep_cfg_mount(&spec.mounts, &wrapper_mount) {
        spec.mounts.push(wrapper_mount);
    }
    for key in CACHE_ENDPOINT_VARS {
        block(spec, key);
    }
    for key in WRAPPER_VARS {
        block(spec, key);
    }
    block(spec, "SCCACHE_IGNORE_SERVER_IO_ERROR");
    spec.env_overrides
        .insert("RUSTC_WRAPPER".to_string(), fail_soft_wrapper.clone());
    spec.env_overrides
        .insert("SCCACHE_DIR".to_string(), cache_dir.clone());
    // The bwrap /tmp is private. Never inherit a host UDS path into it: start a
    // sandbox-local server instead, backed by the narrowly mounted data dir.
    spec.env_overrides.insert(
        "SCCACHE_SERVER_UDS".to_string(),
        "/tmp/thegn-sccache.sock".to_string(),
    );
    let mut probe_spec = spec.clone();
    probe_spec.init_script = None;
    probe_spec.devenv = false;
    // The app-owned wrapper and readiness probe both block sccache's native
    // ignore-I/O switch. Otherwise transport failure can look successful before
    // the wrapper has a chance to record a durable degradation decision.
    let rustc = thegn_core::util::which_path("rustc").unwrap_or_else(|| "rustc".to_string());
    let check = format!(
        "test -x {} && test -x {} && test -w {} && probe_file={}/.thegn-write-probe-$$ && : >\"$probe_file\" && rm -f \"$probe_file\" && {} {} --version >/dev/null 2>&1",
        thegn_core::util::sh_quote(&fail_soft_wrapper),
        thegn_core::util::sh_quote(&binary),
        thegn_core::util::sh_quote(&cache_dir),
        thegn_core::util::sh_quote(&cache_dir),
        thegn_core::util::sh_quote(&binary),
        thegn_core::util::sh_quote(&rustc),
    );
    let argv = thegn_core::sandbox::enter_argv(&probe_spec, &check);
    if !probe(&argv) {
        strip_sccache(spec, agent_env);
        spec.mounts.truncate(mount_count_before_cache_policy);
        return SandboxCacheDecision {
            active: false,
            reason:
                "sccache executable/data/transport probe failed inside bwrap; using rustc directly"
                    .to_string(),
        };
    }
    SandboxCacheDecision {
        active: true,
        reason: "sccache reachable in bwrap with a writable cache and fail-soft transport"
            .to_string(),
    }
}

/// Build-tooling env injected into interactive panes from `[disk]`: a shared
/// `sccache` compile cache and/or a shared `CARGO_TARGET_DIR`. Empty when both
/// are off (the common case), so panes are untouched unless opted in.
pub(crate) fn build_env_vars(cfg: &Config, repo_root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if cfg.disk.sccache
        && thegn_core::util::have("sccache")
        && let Some(dir) = resolved_sccache_dir(cfg, repo_root)
    {
        out.push(("RUSTC_WRAPPER".to_string(), "sccache".to_string()));
        // Pin SCCACHE_DIR — even the default ~/.cache/sccache — so the pane env
        // and the sandbox's read-write cache mount can't disagree via an
        // in-container XDG_CACHE_HOME, which would put sccache back under the
        // read-only $HOME ("Read-only file system").
        out.push(("SCCACHE_DIR".to_string(), dir));
    }
    if !cfg.disk.shared_target_dir.is_empty() {
        out.push((
            "CARGO_TARGET_DIR".to_string(),
            resolve_build_path(&cfg.disk.shared_target_dir, repo_root),
        ));
    }
    out
}

// Convenience for existing path-planning tests; shipping overmount code
// supplies its already-resolved inherited home directly.
#[cfg(test)]
fn sandbox_cache_mounts(cfg: &Config, repo_root: &Path) -> Vec<Mount> {
    let inherited_home = std::env::var("HOME").ok();
    sandbox_cache_mounts_at_home(cfg, repo_root, inherited_home.as_deref())
}

/// Read-write cache directories the in-sandbox pre-commit toolchain needs under
/// a read-only `$HOME`: the hook FRAMEWORK caches (`prek`, and legacy
/// `pre-commit`) — without which `git commit` hooks can't write their hook
/// environments — plus any out-of-worktree
/// `CARGO_TARGET_DIR` that clippy/tests write. Each is a path-preserving,
/// read-write [`Mount`]; the caller creates the source dir and filters with
/// `keep_cfg_mount`. An in-tree `CARGO_TARGET_DIR` is already writable (it lives
/// under the read-write worktree bind), so it's skipped.
fn sandbox_cache_mounts_at_home(
    cfg: &Config,
    repo_root: &Path,
    inherited_home: Option<&str>,
) -> Vec<Mount> {
    let mut dirs: Vec<String> = Vec::new();
    if let Some(home) = inherited_home {
        dirs.push(format!("{home}/.cache/prek"));
        dirs.push(format!("{home}/.cache/pre-commit"));
    }
    if !cfg.disk.shared_target_dir.is_empty() {
        let t = resolve_build_path(&cfg.disk.shared_target_dir, repo_root);
        if !Path::new(&t).starts_with(repo_root) {
            dirs.push(t);
        }
    }
    dirs.into_iter()
        .map(|host| Mount {
            dest: host.clone(),
            host,
            ro: false,
            cache: true,
        })
        .collect()
}

/// Overmount the pre-commit toolchain's caches read-write when a sandbox binds
/// `$HOME` read-only (the hardened/sealed default, or any OCI backend). No-op
/// under an `open`/`all` profile — detected by the presence of a read-only
/// `$HOME` bind in the already-resolved spec, so we never add a redundant mount
/// where `$HOME` is already writable.
pub(crate) fn inject_cache_mounts(spec: &mut SandboxSpec, cfg: &Config, repo_root: &Path) {
    overmount_caches(&mut spec.mounts, cfg, repo_root);
}

/// Core of [`inject_cache_mounts`], on the raw mount list so it's testable
/// without a full `SandboxSpec`. No-op unless the list already binds `$HOME`
/// read-only.
fn overmount_caches(mounts: &mut Vec<Mount>, cfg: &Config, repo_root: &Path) {
    let inherited_home = std::env::var("HOME").unwrap_or_default();
    overmount_caches_at_home(mounts, cfg, repo_root, &inherited_home);
}

// The live wrapper supplies its unchanged inherited home. Keeping this owned
// path explicit lets the cold-cache regression use a private temporary tree.
fn overmount_caches_at_home(
    mounts: &mut Vec<Mount>,
    cfg: &Config,
    repo_root: &Path,
    inherited_home: &str,
) {
    let home_ro =
        !inherited_home.is_empty() && mounts.iter().any(|m| m.host == inherited_home && m.ro);
    if !home_ro {
        return;
    }
    for m in sandbox_cache_mounts_at_home(cfg, repo_root, Some(inherited_home)) {
        // best-effort: bwrap needs the bind source to exist; create a cold cache
        // dir before overmounting it (keep_cfg_mount also requires it to be a
        // real directory to overmount the read-only parent).
        let _ = std::fs::create_dir_all(&m.host); // best-effort: dir prep: a later write reports the real failure
        if thegn_core::sandbox_mounts::keep_cfg_mount(mounts, &m) {
            mounts.push(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved_bwrap_spec(cfg: &Config) -> Option<SandboxSpec> {
        let mut sandbox = cfg.sandbox.clone();
        sandbox.backend = thegn_core::config::SandboxBackend::Bwrap;
        sandbox.on_missing = thegn_core::config::OnMissing::Fail;
        let root =
            std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).ok()?;
        let loc = thegn_core::remote::GitLoc::for_worktree(&root);
        thegn_core::sandbox::resolve(&sandbox, &loc, "cache-policy-test")
    }

    #[test]
    fn build_env_vars_off_by_default() {
        let cfg = Config::default();
        assert!(
            build_env_vars(&cfg, Path::new("/repo")).is_empty(),
            "no build env injected unless opted in"
        );
    }

    #[test]
    fn build_env_vars_injects_shared_target() {
        let mut cfg = Config::default();
        cfg.disk.shared_target_dir = "shared-target".into();
        let env = build_env_vars(&cfg, Path::new("/repo"));
        // shared_target_dir present → CARGO_TARGET_DIR resolved against repo root.
        assert!(env.contains(&(
            "CARGO_TARGET_DIR".to_string(),
            "/repo/shared-target".to_string()
        )));
        // sccache off → no RUSTC_WRAPPER regardless of PATH.
        assert!(!env.iter().any(|(k, _)| k == "RUSTC_WRAPPER"));

        // An absolute shared dir is used verbatim.
        cfg.disk.shared_target_dir = "/abs/target".into();
        let env = build_env_vars(&cfg, Path::new("/repo"));
        assert!(env.contains(&("CARGO_TARGET_DIR".to_string(), "/abs/target".to_string())));
    }

    #[test]
    fn resolved_sccache_dir_defaults_to_home_cache_and_honors_custom() {
        let mut cfg = Config::default();
        assert_eq!(resolved_sccache_dir(&cfg, Path::new("/repo")), None);
        cfg.disk.sccache = true;
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                resolved_sccache_dir(&cfg, Path::new("/repo")),
                Some(format!("{home}/.cache/sccache"))
            );
        }
        cfg.disk.sccache_dir = "/custom/sccache".into();
        assert_eq!(
            resolved_sccache_dir(&cfg, Path::new("/repo")),
            Some("/custom/sccache".to_string())
        );
    }

    #[test]
    fn sandbox_cache_mounts_always_covers_the_hook_frameworks() {
        let cfg = Config::default(); // sccache off, no shared target
        let mounts = sandbox_cache_mounts(&cfg, Path::new("/repo"));
        if let Ok(home) = std::env::var("HOME") {
            // The prek / pre-commit hook-framework caches are mounted regardless
            // of sccache, and always read-write & path-preserving.
            for name in ["prek", "pre-commit"] {
                let want = format!("{home}/.cache/{name}");
                let m = mounts.iter().find(|m| m.host == want);
                let m = m.unwrap_or_else(|| panic!("{name} cache mount missing"));
                assert!(!m.ro && m.dest == want);
            }
        }
        // sccache off → no sccache mount.
        assert!(!mounts.iter().any(|m| m.host.ends_with("/sccache")));
    }

    #[test]
    fn sandbox_cache_mounts_leave_sccache_to_sandbox_authority() {
        let mut cfg = Config::default();
        cfg.disk.sccache = true;
        cfg.disk.sccache_dir = "/cache/sccache".into();
        cfg.disk.shared_target_dir = "/cache/target".into();
        let mounts = sandbox_cache_mounts(&cfg, Path::new("/repo"));
        assert!(!mounts.iter().any(|m| m.host == "/cache/sccache"));
        assert!(mounts.iter().any(|m| m.host == "/cache/target" && !m.ro));

        // An IN-tree target dir is already writable via the worktree bind → skip.
        cfg.disk.shared_target_dir = "target".into();
        let mounts = sandbox_cache_mounts(&cfg, Path::new("/repo"));
        assert!(!mounts.iter().any(|m| m.host == "/repo/target"));
    }

    #[test]
    fn overmount_caches_noop_without_a_readonly_home() {
        // No read-only $HOME bind (open/all profile) → nothing overmounted.
        let mut mounts: Vec<Mount> = Vec::new();
        overmount_caches(&mut mounts, &Config::default(), Path::new("/repo"));
        assert!(mounts.is_empty());
    }

    #[test]
    fn overmount_caches_overmounts_under_readonly_home() {
        let fixture = tempfile::tempdir().unwrap();
        let private_home = fixture.path().join("home");
        std::fs::create_dir(&private_home).unwrap();
        let home = private_home.to_str().unwrap();
        let config = Config::default();
        let cache_paths = ["prek", "pre-commit"].map(|name| private_home.join(".cache").join(name));
        let home_mount = |ro| Mount {
            host: home.into(),
            dest: home.into(),
            ro,
            cache: false,
        };

        // No read-only parent means no directory creation or extra mounts,
        // including an explicitly writable home bind.
        for mut mounts in [Vec::new(), vec![home_mount(false)]] {
            let count = mounts.len();
            overmount_caches_at_home(&mut mounts, &config, fixture.path(), home);
            assert_eq!(mounts.len(), count);
            assert!(cache_paths.iter().all(|path| !path.exists()));
        }

        let mut mounts = vec![home_mount(true)];
        overmount_caches_at_home(&mut mounts, &config, fixture.path(), home);
        assert_eq!(mounts.len(), 3);
        assert!(mounts[0].ro, "the parent remains read-only");
        for path in cache_paths {
            assert!(
                path.is_dir(),
                "cold cache must be created inside the fixture"
            );
            assert!(
                mounts.iter().any(|mount| {
                    Path::new(&mount.host) == path.as_path()
                        && mount.dest == mount.host
                        && !mount.ro
                        && mount.cache
                }),
                "private cache needs a path-preserving writable overmount"
            );
        }
    }

    #[test]
    fn sandbox_cache_off_strips_sccache_but_preserves_custom_wrappers() {
        let cfg = Config::default();
        let Some(mut spec) = resolved_bwrap_spec(&cfg) else {
            return;
        };
        spec.env_overrides
            .insert("RUSTC_WRAPPER".into(), "sccache".into());
        spec.env_overrides
            .insert("SCCACHE_SERVER_UDS".into(), "/tmp/host.sock".into());
        let decision =
            apply_sandbox_compiler_cache_with(&mut spec, &cfg, Path::new("/repo"), &[], |_| {
                panic!("off must not probe")
            });
        assert!(!decision.active);
        assert!(!spec.env_overrides.contains_key("RUSTC_WRAPPER"));
        assert!(spec.env_block.iter().any(|k| k == "RUSTC_WRAPPER"));
        assert!(spec.env_block.iter().any(|k| k == "SCCACHE_SERVER_UDS"));

        spec.env_block.retain(|k| k != "RUSTC_WRAPPER");
        spec.env_overrides
            .insert("RUSTC_WRAPPER".into(), "/opt/custom/wrapper".into());
        let _ = apply_sandbox_compiler_cache_with(&mut spec, &cfg, Path::new("/repo"), &[], |_| {
            panic!("off must not probe")
        });
        assert_eq!(
            spec.env_overrides.get("RUSTC_WRAPPER").map(String::as_str),
            Some("/opt/custom/wrapper")
        );
        assert!(!spec.env_block.iter().any(|k| k == "RUSTC_WRAPPER"));
    }

    #[test]
    fn sandbox_cache_auto_installs_private_fail_soft_transport_after_probe() {
        if thegn_core::util::which_path("sccache").is_none() {
            return;
        }
        let mut cfg = Config::default();
        cfg.sandbox.compiler_cache = SandboxCompilerCache::Auto;
        cfg.disk.sccache_dir = "/tmp/thegn-sccache-policy-test".into();
        let Some(mut spec) = resolved_bwrap_spec(&cfg) else {
            return;
        };
        spec.env_overrides
            .insert("RUSTC_WRAPPER".into(), "sccache".into());
        spec.env_overrides
            .insert("SCCACHE_SERVER_UDS".into(), "/tmp/inherited.sock".into());
        let decision =
            apply_sandbox_compiler_cache_with(&mut spec, &cfg, Path::new("/repo"), &[], |argv| {
                let rendered = argv.join(" ");
                assert!(
                    argv.iter().any(|v| v.ends_with("bwrap")),
                    "probe must enter bwrap: {argv:?}"
                );
                assert!(
                    rendered.contains("unset SCCACHE_IGNORE_SERVER_IO_ERROR"),
                    "probe must disable fail-soft mode: {argv:?}"
                );
                assert!(
                    rendered.contains("rustc"),
                    "probe must invoke rustc: {argv:?}"
                );
                assert!(
                    rendered.contains("--version"),
                    "probe must compile through sccache: {argv:?}"
                );
                true
            });
        assert!(decision.active);
        assert_eq!(
            spec.env_overrides
                .get("SCCACHE_SERVER_UDS")
                .map(String::as_str),
            Some("/tmp/thegn-sccache.sock")
        );
        assert!(
            !spec
                .env_overrides
                .contains_key("SCCACHE_IGNORE_SERVER_IO_ERROR")
        );
        assert!(
            spec.env_block
                .iter()
                .any(|key| key == "SCCACHE_IGNORE_SERVER_IO_ERROR")
        );
        assert!(
            spec.mounts
                .iter()
                .any(|m| { m.host == "/tmp/thegn-sccache-policy-test" && !m.ro && m.cache })
        );
    }

    #[test]
    fn sandbox_cache_auto_probe_failure_is_plain_rustc() {
        if thegn_core::util::which_path("sccache").is_none() {
            return;
        }
        let mut cfg = Config::default();
        cfg.sandbox.compiler_cache = SandboxCompilerCache::Auto;
        cfg.disk.sccache_dir = "/tmp/thegn-sccache-policy-test-fail".into();
        let Some(mut spec) = resolved_bwrap_spec(&cfg) else {
            return;
        };
        spec.env_overrides
            .insert("RUSTC_WRAPPER".into(), "sccache".into());
        let decision =
            apply_sandbox_compiler_cache_with(&mut spec, &cfg, Path::new("/repo"), &[], |_| false);
        assert!(!decision.active);
        assert!(!spec.env_overrides.contains_key("RUSTC_WRAPPER"));
        assert!(
            !spec
                .env_overrides
                .contains_key("SCCACHE_IGNORE_SERVER_IO_ERROR")
        );
        assert!(spec.env_block.iter().any(|k| k == "RUSTC_WRAPPER"));
    }

    #[test]
    fn doctor_separates_authority_inputs_and_contained_state() {
        let mut cfg = Config::default();
        cfg.sandbox.compiler_cache = SandboxCompilerCache::Auto;
        cfg.disk.sccache = true;
        let report =
            sandbox_cache_doctor_json(&cfg, None, &["sandbox.compiler_cache=auto".to_string()]);
        assert_eq!(report["authority"], "sandbox.compiler_cache");
        assert_eq!(report["mode"], "auto");
        assert_eq!(report["origin"], "runtime");
        assert_eq!(report["candidate_inputs"]["disk_sccache"], true);
        assert_eq!(report["contained"]["supported_backend"], "bwrap");
        assert_eq!(report["transport"]["endpoint"], "/tmp/thegn-sccache.sock");
        assert!(
            report["transport"]["fail_soft"]
                .as_str()
                .is_some_and(|text| text.contains("compiler-version transport probe"))
        );
        assert!(report["contained"]["grants"].as_array().unwrap().len() >= 2);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn fail_soft_wrapper_retries_transport_failure_but_not_compiler_failure() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let fake_sccache = temp.path().join("sccache");
        let fake_compiler = temp.path().join("rustc");
        let compiler_log = temp.path().join("compiler.log");
        std::fs::write(
            &fake_sccache,
            "#!/bin/sh\n[ -z \"${SCCACHE_IGNORE_SERVER_IO_ERROR:-}\" ] || exit 80\nif [ \"${2:-}\" = --version ]; then\n  [ \"${THEGN_FAKE_SCCACHE_ENDPOINT:-}\" = reachable ] && exit 0\n  exit 70\nfi\nexit 42\n",
        )
        .unwrap();
        std::fs::write(
            &fake_compiler,
            format!(
                "#!/bin/sh\nprintf invoked >> {}\nexit 0\n",
                thegn_core::util::sh_quote(&compiler_log.to_string_lossy())
            ),
        )
        .unwrap();
        for path in [&fake_sccache, &fake_compiler] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let fallback_log = fallback_log_path();
        let _ = std::fs::remove_file(&fallback_log);
        let wrapper = write_fail_soft_wrapper(&fake_sccache.to_string_lossy()).unwrap();

        #[expect(
            clippy::disallowed_methods,
            reason = "test-only wrapper smoke waits for a local fake compiler, never on the event loop"
        )]
        let compiler_failure = Command::new(&wrapper)
            .arg(&fake_compiler)
            .env("SCCACHE_IGNORE_SERVER_IO_ERROR", "1")
            .env("THEGN_FAKE_SCCACHE_ENDPOINT", "reachable")
            .status()
            .unwrap();
        assert_eq!(compiler_failure.code(), Some(42));
        assert!(!compiler_log.exists());
        assert!(!fallback_log.exists());

        #[expect(
            clippy::disallowed_methods,
            reason = "test-only wrapper smoke waits for a local fake compiler, never on the event loop"
        )]
        let transport_failure = Command::new(&wrapper)
            .arg(&fake_compiler)
            .env("SCCACHE_IGNORE_SERVER_IO_ERROR", "1")
            .env("THEGN_FAKE_SCCACHE_ENDPOINT", "unreachable")
            .status()
            .unwrap();
        assert!(transport_failure.success());
        assert!(compiler_log.exists());
        assert!(
            std::fs::read_to_string(&fallback_log)
                .is_ok_and(|log| log.contains("retried compiler directly"))
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "real bwrap+sccache smoke; run explicitly on a host with user namespaces"]
    fn real_bwrap_cache_hit_and_unreachable_transport_compile_fallback() {
        let sccache = thegn_core::util::which_path("sccache")
            .expect("real cache smoke requires sccache on PATH");
        let rustc =
            thegn_core::util::which_path("rustc").expect("real cache smoke requires rustc on PATH");
        assert!(
            status_with_timeout(
                &[
                    "bwrap".into(),
                    "--ro-bind".into(),
                    "/".into(),
                    "/".into(),
                    "--unshare-pid".into(),
                    "--die-with-parent".into(),
                    "--".into(),
                    "true".into(),
                ],
                Duration::from_secs(2),
            ),
            "real cache smoke requires a working bwrap user namespace"
        );
        let mut cfg = Config::default();
        cfg.sandbox.compiler_cache = SandboxCompilerCache::Auto;
        cfg.disk.sccache_dir = format!("/tmp/thegn-sccache-real-bwrap-{}", std::process::id());
        let mut spec = resolved_bwrap_spec(&cfg)
            .expect("real cache smoke requires a resolvable bwrap sandbox");
        assert!(
            status_with_timeout(
                &thegn_core::sandbox::enter_argv(&spec, "rustc --version >/dev/null"),
                Duration::from_secs(10),
            ),
            "real cache smoke requires the fully resolved repository sandbox to run"
        );
        spec.env_overrides
            .insert("RUSTC_WRAPPER".into(), "sccache".into());
        let decision = apply_sandbox_compiler_cache(
            &mut spec,
            &cfg,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            &[],
        );
        assert!(
            decision.active,
            "real cache smoke requires the contained readiness probe to pass: {}",
            decision.reason
        );
        let wrapper = spec.env_overrides["RUSTC_WRAPPER"].clone();
        let compile_twice = format!(
            "mkdir -p /tmp/thegn-cache-smoke-out && \
             printf 'fn main() {{}}\\n' >/tmp/thegn-cache-smoke.rs && \
             {wrapper} {rustc} --crate-name thegn_cache_smoke --crate-type rlib --emit=link --out-dir /tmp/thegn-cache-smoke-out /tmp/thegn-cache-smoke.rs && \
             {wrapper} {rustc} --crate-name thegn_cache_smoke --crate-type rlib --emit=link --out-dir /tmp/thegn-cache-smoke-out /tmp/thegn-cache-smoke.rs && \
             test -f /tmp/thegn-cache-smoke-out/libthegn_cache_smoke.rlib && \
             {sccache} --show-stats | grep -Eq 'Cache hits[[:space:]]+[1-9]'",
            wrapper = thegn_core::util::sh_quote(&wrapper),
            rustc = thegn_core::util::sh_quote(&rustc),
            sccache = thegn_core::util::sh_quote(&sccache),
        );
        assert!(
            status_with_timeout(
                &thegn_core::sandbox::enter_argv(&spec, &compile_twice),
                Duration::from_secs(20),
            ),
            "contained repeated compile did not produce a real sccache hit"
        );

        let fallback_log = fallback_log_path();
        let _ = std::fs::remove_file(&fallback_log);
        let compiler_error = format!(
            "mkdir -p /tmp/thegn-cache-error-out; \
             printf 'fn main() {{ let _ = ; }}\\n' >/tmp/thegn-cache-error.rs; \
             ! {} {} --crate-name thegn_cache_error --crate-type rlib --emit=link --out-dir /tmp/thegn-cache-error-out /tmp/thegn-cache-error.rs",
            thegn_core::util::sh_quote(&wrapper),
            thegn_core::util::sh_quote(&rustc),
        );
        assert!(
            status_with_timeout(
                &thegn_core::sandbox::enter_argv(&spec, &compiler_error),
                Duration::from_secs(10),
            ),
            "a genuine compiler error must remain a failure"
        );
        assert!(
            !fallback_log.exists(),
            "a reachable sccache compiler error must not be retried or logged as transport fallback"
        );

        let unreachable = format!(
            "export SCCACHE_SERVER_UDS=/definitely-missing/thegn.sock; \
             mkdir -p /tmp/thegn-cache-fallback-out; \
             printf 'fn main() {{}}\\n' >/tmp/thegn-cache-fallback.rs; \
             {} {} --crate-name thegn_cache_fallback --crate-type rlib --emit=link --out-dir /tmp/thegn-cache-fallback-out /tmp/thegn-cache-fallback.rs && \
             test -f /tmp/thegn-cache-fallback-out/libthegn_cache_fallback.rlib",
            thegn_core::util::sh_quote(&wrapper),
            thegn_core::util::sh_quote(&rustc),
        );
        assert!(
            status_with_timeout(
                &thegn_core::sandbox::enter_argv(&spec, &unreachable),
                Duration::from_secs(10),
            ),
            "the fail-soft wrapper must run rustc when sccache transport fails"
        );
        assert!(
            std::fs::read_to_string(&fallback_log)
                .is_ok_and(|log| log.contains("retried compiler directly")),
            "runtime fallback must leave a durable diagnostic"
        );

        cfg.sandbox.compiler_cache = SandboxCompilerCache::Off;
        spec.env_overrides
            .insert("RUSTC_WRAPPER".into(), "sccache".into());
        spec.env_overrides.insert(
            "SCCACHE_SERVER_UDS".into(),
            "/tmp/inherited-host.sock".into(),
        );
        let off = apply_sandbox_compiler_cache_with(
            &mut spec,
            &cfg,
            Path::new(env!("CARGO_MANIFEST_DIR")),
            &[],
            |_| panic!("off must not probe"),
        );
        assert!(!off.active);
        assert!(status_with_timeout(
            &thegn_core::sandbox::enter_argv(
                &spec,
                "test -z \"${RUSTC_WRAPPER:-}\" && \
                 test -z \"${SCCACHE_SERVER_UDS:-}\" && \
                 test -z \"${SCCACHE_IGNORE_SERVER_IO_ERROR:-}\" && \
                 mkdir -p /tmp/thegn-cache-off-out && \
                 printf 'fn main() {}\\n' >/tmp/thegn-cache-off.rs && \
                 rustc --crate-name thegn_cache_off --crate-type rlib --emit=link \
                   --out-dir /tmp/thegn-cache-off-out /tmp/thegn-cache-off.rs && \
                 test -f /tmp/thegn-cache-off-out/libthegn_cache_off.rlib",
            ),
            Duration::from_secs(10),
        ));
    }
}
