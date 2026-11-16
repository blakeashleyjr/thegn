//! Sandbox workspace-dir resolution keyed on the sandbox user's real `$HOME`.
//!
//! The provisioning "Prepare workspace" step, the pane's `cd`, chrome's git/fs
//! reads (via the persisted `GitLoc`), and the "already-provisioned" marker
//! checks must all agree on ONE workdir. The default can't be a fixed path: a
//! sandbox may run as any user, and only that user's `$HOME` is writable — a
//! bare `/workspace` at the root fs fails the provisioning `mkdir` with EACCES.
//! (machine0's stock NixOS image logs in as `nix`, HOME=`/home/nix`; ubuntu
//! images as `ubuntu`; others as root. The run-as user is a property of the
//! image, not of thegn.)
//!
//! So provisioning probes the sandbox `$HOME` once and caches it here (a tiny
//! per-sandbox state file, same pattern as `machine0_bridge`'s (ip,user) cache);
//! every workdir consumer resolves against that cache so they stay consistent
//! across the process and across restarts (the file persists). An explicit
//! `[env.<name>.provider] workdir` always wins and short-circuits the cache.

use std::path::{Path, PathBuf};

use thegn_core::config::EnvProviderConfig;
use thegn_svc::provider::Provider;

/// The root of the per-sandbox provider state: `$XDG_STATE_HOME/thegn/provider`.
/// This is the DB's own resolver (not `thegn_dir()`, which follows `$HOME`), so
/// every process of one instance agrees on it — a session opened from a Claude
/// profile shell (`HOME=~/.claude-profiles/x`) sees the markers the daemon wrote.
fn state_root() -> PathBuf {
    thegn_core::util::xdg_state_home().join("thegn/provider")
}

fn cache_dir() -> PathBuf {
    state_root().join("sandbox-home")
}

/// The pre-THE-2040 location under `thegn_dir()` (`$HOME`-relative). Read-only:
/// a hit is promoted into [`cache_dir`].
fn legacy_cache_dir() -> PathBuf {
    thegn_core::util::thegn_dir().join("sandbox-home")
}

/// Create (owner-only) and hold `state_root()/<sub>`. The anchor is the
/// instance's `$XDG_STATE_HOME/thegn` (the DB dir, trusted, may itself resolve
/// through a symlink); `provider/<sub>` beneath it is walked without following
/// links and pinned by the returned handle.
fn private_state_dir(sub: &str) -> std::io::Result<thegn_core::fsperm::PrivateDir> {
    let anchor = thegn_core::util::xdg_state_home().join("thegn");
    std::fs::create_dir_all(&anchor)?;
    thegn_core::fsperm::ensure_private_dir_beneath(&anchor, &Path::new("provider").join(sub))
}

fn cache_file(id: &str) -> PathBuf {
    // The sandbox id is already a filesystem-safe token (repo/worktree tokens +
    // a path-hash), so use it verbatim.
    cache_dir().join(id)
}

/// Whether `h` is a plausible absolute `$HOME`: an absolute path with no interior
/// whitespace. The whitespace check is load-bearing — an ssh client diagnostic
/// that leaks into the probe's captured stream (e.g. `ControlSocket <path> already
/// exists, disabling multiplexing`) concatenates onto the real home and yields a
/// space-containing token that `starts_with('/')` alone would wrongly accept and
/// cache, poisoning every later workdir lookup.
fn is_valid_home(h: &str) -> bool {
    h.starts_with('/') && !h.chars().any(char::is_whitespace)
}

/// Record the sandbox's resolved `$HOME` so later workdir lookups (pane, chrome,
/// marker) match what provisioning used. Best-effort: the cache is an
/// optimization — a miss just falls back to the bare default until re-probed.
pub fn cache_home(id: &str, home: &str) {
    let home = home.trim();
    if !is_valid_home(home) {
        return;
    }
    // best-effort: cache write (a failure just costs a later re-probe)
    let _ = private_state_dir("sandbox-home")
        .and_then(|dir| dir.write_owner_only_atomic(id, home.as_bytes()));
}

/// The cached sandbox `$HOME`, if provisioning recorded one. Rejects a
/// previously-poisoned value (interior whitespace) so a bad cache written by an
/// older build self-heals to the bare default until the next provision re-probes.
///
/// Falls back to the legacy `thegn_dir()` location; a valid legacy value is
/// promoted into the shared state dir (best-effort) and returned.
pub fn cached_home(id: &str) -> Option<String> {
    let read = |path: PathBuf| {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| is_valid_home(s))
    };
    if let Some(home) = read(cache_file(id)) {
        return Some(home);
    }
    let home = read(legacy_cache_dir().join(id))?;
    cache_home(id, &home);
    Some(home)
}

