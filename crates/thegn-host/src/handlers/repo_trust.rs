//! Host-side trust-on-first-use for a repo `.thegn.*` overlay's sandbox
//! requests. The core clamp ([`thegn_core::config_resolve`]) decides *what*
//! a repo may request within the trusted bounds; this module supplies the
//! persisted approvals from the DB, resolves the effective environment with
//! them, and surfaces denials + pending requests to the user (a log line always,
//! plus one deduped notification per repo request-set).
//!
//! Approving a pending request is a deliberate, out-of-band act:
//! `thegn repo-trust --approve <id>` (see `cmd::repos`). Until then the
//! request is simply not applied — the worktree still opens.

use std::path::Path;

use thegn_core::config::{Config, SandboxConfig};
use thegn_core::config_resolve::{Approvals, ClampEvent, GatedRequest, summarize_events};
use thegn_core::db::Db;
use thegn_core::devcontainer::{self, ImageSource, SubstCtx};
use thegn_core::devcontainer_overlay;
use thegn_core::devcontainer_select;
use thegn_core::env::Environment;
use thegn_core::remote::GitLoc;
use thegn_core::store::{NotificationStore, RepoTrustStore, ZoneStore};

/// Notification kind for a clamped/pending repo overlay request.
pub(crate) const CLAMP_KIND: &str = "repo_config_trust";

#[derive(Debug, Clone)]
pub(crate) struct TrustedEnvironment {
    pub environment: Environment,
    pub devcontainer: Option<TrustedDevcontainer>,
}

#[derive(Debug, Clone)]
pub(crate) struct TrustedDevcontainer {
    pub config_path: std::path::PathBuf,
    pub provider_eligible: bool,
}

