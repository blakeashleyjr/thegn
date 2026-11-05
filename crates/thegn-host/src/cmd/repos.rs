//! `thegn repo` (and the legacy `repos` / `recent` / `repo-trust`
//! spellings) — repo discovery + history feeds, plus `repo trust` —
//! trust-on-first-use review/approval of a repo `.thegn.*` overlay's gated
//! sandbox and lifecycle-hook requests.

use anyhow::Result;
use std::path::PathBuf;
use thegn_core::config::Config;
use thegn_core::config_resolve::{Approvals, GatedRequest};
use thegn_core::db::Db;
use thegn_core::db_clone_provenance::{
    CloneProvenance, ProvenanceStatus, classify_clone_provenance,
};
use thegn_core::remote::GitLoc;
use thegn_core::store::{RepoTrustStore, WorkspaceStore};
use thegn_core::{outln, repo, repo_trust, util};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List git repos discovered under repo_roots.
    List {
        /// Emit one JSON array of paths instead of plain lines.
        #[arg(long)]
        json: bool,
    },
    /// List recently opened repos, most-recent first.
    Recent {
        count: Option<i64>,
        /// Emit one JSON array of paths instead of plain lines.
        #[arg(long)]
        json: bool,
    },
    /// Review/approve a repo `.thegn.*` overlay's gated sandbox and lifecycle
    /// hook requests (trust-on-first-use).
    Trust {
        /// Repo path (default: current directory).
        path: Option<String>,
        /// Approve a pending request by its id.
        #[arg(long)]
        approve: Option<String>,
        /// Revoke a recorded decision by its id.
        #[arg(long)]
        revoke: Option<String>,
    },
    /// Show where cloned workspaces came from and whether each clone finished
    /// (add PATH for one repo).
    Provenance {
        /// Destination path (default: list recorded clones).
        path: Option<String>,
        /// Emit one JSON array instead of plain lines.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => repos(cfg, json),
        Action::Recent { count, json } => recent(count, json),
        Action::Trust {
            path,
            approve,
            revoke,
        } => trust(cfg, path, approve, revoke),
        Action::Provenance { path, json } => provenance(path, json),
    }
}

/// Git repos discovered under `repo_roots` (what the picker offers).
pub fn repos(cfg: &Config, json: bool) -> Result<()> {
    let paths = repo::discover_repos(cfg);
    if json {
        return super::emit_json(&paths);
    }
    for path in paths {
        outln!("{path}");
    }
    Ok(())
}

/// Recently opened repos, most-recent first.
pub fn recent(count: Option<i64>, json: bool) -> Result<()> {
    let db = Db::open()?;
    let paths = db.recent_repos(count.unwrap_or(20))?;
    if json {
        return super::emit_json(&paths);
    }
    for path in paths {
        outln!("{path}");
    }
    Ok(())
}

const PROVENANCE_LIST_LIMIT: i64 = 50;

#[derive(serde::Serialize)]
struct ProvenanceReport {
    #[serde(flatten)]
    row: CloneProvenance,
    status: ProvenanceStatus,
}

/// Report recorded clone origins with their current on-disk verification status.
pub fn provenance(path: Option<String>, json: bool) -> Result<()> {
    let db = Db::open()?;
    let reports = provenance_reports(&db, path.as_deref())?;
    if json {
        return super::emit_json(&reports);
    }
    if reports.is_empty() {
        outln!("no cloned workspaces recorded");
        return Ok(());
    }
    for report in reports {
        outln!(
            "{}\t{}\t{}",
            report.status.as_str(),
            report.row.dest,
            report.row.canonical_origin
        );
    }
    Ok(())
}

