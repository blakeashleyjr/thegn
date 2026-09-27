//! The background `du` size scan behind the sidebar size badges, the bottom-bar
//! `disk` chip, and the statusbar's disk-warning rollup. Was
//! `hydrate::spawn_disk_scan`.

use std::sync::atomic::AtomicBool;

use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;
use thegn_core::scan_sched;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

use super::LOG;

static INFLIGHT: AtomicBool = AtomicBool::new(false);

/// Background per-worktree disk scan.
///
/// Ordered by [`scan_sched::plan`], so the ACTIVE worktree and any
/// never-measured one are `du`d before every stale multi-GB one. That ordering
/// is the whole point: the previous version walked the registry in
/// `ORDER BY position, created_at`, which measured a brand-new worktree **last**
/// — the reported "sizes take forever to show up on a new worktree".
///
/// Bounded to `max_scan_per_round`, one round at a time, and never on the loop.
pub(crate) fn spawn_scan(
    cfg: thegn_core::config::DiskConfig,
    policy: thegn_core::disk_reclaim::Policy,
    active: Option<std::path::PathBuf>,
    waker: Option<TerminalWaker>,
) {
    tokio::task::spawn_blocking(move || {
        let Some(_round) = super::begin(&INFLIGHT, "disk") else {
            return;
        };
        let Some(_permit) = super::permit("disk") else {
            return;
        };
        let Ok(db) = Db::open() else {
            return;
        };

        // The orphan sweep runs BEFORE the `show_sizes` gate. A row for a
        // removed worktree is never re-measured by the loop below and would
        // otherwise inflate the statusbar total forever — and turning badges off
        // is not a reason to stop reclaiming them.
        let reaped = sweep_orphans(&db);

        // `show_sizes` hides the badges; it is not a reason to stop reclaiming
        // disk. The round still runs when a reclaim rule is on, because the
        // reclaim decision is made from the very measurements this round takes
        // (see `reclaim`). With badges off AND both rules off there is nothing
        // to do beyond the orphan sweep above.
        let reclaim_on = policy.idle_days > 0 || policy.on_low_disk;
        if !cfg.show_sizes && !reclaim_on {
            if reaped > 0
                && let Some(w) = &waker
            {
                let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
            return;
        }

        let stamps = db.all_worktree_disk_stamps().unwrap_or_default();
        let active = super::active_key(active.as_deref());
        let targets = super::targets(&db, &stamps, active.as_deref());
        let due = scan_sched::plan(
            &targets,
            thegn_core::util::now(),
            cfg.scan_interval_secs,
            cfg.max_scan_per_round as usize,
        );
        tracing::debug!(
            target: LOG,
            scan = "disk",
            known = targets.len(),
            due = due.len(),
            reaped,
            "planned round"
        );

        let mut measured = 0u32;
        // Freshly measured this round, for the reclaim pass below.
        let mut fresh: Vec<(String, thegn_core::disk::DiskUsage)> = Vec::new();
        for path_s in &due {
            let path = std::path::Path::new(path_s);
            if !path.is_dir() {
                // Vanished since the registry row was written — drop any stale
                // size so the badge clears instead of freezing at its last value.
                let _ = db.delete_worktree_disk(path_s); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                measured += 1;
                continue;
            }
            let usage = thegn_core::disk::measure_worktree(path);
            // A repo root's `du` already includes any worktree checked out
            // beneath it (`worktree_mode = "in_repo"` puts them at
            // `<root>/.worktrees/<slug>`), so subtract the children we have
            // already measured or the same bytes get counted twice.
            //
            // Re-read per target rather than once per round: the planner orders
            // by staleness, not by nesting, so a child measured earlier in THIS
            // round must already be visible when its parent is folded. (A child
            // not yet measured at all simply isn't subtracted — the root reads
            // high for one round and self-corrects on the next.) An indexed
            // scan of a table this small is free next to the `du` above.
            let known = cached_sizes(&db);
            let known: Vec<(&std::path::Path, u64)> =
                known.iter().map(|(p, b)| (p.as_path(), *b)).collect();
            let total = thegn_core::disk::net_root_bytes(path, usage.total_bytes, &known);
            let _ = db.put_worktree_disk(path_s, total as i64, usage.target_bytes as i64); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            fresh.push((path_s.clone(), usage));
            measured += 1;
        }

        let reclaimed = reclaim(&db, &policy, &fresh, active.as_deref());
        let awaiting = db.worktrees_with_active_dispatch().unwrap_or_default();
        let generation_reclaimed = reap_generation_footprints(
            &fresh,
            active.as_deref(),
            &awaiting,
            cfg.generation_min_age_days,
            thegn_core::util::now().max(0) as u64,
        );

        if (measured > 0 || reaped > 0 || reclaimed > 0 || generation_reclaimed > 0)
            && let Some(w) = &waker
        {
            let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Every cached total, as owned paths for [`thegn_core::disk::net_root_bytes`].
fn cached_sizes(db: &Db) -> Vec<(std::path::PathBuf, u64)> {
    // This table contains registered worktrees only. Submodule directories are
    // deliberately never inserted as synthetic rows, so their physical bytes
    // remain inside the owning worktree and are counted exactly once.
    db.all_worktree_disk()
        .unwrap_or_default()
        .into_iter()
        .map(|(p, (total, _))| (std::path::PathBuf::from(p), total.max(0) as u64))
        .collect()
}

/// Delete size-cache rows whose worktree has left the registry. Returns how many.
fn sweep_orphans(db: &Db) -> usize {
    let Ok(cached) = db.all_worktree_disk() else {
        return 0;
    };
    let live = super::live_paths(db);
    let gone = scan_sched::orphans(
        cached.keys().map(String::as_str),
        live.iter().map(String::as_str),
    );
    for path in &gone {
        let _ = db.delete_worktree_disk(path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
    gone.len()
}

/// Idle / low-disk `target/` reclaim, run at the tail of a size-scan round.
///
/// Only the worktrees measured **this round** are considered, because
/// `newest_mtime` comes out of the walk that just happened and is not cached in
/// the DB (a size row carries bytes, not mtimes). That is a deliberate
/// simplification, and it degrades well: the scheduler sweeps every worktree
/// inside a couple of `[disk] scan_interval_secs` windows, and the low-disk rule
/// re-reads free space each round — so it is a control loop that converges on
/// the free-space target rather than a single global LRU sort. It also means no
/// schema change, and no reclaim decision is ever made from stale measurements.
///
/// Returns the bytes reclaimed. Best-effort throughout: this runs on the
/// background lane and must never take down a scan round.
fn reclaim(
    db: &Db,
    policy: &thegn_core::disk_reclaim::Policy,
    fresh: &[(String, thegn_core::disk::DiskUsage)],
    active: Option<&str>,
) -> u64 {
    use thegn_core::disk_reclaim as rc;

    if fresh.is_empty() || (policy.idle_days == 0 && !policy.on_low_disk) {
        return 0;
    }
    let now = thegn_core::util::now().max(0) as u64;
    let idle_threshold = rc::idle_threshold_secs(policy);

    // Worktrees whose pipeline work nobody has closed yet. Reclaiming one costs
    // the next stage a cold rebuild of work that is still mid-flight, so they
    // are exempt from both rules. One query for the whole round.
    // best-effort: a roster read failure must not disable the whole reclaim —
    // it only means this round loses the exemption, so fall back to "none".
    let awaiting = db.worktrees_with_active_dispatch().unwrap_or_default();

    let candidates: Vec<rc::Candidate> = fresh
        .iter()
        .map(|(path, usage)| {
            let p = std::path::Path::new(path);
            let idle_secs = now.saturating_sub(usage.newest_mtime);
            // `git status` is the only per-candidate subprocess here, so it is
            // deferred to the few that could possibly be picked by the idle rule
            // (the pressure rule ignores dirtiness anyway).
            let dirty = idle_threshold.is_some_and(|t| idle_secs >= t)
                && usage.target_bytes >= rc::MIN_RECLAIM_BYTES
                && thegn_core::util::git_out(p, &["status", "--porcelain"])
                    .is_none_or(|out| !out.trim().is_empty());
            rc::Candidate {
                path: path.clone(),
                target_bytes: usage.target_bytes,
                idle_secs,
                active: active == Some(path.as_str()),
                building: crate::task::slot_active(p),
                dirty,
                awaiting_verification: awaiting.iter().any(|w| w == path),
                reclaimed_secs_ago: last_reclaim_secs_ago(db, path, now),
            }
        })
        .collect();

    // Free space on the filesystem the worktrees live on. Absent (non-unix, or
    // a statvfs error) simply disables the pressure rule for this round.
    let pressure = fresh
        .first()
        .and_then(|(path, _)| thegn_metrics::disk_space(std::path::Path::new(path)))
        .map(|(total_bytes, free_bytes, free_pct)| rc::Pressure {
            free_pct,
            total_bytes,
            free_bytes,
        });

    let plan = rc::plan(&candidates, policy, pressure);
    let mut total = 0u64;
    for item in &plan {
        let path = std::path::Path::new(&item.path);
        match thegn_core::worktree::clean_target(path) {
            Ok(bytes) if bytes > 0 => {
                total += bytes;
                // best-effort: the size cache is a cache; a failed delete just
                // means the badge shows a stale number until the next round.
                let _ = db.delete_worktree_disk(&item.path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                // Stamp the reclaim so the cooldown can see it next round. Without
                // this the pressure rule re-picks the same worktree as soon as a
                // build repopulates `target/` — delete, rebuild, delete.
                // best-effort: losing the stamp costs hysteresis, never correctness.
                let _ = db.set_ui_state(RECLAIM_SCOPE, &item.path, &now.to_string()); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                let branch = branch_of(db, &item.path);
                let msg = format!(
                    "target/ reclaimed ({} — {})",
                    thegn_core::disk::human(bytes),
                    item.reason.note()
                );
                // The marker the next attach reads: a cold rebuild is coming,
                // and this says why rather than looking like a broken cache.
                // best-effort: the reclaim already happened and is logged; a
                // failed insert must not abort the rest of the round.
                let _ =
                    crate::automation_events::emit(db, "disk_cleaned", &branch, &msg, &item.path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                tracing::info!(
                    target: LOG,
                    worktree = %item.path,
                    bytes,
                    reason = %item.reason.note(),
                    "reclaimed target/"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(target: LOG, worktree = %item.path, error = %e, "reclaim failed");
            }
        }
    }
    total
}

#[derive(Debug)]
struct GenerationFootprint {
    package_name: String,
    generation_id: String,
    fingerprint: std::path::PathBuf,
    deps: Vec<std::path::PathBuf>,
    incremental: Vec<std::path::PathBuf>,
    newest_mtime_secs: u64,
    bytes: u64,
}

/// Prune only generation records in profiles measured during this scan. The
/// lock is held from before inventory through the final unlink so Cargo cannot
/// begin a profile build midway through pruning.
fn reap_generation_footprints(
    fresh: &[(String, thegn_core::disk::DiskUsage)],
    active: Option<&str>,
    awaiting_verification: &[String],
    min_age_days: u32,
    now_secs: u64,
) -> u64 {
    use std::fs::OpenOptions;
    use thegn_core::disk_reclaim::{Generation, GenerationPolicy};

    let mut all_removed = 0u64;
    for (worktree, _) in fresh {
        let worktree_path = std::path::Path::new(worktree);
        // `git_out` collapses EMPTY stdout to `None`, and an empty
        // `status --porcelain` is precisely "clean" — the only state we may
        // prune in. Using it here made this guard skip every clean worktree, so
        // the reclaim never ran at all. `git_out_allow_empty` keeps "clean"
        // (`Some("")`) distinct from "git failed" (`None`), and a failed status
        // read must still refuse.
        if active == Some(worktree.as_str())
            || awaiting_verification.iter().any(|path| path == worktree)
            || crate::task::slot_active(worktree_path)
            || thegn_core::util::git_out_allow_empty(worktree_path, &["status", "--porcelain"])
                .is_none_or(|output| !output.trim().is_empty())
        {
            continue;
        }
        let target = worktree_path.join("target");
        for profile in ["debug", "release"] {
            let profile_dir = target.join(profile);
            let lock_path = profile_dir.join(".cargo-lock");
            // Do not create a missing Cargo lock file. No existing lock means
            // this profile is not a Cargo profile we can safely synchronize.
            if !safe_regular_file(&lock_path) || !safe_directory(&profile_dir) {
                continue;
            }
            let Ok(lock) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
                continue;
            };
            // `File::try_lock` returns `Ok(())` only after acquiring the lock;
            // both contention and lock errors skip this profile.
            if !matches!(lock.try_lock(), Ok(())) {
                tracing::debug!(
                    target: LOG,
                    profile = %profile_dir.display(),
                    planned_bytes = 0u64,
                    removed_bytes = 0u64,
                    failures = 1usize,
                    "generation prune skipped: cargo lock unavailable"
                );
                continue;
            }

            let footprints = match generation_inventory(&profile_dir) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        target: LOG,
                        profile = %profile_dir.display(),
                        error = %error,
                        planned_bytes = 0u64,
                        removed_bytes = 0u64,
                        failures = 1usize,
                        "generation inventory failed"
                    );
                    continue;
                }
            };
            let inventory: Vec<_> = footprints
                .iter()
                .map(|footprint| Generation {
                    package_name: footprint.package_name.clone(),
                    generation_id: footprint.generation_id.clone(),
                    newest_mtime_secs: footprint.newest_mtime_secs,
                    bytes: footprint.bytes,
                })
                .collect();
            let before_bytes = inventory.iter().map(|entry| entry.bytes).sum::<u64>();
            let plan = thegn_core::disk_reclaim::plan_generations(
                &inventory,
                GenerationPolicy {
                    min_age_secs: u64::from(min_age_days).saturating_mul(86_400),
                },
                now_secs,
            );
            let mut planned_bytes = 0u64;
            let mut removed_bytes = 0u64;
            let mut failures = 0usize;
            for eviction in plan {
                let Some(footprint) = footprints.iter().find(|candidate| {
                    candidate.package_name == eviction.package_name
                        && candidate.generation_id == eviction.generation_id
                }) else {
                    failures += 1;
                    continue;
                };
                planned_bytes = planned_bytes.saturating_add(footprint.bytes);
                // Cargo treats a missing fingerprint as stale. Delete it
                // before artifacts: if interrupted, leftovers are rebuildable;
                // deleting outputs first while freshness metadata survives is
                // the unsafe ordering.
                if remove_tree_counted(&footprint.fingerprint, &profile_dir, &mut removed_bytes)
                    .is_err()
                {
                    failures += 1;
                    continue;
                }
                for path in footprint.deps.iter().chain(&footprint.incremental) {
                    if remove_tree_counted(path, &profile_dir, &mut removed_bytes).is_err() {
                        failures += 1;
                    }
                }
            }
            // `planned_bytes` is the measured complete selection; `removed_bytes`
            // counts only successful files actually unlinked. Keep the two
            // metrics distinct when permission or I/O errors cause partial work.
            tracing::info!(
                target: LOG,
                profile = %profile_dir.display(),
                before_bytes,
                planned_bytes,
                removed_bytes,
                after_bytes = before_bytes.saturating_sub(removed_bytes),
                failures,
                "Cargo generation prune"
            );
            all_removed = all_removed.saturating_add(removed_bytes);
            drop(lock);
        }
    }
    all_removed
}

/// Inventory `<profile>/.fingerprint/<package>-<id>` records and associate only
/// Cargo outputs for that package and opaque id. Cargo library outputs start
/// with `lib`, while dep-info files and executables do not. Package/target
/// separators admit target names without confusing `foo` with `foobar`.
/// Any unreadable or symlinked path rejects the whole profile/candidate rather
/// than guessing.
fn generation_inventory(profile: &std::path::Path) -> std::io::Result<Vec<GenerationFootprint>> {
    use std::fs;

    fn dirs(path: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(std::io::Error::other("symlink in generation profile"));
            }
            if ty.is_dir() {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }

    let fingerprint_root = profile.join(".fingerprint");
    let deps_root = profile.join("deps");
    let incremental_root = profile.join("incremental");
    if !safe_directory(profile)
        || !safe_directory(&fingerprint_root)
        || (deps_root.exists() && !safe_directory(&deps_root))
        || (incremental_root.exists() && !safe_directory(&incremental_root))
    {
        return Err(std::io::Error::other("unsafe generation profile directory"));
    }
    let fingerprints = dirs(&fingerprint_root)?;
    let deps = if deps_root.is_dir() {
        fs::read_dir(&deps_root)?.collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let incremental = if incremental_root.is_dir() {
        dirs(&incremental_root)?
    } else {
        Vec::new()
    };
    let mut result = Vec::new();
    for fingerprint in fingerprints {
        let Some(dirname) = fingerprint.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((package_name, generation_id)) = dirname.rsplit_once('-') else {
            continue;
        };
        if package_name.is_empty() || generation_id.is_empty() {
            continue;
        }
        let marker = format!("-{generation_id}");
        let mut dep_paths = Vec::new();
        for entry in &deps {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let stem = name.split_once('.').map_or(name, |(stem, _)| stem);
            if !artifact_matches_generation(stem, package_name, &marker) {
                continue;
            }
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(std::io::Error::other("symlink in generation deps"));
            }
            if ty.is_file() {
                dep_paths.push(entry.path());
            }
        }
        let incremental_paths: Vec<_> = incremental
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| artifact_matches_generation(name, package_name, &marker))
            })
            .cloned()
            .collect();
        let paths = std::iter::once(&fingerprint)
            .chain(dep_paths.iter())
            .chain(incremental_paths.iter());
        let mut bytes = 0u64;
        let mut newest = 0u64;
        for path in paths {
            let (path_bytes, path_newest) = measure_safe_tree(path, profile)?;
            bytes = bytes.saturating_add(path_bytes);
            newest = newest.max(path_newest);
        }
        result.push(GenerationFootprint {
            package_name: package_name.to_string(),
            generation_id: generation_id.to_string(),
            fingerprint,
            deps: dep_paths,
            incremental: incremental_paths,
            newest_mtime_secs: newest,
            bytes,
        });
    }
    Ok(result)
}

/// Match `<package>-<id>` and `<package>-<target>-<id>` Cargo output names,
/// accepting the `lib` prefix used by Rust library artifacts.
fn artifact_matches_generation(stem: &str, package_name: &str, marker: &str) -> bool {
    let normalized_package = package_name.replace('-', "_");
    let matches_name = |name: &str, package: &str| {
        name.strip_suffix(marker).is_some_and(|base| {
            base == package
                || base
                    .strip_prefix(package)
                    .is_some_and(|suffix| suffix.starts_with('-') || suffix.starts_with('_'))
        })
    };
    let matches_package =
        |name: &str| matches_name(name, package_name) || matches_name(name, &normalized_package);
    matches_package(stem) || stem.strip_prefix("lib").is_some_and(matches_package)
}

fn measure_safe_tree(
    path: &std::path::Path,
    profile: &std::path::Path,
) -> std::io::Result<(u64, u64)> {
    use std::fs;
    let root = profile.canonicalize()?;
    let mut bytes = 0u64;
    let mut newest = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let meta = fs::symlink_metadata(&current)?;
        if meta.file_type().is_symlink() || !current.canonicalize()?.starts_with(&root) {
            return Err(std::io::Error::other("unsafe generation footprint path"));
        }
        if meta.is_dir() {
            for entry in fs::read_dir(&current)? {
                stack.push(entry?.path());
            }
        } else if meta.is_file() {
            bytes = bytes.saturating_add(meta.len());
            if let Ok(time) = meta.modified()
                && let Ok(time) = time.duration_since(std::time::UNIX_EPOCH)
            {
                newest = newest.max(time.as_secs());
            }
        }
    }
    Ok((bytes, newest))
}

fn safe_directory(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
}

fn safe_regular_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
}

/// Remove a tree without following links and add only successfully unlinked
/// file bytes to `removed`.
fn remove_tree_counted(
    path: &std::path::Path,
    profile: &std::path::Path,
    removed: &mut u64,
) -> std::io::Result<()> {
    use std::fs;
    let root = profile.canonicalize()?;
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !path.canonicalize()?.starts_with(&root) {
        return Err(std::io::Error::other("unsafe generation removal path"));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            remove_tree_counted(&entry.path(), profile, removed)?;
        }
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
        *removed = (*removed).saturating_add(meta.len());
    }
    Ok(())
}

