//! Resolve a repo's Nix flake `devShell` environment on the host and cache it,
//! so a sandboxed worktree pane — which can't reach the Nix daemon or write to
//! the read-only `/nix/store` — still gets the project toolchain
//! (linters/formatters/compilers) on `PATH` out of the box.
//!
//! Flow (Tier A in
//! `docs/superpowers/specs/2026-06-26-sandbox-devshell-injection-design.md`):
//!
//! 1. The compositor runs on the host, where the store is writable and the
//!    daemon lives; [`prewarm`] shells out to `nix print-dev-env --json` on a
//!    background thread and writes the parsed env to a content-addressed cache.
//! 2. At pane-spawn the host calls [`cached`] (fast — file IO only) and prepends
//!    the resolved `PATH` to the pane's environment. The referenced store paths
//!    are already realized and bind-mounted read-only, so the tools just run.
//!
//! Everything degrades silently: no flake → no-op; `nix` missing or the eval
//! fails → `None`, and the pane gets exactly today's environment.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// The injectable slice of a devShell's environment. `path` is the devShell's
/// `PATH` (prepended to the pane's own `PATH`); `vars` are other *safe* exported
/// variables (Nix build-noise is filtered out — see `is_noise`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Devshell {
    pub path: Option<String>,
    pub vars: Vec<(String, String)>,
}

impl Devshell {
    /// Nothing worth injecting — used both to skip injection and as a negative
    /// cache marker (a flake with no usable devShell still writes an empty file
    /// so [`prewarm`] doesn't re-run `nix` on every cold pane spawn).
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
    DENY.contains(&name) || name.starts_with("NIX_") || name.starts_with("BASH_FUNC_")
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
    out
}

/// Content-addressed cache key for a repo's flake inputs: a hash of the repo
/// path (to disambiguate repos), `flake.nix`, and `flake.lock`. `None` when the
/// repo has no `flake.nix`, which is also the "not a nix repo, skip entirely"
/// signal. Pure-ish (reads two small files); unit-tested.
pub fn cache_key(repo_root: &Path) -> Option<String> {
    let flake = repo_root.join("flake.nix");
    let flake_src = std::fs::read(&flake).ok()?; // absent ⇒ not a nix repo
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo_root.to_string_lossy().hash(&mut h);
    flake_src.hash(&mut h);
    // flake.lock may be absent (lockless flake); fold it in when present so a
    // lock bump (e.g. nixpkgs bump → new tool versions) invalidates the cache.
    if let Ok(lock) = std::fs::read(repo_root.join("flake.lock")) {
        lock.hash(&mut h);
    }
    Some(format!("{:016x}", h.finish()))
}

fn cache_path(key: &str) -> PathBuf {
    crate::util::xdg_state_home()
        .join("thegn/devenv")
        .join(format!("{key}.json"))
}

/// The cached devShell env for `repo_root`, or `None` when the repo has no
/// flake, the cache is cold or stale, or the resolve produced nothing usable.
/// Fast (a single small file read + parse) — safe to call on the pane-spawn
/// path. Pair with [`prewarm`] to populate a cold cache off the event loop.
pub fn cached(repo_root: &Path) -> Option<Devshell> {
    let key = cache_key(repo_root)?;
    let raw = std::fs::read_to_string(cache_path(&key)).ok()?;
    serde_json::from_str::<Devshell>(&raw)
        .ok()
        .filter(|d| !d.is_empty())
}

/// Tracks cache keys with an in-flight background resolve, so [`prewarm`] never
/// spawns two `nix` invocations for the same key concurrently.
fn in_flight() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Kick a background resolve (`nix print-dev-env --json`) that writes the cache,
/// if it isn't already warm. No-op when: the repo has no `flake.nix`, `nix`
/// isn't on PATH, the cache file already exists, or a resolve for this key is
/// already running. Returns immediately — **never blocks the caller** (the
/// `nix` eval can take seconds). Results are picked up by the next [`cached`].
pub fn prewarm(repo_root: &Path) {
    let Some(key) = cache_key(repo_root) else {
        return;
    };
    if cache_path(&key).is_file() {
        return; // already resolved (warm, or a negative-cache marker)
    }
    if !crate::util::have("nix") {
        return;
    }
    {
        let mut set = in_flight().lock().unwrap();
        if !set.insert(key.clone()) {
            return; // a resolve for this key is already in flight
        }
    }
    let root = repo_root.to_path_buf();
    std::thread::spawn(move || {
        resolve_and_cache(&root, &key);
        in_flight().lock().unwrap().remove(&key);
    });
}

/// Run `nix print-dev-env --json` for `repo_root`, parse it, and write the cache
/// file (always — an empty `Devshell` on failure acts as a negative cache so we
/// don't re-shell-out every cold spawn for a flake with no usable devShell).
/// Subprocess seam: excluded from coverage, exercised by smoke.
fn resolve_and_cache(repo_root: &Path, key: &str) {
    let dev = run_print_dev_env(repo_root).unwrap_or_default();
    let path = cache_path(key);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(&dev) {
        let _ = std::fs::write(&path, json);
    }
}

/// Invoke `nix print-dev-env --json` against the flake at `repo_root` and parse
/// it. `None` on a non-zero exit or spawn failure (degrade silently). Subprocess
/// seam.
fn run_print_dev_env(repo_root: &Path) -> Option<Devshell> {
    let installable = format!("{}#", repo_root.display());
    let out = std::process::Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "print-dev-env",
            "--json",
            &installable,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dev = parse_print_dev_env(&String::from_utf8_lossy(&out.stdout));
    (!dev.is_empty()).then_some(dev)
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
        let dir = std::env::temp_dir().join(format!("tg-devenv-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No flake.nix ⇒ not a nix repo.
        assert!(cache_key(&dir).is_none());
        // With a flake the key is stable for identical content...
        std::fs::write(dir.join("flake.nix"), "{ outputs = _: {}; }").unwrap();
        let k1 = cache_key(&dir).unwrap();
        assert_eq!(Some(&k1), cache_key(&dir).as_ref());
        // ...and changes when flake.lock changes (invalidation).
        std::fs::write(dir.join("flake.lock"), "{\"nodes\":{}}").unwrap();
        let k2 = cache_key(&dir).unwrap();
        assert_ne!(k1, k2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn devshell_round_trips_through_cache_json() {
        // The on-disk cache is just serialized Devshell; round-trip must hold so
        // `cached()` reads back what `resolve_and_cache()` wrote.
        let dev = Devshell {
            path: Some("/nix/store/x/bin".into()),
            vars: vec![("FOO".into(), "bar".into())],
        };
        let json = serde_json::to_string(&dev).unwrap();
        assert_eq!(serde_json::from_str::<Devshell>(&json).unwrap(), dev);
    }
}
