//! Host-side trust-on-first-use for a repo `.superzej.*` overlay's sandbox
//! requests. The core clamp ([`superzej_core::config_resolve`]) decides *what*
//! a repo may request within the trusted bounds; this module supplies the
//! persisted approvals from the DB, resolves the effective environment with
//! them, and surfaces denials + pending requests to the user (a log line always,
//! plus one deduped notification per repo request-set).
//!
//! Approving a pending request is a deliberate, out-of-band act:
//! `superzej repo-trust --approve <id>` (see `cmd::repos`). Until then the
//! request is simply not applied — the worktree still opens.

use std::path::Path;

use superzej_core::config::{Config, SandboxConfig};
use superzej_core::config_resolve::{Approvals, ClampEvent, GatedRequest, summarize_events};
use superzej_core::db::Db;
use superzej_core::devcontainer::{self, SubstCtx};
use superzej_core::devcontainer_overlay;
use superzej_core::env::Environment;
use superzej_core::remote::GitLoc;
use superzej_core::store::{NotificationStore, RepoTrustStore, ZoneStore};

/// Notification kind for a clamped/pending repo overlay request.
pub(crate) const CLAMP_KIND: &str = "repo_config_trust";

/// The approvals a repo currently has (from the `repo_trust` table). Empty
/// (fail-closed) on any DB error.
pub(crate) fn approvals_for(db: &Db, repo_root: &str) -> Approvals {
    match db.repo_trust_approved(repo_root) {
        Ok(list) => Approvals::from_canonical(list),
        Err(e) => {
            tracing::warn!(target: "szhost::config_trust", error = %e, "repo_trust read failed; deny-all");
            Approvals::deny_all()
        }
    }
}

/// Resolve the effective [`Environment`] for a worktree honouring persisted
/// trust-on-first-use approvals, and surface anything the clamp denied or gated.
/// Never fails resolution — on a DB-open error it falls back to the fail-closed
/// [`Config::resolve_env`].
pub(crate) fn resolve_env_trusted(
    cfg: &Config,
    repo_root: &Path,
    loc: &GitLoc,
    worktree: &str,
    selected_env: Option<&str>,
) -> Environment {
    let Ok(db) = Db::open() else {
        return cfg.resolve_env(repo_root, loc, Path::new(worktree), selected_env);
    };
    let root_s = repo_root.to_string_lossy().to_string();
    let approvals = approvals_for(&db, &root_s);
    let (mut env, resolved) = cfg.resolve_env_with(
        repo_root,
        loc,
        Path::new(worktree),
        selected_env,
        &approvals,
    );
    surface(&db, &root_s, worktree, &resolved.events, &resolved.pending);
    // Fold a repo `devcontainer.json` onto the resolved sandbox, trust-gated
    // exactly like the `.superzej.toml` overlay above. The worktree is bind-
    // mounted at its real path, so the devcontainer's workspace folder is that
    // same path. No-op without a devcontainer.json.
    overlay_devcontainer(repo_root, worktree, &mut env.sandbox);
    apply_zone(&db, cfg, worktree, &mut env);
    env
}

/// A [`SubstCtx`] for a worktree that superzej bind-mounts at its **real path**
/// (the local-sandbox invariant): the host and in-container workspace folders
/// are the same path, so devcontainer `${localWorkspaceFolder}` and
/// `${containerWorkspaceFolder}` both resolve to `worktree`.
fn subst_ctx(worktree: &str) -> SubstCtx<'static> {
    let wt = worktree.to_string();
    SubstCtx {
        local_workspace_folder: wt.clone(),
        container_workspace_folder: wt,
        local_env: &|k| std::env::var(k).ok(),
        container_env: &|_| None,
    }
}

/// Overlay a repo's `devcontainer.json` onto the resolved sandbox, trust-gated.
/// Mutates `sb` (image/build/compose/mounts/ports/env/init_script/prepare),
/// logs any warnings, and surfaces pending `devcontainer.*` approvals the same
/// way a `.superzej.toml` overlay does. No-op when there's no devcontainer.json.
fn overlay_devcontainer(repo_root: &Path, worktree: &str, sb: &mut SandboxConfig) {
    let dc = match devcontainer::detect_and_parse(Path::new(worktree)) {
        Some(Ok(dc)) => dc,
        Some(Err(e)) => {
            tracing::warn!(target: "szhost::config_trust", "devcontainer.json ignored: {e}");
            return;
        }
        None => return,
    };
    let Ok(db) = Db::open() else { return };
    let root_s = repo_root.to_string_lossy().to_string();
    let approvals = approvals_for(&db, &root_s);
    let ctx = subst_ctx(worktree);
    let outcome = devcontainer_overlay::apply_gated(&dc, sb, &ctx, worktree, &approvals);
    for w in &outcome.warnings {
        tracing::warn!(target: "szhost::config_trust", "{w}");
    }
    surface(&db, &root_s, worktree, &[], &outcome.pending);
}

