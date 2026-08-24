//! **Profiles** — the heavyweight, firewall-level work/personal isolation
//! (roadmap group H). A profile is a whole-process boundary: its own state/DB,
//! logs, config overlay, credentials, and (later) sandbox/network policy.
//!
//! The codebase is already env-driven — `util::xdg_state_home`,
//! `util::thegn_dir`, `db::db_path`, `sandbox::resolve`'s `env_passthrough`,
//! and `gh::resolve_token` all read `std::env` on every call. So the firewall is
//! enforced by **rerooting the process environment once, as the first statements
//! in `main`** (before the tokio runtime or any PTY thread) — then every path,
//! sandbox env, and token resolution becomes profile-scoped for free.
//!
//! ## Default stays in place (no whole-user migration)
//!
//! The `default` profile (no `--profile` / `THEGN_PROFILE`, or the literal
//! `"default"`) keeps **today's exact paths** — no reroot, no data migration.
//! Only a *named* profile reroots, into a fresh `<thegn_dir>/profiles/<name>/`
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
    /// The profile's `THEGN_DIR` root (legacy `thegn_dir()` for default).
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

/// Longest profile name used for a *newly created* profile root.
///
/// A profile name is pure cost in the control-socket path: a named profile
/// reroots `XDG_STATE_HOME` to `<thegn_dir>/profiles/<name>/state`, and the
/// daemon socket hangs off that. This is a sanity bound, NOT a guarantee — the
/// budget also depends on `$HOME`, so a short name can still overflow on a
/// deeply-nested home. The real safety net is the in-process degradation in
/// `handlers::startup::daemon_active`.
pub const MAX_NEW_PROFILE_NAME: usize = 32;

/// Bound a profile name's length, keeping distinct names distinct.
///
/// Truncating alone would silently collapse two long names onto one directory
/// (and one DB, one credential set) — so the truncation carries a suffix from
/// [`util::short_hash`] of the FULL name, which exists for exactly this
/// "collision-defusing" job. Names within the bound pass through untouched.
pub fn cap_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    // `slugify` output is ASCII, so byte-slicing is char-safe here.
    const SUFFIX: usize = 6;
    let keep = max.saturating_sub(SUFFIX + 1);
    format!("{}-{}", &name[..keep], util::short_hash(name, SUFFIX))
}

/// The filesystem root for a named profile under `base` (a pre-reroot
/// `thegn_dir()`), or `None` for the in-place default profile.
pub fn profile_root(base: &std::path::Path, name: &str) -> Option<PathBuf> {
    (name != "default").then(|| base.join("profiles").join(name))
}

/// Resolve the on-disk name for a profile, applying [`cap_name`] only to
/// profiles that do not exist yet.
///
/// Grandfathering is the point: capping unconditionally would repoint an
/// existing long-named profile at a different directory, orphaning its state,
/// DB and credentials with no migration. An already-created profile keeps its
/// name forever; only new ones are bounded.
fn on_disk_name(base: &std::path::Path, name: &str) -> String {
    match profile_root(base, name) {
        Some(root) if !root.exists() => cap_name(name, MAX_NEW_PROFILE_NAME),
        // Already on disk (or the default profile): leave it exactly as-is.
        _ => name.to_string(),
    }
}

/// Resolve the active profile from `--profile` (falling back to
/// `THEGN_PROFILE`) and, for a *named* profile, reroot the process
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
        .or_else(|| std::env::var("THEGN_PROFILE").ok())
        .unwrap_or_default();
    // Bound the name for a profile being created here; an existing one keeps
    // whatever it was created as (see `on_disk_name`).
    let base = util::thegn_dir();
    let name = on_disk_name(&base, &normalize_name(&raw));

    let paths = match profile_root(&base, &name) {
        // Named profile: reroot storage + advertise the name to children/config.
        Some(root) => {
            let state = root.join("state");
            let _ = std::fs::create_dir_all(&state);
            unsafe {
                std::env::set_var("THEGN_DIR", &root);
                std::env::set_var("XDG_STATE_HOME", &state);
                // So Config::load_layered picks up the profile overlay and every
                // pane/child shell knows its profile (util::HOST_ENV_ALLOW_PREFIX
                // admits `THEGN_*`).
                std::env::set_var("THEGN_PROFILE", &name);
            }
            apply_credential_env(&root);
            ProfilePaths { name, root }
        }
        // Default profile: leave every path exactly as today.
        None => ProfilePaths {
            name,
            root: util::thegn_dir(),
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
        root: util::thegn_dir(),
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
    /// nested-thegn dev workflow).
    AlreadyRunning,
}

/// Try to take the exclusive, non-blocking file lock at `path` (`flock` on
/// unix, `LockFileEx` on Windows — std's cross-platform `File::try_lock`).
/// `Ok(Some(file))` = acquired (keep the file to hold it); `Ok(None)` =
/// already held elsewhere. Released on drop AND on process death — never stale.
fn try_lock_nb(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// The active profile's singleton lock file (`<root>/run/thegn.lock`),
/// creating the `run/` dir best-effort.
fn singleton_lock_path() -> std::path::PathBuf {
    let run = active().root.join("run");
    let _ = std::fs::create_dir_all(&run);
    run.join("thegn.lock")
}

/// Acquire the active profile's advisory singleton lock at
/// `<root>/run/thegn.lock`. One-shot non-blocking (never a poll loop — the
/// 0%-idle contract). Every profile (incl. default) takes the lock so
/// [`instance_running`] can detect a live compositor; contention on the
/// **default** profile still returns `Acquired` silently (no warn, no refusal)
/// — the lock was always advisory-only there and nested thegn launches must
/// keep working exactly as before.
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

/// Best-effort: is another thegn process holding this profile's singleton
/// lock (i.e. a live interactive compositor)? Probes the lock without keeping
/// it. `false` on any error — callers degrade to "no instance" (launch).
pub fn instance_running() -> bool {
    matches!(try_lock_nb(&singleton_lock_path()), Ok(None))
}

/// Argv to launch a fresh window for `profile` in a new terminal: the
/// configured `terminal` command (or `$TERMINAL`, then a small fallback list)
/// running `<thegn_exe> --profile <name>`. Returns `None` if no terminal
/// emulator can be found. Pure (no spawning) — the caller spawns it.
pub fn launch_window_argv(
    terminal: Option<&str>,
    thegn_exe: &str,
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
        thegn_exe.to_string(),
        "--profile".to_string(),
        profile.to_string(),
    ])
}