fn provenance_reports(db: &Db, path: Option<&str>) -> Result<Vec<ProvenanceReport>> {
    let rows = if let Some(path) = path {
        let root = repo_root_arg(Some(path.to_owned()));
        let canonical = std::fs::canonicalize(&root)
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        let raw = root.to_string_lossy().into_owned();
        let mut found = None;
        if let Some(canonical) = canonical {
            found = db.clone_provenance(&canonical)?;
        }
        if found.is_none() {
            found = db.clone_provenance(&raw)?;
        }
        vec![found.ok_or_else(|| {
            anyhow::anyhow!("no clone provenance recorded for {}", root.display())
        })?]
    } else {
        db.recent_clone_provenance(PROVENANCE_LIST_LIMIT)?
    };
    Ok(rows.into_iter().map(observe).collect())
}

fn observe(row: CloneProvenance) -> ProvenanceReport {
    let dest = std::path::Path::new(&row.dest);
    let dest_present = dest.join(".git").exists();
    let observed_origin = if dest_present {
        crate::workspace_create::configured_origin(dest, "origin")
            .ok()
            .flatten()
    } else {
        None
    };
    let owner_alive = row
        .clone_pid
        .is_some_and(|pid| crate::platform::pid_alive(i64::from(pid)));
    let status =
        classify_clone_provenance(&row, dest_present, observed_origin.as_deref(), owner_alive);
    ProvenanceReport { row, status }
}

/// Resolve a repo-path argument (default: cwd) to a repo root.
fn repo_root_arg(path: Option<String>) -> PathBuf {
    let start = path
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    repo::main_worktree(&start).unwrap_or(start)
}

/// Pick the target whose trust request is being reviewed. A remote worktree
/// may have only a local placeholder, so prefer the explicitly supplied
/// remote row and otherwise use the first remote row registered for the repo.
/// The target-side bytes are resolved by the provider below; this helper never
/// treats the placeholder as the source of truth.
fn trust_target(
    start: &std::path::Path,
    root: &std::path::Path,
    db: &Db,
) -> anyhow::Result<(PathBuf, GitLoc)> {
    let direct = GitLoc::for_worktree_op(start, "repo trust").map_err(anyhow::Error::msg)?;
    if direct.is_remote() {
        return Ok((start.to_path_buf(), direct));
    }
    if let Ok(rows) = db.worktrees()
        && let Some(row) = rows.into_iter().find(|row| {
            row.repo_root == root.to_string_lossy()
                && GitLoc::from_db(&row.worktree, Some(&row.location)).is_remote()
        })
    {
        let worktree = PathBuf::from(row.worktree);
        let loc = GitLoc::for_worktree_op(&worktree, "repo trust").map_err(anyhow::Error::msg)?;
        return Ok((worktree, loc));
    }
    Ok((root.to_path_buf(), direct))
}

