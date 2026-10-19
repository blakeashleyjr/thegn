//! Read an already-existing cached Nix flake `devShell` environment for a
//! sandboxed worktree pane. This module deliberately does not evaluate flakes,
//! spawn `nix`, or write cache entries; cold environments remain uncached until
//! explicit user or target-side setup.
//!
//! Flow (Tier A in
//! `docs/superpowers/specs/2026-06-26-sandbox-devshell-injection-design.md`):
//!
//! 1. At pane-spawn the host calls [`cached`] (fast — file IO only) and prepends
//!    the resolved `PATH` to the pane's environment. The referenced store paths
//!    are already realized and bind-mounted read-only, so the tools just run.
//!
//! No flake or cold, invalid cache → `None`, and the pane gets exactly today's
//! environment. Cache snapshots are treated as untrusted input.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The injectable slice of a devShell's environment. `path` is the devShell's
/// `PATH` (prepended to the pane's own `PATH`); `vars` are other *safe* exported
/// variables (Nix build-noise is filtered out — see `is_noise`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Devshell {
    pub path: Option<String>,
    pub vars: Vec<(String, String)>,
}

impl Devshell {
    /// Nothing worth injecting — used to skip injection and reject empty cache
    /// entries.
    pub fn is_empty(&self) -> bool {
        self.path.is_none() && self.vars.is_empty()
    }
}

/// Variable names we never inject into a pane: shell/session-managed state and
/// the build-internal vars `nix print-dev-env` emits as "exported". Injecting
/// these would corrupt an interactive shell (e.g. `HOME`, `SHELL`) or leak Nix
/// builder scaffolding (`out`, `stdenv`, `system`, …). `PATH` is handled
/// separately (prepended, never blindly overwritten).
const DENY: &[&str] = &[
    // shell / session managed
    "PATH",
    "HOME",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "SHELL",
    "USER",
    "LOGNAME",
    "HOSTNAME",
    "TERM",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TEMPDIR",
    "_",
    "IN_NIX_SHELL",
    "HOST_PATH",
    // nix build internals
    "out",
    "outputs",
    "src",
    "name",
    "pname",
    "version",
    "system",
    "builder",
    "stdenv",
    "shell",
    "shellHook",
    "buildInputs",
    "nativeBuildInputs",
    "propagatedBuildInputs",
    "propagatedNativeBuildInputs",
    "depsBuildBuild",
    "configureFlags",
    "cmakeFlags",
    "mesonFlags",
    "patches",
    "phases",
    "buildPhase",
    "configurePhase",
    "installPhase",
    "patchPhase",
    "unpackPhase",
    "dontAddDisableDepTrack",
    "strictDeps",
    "doCheck",
    "doInstallCheck",
];

