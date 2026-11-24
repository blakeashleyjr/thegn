//! `thegn disk` / `thegn clean` — per-worktree disk-usage reporting and
//! reclaiming. `disk` scans and prints sizes (and refreshes the cache the live
//! UI reads); `clean` removes a worktree's regenerable `target/` while keeping
//! the checkout.

use anyhow::Result;
use serde::Serialize;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};
use thegn_core::{disk, disk_reclaim, outln, worktree};

use super::{confirm, resolve_worktree};

/// Cargo's per-profile build lock, at `target/<profile>/.cargo-lock` (or one
/// level deeper under a cross-compilation triple). Its presence is not evidence
/// of anything; whether it can be locked is.
const CARGO_LOCK: &str = ".cargo-lock";

/// One lane directory under `<checkout>/target/lanes`, measured and explained.
#[derive(Debug, Serialize)]
struct LaneRow {
    /// Directory name under `target/lanes/`.
    lane: String,
    path: String,
    /// Apparent bytes of the whole lane directory.
    bytes: u64,
    /// The regenerable build output inside it ([`lane_build_bytes`]).
    build_bytes: u64,
    /// `checkout`, `build-dir` or `other` ([`LaneKind`]).
    kind: &'static str,
    /// The lane's own git checkout, when it has one. `None` means unverifiable,
    /// never a guess from the enclosing worktree.
    owner: Option<String>,
    branch: Option<String>,
    /// `building`, `active`, `idle` or `unknown` ([`lane_activity`]).
    activity: &'static str,
    activity_evidence: String,
    /// `eligible`, `held` or `unknown`.
    eligibility: &'static str,
    reason: String,
}

/// The `--json` document: one object, totals in the same unit as the rows.
#[derive(Debug, Serialize)]
struct LaneReport {
    root: String,
    unit: &'static str,
    scan_duration_ms: u64,
    total_bytes: u64,
    build_bytes: u64,
    /// Build output of lanes classified `eligible` only.
    eligible_bytes: u64,
    lanes: Vec<LaneRow>,
}

/// What a lane directory is, from markers Cargo and git leave behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneKind {
    /// A copy of the repository: regenerable output is its `target/`.
    Checkout,
    /// A bare Cargo target dir (`CARGO_TARGET_DIR` pointed straight at it).
    BuildDir,
    /// Logs, patches, notes; not a build lane.
    Other,
}

impl LaneKind {
    fn label(self) -> &'static str {
        match self {
            LaneKind::Checkout => "checkout",
            LaneKind::BuildDir => "build-dir",
            LaneKind::Other => "other",
        }
    }
}

/// `thegn wt disk --lanes [--worktree P] [--json]` — read-only inventory of the
/// build lanes beneath one checkout's `target/lanes/`.
///
/// The audit (`docs/audits/2026-11-18-repository-hygiene.md`) found 129 GiB in
/// `target/lanes/` with no way to say which lane was large, whose it was, or
/// whether it could be reclaimed. Each immediate subdirectory is measured with
/// the same parallel walk — and therefore the same apparent-byte accounting —
/// as a worktree scan (`disk::measure_tree`), then classified from evidence the
/// directory itself carries: its own git checkout, Cargo's build lock, and the
/// newest write anywhere in the tree.
///
/// Nothing is deleted or rewritten, no `worktree_disk` cache row is touched, and
/// no other worktree is visited. The walk runs on this command's thread; it is
/// never on the host event loop.
pub fn lanes(worktree_arg: Option<String>, json: bool) -> Result<()> {
    let started = std::time::Instant::now();
    let root = resolve_worktree(worktree_arg).join("target").join("lanes");
    let now = u64::try_from(thegn_core::util::now()).unwrap_or(0);
    let lanes = lane_report(&root, now)?;

    let total_bytes = lanes.iter().map(|lane| lane.bytes).sum();
    let build_bytes = lanes.iter().map(|lane| lane.build_bytes).sum();
    let eligible_bytes = lanes
        .iter()
        .filter(|lane| lane.eligibility == "eligible")
        .map(|lane| lane.build_bytes)
        .sum();
    let report = LaneReport {
        root: root.to_string_lossy().into_owned(),
        unit: "apparent file bytes",
        scan_duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        total_bytes,
        build_bytes,
        eligible_bytes,
        lanes,
    };

    if json {
        return super::emit_json(&report);
    }
    if report.lanes.is_empty() {
        outln!("No build lanes under {}.", report.root);
        return Ok(());
    }
    outln!(
        "{:>10}  {:>10}  {:<9}  {:<11}  LANE",
        "BYTES",
        "BUILD",
        "ACTIVITY",
        "ELIGIBILITY"
    );
    for lane in &report.lanes {
        outln!(
            "{:>10}  {:>10}  {:<9}  {:<11}  {} — {}",
            disk::human(lane.bytes),
            disk::human(lane.build_bytes),
            lane.activity,
            lane.eligibility,
            lane_label(lane),
            lane.reason
        );
    }
    outln!(
        "{} lane(s): {} total, {} build output, {} eligible; scan {} ms (apparent file bytes)",
        report.lanes.len(),
        disk::human(report.total_bytes),
        disk::human(report.build_bytes),
        disk::human(report.eligible_bytes),
        report.scan_duration_ms
    );
    Ok(())
}

