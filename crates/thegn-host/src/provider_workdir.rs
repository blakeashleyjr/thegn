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

use std::path::PathBuf;

use thegn_core::config::EnvProviderConfig;
use thegn_svc::provider::Provider;

fn cache_dir() -> PathBuf {
    thegn_core::util::thegn_dir().join("sandbox-home")
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
    let _ = std::fs::create_dir_all(cache_dir());
    let _ = std::fs::write(cache_file(id), home);
}

/// The cached sandbox `$HOME`, if provisioning recorded one. Rejects a
/// previously-poisoned value (interior whitespace) so a bad cache written by an
/// older build self-heals to the bare default until the next provision re-probes.
pub fn cached_home(id: &str) -> Option<String> {
    std::fs::read_to_string(cache_file(id))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| is_valid_home(s))
}

/// The default workspace dir under a sandbox `$HOME` (the repo clone root).
pub fn default_workdir(home: &str) -> String {
    format!("{}/workspace", home.trim_end_matches('/'))
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
    thegn_core::util::thegn_dir().join("provisioned")
}

/// Record that sandbox `id` finished provisioning (local mirror of the remote
/// `.tg` marker). Best-effort — a missed write costs one extra idempotent
/// re-provision.
pub fn mark_provisioned(id: &str) {
    let _ = std::fs::create_dir_all(provisioned_dir());
    let _ = std::fs::write(provisioned_dir().join(id), b"1");
}

/// Whether sandbox `id` has the LOCAL provisioned marker — a cheap file stat,
/// safe to call on the event loop (unlike the network remote-marker read).
pub fn is_provisioned_locally(id: &str) -> bool {
    provisioned_dir().join(id).exists()
}

/// Drop the local provisioned marker (sandbox destroyed/recycled) so a recreated
/// bare VM under the same id isn't treated as provisioned.
pub fn clear_provisioned(id: &str) {
    let _ = std::fs::remove_file(provisioned_dir().join(id));
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
    use super::*;

    fn pc(provider: &str, workdir: &str) -> EnvProviderConfig {
        EnvProviderConfig {
            provider: provider.into(),
            workdir: workdir.into(),
            ..Default::default()
        }
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
    fn provisioned_marker_roundtrips() {
        let tmp = std::env::temp_dir().join(format!("tg-prov-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: single-threaded test; `thegn_dir()` honors `THEGN_DIR`.
        unsafe { std::env::set_var("THEGN_DIR", &tmp) };
        let id = "thegn-tg-clever-falcon-rmuy88";
        assert!(!is_provisioned_locally(id), "absent before mark");
        mark_provisioned(id);
        assert!(is_provisioned_locally(id), "present after mark");
        clear_provisioned(id);
        assert!(!is_provisioned_locally(id), "absent after clear");
        // Clearing a never-marked id is a no-op (no panic).
        clear_provisioned("never-marked");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cache_roundtrip_and_resolution() {
        // Isolate the state dir so the test never touches a live cache.
        let tmp = std::env::temp_dir().join(format!("tg-workdir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: single-threaded test; scoped to this process. `thegn_dir()`
        // honors `THEGN_DIR`, so this reroutes the cache into the tmp dir.
        unsafe { std::env::set_var("THEGN_DIR", &tmp) };

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

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
