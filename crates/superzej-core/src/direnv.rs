//! Warm a worktree's `direnv` cache **on the host** so the in-sandbox `direnv`
//! hook works instead of failing against the read-only `/nix/store`.
//!
//! The problem: superzej runs each worktree pane as a login shell, so the
//! user's `direnv hook` fires *inside* the sandbox. With `nix-direnv`'s
//! `use flake`, a cold cache makes direnv rebuild the devShell — which must
//! write/lock `/nix/store`. But the sandbox mounts the store read-only and (by
//! default) exposes no Nix daemon, so the rebuild errors with
//! `Read-only file system` and direnv falls back to the previous environment.
//!
//! The fix mirrors [`crate::devenv`] (Tier A): the compositor runs on the host,
//! where the store is writable and the daemon lives. [`warm`] shells out to
//! `direnv exec <worktree> true` on a background thread, which builds + caches
//! `.direnv/flake-profile-<rev>.rc` (the dumped env) plus a gcroot symlink into
//! the store. On the *next* pane spawn the in-sandbox direnv finds that warm
//! cache and `direnv_load`s the `.rc` — a pure file read; the referenced store
//! path is already realized and bind-mounted read-only, so **no store write is
//! attempted**. Unlike `inject_devshell` (which only replays a flake devShell's
//! PATH) this makes the *whole* `.envrc` effect work in-pane.
//!
//! Everything degrades silently: no `.envrc`/flake → no-op; `direnv` missing →
//! no-op; a blocked (un-`allow`ed) worktree in `allowed-only` mode → warms
//! nothing and the pane gets exactly today's behavior.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Does this worktree have a flake-backed `.envrc` whose `nix-direnv` cache is
/// cold or stale — i.e. would the in-sandbox direnv try (and fail) to rebuild
/// it against the read-only store? Pure filesystem stat (no subprocess), so
/// it's cheap enough to gate the spawn path and is unit-tested.
///
/// Only `use flake` (`nix-direnv`) hits the read-only store, so we require a
/// `flake.nix`: a plain `.envrc` re-evals harmlessly in-sandbox, and a repo
/// without `.envrc`/flake has nothing to warm.
pub fn needs_warm(worktree: &Path) -> bool {
    let envrc = worktree.join(".envrc");
    let flake = worktree.join("flake.nix");
    if !envrc.is_file() || !flake.is_file() {
        return false;
    }
    let newest_input = [envrc, flake, worktree.join("flake.lock")]
        .iter()
        .filter_map(|p| mtime(p))
        .max();
    let Some(newest_input) = newest_input else {
        return false;
    };
    !cache_is_fresh(newest_input, newest_cache_rc_mtime(worktree))
}

/// Is the cached env dump new enough to replay without a rebuild? Pure — split
/// out from [`needs_warm`] so the mtime comparison is unit-tested without
/// depending on filesystem mtime granularity. A cold cache (`None`) is stale.
fn cache_is_fresh(newest_input: SystemTime, cache_rc: Option<SystemTime>) -> bool {
    matches!(cache_rc, Some(rc) if rc >= newest_input)
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

/// Newest mtime among `nix-direnv`'s cached env dumps (`.direnv/*.rc`). `None`
/// when no cache exists (cold).
fn newest_cache_rc_mtime(worktree: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    for entry in std::fs::read_dir(worktree.join(".direnv")).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("rc")
            && let Some(m) = mtime(&p)
        {
            newest = Some(newest.map_or(m, |n| n.max(m)));
        }
    }
    newest
}

/// Tracks worktrees with an in-flight background warm, so [`warm`] never spawns
/// two `direnv` invocations for the same worktree concurrently.
fn in_flight() -> &'static Mutex<HashSet<PathBuf>> {
    static S: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Kick a background `direnv` warm for `worktree` if its cache is cold/stale.
/// Returns immediately — **never blocks the caller** (a cold flake build can
/// take seconds). No-op when: there's no flake-backed `.envrc`, the cache is
/// already fresh, `direnv` isn't on PATH, or a warm for this worktree is
/// already running.
///
/// `allow` controls the trust step: `true` runs `direnv allow` first (the repo
/// owns its `.envrc`, the same trust boundary `inject_devshell` already
/// crosses by running the flake on the host); `false` only warms a worktree the
/// user has already `direnv allow`-ed — a blocked dir warms nothing.
pub fn warm(worktree: &Path, allow: bool) {
    if !needs_warm(worktree) || !crate::util::have("direnv") {
        return;
    }
    let wt = worktree.to_path_buf();
    {
        let mut set = in_flight().lock().unwrap();
        if !set.insert(wt.clone()) {
            return; // a warm for this worktree is already in flight
        }
    }
    std::thread::spawn(move || {
        warm_blocking(&wt, allow);
        in_flight().lock().unwrap().remove(&wt);
    });
}

/// Run the `direnv` warm synchronously. Subprocess seam: excluded from coverage
/// (see the justfile `cov_ignore`), exercised by smoke.
fn warm_blocking(worktree: &Path, allow: bool) {
    if allow {
        // Trust the repo's own `.envrc`. Ignore failure: an old direnv without
        // an explicit-path `allow` still gets warmed by the `exec` below when
        // already allowed.
        let _ = std::process::Command::new("direnv")
            .arg("allow")
            .arg(worktree)
            .output();
    }
    // `direnv exec DIR true` loads DIR's `.envrc` and runs `true`, building +
    // caching the flake devShell via the host's writable store + daemon. On a
    // blocked (un-allowed) dir it fails and warms nothing — the `allowed-only`
    // contract. `DIRENV_LOG_FORMAT=""` silences the per-line export noise.
    let _ = std::process::Command::new("direnv")
        .arg("exec")
        .arg(worktree)
        .arg("true")
        .env("DIRENV_LOG_FORMAT", "")
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sj-direnv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_warm_without_envrc_or_flake() {
        let dir = tmp("none");
        // Empty dir: nothing to warm.
        assert!(!needs_warm(&dir));
        // `.envrc` alone (plain direnv, no flake) re-evals harmlessly in-sandbox.
        std::fs::write(dir.join(".envrc"), "export FOO=bar\n").unwrap();
        assert!(!needs_warm(&dir));
        // flake.nix alone, no `.envrc` ⇒ direnv never fires.
        let dir2 = tmp("flake-only");
        std::fs::write(dir2.join("flake.nix"), "{}").unwrap();
        assert!(!needs_warm(&dir2));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn warm_when_flake_envrc_present_and_cache_cold() {
        let dir = tmp("cold");
        std::fs::write(dir.join(".envrc"), "use flake\n").unwrap();
        std::fs::write(dir.join("flake.nix"), "{ outputs = _: {}; }").unwrap();
        // No `.direnv` cache yet ⇒ cold ⇒ needs warming.
        assert!(needs_warm(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_cache_skips_warm_stale_cache_rewarms() {
        // Pure freshness logic, exercised without relying on filesystem mtime
        // granularity (which can collapse writes to the same instant).
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_secs(10);
        // Cache newer-or-equal than newest input ⇒ fresh (no rebuild needed).
        assert!(cache_is_fresh(t0, Some(t1)));
        assert!(cache_is_fresh(t0, Some(t0)));
        // Cache older than an input (e.g. a flake.lock bump) ⇒ stale.
        assert!(!cache_is_fresh(t1, Some(t0)));
        // No cache at all ⇒ stale (cold).
        assert!(!cache_is_fresh(t0, None));
    }
}