/// Measure and classify the immediate directories under `root`, largest first.
///
/// A missing `target/lanes/` is an empty inventory, not an error: most
/// checkouts never coordinate a build lane.
fn lane_report(root: &std::path::Path, now: u64) -> Result<Vec<LaneRow>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut lanes = Vec::new();
    for entry in entries {
        let entry = entry?;
        // Only real directories directly under the root: a symlinked lane could
        // point anywhere, and the walk would count nothing anyway.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            lanes.push(LaneRow {
                lane: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path().to_string_lossy().into_owned(),
                bytes: 0,
                build_bytes: 0,
                kind: LaneKind::Other.label(),
                owner: None,
                branch: None,
                activity: "unknown",
                activity_evidence: "symlink, not followed".to_string(),
                eligibility: "unknown",
                reason: "symlink, not followed; contents are not inspected".to_string(),
            });
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }

        let path = entry.path();
        let usage = disk::measure_tree(&path);
        let kind = lane_kind(&path);
        let (owner, branch) = lane_checkout(&path);
        let (activity, activity_evidence) = lane_activity(&path, kind, usage.newest_mtime, now);
        let idle_secs = now.saturating_sub(usage.newest_mtime);

        let (eligibility, reason) = if kind == LaneKind::Other {
            (
                "unknown",
                "not a build lane: no target/, Cargo.toml or Cargo target marker".to_string(),
            )
        } else if activity == "building" {
            (
                "held",
                format!("{activity_evidence}; a build may still own these artifacts"),
            )
        } else if activity == "active" {
            (
                "held",
                format!("{activity_evidence}; a build may be running"),
            )
        } else if owner.is_none() {
            (
                "unknown",
                "no git checkout to attribute the lane to; ownership unknown".to_string(),
            )
        } else if activity == "idle" {
            (
                "eligible",
                format!(
                    "verified checkout; nothing written for {} and no build lock held",
                    human_age(idle_secs)
                ),
            )
        } else {
            ("unknown", "no activity evidence; retained".to_string())
        };

        lanes.push(LaneRow {
            lane: entry.file_name().to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            bytes: usage.total_bytes,
            build_bytes: lane_build_bytes(kind, &usage),
            kind: kind.label(),
            owner,
            branch,
            activity,
            activity_evidence,
            eligibility,
            reason,
        });
    }
    lanes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.lane.cmp(&b.lane)));
    Ok(lanes)
}

/// The regenerable output inside a lane: the `target/` subtree of a checkout
/// copy, or the whole directory when the lane is itself a Cargo target dir.
/// Apparent bytes either way, so a lane and a worktree are the same unit.
fn lane_build_bytes(kind: LaneKind, usage: &disk::DiskUsage) -> u64 {
    match kind {
        LaneKind::Checkout => usage.target_bytes,
        LaneKind::BuildDir => usage.total_bytes,
        LaneKind::Other => 0,
    }
}

/// Classify a lane from markers Cargo leaves on disk. `target/` or `Cargo.toml`
/// makes it a checkout copy; a target dir pointed at directly has Cargo's own
/// `.rustc_info.json` (or a profile directory) at its root.
fn lane_kind(lane: &std::path::Path) -> LaneKind {
    if real_dir(&lane.join("target")) || real_file(&lane.join("Cargo.toml")) {
        LaneKind::Checkout
    } else if real_file(&lane.join(".rustc_info.json")) || real_dir(&lane.join("debug")) {
        LaneKind::BuildDir
    } else {
        LaneKind::Other
    }
}