/// Persistent, user-controlled ordering of profiles in the switcher.
///
/// Profiles have no DB rows (each profile is its own rerooted `thegn.db`), so a
/// cross-profile order cannot live in the DB. It lives in **shared, never-rerooted
/// config** at the real `XDG_CONFIG_HOME` — the one location [`reroot`] leaves
/// untouched, so every profile's process observes one shared order. The switcher
/// arranges the known profiles by this order and appends any not named in it in a
/// stable (caller-sorted) order, so a freshly-created profile never reshuffles the
/// existing ones. The sidecar JSON is a cache (config on disk is truth): writes
/// are best-effort and a missing/malformed file falls back to alphabetical.
pub mod order {
    use crate::util;
    use std::path::PathBuf;

    /// `~/.config/thegn/profiles-order.json` (the real, un-rerooted config home).
    fn order_path() -> PathBuf {
        util::xdg_config_home()
            .join("thegn")
            .join("profiles-order.json")
    }

    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct OrderFile {
        #[serde(default)]
        order: Vec<String>,
    }

    /// Read a persisted order from `path`. Empty on missing/unreadable/malformed.
    fn load_from(path: &std::path::Path) -> Vec<String> {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        serde_json::from_str::<OrderFile>(&raw)
            .map(|f| f.order)
            .unwrap_or_default()
    }