/// The default workspace dir under a sandbox `$HOME` (the repo clone root).
pub fn default_workdir(home: &str) -> String {
    format!("{}/workspace", home.trim_end_matches('/'))
}

/// Prefix `cmd` with a `cd` into the provisioned workdir for a managed-PROVIDER
/// pane. Its ssh/exec session lands at the login user's `$HOME` and no prefix cds
/// to the workspace (only the sprite native-exec path does), so `shell_inner`'s
/// `direnv exec .` would run at `$HOME` (no `.envrc`) and never enter the devShell
/// — the machine0 bare `sh` at `/home/nix`. No-op for `clean-shell` (the bare
/// escape hatch) or a non-provider placement.
///
/// Before the home is cached locally (e.g. this host never ran the provision),
/// the `cd` targets the REMOTE `$HOME/workspace` — `$HOME` is expanded by the
/// provider's shell, not here — falling back to the bare `/workspace`.
pub fn with_workdir_cd(
    choice: &str,
    placement: Option<&thegn_core::placement::Placement>,
    cmd: String,
) -> String {
    if choice == "clean-shell" {
        return cmd;
    }
    let Some(thegn_core::placement::Placement::Provider(p)) = placement else {
        return cmd;
    };
    let Some(home) = cached_home(&p.id) else {
        return format!("cd \"$HOME/workspace\" 2>/dev/null || cd /workspace 2>/dev/null; {cmd}");
    };
    let wd = thegn_core::util::sh_quote(&default_workdir(&home));
    format!("cd {wd} 2>/dev/null; {cmd}")
}

// --- local "provisioned" marker -------------------------------------------
//
// A cheap, LOCAL record (a tiny file, no network) that a sandbox `id` completed
// its full provisioning (repo clone + toolchain + the remote `.tg` marker). The
// authoritative signal is the REMOTE marker, but reading it costs a network
// round-trip — unsafe on the event loop (the 0%-idle invariant). This local
// mirror lets the on-loop attach path (`spawn_worktree_shell_pane` → `launch_spec`)
// cheaply refuse to drop a BARE shell onto an unprovisioned provider VM. It's
// set at provision completion and cleared on teardown; a stale/missing marker is
// self-correcting — a false "unprovisioned" just routes through the (idempotent)
// materialize provision, which re-sets it.

fn provisioned_dir() -> PathBuf {
    state_root().join("provisioned")
}

/// The pre-THE-2042 marker location under `thegn_dir()` (`$HOME`-relative).
fn legacy_provisioned_dir() -> PathBuf {
    thegn_core::util::thegn_dir().join("provisioned")
}

/// Record that sandbox `id` finished provisioning (local mirror of the remote
/// `.tg` marker). Best-effort — a missed write costs one extra idempotent
/// re-provision.
pub fn mark_provisioned(id: &str) {
    // best-effort: cache write: a miss just costs one idempotent re-provision
    let _ = private_state_dir("provisioned").and_then(|dir| dir.write_owner_only_atomic(id, b"1"));
}

/// Whether sandbox `id` has the LOCAL provisioned marker — a cheap file stat,
/// safe to call on the event loop (unlike the network remote-marker read).
/// A marker found only at the legacy `thegn_dir()` location counts, and is
/// promoted into the shared state dir (best-effort).
pub fn is_provisioned_locally(id: &str) -> bool {
    if provisioned_dir().join(id).exists() {
        return true;
    }
    if legacy_provisioned_dir().join(id).exists() {
        mark_provisioned(id);
        return true;
    }
    false
}

/// Drop the local provisioned marker (sandbox destroyed/recycled) so a recreated
/// bare VM under the same id isn't treated as provisioned.
/// Removes both the shared-state marker and any legacy `thegn_dir()` one, so a
/// stale legacy marker can't be re-promoted.
pub fn clear_provisioned(id: &str) {
    // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    let _ = thegn_core::fsperm::open_private_dir(&provisioned_dir())
        .and_then(|dir| dir.remove_file(id));
    let _ = std::fs::remove_file(legacy_provisioned_dir().join(id)); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
}