/// `symlink_metadata` predicates: a symlinked `target/` must not be mistaken for
/// build output, because the size walk does not follow it.
fn real_dir(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

fn real_file(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// The checkout that owns a lane, plus its branch, read from the lane's own
/// `.git` — a directory for a clone, a `gitdir:` file for a linked worktree.
///
/// A plain scratch directory inside the enclosing checkout has no checkout of
/// its own, so its owner stays unknown instead of being attributed to the
/// worktree that happens to contain it: ownership has to be verifiable, because
/// it is what makes a lane reclaimable.
fn lane_checkout(lane: &std::path::Path) -> (Option<String>, Option<String>) {
    let Some(git_dir) = lane_git_dir(lane) else {
        return (None, None);
    };
    // A `.git` entry alone proves nothing: a pruned linked worktree leaves a
    // `gitdir:` file pointing nowhere. Ownership needs a git dir that resolves
    // and a readable `HEAD`.
    if !real_file(&git_dir.join("HEAD")) || std::fs::read_to_string(git_dir.join("HEAD")).is_err() {
        return (None, None);
    }
    (
        Some(lane.to_string_lossy().into_owned()),
        head_branch(&git_dir),
    )
}

/// The lane's git dir: `.git` as a directory (a clone), or the path in a
/// `gitdir: <path>` file (a linked worktree), resolved against the lane.
fn lane_git_dir(lane: &std::path::Path) -> Option<std::path::PathBuf> {
    let dot_git = lane.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    if !meta.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let path = std::path::Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        lane.join(path)
    })
}

/// Branch from a checkout's `HEAD`. A detached HEAD has no branch name, which
/// is reported as such rather than guessed from a ref that may not exist.
fn head_branch(git_dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    text.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

/// What the filesystem says about a lane's activity.
///
/// A held Cargo lock wins over mtime: a long single-crate compile can go quiet
/// for longer than the recency window while the build is very much alive. The
/// lock is only `try_lock`ed — acquiring it proves nothing is building, and the
/// handle is dropped immediately so the probe can never block or disturb a
/// build.
fn lane_activity(
    lane: &std::path::Path,
    kind: LaneKind,
    newest_mtime: u64,
    now: u64,
) -> (&'static str, String) {
    if kind == LaneKind::Other {
        return (
            "unknown",
            "not a build lane: no build output to inspect".to_string(),
        );
    }
    if let Some(lock) = held_cargo_lock(&lane_target(lane, kind)) {
        let shown = lock.strip_prefix(lane).unwrap_or(&lock);
        return (
            "building",
            format!("cargo build lock held at {}", shown.display()),
        );
    }
    if newest_mtime == 0 {
        return ("unknown", "no readable file mtime".to_string());
    }
    let idle_secs = now.saturating_sub(newest_mtime);
    let status = if idle_secs <= disk_reclaim::LOW_DISK_MIN_IDLE_SECS {
        "active"
    } else {
        "idle"
    };
    (status, format!("newest write {} ago", human_age(idle_secs)))
}

/// Where a lane keeps Cargo's profiles: its `target/` subtree for a checkout
/// copy, or the lane itself when the lane is a bare target dir.
fn lane_target(lane: &std::path::Path, kind: LaneKind) -> std::path::PathBuf {
    match kind {
        LaneKind::BuildDir => lane.to_path_buf(),
        _ => lane.join("target"),
    }
}

/// The first Cargo build lock a live build holds, if any.
///
/// Cargo puts the lock at `target/<profile>/.cargo-lock`, and for a cross
/// compilation at `target/<triple>/<profile>/.cargo-lock`. Only those two shapes
/// are probed — a full second traversal of a 100 GiB target tree to find a lock
/// would double the cost of the report.
fn held_cargo_lock(target: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    let Ok(profiles) = std::fs::read_dir(target) else {
        return None;
    };
    for profile in profiles.flatten() {
        let Ok(file_type) = profile.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let dir = profile.path();
        let direct = dir.join(CARGO_LOCK);
        if real_file(&direct) {
            candidates.push(direct);
            continue;
        }
        // Not a profile dir with a lock, so it may be a target triple; look one
        // level down for the profile's lock.
        let Ok(inner) = std::fs::read_dir(&dir) else {
            continue;
        };
        for profile in inner.flatten() {
            let Ok(file_type) = profile.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let nested = profile.path().join(CARGO_LOCK);
            if real_file(&nested) {
                candidates.push(nested);
            }
        }
    }
    candidates.sort();
    candidates.into_iter().find(|path| lock_is_held(path))
}

/// Whether another process (or another open handle) holds `path`'s lock. A lock
/// file that cannot be opened counts as held: the conservative answer keeps a
/// lane out of the eligible set rather than guessing.
fn lock_is_held(path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return true;
    };
    let held = file.try_lock().is_err();
    // The probe's lock (if any) is released here, before the report is printed.
    drop(file);
    held
}

