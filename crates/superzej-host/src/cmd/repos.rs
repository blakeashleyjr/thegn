//! `superzej repos` / `superzej recent` — repo discovery + history feeds, plus
//! `superzej repos trust` — trust-on-first-use review/approval of a repo
//! `.superzej.*` overlay's gated sandbox requests.

use anyhow::Result;
use std::path::PathBuf;
use superzej_core::config::Config;
use superzej_core::config_resolve::Approvals;
use superzej_core::db::Db;
use superzej_core::store::{RepoTrustStore, WorkspaceStore};
use superzej_core::{outln, repo, repo_trust, util};

/// Git repos discovered under `repo_roots` (what the picker offers).
pub fn repos(cfg: &Config) -> Result<()> {
    for path in repo::discover_repos(cfg) {
        outln!("{path}");
    }
    Ok(())
}

/// Recently opened repos, most-recent first.
pub fn recent(count: Option<i64>) -> Result<()> {
    let db = Db::open()?;
    for path in db.recent_repos(count.unwrap_or(20))? {
        outln!("{path}");
    }
    Ok(())
}

/// Resolve a repo-path argument (default: cwd) to a repo root.
fn repo_root_arg(path: Option<String>) -> PathBuf {
    let start = path
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    repo::main_worktree(&start).unwrap_or(start)
}

/// `superzej repos trust [path] [--approve <id>] [--revoke <id>]` — review and
/// decide the gated sandbox requests a repo `.superzej.*` overlay makes. With no
/// flag, lists the current denials, pending requests (with ids), and recorded
/// decisions. Approving applies the request on the next worktree launch.
pub fn trust(
    cfg: &Config,
    path: Option<String>,
    approve: Option<String>,
    revoke: Option<String>,
) -> Result<()> {
    let root = repo_root_arg(path);
    let root_s = root.to_string_lossy().to_string();
    let db = Db::open()?;

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
    let approvals = Approvals::from_canonical(db.repo_trust_approved(&root_s)?);
    let resolved = cfg.repo_sandbox_resolved(&root, &approvals);

    if let Some(id) = approve {
        let req = resolved
            .pending
            .iter()
            .find(|p| repo_trust::request_id(&p.canonical()) == id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no pending request with id {id:?}; run `repo-trust {}` to list",
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
    if resolved.events.is_empty() && resolved.pending.is_empty() {
        outln!("  no denied or pending overlay requests");
    }
    for line in superzej_core::config_resolve::summarize_events(&resolved.events) {
        outln!("  {line}");
    }
    for p in &resolved.pending {
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
    if !resolved.pending.is_empty() {
        outln!(
            "approve with: superzej repo-trust {} --approve <id>",
            root.display()
        );
    }
    Ok(())
}
