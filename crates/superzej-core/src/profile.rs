//! **Profiles** — the heavyweight, firewall-level work/personal isolation
//! (roadmap group H). A profile is a whole-process boundary: its own state/DB,
//! logs, config overlay, credentials, and (later) sandbox/network policy.
//!
//! The codebase is already env-driven — `util::xdg_state_home`,
//! `util::superzej_dir`, `db::db_path`, `sandbox::resolve`'s `env_passthrough`,
//! and `gh::resolve_token` all read `std::env` on every call. So the firewall is
//! enforced by **rerooting the process environment once, as the first statements
//! in `main`** (before the tokio runtime or any PTY thread) — then every path,
//! sandbox env, and token resolution becomes profile-scoped for free.
//!
//! ## Default stays in place (no whole-user migration)
//!
//! The `default` profile (no `--profile` / `SUPERZEJ_PROFILE`, or the literal
//! `"default"`) keeps **today's exact paths** — no reroot, no data migration.
//! Only a *named* profile reroots, into a fresh `<superzej_dir>/profiles/<name>/`
//! tree (its own worktrees dir + `state/` DB/logs). Existing worktrees are never
//! moved (their absolute paths are baked into git gitdir pointers + the DB); a
//! named profile simply starts with its own empty world. This is a deliberate
//! simplification of the design's "migrate default → profiles/default": it
//! delivers the same isolation while eliminating the risky one-time migration of
//! every existing user's live data.

use crate::util;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The resolved active-profile roots, installed once at startup.
#[derive(Debug, Clone)]
pub struct ProfilePaths {
    /// Profile name (`"default"` for the legacy/in-place profile).
    pub name: String,
    /// The profile's `SUPERZEJ_DIR` root (legacy `superzej_dir()` for default).
    pub root: PathBuf,
}

impl ProfilePaths {
    /// Whether this is the in-place default profile (no reroot performed).
    pub fn is_default(&self) -> bool {
        self.name == "default"
    }
}

static ACTIVE: OnceLock<ProfilePaths> = OnceLock::new();

/// Normalize a raw selector to a profile name: empty / `"default"` (any case) →
/// `"default"`; otherwise the slugified name (so it is a safe path component).
pub fn normalize_name(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("default") {
        "default".to_string()
    } else {
        util::slugify(t)
    }
}

/// The filesystem root for a named profile under `base` (a pre-reroot
/// `superzej_dir()`), or `None` for the in-place default profile.
pub fn profile_root(base: &std::path::Path, name: &str) -> Option<PathBuf> {
    (name != "default").then(|| base.join("profiles").join(name))
}

/// Resolve the active profile from `--profile` (falling back to
/// `SUPERZEJ_PROFILE`) and, for a *named* profile, reroot the process
/// environment so all path/credential/sandbox reads become profile-scoped.
///
/// MUST be called as one of the first statements in `main`, before the tokio
/// runtime or any other thread starts — `std::env::set_var` is `unsafe` and
/// unsound while other threads may read the environment. Idempotent
/// (`OnceLock`): a second call is a no-op.
///
/// # Safety
/// Single-threaded-startup invariant as above (same contract as
/// [`util::scrub_git_env`]).
pub fn reroot(cli_profile: Option<&str>) {
    if ACTIVE.get().is_some() {
        return;
    }
    let raw = cli_profile
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("SUPERZEJ_PROFILE").ok())
        .unwrap_or_default();
    let name = normalize_name(&raw);

    let paths = match profile_root(&util::superzej_dir(), &name) {
        // Named profile: reroot storage + advertise the name to children/config.
        Some(root) => {
            let state = root.join("state");
            let _ = std::fs::create_dir_all(&state);
            unsafe {
                std::env::set_var("SUPERZEJ_DIR", &root);
                std::env::set_var("XDG_STATE_HOME", &state);
                // So Config::load_layered picks up the profile overlay and every
                // pane/child shell knows its profile (util::HOST_ENV_ALLOW_PREFIX
                // admits `SUPERZEJ_*`).
                std::env::set_var("SUPERZEJ_PROFILE", &name);
            }
            apply_credential_env(&root);
            ProfilePaths { name, root }
        }
        // Default profile: leave every path exactly as today.
        None => ProfilePaths {
            name,
            root: util::superzej_dir(),
        },
    };
    let _ = ACTIVE.set(paths);
}