    /// Write `order` to `path`, creating the parent dir.
    fn save_to(path: &std::path::Path, order: &[String]) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let doc = OrderFile {
            order: order.to_vec(),
        };
        let json =
            serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{\"order\":[]}".to_string());
        std::fs::write(path, json)
    }

    /// The persisted profile order from the shared config sidecar (empty when
    /// absent — the switcher then falls back to a config seed / alphabetical).
    pub fn load_order() -> Vec<String> {
        load_from(&order_path())
    }

    /// Persist the ENTIRE profile order (not a swap) to the shared config sidecar.
    /// Best-effort — the sidecar is a cache; callers ignore the `Result`.
    pub fn save_order(order: &[String]) -> std::io::Result<()> {
        save_to(&order_path(), order)
    }

    /// Pure: arrange `known` by `order` — every `order` entry that exists in
    /// `known` first (de-duped, in order), then any `known` not named in `order`
    /// appended in their given (caller-sorted) sequence. Unknown/new profiles
    /// never reshuffle the ones the user has arranged.
    pub fn apply_order(known: &[String], order: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(known.len());
        for name in order {
            if known.iter().any(|k| k == name) && !out.iter().any(|o| o == name) {
                out.push(name.clone());
            }
        }
        for name in known {
            if !out.iter().any(|o| o == name) {
                out.push(name.clone());
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn v(xs: &[&str]) -> Vec<String> {
            xs.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn apply_order_places_ordered_first_then_appends_rest() {
            assert_eq!(
                apply_order(&v(&["a", "b", "c", "d"]), &v(&["c", "a"])),
                v(&["c", "a", "b", "d"])
            );
        }

        #[test]
        fn apply_order_empty_order_preserves_known() {
            assert_eq!(apply_order(&v(&["a", "b", "c"]), &[]), v(&["a", "b", "c"]));
        }

        #[test]
        fn apply_order_ignores_unknown_names_and_dedups() {
            // "x" is not a known profile ⇒ ignored; "a" repeated ⇒ deduped.
            assert_eq!(
                apply_order(&v(&["a", "b"]), &v(&["x", "a", "a", "b"])),
                v(&["a", "b"])
            );
        }

        #[test]
        fn apply_order_new_profile_appends_without_reshuffling() {
            // "personal"/"washu" are ordered; a freshly-added "hubone" lands last.
            assert_eq!(
                apply_order(
                    &v(&["default", "hubone", "personal", "washu"]),
                    &v(&["personal", "washu", "default"])
                ),
                v(&["personal", "washu", "default", "hubone"])
            );
        }

        #[test]
        fn save_then_load_roundtrips() {
            let dir = std::env::temp_dir().join(format!(
                "tg-order-{}-{}",
                std::process::id(),
                crate::util::now()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("profiles-order.json");
            assert!(load_from(&path).is_empty(), "missing file ⇒ empty");
            save_to(&path, &v(&["washu", "default", "personal"])).unwrap();
            assert_eq!(load_from(&path), v(&["washu", "default", "personal"]));
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn load_malformed_is_empty_not_a_panic() {
            let dir = std::env::temp_dir().join(format!(
                "tg-order-bad-{}-{}",
                std::process::id(),
                crate::util::now()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("profiles-order.json");
            std::fs::write(&path, b"{ not json").unwrap();
            assert!(load_from(&path).is_empty());
            std::fs::remove_dir_all(&dir).ok();
        }
    }
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
    fn cap_name_bounds_length_without_collapsing_distinct_names() {
        // Inside the bound: untouched.
        assert_eq!(cap_name("work", MAX_NEW_PROFILE_NAME), "work");
        let exact = "a".repeat(MAX_NEW_PROFILE_NAME);
        assert_eq!(cap_name(&exact, MAX_NEW_PROFILE_NAME), exact);

        // Over the bound: capped, and deterministic.
        let long = "client-acme-frontend-migration-squad-two";
        let capped = cap_name(long, MAX_NEW_PROFILE_NAME);
        assert!(capped.len() <= MAX_NEW_PROFILE_NAME, "{capped}");
        assert_eq!(capped, cap_name(long, MAX_NEW_PROFILE_NAME), "stable");

        // The point of the hash suffix: two names sharing a long prefix must
        // NOT collapse onto one directory (one DB, one credential set).
        let sibling = "client-acme-frontend-migration-squad-one";
        assert_ne!(capped, cap_name(sibling, MAX_NEW_PROFILE_NAME));

        // Still a safe path component.
        assert!(
            capped
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{capped}"
        );
    }

    #[test]
    fn on_disk_name_grandfathers_an_existing_long_profile() {
        let base = std::env::temp_dir().join(format!("thegn-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let long = "client-acme-frontend-migration-squad-two";

        // Not yet created ⇒ the new name is capped.
        assert_eq!(
            on_disk_name(&base, long),
            cap_name(long, MAX_NEW_PROFILE_NAME)
        );

        // Already on disk ⇒ used verbatim, so its state is never orphaned.
        std::fs::create_dir_all(base.join("profiles").join(long)).unwrap();
        assert_eq!(on_disk_name(&base, long), long);

        // `default` is never rerooted, so never capped.
        assert_eq!(on_disk_name(&base, "default"), "default");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn profile_root_none_for_default_named_under_base() {
        let base = std::path::Path::new("/home/x/.thegn");
        assert_eq!(profile_root(base, "default"), None);
        assert_eq!(
            profile_root(base, "work"),
            Some(PathBuf::from("/home/x/.thegn/profiles/work"))
        );
    }

    #[test]
    fn credential_env_pins_config_dirs_and_drops_tokens() {
        let root = std::path::Path::new("/home/x/.thegn/profiles/work");
        let env = credential_env(root);
        let find = |k: &str| {
            env.iter()
                .find(|(v, _)| *v == k)
                .map(|(_, val)| val.clone())
        };
        assert_eq!(
            find("GIT_CONFIG_GLOBAL").flatten().as_deref(),
            Some("/home/x/.thegn/profiles/work/config/git/config")
        );
        assert_eq!(
            find("GH_CONFIG_DIR").flatten().as_deref(),
            Some("/home/x/.thegn/profiles/work/config/gh")
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
            std::env::temp_dir().join(format!("tg-lock-{}-{}", std::process::id(), util::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("thegn.lock");
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
        let argv = launch_window_argv(Some("ghostty"), "/usr/bin/thegn", "work").unwrap();
        assert_eq!(
            argv,
            vec!["ghostty", "-e", "/usr/bin/thegn", "--profile", "work"]
        );
    }

    #[test]
    fn active_defaults_when_unset() {
        // In a test process reroot() is never called → default, in-place root.
        let p = active();
        assert!(p.is_default());
        assert_eq!(p.root, util::thegn_dir());
    }
}