/// Should this exported var be skipped when injecting into a pane?
fn is_noise(name: &str) -> bool {
    DENY.contains(&name)
        || name.starts_with("NIX_")
        || name.starts_with("BASH_FUNC_")
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("GIT_")
        || name.starts_with("SSH_")
        || name.starts_with("GPG_")
        || name.ends_with("_PROXY")
        || name.ends_with("_proxy")
        || name.starts_with("CARGO_TARGET_")
        || matches!(
            name,
            "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "RUSTC"
                | "CARGO_HOME"
                | "CARGO_BUILD_RUSTC_WRAPPER"
                | "CARGO_BUILD_RUSTC"
                | "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"
                | "RUSTFLAGS"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "PYTHONSTARTUP"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "PERL5OPT"
                | "PERL5LIB"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "RUBYOPT"
                | "RUBYLIB"
                | "JAVA_TOOL_OPTIONS"
                | "_JAVA_OPTIONS"
                | "BASH_ENV"
                | "ENV"
                | "PROMPT_COMMAND"
                | "ZDOTDIR"
                | "XDG_CONFIG_HOME"
        )
        || crate::bundle::is_credential_key(name)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn sanitize(dev: Devshell) -> Devshell {
    const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;
    let path = dev.path.and_then(|path| {
        if path.len() > MAX_ENV_VALUE_BYTES {
            return None;
        }
        let entries: Vec<_> = path
            .split(':')
            .filter(|entry| {
                let path = Path::new(entry);
                path.is_absolute()
                    && path
                        .strip_prefix("/nix/store")
                        .is_ok_and(|rest| !rest.as_os_str().is_empty())
                    && !path
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
            })
            .collect();
        (!entries.is_empty()).then(|| entries.join(":"))
    });
    let mut vars: Vec<_> = dev
        .vars
        .into_iter()
        .filter(|(name, value)| {
            valid_name(name) && !is_noise(name) && value.len() <= MAX_ENV_VALUE_BYTES
        })
        .collect();
    vars.sort();
    Devshell { path, vars }
}

/// Parse `nix print-dev-env --json` output into the injectable [`Devshell`].
/// Pure (no IO) so it is unit-tested directly. `PATH` is pulled into
/// [`Devshell::path`]; other `"exported"`, non-`is_noise` vars ride in `vars`
/// (sorted for determinism). Malformed/empty input yields an empty `Devshell`.
pub fn parse_print_dev_env(json: &str) -> Devshell {
    let mut out = Devshell::default();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return out;
    };
    let Some(vars) = v.get("variables").and_then(|x| x.as_object()) else {
        return out;
    };
    for (name, spec) in vars {
        if spec.get("type").and_then(|t| t.as_str()) != Some("exported") {
            continue;
        }
        let Some(val) = spec.get("value").and_then(|x| x.as_str()) else {
            continue;
        };
        if name == "PATH" {
            out.path = Some(val.to_string());
        } else if !is_noise(name) {
            out.vars.push((name.clone(), val.to_string()));
        }
    }
    out.vars.sort();
    sanitize(out)
}

/// Domain-separated SHA-256 cache key over the canonical repo path, `flake.nix`,
/// and the presence and contents of `flake.lock`. It does not bind imported
/// local Nix files, the resolved closure, or launch policy; those remain
/// follow-ups. `None` means the repo cannot be safely identified as a flake
/// with bounded inputs, so callers use a cold cache.
pub fn cache_key(repo_root: &Path) -> Option<String> {
    const MAX_FLAKE_INPUT_BYTES: u64 = 4 * 1024 * 1024;
    let canonical_repo = std::fs::canonicalize(repo_root).ok()?;
    let flake_src = read_bounded_input(&canonical_repo.join("flake.nix"), MAX_FLAKE_INPUT_BYTES)?;
    let lock = match read_bounded_input(&canonical_repo.join("flake.lock"), MAX_FLAKE_INPUT_BYTES) {
        Ok(lock) => Some(lock),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };

    let mut hash = Sha256::new();
    hash.update(b"thegn-devenv-cache-key-v2\0");
    hash_field(&mut hash, canonical_repo.as_os_str().as_encoded_bytes());
    hash_field(&mut hash, &flake_src);
    match lock {
        Some(lock) => {
            hash.update([1]);
            hash_field(&mut hash, &lock);
        }
        None => hash.update([0]),
    }
    Some(format!("{:x}", hash.finalize()))
}

fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn read_bounded_input(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Nix input exceeds the cache-key size limit",
        ));
    }
    Ok(bytes)
}

fn cache_path(key: &str) -> PathBuf {
    crate::util::xdg_state_home()
        .join("thegn/devenv")
        .join(format!("{key}.json"))
}

/// The cached devShell env for `repo_root`, or `None` when the repo has no
/// flake, the cache is cold or stale, or the resolve produced nothing usable.
/// Fast (a single small file read + parse) — safe to call on the pane-spawn
/// path. A cold cache remains cold; this function never evaluates or writes it.
pub fn cached(repo_root: &Path) -> Option<Devshell> {
    let key = cache_key(repo_root)?;
    read_cache(&cache_path(&key))
}