/// The approvals a repo currently has (from the `repo_trust` table). Empty
/// (fail-closed) on any DB error.
pub(crate) fn approvals_for(db: &Db, repo_root: &str) -> Approvals {
    match db.repo_trust_approved(repo_root) {
        Ok(list) => Approvals::from_canonical(list),
        Err(e) => {
            tracing::warn!(target: "thegn::config_trust", error = %e, "repo_trust read failed; deny-all");
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
) -> TrustedEnvironment {
    let Ok(db) = Db::open() else {
        return TrustedEnvironment {
            environment: cfg.resolve_env(repo_root, loc, Path::new(worktree), selected_env),
            devcontainer: None,
        };
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
    // exactly like the `.thegn.toml` overlay above. The worktree is bind-
    // mounted at its real path, so the devcontainer's workspace folder is that
    // same path. No-op without a devcontainer.json.
    let devcontainer = overlay_devcontainer(cfg, repo_root, worktree, &mut env.sandbox);
    apply_zone(&db, cfg, worktree, &mut env);
    TrustedEnvironment {
        environment: env,
        devcontainer,
    }
}

/// A [`SubstCtx`] for a worktree that thegn bind-mounts at its **real path**
/// (the local-sandbox invariant): the host and in-container workspace folders
/// are the same path, so devcontainer `${localWorkspaceFolder}` and
/// `${containerWorkspaceFolder}` both resolve to `worktree`.
fn subst_ctx<'a>(worktree: &str, local_env: &'a dyn Fn(&str) -> Option<String>) -> SubstCtx<'a> {
    let wt = worktree.to_string();
    SubstCtx {
        local_workspace_folder: wt.clone(),
        container_workspace_folder: wt,
        local_env,
        container_env: &|_| None,
    }
}

/// Overlay a repo's `devcontainer.json` onto the resolved sandbox, trust-gated.
/// Mutates `sb` (image/build/compose/mounts/ports/env/init_script/prepare),
/// logs any warnings, and surfaces pending `devcontainer.*` approvals the same
/// way a `.thegn.toml` overlay does. No-op when there's no devcontainer.json.
fn overlay_devcontainer(
    cfg: &Config,
    repo_root: &Path,
    worktree: &str,
    sb: &mut SandboxConfig,
) -> Option<TrustedDevcontainer> {
    if sb.devcontainer == thegn_core::config::DevcontainerMode::Off {
        return None;
    }
    let selected = devcontainer_select::select_and_parse(
        Path::new(worktree),
        Some(&cfg.repo_devcontainer_selector(repo_root)),
    );
    if selected.candidates.is_empty() {
        return None;
    }
    let dc = match selected.config.as_ref() {
        Some(dc) => dc,
        None => {
            if let Some(error) = selected.error.as_ref() {
                tracing::warn!(target: "thegn::config_trust", "devcontainer.json ignored: {error}");
            }
            return None;
        }
    };
    let Ok(db) = Db::open() else { return None };
    let root_s = repo_root.to_string_lossy().to_string();
    let approvals = approvals_for(&db, &root_s);
    let allowed = sb.env_passthrough.clone();
    let local_env = |key: &str| {
        allowed
            .iter()
            .any(|allowed_key| allowed_key == key)
            .then(|| std::env::var(key).ok())
            .flatten()
    };
    let allow_local_env = |key: &str| allowed.iter().any(|allowed_key| allowed_key == key);
    let ctx = subst_ctx(worktree, &local_env);
    let outcome = devcontainer_overlay::apply_gated_with_policy(
        dc,
        sb,
        &ctx,
        worktree,
        &approvals,
        &allow_local_env,
    );
    for w in &outcome.warnings {
        tracing::warn!(target: "thegn::config_trust", "{w}");
    }
    surface(&db, &root_s, worktree, &[], &outcome.pending);
    for key in &outcome.substitution.blocked_local_env {
        tracing::warn!(
            target: "thegn::config_trust",
            "devcontainer: localEnv:{key} blocked (not in sandbox.env_passthrough)"
        );
    }
    let source_present = match &dc.source {
        ImageSource::Image(image) => !image.is_empty(),
        ImageSource::Build(_) | ImageSource::Compose(_) => true,
    };
    let inventory = devcontainer::recognized_unapplied(dc);
    // The CLI consumes the original repo file, so it is only safe when every
    // execution-affecting key is in the applied subset. Otherwise refused,
    // reserved, or unknown keys could reach the vendor process despite the
    // core overlay correctly dropping them on the OCI fallback path.
    let provider_eligible = source_present
        && outcome.pending.is_empty()
        && inventory.refused.is_empty()
        && inventory.reserved.is_empty()
        && inventory.unknown.is_empty();
    Some(TrustedDevcontainer {
        config_path: selected.selected.unwrap_or_default(),
        provider_eligible,
    })
}

/// The trust-gated devcontainer one-time lifecycle steps
/// (`onCreate`/`updateContent`/`postCreate`) for the provisioner to append
/// after `envplan::plan`. Empty when there's no devcontainer.json or the
/// lifecycle category isn't approved.
pub(crate) fn devcontainer_lifecycle_steps(
    repo_root: &Path,
    worktree: &str,
    workdir: &str,
) -> Vec<thegn_core::envplan::ProvisionStep> {
    let cfg = crate::hydrate::load_hydration_config();
    let sb = cfg.repo_sandbox(repo_root);
    if sb.devcontainer == thegn_core::config::DevcontainerMode::Off {
        return Vec::new();
    }
    let selector = cfg.repo_devcontainer_selector(repo_root);
    let selected = devcontainer_select::select_and_parse(Path::new(worktree), Some(&selector));
    let Some(dc) = selected.config else {
        return Vec::new();
    };
    let Ok(db) = Db::open() else {
        return Vec::new();
    };
    let approvals = approvals_for(&db, &repo_root.to_string_lossy());
    let allowed = sb.env_passthrough.clone();
    let local_env = |key: &str| {
        allowed
            .iter()
            .any(|allowed_key| allowed_key == key)
            .then(|| std::env::var(key).ok())
            .flatten()
    };
    let ctx = subst_ctx(worktree, &local_env);
    devcontainer_overlay::gated_steps(&dc, workdir, &ctx, &approvals)
}

/// The trust-gated devcontainer feature-install steps (run after the toolchain,
/// before lifecycle commands). Empty without a devcontainer.json, without
/// `features`, or until the `devcontainer.features` category is approved.
pub(crate) fn devcontainer_feature_steps(
    repo_root: &Path,
    worktree: &str,
) -> Vec<thegn_core::envplan::ProvisionStep> {
    let cfg = crate::hydrate::load_hydration_config();
    let sb = cfg.repo_sandbox(repo_root);
    if sb.devcontainer == thegn_core::config::DevcontainerMode::Off {
        return Vec::new();
    }
    let selector = cfg.repo_devcontainer_selector(repo_root);
    let selected = devcontainer_select::select_and_parse(Path::new(worktree), Some(&selector));
    let Some(dc) = selected.config else {
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
/// policy. Membership is DB-tracked (never path-inferred). See [`thegn_core::zone`].
fn apply_zone(db: &Db, cfg: &Config, worktree: &str, env: &mut Environment) {
    let Ok(Some(zrow)) = db.zone_of_worktree(worktree) else {
        return;
    };
    let Some(zc) = cfg.zone.get(&zrow.name) else {
        return;
    };
    let dropped = thegn_core::zone::apply_zone_ceilings(&mut env.sandbox, &zrow.name, zc);
    for d in &dropped {
        tracing::warn!(
            target: "thegn::config_trust", zone = %d.zone,
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
            let _ = db.put_notification("zone_egress", &issue, &msg, worktree); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
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
        tracing::warn!(target: "thegn::config_trust", "{line}");
    }
    for gr in pending {
        tracing::warn!(
            target: "thegn::config_trust", key = %gr.key,
            "repo overlay requests approval: {} ({})",
            gr.summary, thegn_core::repo_trust::request_id(&gr.canonical())
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
        thegn_core::repo_trust::request_id(&sig.join("\n"))
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
        "{}'s .thegn config: {denied} request(s) denied, {need} awaiting approval. \
         Review with `thegn repo-trust`.",
        Path::new(repo_root)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| repo_root.to_string())
    );
    let _ = db.put_notification(CLAMP_KIND, &issue_id, &msg, worktree); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
}
