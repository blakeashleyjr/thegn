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
use std::time::{Duration, Instant, SystemTime};

/// How long a synchronous [`warm_now`] blocks waiting for a cold flake devShell
/// to build/link before giving up and letting the async warm finish for the
/// next launch. The host store is usually already realized, so real waits are a
/// few seconds; this bound just caps a pathological cold build.
pub const WARM_NOW_TIMEOUT: Duration = Duration::from_secs(20);

/// Does this worktree have a flake-backed `.envrc` (`use flake` → `nix-direnv`)?
/// Only that combination hits the read-only `/nix/store` in-sandbox: a plain
/// `.envrc` re-evals harmlessly, and a repo without `.envrc`/flake has nothing
/// to warm. Pure filesystem stat, so it's cheap enough to gate both the warm
/// and the daemon-backstop mount ([`crate::sandbox`]); unit-tested.
pub fn has_flake_envrc(worktree: &Path) -> bool {
    worktree.join(".envrc").is_file() && worktree.join("flake.nix").is_file()
}

/// Does this worktree have a flake-backed `.envrc` whose `nix-direnv` cache is
/// cold or stale — i.e. would the in-sandbox direnv try (and fail) to rebuild
/// it against the read-only store? Pure filesystem stat (no subprocess), so
/// it's cheap enough to gate the spawn path and is unit-tested.
pub fn needs_warm(worktree: &Path) -> bool {
    if !has_flake_envrc(worktree) {
        return false;
    }
    let Some(newest_input) = newest_input_mtime(worktree) else {
        return false;
    };
    !cache_is_fresh(newest_input, newest_cache_rc_mtime(worktree))
}

/// Newest mtime among the flake inputs (`.envrc`, `flake.nix`, `flake.lock`) —
/// the watched files whose bump `nix-direnv` compares its cache against. `None`
/// when none exist.
fn newest_input_mtime(worktree: &Path) -> Option<SystemTime> {
    [".envrc", "flake.nix", "flake.lock"]
        .iter()
        .filter_map(|f| mtime(&worktree.join(f)))
        .max()
}

/// Is the cached env dump new enough to replay without a rebuild? Pure — split
/// out from [`needs_warm`] so the mtime comparison is unit-tested without
/// depending on filesystem mtime granularity. A cold cache (`None`) is stale.
fn cache_is_fresh(newest_input: SystemTime, cache_rc: Option<SystemTime>) -> bool {
    matches!(cache_rc, Some(rc) if rc >= newest_input)
}