/// Compact age for the report: `12s`, `9m`, `3h`, `5d`.
fn human_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// How a lane is identified in the text table: directory name plus the branch
/// of its own checkout, or why there is no owner to name.
fn lane_label(lane: &LaneRow) -> String {
    match &lane.branch {
        Some(branch) => format!("{} ({branch})", lane.lane),
        None if lane.owner.is_some() => format!("{} (detached)", lane.lane),
        None => format!("{} (owner unknown)", lane.lane),
    }
}

/// `thegn disk [--worktree P] [--all]` — measure and print worktree sizes,
/// largest first, with a grand total and a threshold note. Also refreshes the
/// `worktree_disk` cache so a running host paints fresh sizes immediately.
pub fn disk(cfg: &Config, worktree_arg: Option<String>, all: bool, json: bool) -> Result<()> {
    let db = Db::open()?;

    // Target set: every known worktree (default / --all), or just the resolved one.
    let targets: Vec<(String, String)> = if all || worktree_arg.is_none() {
        db.worktrees()?
            .into_iter()
            .map(|w| (w.worktree, w.branch))
            .collect()
    } else {
        let p = resolve_worktree(worktree_arg)
            .to_string_lossy()
            .into_owned();
        let branch = db
            .worktrees()
            .ok()
            .and_then(|ws| ws.into_iter().find(|w| w.worktree == p).map(|w| w.branch))
            .unwrap_or_default();
        vec![(p, branch)]
    };

    let mut rows: Vec<(String, String, disk::DiskUsage)> = targets
        .into_iter()
        .filter_map(|(path, branch)| {
            let p = std::path::Path::new(&path);
            if !p.is_dir() {
                return None;
            }
            let usage = disk::measure_worktree(p);
            // Refresh the cache the UI reads.
            // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            let _ =
                db.put_worktree_disk(&path, usage.total_bytes as i64, usage.target_bytes as i64);
            Some((path, branch, usage))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.2.total_bytes));

    if json {
        #[derive(serde::Serialize)]
        struct DiskJson<'a> {
            worktree: &'a str,
            branch: &'a str,
            total_bytes: u64,
            target_bytes: u64,
        }
        let out: Vec<DiskJson> = rows
            .iter()
            .map(|(path, branch, u)| DiskJson {
                worktree: path,
                branch,
                total_bytes: u.total_bytes,
                target_bytes: u.target_bytes,
            })
            .collect();
        return super::emit_json(&out);
    }

    if rows.is_empty() {
        outln!("No worktrees found.");
        return Ok(());
    }

    outln!("{:>9}  {:>9}  {}", "SIZE", "TARGET", "WORKTREE");
    let mut grand = 0u64;
    let mut grand_target = 0u64;
    for (path, branch, u) in &rows {
        grand += u.total_bytes;
        grand_target += u.target_bytes;
        let label = if branch.is_empty() {
            path.clone()
        } else {
            format!("{branch}  {path}")
        };
        outln!(
            "{:>9}  {:>9}  {}",
            disk::human(u.total_bytes),
            disk::human(u.target_bytes),
            label
        );
    }
    outln!(
        "{:>9}  {:>9}  {} worktree(s)",
        disk::human(grand),
        disk::human(grand_target),
        rows.len()
    );
    outln!(
        "Reclaimable (target/): {} — `thegn wt clean --all` to recover.",
        disk::human(grand_target)
    );

    let threshold = cfg.disk.warn_threshold_gb;
    if threshold > 0 && grand > threshold * 1024 * 1024 * 1024 {
        outln!(
            "⚠ Total {} exceeds the {}G warning threshold ([disk].warn_threshold_gb).",
            disk::human(grand),
            threshold
        );
    }
    Ok(())
}