/// The profile-scoped credential environment for a named profile's `root`:
/// `(VAR, Some(value))` to set, `(VAR, None)` to unset. This is the
/// credential-firewall half of the profile boundary (H) — git identity, `gh`
/// config, and GnuPG are pinned into the profile tree, and the launching
/// shell's forge **tokens** are unset so neither panes nor sandbox passthrough
/// leak them across profiles (`gh` re-resolves from the profile `GH_CONFIG_DIR`).
///
/// `GIT_SSH_COMMAND` is only pinned when the profile actually ships an SSH key
/// (`ssh/id`) — forcing `IdentitiesOnly=yes` at an absent key would break *all*
/// ssh git ops for the profile. Config-dir vars are safe to inherit into panes
/// (they name a dir, not a secret); the token vars are the ones we drop.
pub fn credential_env(root: &std::path::Path) -> Vec<(&'static str, Option<String>)> {
    let s = |p: PathBuf| Some(p.to_string_lossy().into_owned());
    let mut out = vec![
        ("GIT_CONFIG_GLOBAL", s(root.join("config/git/config"))),
        ("GH_CONFIG_DIR", s(root.join("config/gh"))),
        ("GNUPGHOME", s(root.join("gnupg"))),
        // Drop the launching shell's forge tokens so they can't cross the
        // profile boundary; `gh` resolves the profile token from GH_CONFIG_DIR.
        ("GH_TOKEN", None),
        ("GITHUB_TOKEN", None),
    ];
    let key = root.join("ssh/id");
    if key.is_file() {
        out.push((
            "GIT_SSH_COMMAND",
            Some(format!(
                "ssh -i {} -o IdentitiesOnly=yes",
                key.to_string_lossy()
            )),
        ));
    }
    out
}