/// The sandbox login's real `$HOME` (absolute), via one cheap exec. Falls back to
/// `/root` when the probe fails or returns a non-absolute path (the historical
/// default). Sites the workspace + personal-layer uploads under the actual login
/// user's home, whatever the image boots as (`nix`, `ubuntu`, root, …).
pub fn probe_sandbox_home(provider: &Provider, id: &str) -> String {
    // Wrap `$HOME` in sentinels and extract exactly what's between them. A bare
    // `printf %s "$HOME"` has no delimiter, so any junk that leaks into the
    // captured stream — notably the ssh client's `ControlSocket <path> already
    // exists, disabling multiplexing` mux warning — concatenates directly onto
    // the path (`/home/nix` → `/home/nixControlSocket /run/...`), producing a
    // bogus workdir whose provisioning `mkdir` fails with EACCES. The markers
    // survive leading/trailing leakage; whitespace validation is the backstop.
    const PRE: &str = "<<THEGN_HOME:";
    const POST: &str = ":THEGN_HOME>>";
    crate::agent::block_on_provider(|| async {
        provider
            .run_exec(
                id,
                &[
                    "/bin/sh".to_string(),
                    "-lc".to_string(),
                    format!("printf '{PRE}%s{POST}' \"$HOME\""),
                ],
                None,
                &[],
            )
            .await
    })
    .ok()
    .and_then(|(_, out)| extract_home(&out))
    .filter(|h| is_valid_home(h))
    .unwrap_or_else(|| "/root".to_string())
}

/// Pull the `$HOME` value out of the sentinel-wrapped probe output, tolerating
/// arbitrary leading/trailing junk (leaked ssh mux diagnostics). Returns `None`
/// when the markers are absent (probe genuinely failed).
fn extract_home(out: &str) -> Option<String> {
    const PRE: &str = "<<THEGN_HOME:";
    const POST: &str = ":THEGN_HOME>>";
    let start = out.find(PRE)? + PRE.len();
    let rest = &out[start..];
    let end = rest.find(POST)?;
    Some(rest[..end].trim().to_string())
}

/// Provisioning entry point: probe the sandbox `$HOME`, cache it (so every later
/// workdir consumer resolves the same path), and return `(home, workdir)` — the
/// home for personal-layer uploads and the effective workspace dir (explicit
/// `[env] workdir` config wins, else `<home>/workspace`).
pub fn probe_and_resolve(
    provider: &Provider,
    id: &str,
    pc: &EnvProviderConfig,
) -> (String, String) {
    let home = probe_sandbox_home(provider, id);
    cache_home(id, &home);
    let w = pc.workdir.trim();
    let workdir = if w.is_empty() {
        default_workdir(&home)
    } else {
        w.to_string()
    };
    (home, workdir)
}