/// `thegn clean [--worktree P] [--all] [--force]` — reclaim `target/` for the
/// resolved worktree (default), or every worktree (`--all`). Refuses the active
/// worktree (`$THEGN_WORKTREE`); `cargo clean` takes the build lock so a
/// concurrent build serializes rather than corrupts. Prompts unless `--force`.
pub fn clean(cfg: &Config, worktree_arg: Option<String>, all: bool, force: bool) -> Result<()> {
    let db = Db::open()?;
    let active = std::env::var("THEGN_WORKTREE").unwrap_or_default();

    let targets: Vec<String> = if all {
        db.worktrees()?.into_iter().map(|w| w.worktree).collect()
    } else {
        vec![
            resolve_worktree(worktree_arg)
                .to_string_lossy()
                .into_owned(),
        ]
    };

    let mut total_reclaimed = 0u64;
    let mut cleaned = 0u32;
    let mut failed = 0u32;
    for path in targets {
        let p = std::path::Path::new(&path);
        if !p.is_dir() {
            continue;
        }
        if !active.is_empty() && path == active {
            outln!("skip (active worktree): {path}");
            continue;
        }
        let target = p.join("target");
        if !target.is_dir() {
            continue;
        }
        let size = disk::measure_worktree(p).target_bytes;
        if !force
            && !confirm(&format!(
                "Remove {} of build artifacts in {path}?",
                disk::human(size)
            ))
        {
            outln!("skip: {path}");
            continue;
        }
        match worktree::clean_target(p) {
            Ok(reclaimed) => {
                let _ = db.delete_worktree_disk(&path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                total_reclaimed += reclaimed;
                cleaned += 1;
                outln!("cleaned {} from {path}", disk::human(reclaimed));
            }
            Err(e) => {
                outln!("failed to clean {path}: {e}");
                failed += 1;
            }
        }
    }
    // `cfg` is read for symmetry with `disk` (future per-repo policy); silence
    // the unused warning without changing the signature.
    let _ = cfg;
    outln!(
        "Reclaimed {} across {cleaned} worktree(s).",
        disk::human(total_reclaimed)
    );
    // Exit code matters for scripting/CI: a partial failure (one or more targets
    // failed to clean) must be non-zero so callers can detect it. A fully
    // successful run — including one that cleaned nothing — stays Ok(()).
    if failed > 0 {
        anyhow::bail!("{failed} worktree(s) failed to clean");
    }
    Ok(())
}

#[cfg(test)]
mod lane_tests {
    use super::*;
    use std::fs;

    /// A lane that is a copy of the repository: `.git/HEAD` on `branch`, a
    /// `Cargo.toml`, and one 17-byte artifact under `target/debug/`.
    fn checkout_lane(dir: &std::path::Path, branch: &str) {
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
        fs::write(dir.join("Cargo.toml"), b"[package]\nname = \"lane\"\n").unwrap();
        fs::create_dir_all(dir.join("target/debug")).unwrap();
        fs::write(dir.join("target/debug/artifact.bin"), [1u8; 17]).unwrap();
    }

    /// Backdate every file under `dir` so the lane reads as idle.
    fn backdate(dir: &std::path::Path, at: std::time::SystemTime) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                backdate(&entry.path(), at);
            } else {
                let file = fs::OpenOptions::new()
                    .write(true)
                    .open(entry.path())
                    .unwrap();
                file.set_times(fs::FileTimes::new().set_modified(at))
                    .unwrap();
            }
        }
    }

    fn row<'a>(lanes: &'a [LaneRow], name: &str) -> &'a LaneRow {
        lanes
            .iter()
            .find(|lane| lane.lane == name)
            .unwrap_or_else(|| panic!("no lane {name}"))
    }

    #[test]
    fn lane_report_classifies_active_idle_locked_and_unowned_lanes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("target").join("lanes");
        let now = u64::try_from(thegn_core::util::now()).unwrap();

        checkout_lane(&root.join("active"), "active-branch");
        checkout_lane(&root.join("idle"), "idle-branch");
        checkout_lane(&root.join("locked"), "locked-branch");
        // A stale lane nobody claims: build output, but no checkout of its own.
        let unowned = root.join("unowned");
        fs::create_dir_all(unowned.join("target/debug")).unwrap();
        fs::write(unowned.join("Cargo.toml"), b"[package]\nname = \"lane\"\n").unwrap();
        fs::write(unowned.join("target/debug/artifact.bin"), [1u8; 17]).unwrap();
        // Not a build lane at all.
        fs::create_dir_all(root.join("scratch")).unwrap();
        fs::write(root.join("scratch/notes.md"), b"# notes\n").unwrap();

        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
        backdate(&root.join("idle"), old);
        backdate(&unowned, old);

        let lock_path = root.join("locked/target/debug/.cargo-lock");
        fs::write(&lock_path, b"").unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        held.try_lock().unwrap();

        let lanes = lane_report(&root, now).unwrap();

        assert_eq!(lanes.len(), 5, "every lane directory is reported");

        let active = row(&lanes, "active");
        assert_eq!(active.kind, "checkout");
        assert_eq!(active.activity, "active");
        assert_eq!(active.eligibility, "held");
        assert_eq!(active.branch.as_deref(), Some("active-branch"));
        assert_eq!(
            active.owner.as_deref(),
            Some(root.join("active").to_string_lossy().as_ref()),
            "a lane that is its own checkout owns itself"
        );

        assert_eq!(row(&lanes, "idle").activity, "idle");
        assert_eq!(row(&lanes, "idle").eligibility, "eligible");

        let locked = row(&lanes, "locked");
        assert_eq!(locked.activity, "building");
        assert_eq!(locked.eligibility, "held");
        assert!(locked.activity_evidence.contains(".cargo-lock"));

        let unowned = row(&lanes, "unowned");
        assert_eq!(unowned.activity, "idle");
        assert_eq!(unowned.eligibility, "unknown");
        assert!(unowned.owner.is_none(), "ownership is not guessed");
        assert!(unowned.reason.contains("ownership unknown"));

        let scratch = row(&lanes, "scratch");
        assert_eq!(scratch.kind, "other");
        assert_eq!(scratch.eligibility, "unknown");
        assert_eq!(scratch.build_bytes, 0);

        // Four build lanes, 17 bytes each, and only the verified idle one is
        // eligible. Totals are in the same unit as the rows.
        assert_eq!(lanes.iter().map(|lane| lane.build_bytes).sum::<u64>(), 68);
        let eligible: u64 = lanes
            .iter()
            .filter(|lane| lane.eligibility == "eligible")
            .map(|lane| lane.build_bytes)
            .sum();
        assert_eq!(eligible, 17);

        // Read-only: every artifact still reads back byte for byte.
        for lane in ["active", "idle", "locked", "unowned"] {
            assert_eq!(
                fs::read(root.join(lane).join("target/debug/artifact.bin")).unwrap(),
                [1u8; 17],
                "{lane} contents are intact"
            );
        }
    }

    #[test]
    fn orphaned_gitdir_leaves_owner_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("lanes");
        let lane = root.join("orphan");
        checkout_lane(&lane, "tg/the-1");
        fs::remove_dir_all(lane.join(".git")).unwrap();
        fs::write(lane.join(".git"), b"gitdir: ../gone\n").unwrap();
        // A `.git` directory with no HEAD is no checkout either.
        let headless = root.join("headless");
        checkout_lane(&headless, "tg/the-2");
        fs::remove_file(headless.join(".git/HEAD")).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        backdate(&root, old);

        let now = u64::try_from(thegn_core::util::now()).unwrap();
        let lanes = lane_report(&root, now).unwrap();
        for name in ["orphan", "headless"] {
            let lane = row(&lanes, name);
            assert!(lane.owner.is_none(), "{name} has no resolvable checkout");
            assert_eq!(lane.eligibility, "unknown");
        }
    }

    #[test]
    fn linked_worktree_git_file_names_its_branch() {
        let temp = tempfile::tempdir().unwrap();
        let lane = temp.path().join("lane");
        let git_dir = temp.path().join("git").join("worktrees").join("lane");
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(&lane).unwrap();
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/tg/the-1\n").unwrap();
        // Git writes the gitdir relative to the checkout.
        fs::write(lane.join(".git"), b"gitdir: ../git/worktrees/lane\n").unwrap();

        let (owner, branch) = lane_checkout(&lane);
        assert_eq!(owner.as_deref(), Some(lane.to_string_lossy().as_ref()));
        assert_eq!(branch.as_deref(), Some("tg/the-1"));

        // A detached HEAD still has a checkout to attribute, but no branch.
        fs::write(
            git_dir.join("HEAD"),
            b"0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        let (owner, branch) = lane_checkout(&lane);
        assert!(owner.is_some());
        assert!(branch.is_none());
    }
}