/// The trust-gated devcontainer one-time lifecycle steps
/// (`onCreate`/`updateContent`/`postCreate`) for the provisioner to append
/// after `envplan::plan`. Empty when there's no devcontainer.json or the
/// lifecycle category isn't approved.
pub(crate) fn devcontainer_lifecycle_steps(
    repo_root: &Path,
    worktree: &str,
    workdir: &str,
) -> Vec<superzej_core::envplan::ProvisionStep> {
    let Some(Ok(dc)) = devcontainer::detect_and_parse(Path::new(worktree)) else {
        return Vec::new();
    };
    let Ok(db) = Db::open() else {
        return Vec::new();
    };
    let approvals = approvals_for(&db, &repo_root.to_string_lossy());
    let ctx = subst_ctx(worktree);
    devcontainer_overlay::gated_steps(&dc, workdir, &ctx, &approvals)
}

/// The trust-gated devcontainer feature-install steps (run after the toolchain,
/// before lifecycle commands). Empty without a devcontainer.json, without
/// `features`, or until the `devcontainer.features` category is approved.
pub(crate) fn devcontainer_feature_steps(
    repo_root: &Path,
    worktree: &str,
) -> Vec<superzej_core::envplan::ProvisionStep> {
    let Some(Ok(dc)) = devcontainer::detect_and_parse(Path::new(worktree)) else {
        return Vec::new();
    };
    let Ok(db) = Db::open() else {
        return Vec::new();
    };
    let approvals = approvals_for(&db, &repo_root.to_string_lossy());
    let remote_user = dc.remote_user.as_deref().unwrap_or("root");
    devcontainer_overlay::gated_feature_steps(&dc, remote_user, &approvals)
}

/// Apply the worktree's zone ceilings (egress intersect, block union, sandbox
/// floor) to the resolved sandbox, and surface any egress entries the zone
/// dropped. No-op for an unzoned worktree or a zone with no `[zone.<name>]`
/// policy. Membership is DB-tracked (never path-inferred). See [`superzej_core::zone`].
fn apply_zone(db: &Db, cfg: &Config, worktree: &str, env: &mut Environment) {
    let Ok(Some(zrow)) = db.zone_of_worktree(worktree) else {
        return;
    };
    let Some(zc) = cfg.zone.get(&zrow.name) else {
        return;
    };
    let dropped = superzej_core::zone::apply_zone_ceilings(&mut env.sandbox, &zrow.name, zc);
    for d in &dropped {
        tracing::warn!(
            target: "szhost::config_trust", zone = %d.zone,
            "egress {} dropped by zone ceiling", d.entry
        );
    }
    if !dropped.is_empty() {
        let msg = format!(
            "zone '{}' egress ceiling dropped {} destination(s) for this worktree",
            zrow.name,
            dropped.len()
        );
        let issue = format!("zone-egress:{}:{}", zrow.name, dropped.len());
        if let Ok(existing) = db.get_all_notifications(200)
            && !existing.iter().any(|n| n.source_ref == issue)
        {
            let _ = db.put_notification("zone_egress", &issue, &msg, worktree);
        }
    }
}

/// Log every clamp event + pending request, and record one deduped notification
/// per repo request-set so a dropped/blocked repo request is never silent.
fn surface(
    db: &Db,
    repo_root: &str,
    worktree: &str,
    events: &[ClampEvent],
    pending: &[GatedRequest],
) {
    for line in summarize_events(events) {
        tracing::warn!(target: "szhost::config_trust", "{line}");
    }
    for gr in pending {
        tracing::warn!(
            target: "szhost::config_trust", key = %gr.key,
            "repo overlay requests approval: {} ({})",
            gr.summary, superzej_core::repo_trust::request_id(&gr.canonical())
        );
    }
    if events.is_empty() && pending.is_empty() {
        return;
    }
    // Dedup key: a stable digest of the (denied, pending) request-set for this
    // repo, so re-launches don't re-notify until the set changes.
    let mut sig: Vec<String> = events.iter().map(|e| format!("d:{}", e.key)).collect();
    sig.extend(pending.iter().map(|p| format!("p:{}", p.canonical())));
    sig.sort();
    let issue_id = format!(
        "repo-trust:{}",
        superzej_core::repo_trust::request_id(&sig.join("\n"))
    );
    // Skip if we already recorded this exact set.
    if let Ok(existing) = db.get_all_notifications(200)
        && existing.iter().any(|n| n.source_ref == issue_id)
    {
        return;
    }
    let denied = events.len();
    let need = pending.len();
    let msg = format!(
        "{}'s .superzej config: {denied} request(s) denied, {need} awaiting approval. \
         Review with `superzej repo-trust`.",
        Path::new(repo_root)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| repo_root.to_string())
    );
    let _ = db.put_notification(CLAMP_KIND, &issue_id, &msg, worktree);
}