/// After a warm, does the cache need its mtime bumped to look fresh? True when
/// a `.rc` exists but is *older* than the newest input — i.e. a checkout bumped
/// `.envrc`/`flake.nix`/`flake.lock` mtimes *after* (or racing) the build, so
/// `nix-direnv`'s live mtime check would wrongly re-eval. Pure, so the decision
/// is unit-tested without depending on filesystem mtime granularity. A cold
/// cache (`None`) is never blessed — there's nothing to touch.
fn cache_needs_bless(newest_input: SystemTime, cache_rc: Option<SystemTime>) -> bool {
    matches!(cache_rc, Some(rc) if rc < newest_input)
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

/// After a successful warm, bump every `.direnv/*.rc` mtime to *now* if the
/// newest input is newer than the newest cache `.rc` — closing the race where a
/// worktree materialize's `git checkout`/`reset` bumps `.envrc`/`flake.nix`/
/// `flake.lock` mtimes *after* (or concurrently with) the build, leaving the
/// just-built cache looking stale so the in-sandbox `nix-direnv` re-evals and
/// dies on the read-only `/nix/store`.
///
/// Sound because `nix-direnv`'s staleness check is a *live* comparison (profile
/// store path exists + `profile_rc` exists + no watched input newer than
/// `profile_rc`): the store path was just realized by the warm, and the
/// mtime-bumping checkout wrote byte-identical inputs (same commit), so only the
/// mtime ordering — not the cache content — is wrong. Best-effort: a touch
/// failure just leaves today's fallback for that launch.
fn bless_cache_fresh(worktree: &Path) {
    let Some(newest_input) = newest_input_mtime(worktree) else {
        return;
    };
    if !cache_needs_bless(newest_input, newest_cache_rc_mtime(worktree)) {
        return;
    }
    let now = SystemTime::now();
    let Ok(entries) = std::fs::read_dir(worktree.join(".direnv")) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("rc")
            && let Ok(f) = std::fs::File::options().write(true).open(&p)
        {
            // best-effort: a failed touch just leaves this launch on fallback.
            let _ = f.set_modified(now);
        }
    }
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

/// Synchronously warm `worktree`'s `direnv` cache, bounded by `timeout`, and
/// report whether a fresh `.direnv/*.rc` now exists (so the in-sandbox direnv
/// replays it read-only instead of failing against the read-only `/nix/store`).
///
/// **Never call on the event loop** — a cold flake build blocks for seconds.
/// This is for the guaranteed-off-loop pane-materialize path (spawn_blocking).
///
/// Coordinates with the async [`warm`]: if a warm for this worktree is already
/// in flight (startup pre-warm, a prior launch, a sibling prewarm), it WAITS for
/// that one rather than double-spawning `direnv`. On timeout it returns `false`
/// and leaves the owning thread running — the cache warms for the next launch,
/// exactly today's fallback behavior.
///
/// Subprocess/thread orchestration seam: excluded from coverage (justfile
/// `cov_ignore`); the pure decision logic lives in [`warm_now_plan`] /
/// [`cache_is_fresh`], which are unit-tested.
pub fn warm_now(worktree: &Path, allow: bool, timeout: Duration) -> bool {
    // Nothing flake-backed to warm (or already fresh) ⇒ the pane replays as-is.
    if !needs_warm(worktree) {
        return true;
    }
    if !crate::util::have("direnv") {
        return false;
    }
    let wt = worktree.to_path_buf();
    // Ensure exactly one warm is in flight: ours, or an async one already
    // running for this worktree. If we win the slot, drive the (blocking) warm
    // on a helper thread so `timeout` actually caps our wait even when the
    // subprocess overruns it.
    let we_own = in_flight().lock().unwrap().insert(wt.clone());
    if we_own {
        let wt2 = wt.clone();
        std::thread::spawn(move || {
            warm_blocking(&wt2, allow);
            in_flight().lock().unwrap().remove(&wt2);
        });
    }
    // Poll the pure, cheap freshness stat until the rc lands or we time out.
    let deadline = Instant::now() + timeout;
    loop {
        if !needs_warm(&wt) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Pure config→action mapping for a synchronous warm, split out so callers can
/// be unit-tested without a subprocess. `None` ⇒ warming disabled (leave the
/// pane on today's fallback); `Some(allow)` ⇒ warm, running `direnv allow`
/// first iff `allow`.
pub fn warm_now_plan(mode: crate::config::WarmDirenv) -> Option<bool> {
    use crate::config::WarmDirenv;
    match mode {
        WarmDirenv::Off => None,
        WarmDirenv::AllowedOnly => Some(false),
        WarmDirenv::Auto => Some(true),
    }
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
    // Guard the mtime race: a materialize `git checkout`/`reset` may have bumped
    // the flake inputs' mtimes past the freshly-built `.rc`, which would make the
    // in-sandbox `nix-direnv` re-eval and fail on the read-only store.
    bless_cache_fresh(worktree);
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
    fn warm_now_plan_maps_config_to_action() {
        use crate::config::WarmDirenv;
        assert_eq!(warm_now_plan(WarmDirenv::Off), None);
        assert_eq!(warm_now_plan(WarmDirenv::AllowedOnly), Some(false));
        assert_eq!(warm_now_plan(WarmDirenv::Auto), Some(true));
    }

    #[test]
    fn in_flight_dedupes_concurrent_warms() {
        // The wait-don't-double-warm contract: a second insert for the same
        // worktree returns false, so `warm_now` waits on the in-flight warm
        // instead of spawning a second `direnv`.
        let wt = tmp("in-flight").join("wt");
        assert!(in_flight().lock().unwrap().insert(wt.clone()));
        assert!(!in_flight().lock().unwrap().insert(wt.clone()));
        in_flight().lock().unwrap().remove(&wt);
        assert!(in_flight().lock().unwrap().insert(wt.clone()));
        in_flight().lock().unwrap().remove(&wt);
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

    #[test]
    fn has_flake_envrc_requires_both_files() {
        let dir = tmp("flake-detect");
        assert!(!has_flake_envrc(&dir));
        std::fs::write(dir.join(".envrc"), "use flake\n").unwrap();
        assert!(!has_flake_envrc(&dir)); // `.envrc` alone ⇒ no flake
        std::fs::write(dir.join("flake.nix"), "{ outputs = _: {}; }").unwrap();
        assert!(has_flake_envrc(&dir)); // both present ⇒ flake-backed
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_needs_bless_only_when_rc_older_than_input() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_secs(10);
        // A checkout bumped inputs past the built `.rc` ⇒ bless (touch) it fresh.
        assert!(cache_needs_bless(t1, Some(t0)));
        // Cache already newer-or-equal ⇒ nothing to do.
        assert!(!cache_needs_bless(t0, Some(t1)));
        assert!(!cache_needs_bless(t0, Some(t0)));
        // No cache at all ⇒ nothing to touch (cold ⇒ warm builds it).
        assert!(!cache_needs_bless(t0, None));
    }

    #[test]
    fn bless_cache_fresh_touches_stale_rc_so_needs_warm_clears() {
        let dir = tmp("bless");
        std::fs::write(dir.join(".envrc"), "use flake\n").unwrap();
        std::fs::write(dir.join("flake.nix"), "{ outputs = _: {}; }").unwrap();
        // A built cache whose `.rc` predates a later input bump (the race).
        std::fs::create_dir_all(dir.join(".direnv")).unwrap();
        let rc = dir.join(".direnv").join("flake-profile-x.rc");
        std::fs::write(&rc, "export FOO=bar\n").unwrap();
        // Force the `.rc` mtime behind the inputs so nix-direnv would re-eval.
        std::fs::File::options()
            .write(true)
            .open(&rc)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(needs_warm(&dir), "stale-by-mtime cache should look cold");
        bless_cache_fresh(&dir);
        assert!(
            !needs_warm(&dir),
            "blessing the `.rc` mtime should make the cache look fresh"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
