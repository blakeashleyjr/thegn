//! `thegn repo` (and the legacy `repos` / `recent` / `repo-trust`
//! spellings) — repo discovery + history feeds, plus `repo trust` —
//! trust-on-first-use review/approval of a repo `.thegn.*` overlay's gated
//! sandbox and lifecycle-hook requests.

use anyhow::Result;
use std::path::PathBuf;
use thegn_core::config::Config;
use thegn_core::config_resolve::{Approvals, GatedRequest};
use thegn_core::db::Db;
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

/// Resolve a repo-path argument (default: cwd) to a repo root.
fn repo_root_arg(path: Option<String>) -> PathBuf {
    let start = path
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    repo::main_worktree(&start).unwrap_or(start)
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
    let workspace = cfg.workspace.get(&thegn_core::config::workspace_slug(root));
    let lifecycle = thegn_core::hooks::resolve(
        &cfg.hooks,
        workspace.map(|workspace| &workspace.hooks),
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
    let pending = pending_repo_requests(cfg, &root, &approvals);

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
    use thegn_core::hooks::{HookContext, HookEvent, HookExecutionMode};
    use thegn_core::store::RepoTrustStore;

    #[test]
    fn repo_trust_lists_approves_and_executes_pending_hook() {
        let state_home =
            std::env::temp_dir().join(format!("tg-repo-trust-hook-state-{}", std::process::id()));
        let state_home_s = state_home.to_string_lossy().into_owned();
        let _env = crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", &state_home_s)]);
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
}