/// Resolve the sandbox workdir for `pc`'s env, keyed on the sandbox `id`: an
/// explicit `workdir` config wins; else the cached-`$HOME`-relative default
/// (`<home>/workspace`); else the bare [`EnvProviderConfig::sync_workdir`]
/// fallback (`/workspace`) when no home is cached yet (before the first
/// provision — where the marker is absent anyway, so a provision runs and
/// populates the cache).
pub fn resolve(pc: &EnvProviderConfig, id: &str) -> String {
    let w = pc.workdir.trim();
    if !w.is_empty() {
        return w.to_string();
    }
    match cached_home(id) {
        Some(home) => default_workdir(&home),
        None => pc.sync_workdir(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EnvProviderConfig, cache_home, cached_home, clear_provisioned, default_workdir,
        extract_home, is_provisioned_locally, is_valid_home, legacy_cache_dir,
        legacy_provisioned_dir, mark_provisioned, provisioned_dir, resolve, with_workdir_cd,
    };

    // These tests mutate process-global env (HOME / THEGN_DIR / XDG_STATE_HOME)
    // only through `testenv::IsolatedState`, which holds the crate-wide
    // `testenv::ENV_LOCK` and restores every state key on drop (even on panic).

    fn pc(provider: &str, workdir: &str) -> EnvProviderConfig {
        EnvProviderConfig {
            provider: provider.into(),
            workdir: workdir.into(),
            ..Default::default()
        }
    }

    fn provider_placement(id: &str) -> thegn_core::placement::Placement {
        thegn_core::placement::Placement::Provider(thegn_core::placement::ProviderPlacement {
            provider: "machine0".into(),
            id: id.into(),
            interactive_prefix: vec![],
            control_prefix: vec![],
            up_command: vec![],
            down_command: vec![],
        })
    }

    #[test]
    fn default_workdir_joins_under_home() {
        assert_eq!(default_workdir("/home/nix"), "/home/nix/workspace");
        assert_eq!(default_workdir("/home/nix/"), "/home/nix/workspace");
        assert_eq!(default_workdir("/root"), "/root/workspace");
    }

    #[test]
    fn explicit_workdir_wins_over_cache() {
        // No cache lookup at all when the env pins a workdir.
        assert_eq!(resolve(&pc("machine0", "/srv/code"), "id-1"), "/srv/code");
    }

    #[test]
    fn extract_home_survives_leaked_ssh_mux_diagnostics() {
        // Clean output.
        assert_eq!(
            extract_home("<<THEGN_HOME:/home/nix:THEGN_HOME>>").as_deref(),
            Some("/home/nix")
        );
        // The real-world corruption: an ssh mux warning bleeds into the stream.
        // With sentinels we still recover the exact home (no `/home/nixControlSocket`).
        let leaked = "ControlSocket /run/user/1000/tg-ssh/cm-b35d already exists, disabling \
                      multiplexing<<THEGN_HOME:/home/nix:THEGN_HOME>>";
        assert_eq!(extract_home(leaked).as_deref(), Some("/home/nix"));
        // Trailing junk after the closing marker is ignored too.
        assert_eq!(
            extract_home("<<THEGN_HOME:/home/ubuntu:THEGN_HOME>>ControlSocket junk").as_deref(),
            Some("/home/ubuntu")
        );
        // No markers ⇒ the probe genuinely failed.
        assert_eq!(extract_home("/home/nixControlSocket /run/..."), None);
    }

    #[test]
    fn poisoned_home_with_whitespace_is_rejected_not_cached() {
        // A space-containing value (the leaked-diagnostic shape) is refused by
        // both the writer and the reader, so it can never drive a workdir.
        assert!(!is_valid_home(
            "/home/nixControlSocket /run/user/1000/tg-ssh/cm"
        ));
        assert!(is_valid_home("/home/nix"));
        assert!(!is_valid_home("relative/home"));
    }

    #[test]
    fn with_workdir_cd_prefixes_provider_panes_only() {
        use thegn_core::placement::Placement;
        let _state = crate::testenv::IsolatedState::new();
        let provider = provider_placement("tg-x-abc");
        // Cached home ⇒ cd into <home>/workspace first.
        cache_home("tg-x-abc", "/home/nix");
        assert_eq!(
            with_workdir_cd("shell", Some(&provider), "run".into()),
            "cd /home/nix/workspace 2>/dev/null; run"
        );
        // clean-shell escape hatch, a local placement, and no placement ⇒ unchanged.
        assert_eq!(
            with_workdir_cd("clean-shell", Some(&provider), "run".into()),
            "run"
        );
        assert_eq!(
            with_workdir_cd("shell", Some(&Placement::Local), "run".into()),
            "run"
        );
        assert_eq!(with_workdir_cd("shell", None, "run".into()), "run");
    }

    #[test]
    fn with_workdir_cd_without_cache_falls_back_to_remote_home_workspace() {
        let _state = crate::testenv::IsolatedState::new();
        let provider = provider_placement("tg-never-cached");
        // `$HOME` stays literal: the provider's remote shell expands it.
        assert_eq!(
            with_workdir_cd("shell", Some(&provider), "run".into()),
            "cd \"$HOME/workspace\" 2>/dev/null || cd /workspace 2>/dev/null; run"
        );
        // clean-shell is still untouched.
        assert_eq!(
            with_workdir_cd("clean-shell", Some(&provider), "run".into()),
            "run"
        );
    }

    #[test]
    fn provisioned_marker_roundtrips() {
        let state = crate::testenv::IsolatedState::new();
        let id = "thegn-tg-clever-falcon-rmuy88";
        assert!(!is_provisioned_locally(id), "absent before mark");
        mark_provisioned(id);
        assert!(is_provisioned_locally(id), "present after mark");
        assert!(
            provisioned_dir().starts_with(state.state_home()),
            "marker lives under the XDG state home"
        );
        assert!(provisioned_dir().join(id).is_file());
        clear_provisioned(id);
        assert!(!is_provisioned_locally(id), "absent after clear");
        // Clearing a never-marked id is a no-op (no panic).
        clear_provisioned("never-marked");
    }

    #[test]
    fn markers_written_under_one_home_are_seen_from_another_home() {
        let state = crate::testenv::IsolatedState::new();
        let id = "thegn-tg-shared-sprite";
        // "Process" A: the daemon under the real HOME provisions the sandbox.
        mark_provisioned(id);
        cache_home(id, "/home/sprite");
        // "Process" B: a Claude profile shell with a different HOME/THEGN_DIR but
        // the same instance state home.
        // A profile shell differs only in HOME/THEGN_DIR; no marker may live
        // under the HOME-relative thegn_dir(), and wiping it changes nothing.
        let thegn_dir = thegn_core::util::thegn_dir();
        assert_eq!(thegn_dir, state.thegn_dir());
        assert!(!provisioned_dir().starts_with(&thegn_dir));
        assert!(!super::cache_file(id).starts_with(&thegn_dir));
        let _ = std::fs::remove_dir_all(&thegn_dir); // best-effort: cleanup: the dir may not exist; the asserts below are the check
        assert!(is_provisioned_locally(id), "marker visible across HOMEs");
        assert_eq!(cached_home(id).as_deref(), Some("/home/sprite"));
        assert_eq!(resolve(&pc("sprites", ""), id), "/home/sprite/workspace");
        assert_eq!(
            with_workdir_cd("shell", Some(&provider_placement(id)), "run".into()),
            "cd /home/sprite/workspace 2>/dev/null; run"
        );
    }

    #[test]
    fn legacy_markers_are_honoured_and_promoted() {
        let _state = crate::testenv::IsolatedState::new();
        let id = "thegn-tg-legacy-box";
        // An older build wrote both under thegn_dir().
        std::fs::create_dir_all(legacy_provisioned_dir()).unwrap();
        std::fs::write(legacy_provisioned_dir().join(id), b"1").unwrap();
        std::fs::create_dir_all(legacy_cache_dir()).unwrap();
        std::fs::write(legacy_cache_dir().join(id), "/home/nix\n").unwrap();
        assert!(!provisioned_dir().join(id).exists());

        assert!(is_provisioned_locally(id), "legacy marker counts");
        assert!(
            provisioned_dir().join(id).is_file(),
            "legacy marker promoted"
        );
        assert_eq!(cached_home(id).as_deref(), Some("/home/nix"));
        assert_eq!(
            std::fs::read_to_string(super::cache_file(id)).unwrap(),
            "/home/nix",
            "legacy home promoted"
        );
        // A poisoned legacy home is neither returned nor promoted.
        let bad = "thegn-tg-legacy-poisoned";
        std::fs::write(legacy_cache_dir().join(bad), "/home/nix ControlSocket").unwrap();
        assert_eq!(cached_home(bad), None);
        assert!(!super::cache_file(bad).exists());
    }

    #[test]
    fn clear_provisioned_removes_new_and_legacy_markers() {
        let _state = crate::testenv::IsolatedState::new();
        let id = "thegn-tg-both";
        mark_provisioned(id);
        std::fs::create_dir_all(legacy_provisioned_dir()).unwrap();
        std::fs::write(legacy_provisioned_dir().join(id), b"1").unwrap();
        clear_provisioned(id);
        assert!(!provisioned_dir().join(id).exists());
        assert!(!legacy_provisioned_dir().join(id).exists());
        assert!(!is_provisioned_locally(id), "no legacy re-promotion");
    }

    #[test]
    fn cache_roundtrip_and_resolution() {
        // Isolate the state dir so the test never touches a live cache.
        let _state = crate::testenv::IsolatedState::new();
        let id = "sandbox-abc";
        // Cold: no cache ⇒ the bare `/workspace` fallback.
        assert_eq!(resolve(&pc("machine0", ""), id), "/workspace");
        // A non-absolute or empty home is rejected (never poisons the cache).
        cache_home(id, "relative/home");
        assert_eq!(cached_home(id), None);
        // Warm: a probed `$HOME` drives the home-relative default.
        cache_home(id, "/home/nix");
        assert_eq!(cached_home(id).as_deref(), Some("/home/nix"));
        assert_eq!(resolve(&pc("machine0", ""), id), "/home/nix/workspace");
        // A re-probe overwrites the cached value.
        cache_home(id, "/home/ubuntu");
        assert_eq!(cached_home(id).as_deref(), Some("/home/ubuntu"));
    }
}