/// Apply [`credential_env`] to the process and create the config dirs. Called
/// from [`reroot`] (single-threaded startup).
fn apply_credential_env(root: &std::path::Path) {
    let _ = std::fs::create_dir_all(root.join("config/git"));
    let _ = std::fs::create_dir_all(root.join("config/gh"));
    let _ = std::fs::create_dir_all(root.join("gnupg"));
    for (var, val) in credential_env(root) {
        unsafe {
            match val {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
    }
}

/// Path-preserving credential mounts for the active profile (config dirs that
/// exist), so a sandboxed pane sees the profile identity at the same paths its
/// rerooted `GIT_CONFIG_GLOBAL`/`GH_CONFIG_DIR`/`GNUPGHOME` env points at.
/// Empty for the default profile.
pub fn sandbox_cred_mounts() -> Vec<(String, bool)> {
    let p = active();
    if p.is_default() {
        return Vec::new();
    }
    ["config/git", "config/gh", "gnupg"]
        .iter()
        .map(|sub| p.root.join(sub))
        .filter(|path| path.exists())
        .map(|path| (path.to_string_lossy().into_owned(), false))
        .collect()
}

/// The active profile paths (defaults to the in-place `default` profile when
/// [`reroot`] was never called — e.g. in unit tests).
pub fn active() -> ProfilePaths {
    ACTIVE.get().cloned().unwrap_or_else(|| ProfilePaths {
        name: "default".to_string(),
        root: util::superzej_dir(),
    })
}

/// The active profile name (`"default"` when unset).
pub fn name() -> String {
    active().name
}

// --- per-profile singleton lock --------------------------------------------

/// Holds the profile's advisory singleton lock for the process lifetime. The
/// `flock` is tied to the open fd, so it auto-releases on `Drop` and on process
/// death (incl. SIGKILL) — never a stale lock. `None` when the lock could not
/// be taken (contended default profile, permissions quirk, Windows).
#[must_use = "the lock releases as soon as the guard is dropped"]
pub struct SingletonGuard(#[allow(dead_code)] Option<std::fs::File>);

/// Result of the startup singleton check.
pub enum Singleton {
    /// This process owns the profile (default profile always lands here).
    Acquired(SingletonGuard),
    /// Another process already holds this profile's lock. **Advisory** — the
    /// caller warns but continues (per-profile DBs are separate files and
    /// SQLite WAL handles concurrent access; a hard refusal would break the
    /// nested-szhost dev workflow).
    AlreadyRunning,
}

/// Try to take the exclusive, non-blocking `flock` at `path`. `Ok(Some(file))`
/// = acquired (keep the file to hold it); `Ok(None)` = already held elsewhere.
#[cfg(not(windows))]
fn try_lock_nb(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match nix::fcntl::flock(
        file.as_raw_fd(),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    ) {
        Ok(()) => Ok(Some(file)),
        Err(nix::errno::Errno::EWOULDBLOCK) => Ok(None),
        Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
    }
}

/// The active profile's singleton lock file (`<root>/run/szhost.lock`),
/// creating the `run/` dir best-effort.
fn singleton_lock_path() -> std::path::PathBuf {
    let run = active().root.join("run");
    let _ = std::fs::create_dir_all(&run);
    run.join("szhost.lock")
}

/// Acquire the active profile's advisory singleton lock at
/// `<root>/run/szhost.lock`. One-shot non-blocking (never a poll loop — the
/// 0%-idle contract). Every profile (incl. default) takes the lock so
/// [`instance_running`] can detect a live compositor; contention on the
/// **default** profile still returns `Acquired` silently (no warn, no refusal)
/// — the lock was always advisory-only there and nested szhost launches must
/// keep working exactly as before.
#[cfg(not(windows))]
pub fn acquire_singleton() -> Singleton {
    match try_lock_nb(&singleton_lock_path()) {
        Ok(Some(file)) => {
            // Best-effort pid marker for a future focus path; failure is fine.
            use std::io::Write;
            let _ = file.set_len(0);
            let _ = writeln!(&file, "{}", std::process::id());
            Singleton::Acquired(SingletonGuard(Some(file)))
        }
        Ok(None) if active().is_default() => Singleton::Acquired(SingletonGuard(None)),
        Ok(None) => Singleton::AlreadyRunning,
        // A permissions quirk must never wedge the user out — degrade to running.
        Err(_) => Singleton::Acquired(SingletonGuard(None)),
    }
}

/// Best-effort: is another szhost process holding this profile's singleton
/// lock (i.e. a live interactive compositor)? Probes the flock without keeping
/// it. `false` on any error — callers degrade to "no instance" (launch).
#[cfg(not(windows))]
pub fn instance_running() -> bool {
    matches!(try_lock_nb(&singleton_lock_path()), Ok(None))
}

/// Windows singleton detection is a follow-up; report no instance (launch).
#[cfg(windows)]
pub fn instance_running() -> bool {
    false
}

#[cfg(windows)]
pub fn acquire_singleton() -> Singleton {
    // Windows singleton is a follow-up; default to running (no hard guard).
    Singleton::Acquired(SingletonGuard(None))
}

/// Argv to launch a fresh window for `profile` in a new terminal: the
/// configured `terminal` command (or `$TERMINAL`, then a small fallback list)
/// running `<szhost_exe> --profile <name>`. Returns `None` if no terminal
/// emulator can be found. Pure (no spawning) — the caller spawns it.
pub fn launch_window_argv(
    terminal: Option<&str>,
    szhost_exe: &str,
    profile: &str,
) -> Option<Vec<String>> {
    let term = terminal
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("TERMINAL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            ["ghostty", "wezterm", "kitty", "alacritty", "foot", "xterm"]
                .into_iter()
                .find(|t| util::have(t))
                .map(str::to_string)
        })?;
    Some(vec![
        term,
        "-e".to_string(),
        szhost_exe.to_string(),
        "--profile".to_string(),
        profile.to_string(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_maps_empty_and_default_to_default() {
        assert_eq!(normalize_name(""), "default");
        assert_eq!(normalize_name("  "), "default");
        assert_eq!(normalize_name("default"), "default");
        assert_eq!(normalize_name("Default"), "default");
        assert_eq!(normalize_name("work"), "work");
        // Named profiles are slugified into safe path components.
        assert_eq!(normalize_name("Work Laptop!"), "work-laptop");
    }

    #[test]
    fn profile_root_none_for_default_named_under_base() {
        let base = std::path::Path::new("/home/x/.superzej");
        assert_eq!(profile_root(base, "default"), None);
        assert_eq!(
            profile_root(base, "work"),
            Some(PathBuf::from("/home/x/.superzej/profiles/work"))
        );
    }

    #[test]
    fn credential_env_pins_config_dirs_and_drops_tokens() {
        let root = std::path::Path::new("/home/x/.superzej/profiles/work");
        let env = credential_env(root);
        let find = |k: &str| {
            env.iter()
                .find(|(v, _)| *v == k)
                .map(|(_, val)| val.clone())
        };
        assert_eq!(
            find("GIT_CONFIG_GLOBAL").flatten().as_deref(),
            Some("/home/x/.superzej/profiles/work/config/git/config")
        );
        assert_eq!(
            find("GH_CONFIG_DIR").flatten().as_deref(),
            Some("/home/x/.superzej/profiles/work/config/gh")
        );
        // Forge tokens are explicitly unset (None) so they can't cross profiles.
        assert_eq!(find("GH_TOKEN"), Some(None));
        assert_eq!(find("GITHUB_TOKEN"), Some(None));
        // No ssh/id on this synthetic root ⇒ GIT_SSH_COMMAND is not forced.
        assert!(find("GIT_SSH_COMMAND").is_none());
    }

    #[test]
    #[cfg(not(windows))]
    fn singleton_flock_is_exclusive_and_nonblocking() {
        let dir =
            std::env::temp_dir().join(format!("sz-lock-{}-{}", std::process::id(), util::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("szhost.lock");
        // First acquisition succeeds; while its guard is held, a second
        // non-blocking attempt is refused (no spin, no block).
        let held = try_lock_nb(&path).unwrap();
        assert!(held.is_some(), "first lock acquires");
        assert!(
            try_lock_nb(&path).unwrap().is_none(),
            "second is refused while held"
        );
        drop(held);
        // Released on drop → acquirable again.
        assert!(try_lock_nb(&path).unwrap().is_some(), "lock frees on drop");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn launch_window_argv_builds_terminal_exec() {
        let argv = launch_window_argv(Some("ghostty"), "/usr/bin/szhost", "work").unwrap();
        assert_eq!(
            argv,
            vec!["ghostty", "-e", "/usr/bin/szhost", "--profile", "work"]
        );
    }

    #[test]
    fn active_defaults_when_unset() {
        // In a test process reroot() is never called → default, in-place root.
        let p = active();
        assert!(p.is_default());
        assert_eq!(p.root, util::superzej_dir());
    }
}