fn read_cache(path: &Path) -> Option<Devshell> {
    const MAX_CACHE_BYTES: u64 = 1024 * 1024;
    let file = match crate::fsperm::open_regular_file_nofollow(path) {
        Ok(file) => file,
        Err(error) => {
            tracing::debug!(target: "thegn::devenv", path = %path.display(), %error, "rejecting devShell cache file");
            return None;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::debug!(target: "thegn::devenv", path = %path.display(), %error, "cannot inspect devShell cache file");
            return None;
        }
    };
    if let Err(reason) = cache_file_admissible(&metadata) {
        tracing::debug!(target: "thegn::devenv", path = %path.display(), reason, "rejecting devShell cache file");
        return None;
    }
    let parent = match path.parent() {
        Some(parent) => parent,
        None => {
            tracing::debug!(target: "thegn::devenv", path = %path.display(), "rejecting devShell cache file without a parent directory");
            return None;
        }
    };
    if let Err(reason) = cache_parent_admissible(parent) {
        tracing::debug!(target: "thegn::devenv", path = %parent.display(), reason, "rejecting devShell cache directory");
        return None;
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut bytes) {
        tracing::debug!(target: "thegn::devenv", path = %path.display(), %error, "cannot read devShell cache file");
        return None;
    }
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        tracing::debug!(target: "thegn::devenv", path = %path.display(), "rejecting oversized devShell cache file");
        return None;
    }
    let raw = match std::str::from_utf8(&bytes) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::debug!(target: "thegn::devenv", path = %path.display(), %error, "rejecting non-UTF-8 devShell cache file");
            return None;
        }
    };
    let devshell = match serde_json::from_str::<Devshell>(raw) {
        Ok(devshell) => devshell,
        Err(error) => {
            tracing::debug!(target: "thegn::devenv", path = %path.display(), %error, "rejecting malformed devShell cache file");
            return None;
        }
    };
    let devshell = sanitize(devshell);
    if devshell.is_empty() {
        tracing::debug!(target: "thegn::devenv", path = %path.display(), "rejecting empty devShell cache file");
        return None;
    }
    Some(devshell)
}

fn cache_file_admissible(meta: &std::fs::Metadata) -> Result<(), &'static str> {
    if !meta.is_file() {
        return Err("cache entry is not a regular file");
    }
    if !crate::fsperm::owned_by_current_user(meta) {
        return Err("cache entry is not owned by the current user");
    }
    crate::fsperm::ensure_single_link_regular_file(meta)
        .map_err(|_| "cache entry has multiple hard links")
}