/// `ui_state` scope holding the last-reclaim timestamp per worktree. A
/// `ui_state` row rather than a schema column: it is pure hysteresis bookkeeping
/// that may be lost without consequence, so it does not earn a migration.
const RECLAIM_SCOPE: &str = "disk_reclaim_at";

/// Seconds since thegn last reclaimed this worktree, or `None` if it never has
/// (or the stamp is unreadable/corrupt — in which case the worktree is simply
/// treated as never reclaimed, the pre-hysteresis behaviour).
fn last_reclaim_secs_ago(db: &Db, path: &str, now: u64) -> Option<u64> {
    let raw = db.get_ui_state(RECLAIM_SCOPE, path).ok()??;
    let then: u64 = raw.trim().parse().ok()?;
    Some(now.saturating_sub(then))
}

/// Branch label for the reclaim notification; empty when the path is not a
/// registered worktree (a workspace main checkout, say).
fn branch_of(db: &Db, path: &str) -> String {
    db.worktrees()
        .unwrap_or_default()
        .into_iter()
        .find(|w| w.worktree == path)
        .map(|w| w.branch)
        .unwrap_or_default()
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use std::fs;

    #[test]
    fn artifact_names_match_cargo_library_dep_info_and_executable_outputs() {
        assert!(artifact_matches_generation("libpkg-a", "pkg", "-a"));
        assert!(artifact_matches_generation("pkg-a", "pkg", "-a"));
        assert!(artifact_matches_generation("pkg_cli-a", "pkg", "-a"));
        assert!(artifact_matches_generation("libmy_pkg-a", "my-pkg", "-a"));
        assert!(!artifact_matches_generation("libpkgx-a", "pkg", "-a"));
        assert!(!artifact_matches_generation("libpkg-b", "pkg", "-a"));
    }

    fn profile_tree() -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "thegn-generation-reclaim-{}-{}-{}",
            std::process::id(),
            thegn_core::util::now(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = root.join("target/debug");
        fs::create_dir_all(profile.join(".fingerprint/pkg-a")).unwrap();
        fs::create_dir_all(profile.join(".fingerprint/pkg-z")).unwrap();
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::create_dir_all(profile.join("incremental/pkg-a")).unwrap();
        fs::write(profile.join(".cargo-lock"), []).unwrap();
        fs::write(profile.join(".fingerprint/pkg-a/lib-pkg"), b"fingerprint-a").unwrap();
        fs::write(profile.join(".fingerprint/pkg-z/lib-pkg"), b"fingerprint-z").unwrap();
        fs::write(profile.join("deps/libpkg-a.rlib"), b"artifact-a").unwrap();
        fs::write(profile.join("deps/pkg-a.d"), b"dep-info-a").unwrap();
        fs::write(profile.join("deps/pkg-a"), b"executable-a").unwrap();
        fs::write(profile.join("deps/libpkg-z.rlib"), b"artifact-z").unwrap();
        fs::write(profile.join("deps/pkg-z.d"), b"dep-info-z").unwrap();
        fs::write(profile.join("deps/pkg-z"), b"executable-z").unwrap();
        // Prefix-only matching would incorrectly associate this artifact with
        // package `pkg` when pruning the generation whose ID is `a`.
        fs::write(profile.join("deps/libpkgx-a.rlib"), b"other-package").unwrap();
        fs::write(profile.join("incremental/pkg-a/cache"), b"incremental-a").unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "test fixture: a blocking wait on `git init` in a temp dir, off the event loop"
        )]
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed: {status}");
        fs::write(root.join(".git/info/exclude"), "target/\n").unwrap();
        (root, profile)
    }

    #[test]
    fn unlocked_prune_removes_only_selected_footprint_and_records_real_bytes() {
        let (root, profile) = profile_tree();
        let before = fs::read_dir(profile.join(".fingerprint")).unwrap().count();
        let usage = thegn_core::disk::DiskUsage::default();
        let removed = reap_generation_footprints(
            &[(root.to_string_lossy().into_owned(), usage)],
            None,
            &[],
            0,
            u64::MAX,
        );
        assert!(removed > 0);
        assert!(!profile.join(".fingerprint/pkg-a").exists());
        assert!(!profile.join("deps/libpkg-a.rlib").exists());
        assert!(!profile.join("deps/pkg-a.d").exists());
        assert!(!profile.join("deps/pkg-a").exists());
        assert!(!profile.join("incremental/pkg-a").exists());
        assert!(profile.join(".fingerprint/pkg-z").exists());
        assert!(profile.join("deps/libpkg-z.rlib").exists());
        assert!(profile.join("deps/pkg-z.d").exists());
        assert!(profile.join("deps/pkg-z").exists());
        assert!(profile.join("deps/libpkgx-a.rlib").exists());
        assert_eq!(before, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn held_profile_lock_skips_inventory_and_deletion() {
        let (root, profile) = profile_tree();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(profile.join(".cargo-lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let removed = reap_generation_footprints(
            &[(
                root.to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            None,
            &[],
            0,
            u64::MAX,
        );
        assert_eq!(removed, 0);
        assert!(profile.join(".fingerprint/pkg-a").exists());
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_worktree_is_exempt_from_generation_pruning() {
        let (root, profile) = profile_tree();
        fs::write(root.join("uncommitted.txt"), b"work in progress").unwrap();
        let removed = reap_generation_footprints(
            &[(
                root.to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            None,
            &[],
            0,
            u64::MAX,
        );
        assert_eq!(removed, 0);
        assert!(profile.join(".fingerprint/pkg-a").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_worktree_is_exempt_from_generation_pruning() {
        let (root, profile) = profile_tree();
        let worktree = root.to_string_lossy().into_owned();
        // `slot_active` consults only the in-process job registry; it does not
        // open the real thegn database or depend on XDG state.
        assert!(!crate::task::slot_active(&root));
        let removed = reap_generation_footprints(
            &[(worktree.clone(), thegn_core::disk::DiskUsage::default())],
            Some(&worktree),
            &[],
            0,
            u64::MAX,
        );
        assert_eq!(removed, 0);
        assert!(profile.join(".fingerprint/pkg-a").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_profile_inventory_does_not_suppress_a_different_profile() {
        let (root, debug_profile) = profile_tree();
        fs::remove_dir_all(debug_profile.join(".fingerprint")).unwrap();
        fs::write(debug_profile.join(".fingerprint"), b"not a directory").unwrap();

        let release = root.join("target/release");
        fs::create_dir_all(release.join(".fingerprint/pkg-a")).unwrap();
        fs::create_dir_all(release.join(".fingerprint/pkg-z")).unwrap();
        fs::create_dir_all(release.join("deps")).unwrap();
        fs::write(release.join(".cargo-lock"), []).unwrap();
        fs::write(release.join(".fingerprint/pkg-a/marker"), b"a").unwrap();
        fs::write(release.join(".fingerprint/pkg-z/marker"), b"z").unwrap();
        fs::write(release.join("deps/libpkg-a.rlib"), b"artifact-a").unwrap();
        fs::write(release.join("deps/libpkg-z.rlib"), b"artifact-z").unwrap();

        let removed = reap_generation_footprints(
            &[(
                root.to_string_lossy().into_owned(),
                thegn_core::disk::DiskUsage::default(),
            )],
            None,
            &[],
            0,
            u64::MAX,
        );
        assert!(removed > 0);
        assert!(debug_profile.join(".fingerprint").is_file());
        assert!(!release.join(".fingerprint/pkg-a").exists());
        assert!(release.join(".fingerprint/pkg-z").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