/// Resolve all pending repo-overlay requests exposed by `repo trust`. The
/// sandbox resolver owns clamp events; the pure lifecycle resolver owns the
/// canonical `hooks.<event>` requests. Keeping both here makes one approval
/// surface authoritative for every repo-authored executable setting.
fn pending_repo_requests(
    cfg: &Config,
    root: &std::path::Path,
    approvals: &Approvals,
) -> Vec<GatedRequest> {
    let mut pending = cfg.repo_sandbox_resolved(root, approvals).pending;
    let (repo_hooks, repo_prepare) = thegn_core::config::load_repo_hooks(root)
        .unwrap_or_else(|| (thegn_core::config::HooksConfig::default(), Vec::new()));
    let overlay = cfg.workspace_overlay(root);
    let lifecycle = thegn_core::hooks::resolve(
        &cfg.hooks,
        overlay.overlay().map(|workspace| &workspace.hooks),
        Some(&repo_hooks),
        &cfg.sandbox.prepare,
        &repo_prepare,
        approvals,
    );
    pending.extend(lifecycle.pending);
    pending
}
/// `thegn repos trust [path] [--approve <id>] [--revoke <id>]` — review and
/// decide all gated requests a repo `.thegn.*` overlay makes. With no flag,
/// lists the current denials, pending requests (with ids), and recorded
/// decisions. Approving applies the request on the next worktree launch.
pub fn trust(
    cfg: &Config,
    path: Option<String>,
    approve: Option<String>,
    revoke: Option<String>,
) -> Result<()> {
    let start = path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let root = repo_root_arg(path);
    let db = Db::open()?;
    let root = db
        .repo_root_for(&start.to_string_lossy())?
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .unwrap_or(root);
    let root_s = root.to_string_lossy().to_string();
    let (target, target_loc) = trust_target(&start, &root, &db)?;

    if let Some(id) = revoke {
        let row = db
            .repo_trust_list(&root_s)?
            .into_iter()
            .find(|r| r.request_id == id)
            .ok_or_else(|| anyhow::anyhow!("no recorded decision with id {id:?}"))?;
        db.repo_trust_revoke(&root_s, &row.request_json)?;
        outln!("revoked {id}");
        return Ok(());
    }

    // Re-resolve with the CURRENT approvals so already-approved requests don't
    // reappear as pending.
    let approved_canonical = db.repo_trust_approved(&root_s)?;
    let approvals = Approvals::from_canonical(approved_canonical.clone());
    let resolved = cfg.repo_sandbox_resolved(&root, &approvals);
    let mut pending = pending_repo_requests(cfg, &root, &approvals);
    if let Some(request) = crate::mise_provider::pending_request_for_target(
        cfg,
        &target.to_string_lossy(),
        &root,
        &target_loc,
        &approved_canonical,
        true,
    ) {
        pending.push(request);
    }

    if let Some(id) = approve {
        let req = pending
            .iter()
            .find(|p| repo_trust::request_id(&p.canonical()) == id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no pending request with id {id:?}; run `repo trust {}` to list",
                    root.display()
                )
            })?;
        let (rid, canonical) = repo_trust::storage_key(req);
        db.repo_trust_decide(&root_s, &rid, &canonical, "approved", util::now())?;
        outln!("approved {id}: {}", req.summary);
        return Ok(());
    }

    // List mode.
    outln!("repo: {}", root.display());
    // THE-515: the trusted overlay is part of this repo's authority; say so
    // when it is refused rather than silently omitting its hooks/mounts.
    match cfg.workspace_overlay(&root) {
        thegn_core::workspace_overlay::WorkspaceOverlay::Selected { key, .. } => {
            outln!("  trusted overlay: [project.{key}]");
        }
        thegn_core::workspace_overlay::WorkspaceOverlay::Refused(refusal) => {
            outln!("  REFUSED {refusal}");
        }
        thegn_core::workspace_overlay::WorkspaceOverlay::Unconfigured => {}
    }
    if resolved.events.is_empty() && pending.is_empty() {
        outln!("  no denied or pending overlay requests");
    }
    for line in thegn_core::config_resolve::summarize_events(&resolved.events) {
        outln!("  {line}");
    }
    for p in &pending {
        outln!(
            "  pending [{}] {}: {}",
            repo_trust::request_id(&p.canonical()),
            p.key,
            p.summary
        );
    }
    let decided = db.repo_trust_list(&root_s)?;
    if !decided.is_empty() {
        outln!("  ── decisions ──");
        for d in decided {
            outln!("  {} {} ({})", d.decision, d.request_id, d.request_json);
        }
    }
    if !pending.is_empty() {
        outln!(
            "approve with: thegn repo trust {} --approve <id>",
            root.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::db_clone_provenance::{CloneProvenanceState, NewCloneProvenance};
    use thegn_core::hooks::{HookContext, HookEvent, HookExecutionMode};
    use thegn_core::store::RepoTrustStore;

    #[test]
    fn repo_trust_lists_approves_and_executes_pending_hook() {
        let state = crate::testenv::IsolatedState::new();
        let state_home = state.state_home();
        let root = std::env::temp_dir().join(format!(
            "tg-repo-trust-hook-repo-{}-{}",
            std::process::id(),
            thegn_core::util::now()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("hook-ran");
        std::fs::write(
            root.join(".thegn.toml"),
            format!(
                "[hooks]\npost_create = [{{ command = \"printf ran > '{}'\" }}]\n",
                marker.display()
            ),
        )
        .unwrap();

        let cfg = Config::default();
        let db = Db::open_memory().unwrap();
        let root_s = root.to_string_lossy().to_string();
        let denied = pending_repo_requests(&cfg, &root, &Approvals::deny_all());
        let request = denied
            .iter()
            .find(|request| request.key == "hooks.post_create")
            .expect("repo hook is listed by repo trust");
        let id = repo_trust::request_id(&request.canonical());
        let (request_id, canonical) = repo_trust::storage_key(request);
        db.repo_trust_decide(&root_s, &request_id, &canonical, "approved", util::now())
            .unwrap();

        let approvals = Approvals::from_canonical(db.repo_trust_approved(&root_s).unwrap());
        let remaining = pending_repo_requests(&cfg, &root, &approvals);
        assert!(
            !remaining
                .iter()
                .any(|request| request.key == "hooks.post_create")
        );
        assert_eq!(id, request_id);

        let (repo_hooks, repo_prepare) = thegn_core::config::load_repo_hooks(&root).unwrap();
        let resolved = thegn_core::hooks::resolve(
            &cfg.hooks,
            None,
            Some(&repo_hooks),
            &cfg.sandbox.prepare,
            &repo_prepare,
            &approvals,
        );
        let context = HookContext {
            event: HookEvent::PostCreate,
            repo_root: root_s.clone(),
            worktree: root_s.clone(),
            branch: "main".into(),
            workspace: "repo".into(),
        };
        let results = crate::hook_run::run_all(
            resolved.entries(HookEvent::PostCreate),
            &context,
            &root,
            HookExecutionMode::User,
        );
        assert!(results.iter().all(|result| result.succeeded()));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "ran");

        let _ = std::fs::remove_dir_all(root); // best-effort: test cleanup
        let _ = std::fs::remove_dir_all(state_home); // best-effort: test cleanup
    }

    #[test]
    fn trust_target_refuses_an_unrecognised_worktree_location() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        let dir = tempfile::tempdir().unwrap();
        db.put_worktree(
            "repo/trust",
            &dir.path().to_string_lossy(),
            &dir.path().to_string_lossy(),
            "feature",
            Some("garbage"),
            None,
        )
        .unwrap();

        let result = trust_target(dir.path(), dir.path(), &db);
        assert!(
            matches!(result, Err(error) if format!("{error:#}").contains("repo trust refused"))
        );
    }

    #[test]
    fn trust_target_keeps_a_row_less_start_local() {
        // Positive control: passes at 774069ac2 too.
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        let dir = tempfile::tempdir().unwrap();

        let result = trust_target(dir.path(), dir.path(), &db).unwrap();
        assert_eq!(result.0, dir.path());
        assert!(matches!(result.1, GitLoc::Local(_)));
    }

    fn record_clone(db: &Db, dest: &str, origin: &str, pid: u32) {
        assert!(
            db.begin_clone_provenance(
                &NewCloneProvenance {
                    dest,
                    canonical_origin: origin,
                    label: "example",
                    clone_pid: pid,
                    thegn_version: "0.1.0-alpha.52",
                },
                &|_| false,
            )
            .unwrap()
        );
    }

    fn write_origin(dest: &std::path::Path, origin_url: &str) {
        std::fs::create_dir_all(dest.join(".git")).unwrap();
        std::fs::write(
            dest.join(".git/config"),
            format!("[remote \"origin\"]\n\turl = {origin_url}\n"),
        )
        .unwrap();
    }

    #[test]
    fn provenance_reports_classify_verified_origin_changed_missing_and_interrupted() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let origin = "https://example.com/o/r";
        let verified_dest = tmp.path().join("ok");
        let changed_dest = tmp.path().join("moved");
        let missing_dest = tmp.path().join("gone");
        let interrupted_dest = tmp.path().join("crashed");
        write_origin(&verified_dest, "https://example.com/o/r.git");
        write_origin(&changed_dest, "https://example.com/o/other.git");

        let verified = verified_dest.to_string_lossy().into_owned();
        record_clone(&db, &verified, origin, 5);
        assert!(
            db.finish_clone_provenance(&verified, 5, CloneProvenanceState::Complete)
                .unwrap()
        );

        let changed = changed_dest.to_string_lossy().into_owned();
        record_clone(&db, &changed, origin, 6);
        assert!(
            db.finish_clone_provenance(&changed, 6, CloneProvenanceState::Complete)
                .unwrap()
        );

        let missing = missing_dest.to_string_lossy().into_owned();
        record_clone(&db, &missing, origin, 7);
        assert!(
            db.finish_clone_provenance(&missing, 7, CloneProvenanceState::Complete)
                .unwrap()
        );

        let interrupted = interrupted_dest.to_string_lossy().into_owned();
        let dead_pid = u32::try_from(i32::MAX).unwrap();
        record_clone(&db, &interrupted, origin, dead_pid);

        let reports = provenance_reports(&db, None).unwrap();
        assert_eq!(reports.len(), 4);
        let status_for = |dest: &str| {
            reports
                .iter()
                .find(|report| report.row.dest == dest)
                .map(|report| report.status)
        };
        assert_eq!(status_for(&verified), Some(ProvenanceStatus::Verified));
        assert_eq!(status_for(&changed), Some(ProvenanceStatus::OriginChanged));
        assert_eq!(status_for(&missing), Some(ProvenanceStatus::Missing));
        assert_eq!(
            status_for(&interrupted),
            Some(ProvenanceStatus::Interrupted)
        );
    }

    #[test]
    fn provenance_for_a_path_without_a_row_fails() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let error = match provenance_reports(&db, Some(&path)) {
            Ok(_) => panic!("unrecorded path must fail"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("no clone provenance recorded"));

        let dest = std::fs::canonicalize(tmp.path()).unwrap();
        let dest = dest.to_str().unwrap();
        record_clone(&db, dest, "https://example.com/o/r", 8);
        assert!(
            db.finish_clone_provenance(dest, 8, CloneProvenanceState::Complete)
                .unwrap()
        );
        let reports = provenance_reports(&db, Some(&path)).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].row.dest, dest);
    }

    #[test]
    fn provenance_json_carries_the_kebab_status_and_no_clone_url() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dest_path = tmp.path().join("changed");
        write_origin(&dest_path, "https://example.com/o/other.git");
        let dest = dest_path.to_string_lossy().into_owned();
        record_clone(&db, &dest, "https://example.com/o/r", 9);
        assert!(
            db.finish_clone_provenance(&dest, 9, CloneProvenanceState::Complete)
                .unwrap()
        );

        let reports = provenance_reports(&db, None).unwrap();
        let value = serde_json::to_value(reports).unwrap();
        let row = &value[0];
        assert_eq!(row["status"], "origin-changed");
        assert_eq!(row["state"], "complete");
        assert_eq!(row["canonical_origin"], "https://example.com/o/r");
        assert!(row.get("clone_url").is_none());
    }

    #[test]
    fn provenance_lists_every_recorded_row_when_no_path_is_given() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let destinations = ["one", "two", "three"]
            .map(|name| tmp.path().join(name).to_string_lossy().into_owned());
        for (index, dest) in destinations.iter().enumerate() {
            record_clone(&db, dest, "https://example.com/o/r", 10 + index as u32);
        }

        let reports = provenance_reports(&db, None).unwrap();
        assert_eq!(reports.len(), 3);
        for dest in destinations {
            assert!(reports.iter().any(|report| report.row.dest == dest));
        }
    }
}