fn cache_parent_admissible(parent: &Path) -> Result<(), &'static str> {
    let metadata =
        std::fs::symlink_metadata(parent).map_err(|_| "cache directory cannot be inspected")?;
    if metadata.file_type().is_symlink() {
        return Err("cache directory is a symlink");
    }
    if !metadata.is_dir() {
        return Err("cache parent is not a directory");
    }
    if !crate::fsperm::owned_by_current_user(&metadata) {
        return Err("cache directory is not owned by the current user");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_path_and_filters_noise() {
        // A representative slice of `nix print-dev-env --json`: PATH + a project
        // var we want, mixed with shell/build vars we must drop.
        let json = r#"{
            "variables": {
                "PATH": {"type": "exported", "value": "/nix/store/aaa/bin:/nix/store/bbb/bin"},
                "THEGN_YAZI_BIN": {"type": "exported", "value": "/nix/store/yz/bin/yazi"},
                "HOME": {"type": "exported", "value": "/homeless-shelter"},
                "stdenv": {"type": "exported", "value": "/nix/store/stdenv"},
                "NIX_CFLAGS_COMPILE": {"type": "exported", "value": "-frandom"},
                "shellHook": {"type": "var", "value": "export X=1"},
                "name": {"type": "exported", "value": "nix-shell"},
                "AR": {"type": "var", "value": "ar"}
            }
        }"#;
        let dev = parse_print_dev_env(json);
        assert_eq!(
            dev.path.as_deref(),
            Some("/nix/store/aaa/bin:/nix/store/bbb/bin")
        );
        // Only the non-noise, exported, non-PATH var survives.
        assert_eq!(
            dev.vars,
            vec![(
                "THEGN_YAZI_BIN".to_string(),
                "/nix/store/yz/bin/yazi".to_string()
            )]
        );
    }

    #[test]
    fn parse_handles_malformed_and_empty() {
        assert!(parse_print_dev_env("not json").is_empty());
        assert!(parse_print_dev_env("{}").is_empty());
        assert!(parse_print_dev_env(r#"{"variables": {}}"#).is_empty());
        // A var-only (non-exported) entry yields nothing injectable.
        assert!(
            parse_print_dev_env(r#"{"variables":{"X":{"type":"var","value":"y"}}}"#).is_empty()
        );
    }

    #[test]
    fn cache_key_none_without_flake_and_stable_with() {
        let dir = tempfile::tempdir().unwrap();
        // No flake.nix ⇒ not a nix repo.
        assert!(cache_key(dir.path()).is_none());
        // With a flake the key is stable for identical content...
        std::fs::write(dir.path().join("flake.nix"), "{ outputs = _: {}; }").unwrap();
        let k1 = cache_key(dir.path()).unwrap();
        assert_eq!(Some(&k1), cache_key(dir.path()).as_ref());
        // ...and changes when flake.lock changes (invalidation).
        std::fs::write(dir.path().join("flake.lock"), "{\"nodes\":{}}").unwrap();
        let k2 = cache_key(dir.path()).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_is_a_full_sha256_and_binds_the_lock_and_repo_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo_a = dir.path().join("repo-a\nodd");
        let repo_b = dir.path().join("repo-b");
        std::fs::create_dir(&repo_a).unwrap();
        std::fs::create_dir(&repo_b).unwrap();
        for repo in [&repo_a, &repo_b] {
            std::fs::write(repo.join("flake.nix"), "{ outputs = _: {}; }").unwrap();
        }
        let key_a = cache_key(&repo_a).unwrap();
        let key_b = cache_key(&repo_b).unwrap();
        assert_eq!(key_a.len(), 64);
        assert!(
            key_a
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_ne!(key_a, key_b);
        std::fs::write(repo_a.join("flake.lock"), "{\"nodes\":{}}").unwrap();
        assert_ne!(key_a, cache_key(&repo_a).unwrap());
    }

    #[test]
    fn cache_key_refuses_an_oversized_flake() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flake.nix"),
            vec![b'x'; 4 * 1024 * 1024 + 1],
        )
        .unwrap();
        assert!(cache_key(dir.path()).is_none());
    }

    #[test]
    fn devshell_round_trips_through_cache_json() {
        // The on-disk cache is just serialized Devshell; round-trip must hold so
        // `cached()` reads the same serialized shape that target-side tooling
        // may have placed in the cache.
        let dev = Devshell {
            path: Some("/nix/store/x/bin".into()),
            vars: vec![("FOO".into(), "bar".into())],
        };
        let json = serde_json::to_string(&dev).unwrap();
        assert_eq!(serde_json::from_str::<Devshell>(&json).unwrap(), dev);
    }

    #[test]
    fn parser_rejects_process_control_and_credential_variables_and_filters_path() {
        let mut variables = serde_json::Map::new();
        for name in [
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTC",
            "CARGO_HOME",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "GIT_CONFIG_GLOBAL",
            "SSH_AUTH_SOCK",
            "GPG_TTY",
            "HTTPS_PROXY",
            "https_proxy",
            "PYTHONSTARTUP",
            "PYTHONPATH",
            "PYTHONHOME",
            "PERL5OPT",
            "PERL5LIB",
            "NODE_OPTIONS",
            "NODE_PATH",
            "RUBYOPT",
            "RUBYLIB",
            "JAVA_TOOL_OPTIONS",
            "_JAVA_OPTIONS",
            "BASH_ENV",
            "ENV",
            "PROMPT_COMMAND",
            "ZDOTDIR",
            "XDG_CONFIG_HOME",
            "GH_TOKEN",
            "BAD=NAME",
        ] {
            variables.insert(
                name.into(),
                serde_json::json!({"type":"exported", "value":"bad"}),
            );
        }
        variables.insert("PATH".into(), serde_json::json!({
            "type":"exported", "value":"/usr/bin:/nix/store/abc/bin:relative:/nix/storeevil/bin:/nix/store/def/bin"
        }));
        let parsed = parse_print_dev_env(&serde_json::json!({"variables": variables}).to_string());
        assert_eq!(parsed.vars, Vec::<(String, String)>::new());
        assert_eq!(
            parsed.path.as_deref(),
            Some("/nix/store/abc/bin:/nix/store/def/bin")
        );

        let no_store = parse_print_dev_env(
            r#"{"variables":{"PATH":{"type":"exported","value":"/usr/bin:relative"}}}"#,
        );
        assert_eq!(no_store.path, None);

        let oversized_path = serde_json::json!({
            "variables": {
                "PATH": {"type":"exported", "value":format!("/nix/store/{}", "x".repeat(64 * 1024))}
            }
        });
        assert_eq!(parse_print_dev_env(&oversized_path.to_string()).path, None);

        let oversized = serde_json::json!({
            "variables": {
                "TOO_LARGE": {"type":"exported", "value":"x".repeat(64 * 1024 + 1)}
            }
        });
        assert!(parse_print_dev_env(&oversized.to_string()).is_empty());
    }

    #[test]
    fn cache_reader_sanitizes_hostile_entries_and_rejects_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let hostile = Devshell {
            path: Some("/tmp/evil:/nix/store/abc/bin".into()),
            vars: vec![
                ("LD_PRELOAD".into(), "/tmp/payload.so".into()),
                ("SSH_AUTH_SOCK".into(), "/tmp/agent.sock".into()),
                ("GH_TOKEN".into(), "secret".into()),
                ("SAFE_PROJECT_FLAG".into(), "yes".into()),
            ],
        };
        std::fs::write(&path, serde_json::to_vec(&hostile).unwrap()).unwrap();
        assert_eq!(
            read_cache(&path).unwrap(),
            Devshell {
                path: Some("/nix/store/abc/bin".into()),
                vars: vec![("SAFE_PROJECT_FLAG".into(), "yes".into())],
            }
        );
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(read_cache(&path).is_none());
    }

    #[test]
    fn cache_reader_rejects_a_symlinked_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        let link = dir.path().join("cache.json");
        std::fs::write(
            &real,
            serde_json::to_vec(&Devshell {
                path: Some("/nix/store/x/bin".into()),
                vars: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        match crate::fsperm::create_symlink(&real, &link, false) {
            Ok(()) => assert!(read_cache(&link).is_none()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => panic!("could not create symlink fixture: {error}"),
        }
    }

    #[test]
    fn cache_reader_rejects_a_hard_linked_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        let alias = dir.path().join("cache.json");
        std::fs::write(
            &real,
            serde_json::to_vec(&Devshell {
                path: Some("/nix/store/x/bin".into()),
                vars: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::hard_link(&real, &alias).unwrap();
        let metadata = std::fs::metadata(&alias).unwrap();
        assert_eq!(
            read_cache(&alias).is_none(),
            crate::fsperm::ensure_single_link_regular_file(&metadata).is_err()
        );
        if crate::fsperm::extra_hard_links(&metadata) == 1 {
            assert!(read_cache(&alias).is_none());
        }
        assert_eq!(
            cache_file_admissible(&metadata).is_err(),
            crate::fsperm::extra_hard_links(&metadata) == 1
        );
    }

    #[test]
    fn cache_reader_rejects_a_symlinked_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real-dir");
        let linked_dir = dir.path().join("linked-dir");
        std::fs::create_dir(&real_dir).unwrap();
        let cache = real_dir.join("cache.json");
        std::fs::write(
            &cache,
            serde_json::to_vec(&Devshell {
                path: Some("/nix/store/x/bin".into()),
                vars: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        match crate::fsperm::create_symlink(&real_dir, &linked_dir, true) {
            Ok(()) => assert!(read_cache(&linked_dir.join("cache.json")).is_none()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => panic!("could not create symlink fixture: {error}"),
        }
    }

    #[test]
    fn cache_file_admissibility_accepts_an_owned_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(
            cache_file_admissible(&std::fs::metadata(path).unwrap()),
            Ok(())
        );
        assert!(cache_file_admissible(&std::fs::metadata(dir.path()).unwrap()).is_err());
    }

    #[test]
    fn cache_parent_admissibility_rejects_missing_and_non_directory_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cache_parent_admissible(dir.path()).is_ok());
        assert!(cache_parent_admissible(&dir.path().join("missing")).is_err());
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(cache_parent_admissible(&file).is_err());
    }
}
