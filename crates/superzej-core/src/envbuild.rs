//! Runtime construction of a named env's [`Placement`] + provider sandbox id —
//! `resolve_env`'s build step, extracted from the pinned `config.rs`. The id
//! derivation ([`effective_provider_id`]) is the single source of truth for the
//! per-worktree sandbox name; everything downstream (provisioning, exec,
//! checkpoint, teardown, `{id}` command templates) reads the resolved
//! `ProviderPlacement.id` rather than re-deriving.

use std::path::Path;

use crate::config::{EnvConfig, PlacementMode, RemoteTransport, SandboxConfig};
use crate::placement::{K8sPlacement, Placement, ProviderPlacement, SshPlacement, TransportKind};
use crate::remote::GitLoc;
use crate::util;

/// Max length of a resolved sandbox id. Sandbox names become DNS labels (e.g. the
/// sprite URL `<name>-xxxxx.sprites.app`), capped at 63 chars; leave headroom for
/// a provider suffix.
const MAX_PROVIDER_ID: usize = 50;

/// Resolve a provider env's effective sandbox id for `worktree`, expanding tokens
/// in the configured `id`:
/// - `{worktree}` — worktree dir basename slug (e.g. the branch slug)
/// - `{repo}` — the repo name (dir basename of `repo_root`), for readability
/// - `{hash}` — a short STABLE digest of the **full worktree path**, the
///   collision-defuser: two worktrees whose `{repo}`/`{worktree}` coincide
///   (same branch name in different repos, or two checkouts of one repo) still
///   get distinct ids because their paths differ
/// - `{slug}` — full worktree-path slug (also globally unique, but long/ugly)
///
/// An **empty** id uses the conflict-free default `"{repo}-{worktree}-{hash}"`.
/// A configured id with no token is returned as-is (a static, shared sandbox —
/// back-compat). The result is dash-collapsed and clamped to a DNS-safe length,
/// always preserving a path-hash suffix so the clamp can't reintroduce a
/// collision. The id flows into `ProviderPlacement.id` + `{id}` command templates,
/// so it MUST be derived identically everywhere — all callers resolve it through
/// this one function (the host reads `ProviderPlacement.id` rather than
/// re-deriving). `repo` is the repo dir name (`None` ⇒ `{repo}` expands empty).
pub fn effective_provider_id(configured: &str, worktree: &Path, repo: Option<&str>) -> String {
    let base = worktree
        .file_name()
        .map(|n| util::slugify(&n.to_string_lossy()))
        .unwrap_or_default();
    let c = configured.trim();
    let repo_slug = repo.map(util::slugify).unwrap_or_default();
    // 6 base36 chars of a stable hash of the absolute worktree path.
    let hash = util::short_hash(&worktree.to_string_lossy(), 6);

    // Empty ⇒ the conflict-free default. A no-token literal stays verbatim
    // (explicit shared sandbox); only token-bearing templates get expanded.
    let template = if c.is_empty() {
        "{repo}-{worktree}-{hash}"
    } else {
        c
    };
    let expanded = template
        .replace("{worktree}", &base)
        .replace("{slug}", &util::slugify(&worktree.to_string_lossy()))
        .replace("{repo}", &repo_slug)
        .replace("{hash}", &hash);
    // Collapse empty segments (e.g. a `None` repo leaving a leading dash) and trim.
    let name = expanded
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if name.len() <= MAX_PROVIDER_ID {
        return name;
    }
    // Too long: truncate but ALWAYS keep a trailing path-hash so the clamp can't
    // collapse two distinct worktrees onto one id.
    let keep = MAX_PROVIDER_ID.saturating_sub(hash.len() + 1);
    let head: String = name.chars().take(keep).collect();
    format!("{}-{hash}", head.trim_end_matches('-'))
}

/// Build the runtime [`Placement`] for a named env from its `[env.<name>]`
/// placement mode + the matching sub-table. For `ssh`, an empty `[env.*.ssh]
/// host` falls back to the worktree's own remote target, then `[sandbox.remote]`.
pub(crate) fn build_env_placement(
    envc: &EnvConfig,
    sb: &SandboxConfig,
    loc: &GitLoc,
    worktree: &Path,
    repo_root: &Path,
) -> Placement {
    let opt = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    // Repo name (dir basename) for the `{repo}` sandbox-id token.
    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty());
    match envc.placement {
        PlacementMode::Local => Placement::Local,
        PlacementMode::Ssh => {
            let kind = match envc.ssh.transport {
                RemoteTransport::Ssh => TransportKind::Ssh,
                RemoteTransport::Mosh => TransportKind::Mosh,
            };
            let (host, port, forward_agent) = if !envc.ssh.host.trim().is_empty() {
                let port = if envc.ssh.port == 0 {
                    22
                } else {
                    envc.ssh.port
                };
                (
                    envc.ssh.host.trim().to_string(),
                    port,
                    envc.ssh.forward_agent,
                )
            } else if let Some(t) = loc.ssh() {
                (t.host.clone(), t.port, t.forward_agent)
            } else {
                (
                    sb.remote.host.clone(),
                    sb.remote.port,
                    sb.remote.forward_agent,
                )
            };
            Placement::Ssh(SshPlacement {
                host,
                port,
                forward_agent,
                kind,
                ssh_config: opt(&envc.ssh.ssh_config),
                jump_host: opt(&envc.ssh.jump_host),
                identity: opt(&envc.ssh.identity),
                extra_args: envc.ssh.extra_args.clone(),
            })
        }
        PlacementMode::K8s => Placement::K8s(K8sPlacement {
            kubectl: opt(&envc.k8s.kubectl).unwrap_or_else(|| "kubectl".to_string()),
            context: opt(&envc.k8s.context),
            namespace: opt(&envc.k8s.namespace),
            pod: envc.k8s.pod.trim().to_string(),
            container: opt(&envc.k8s.container),
            pod_template: opt(&envc.k8s.pod_template).map(|p| util::expand_tilde(&p)),
            image: opt(&envc.k8s.image),
        }),
        PlacementMode::Provider => {
            // Per-worktree id: each worktree gets its own sandbox (so panes,
            // the bridge key, and the persisted location are all distinct).
            let id = effective_provider_id(&envc.provider.id, worktree, repo_name);
            let sub = |tpl: &[String]| {
                tpl.iter()
                    .map(|s| s.replace("{id}", &id))
                    .collect::<Vec<_>>()
            };
            // `exec_command`, or the szhost `vps-ssh` self-bridge for VPS
            // providers (which have no vendor CLI) — see
            // `EnvProviderConfig::control_command_template`.
            let control_prefix = sub(&envc.provider.control_command_template());
            let interactive_prefix = if envc.provider.interactive_command.is_empty() {
                control_prefix.clone()
            } else {
                sub(&envc.provider.interactive_command)
            };
            // Compute all substitutions before moving `id` into the struct (the
            // `sub` closure borrows it).
            let up_command = sub(&envc.provider.up_command);
            let down_command = sub(&envc.provider.down_command);
            Placement::Provider(ProviderPlacement {
                provider: envc.provider.provider.trim().to_string(),
                id,
                interactive_prefix,
                control_prefix,
                up_command,
                down_command,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vps_provider_defaults_to_the_szhost_self_bridge() {
        use crate::config::{EnvConfig, EnvProviderConfig, PlacementMode};
        let envc = EnvConfig {
            placement: PlacementMode::Provider,
            provider: EnvProviderConfig {
                provider: "hetzner".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let sb = crate::config::SandboxConfig::default();
        let wt = Path::new("/home/u/code/repo/worktrees/repo/dev");
        let loc = GitLoc::Local(wt.to_path_buf());
        let p = build_env_placement(&envc, &sb, &loc, wt, Path::new("/home/u/code/repo"));
        let Placement::Provider(pp) = p else {
            panic!("provider placement expected");
        };
        // The control prefix is the self-bridge with the RESOLVED id baked in —
        // panes, git reads, and the persisted location all route through it.
        assert_eq!(pp.control_prefix.len(), 4, "{:?}", pp.control_prefix);
        assert_eq!(pp.control_prefix[1], "vps-ssh");
        assert_eq!(pp.control_prefix[2], pp.id);
        assert_eq!(pp.control_prefix[3], "--");
        assert_eq!(pp.interactive_prefix, pp.control_prefix);
        // A configured exec_command still wins (no silent override).
        let mut with_cli = envc.clone();
        with_cli.provider.exec_command =
            vec!["mycli".into(), "ssh".into(), "{id}".into(), "--".into()];
        let Placement::Provider(pp2) =
            build_env_placement(&with_cli, &sb, &loc, wt, Path::new("/home/u/code/repo"))
        else {
            panic!("provider placement expected");
        };
        assert_eq!(pp2.control_prefix[0], "mycli");
        // Non-VPS providers with no exec_command keep an empty prefix.
        let mut sprites = envc.clone();
        sprites.provider.provider = "sprites".into();
        let Placement::Provider(pp3) =
            build_env_placement(&sprites, &sb, &loc, wt, Path::new("/home/u/code/repo"))
        else {
            panic!("provider placement expected");
        };
        assert!(pp3.control_prefix.is_empty());
    }

    #[test]
    fn effective_provider_id_is_per_worktree() {
        let wt = Path::new("/home/u/.superzej/worktrees/superzej/sz-quick-dagger");
        // Empty ⇒ conflict-free default: repo-worktree-hash.
        let def = effective_provider_id("", wt, Some("superzej"));
        assert!(
            def.starts_with("superzej-sz-quick-dagger-"),
            "default is repo-worktree-hash: {def}"
        );
        // Deterministic: same inputs ⇒ same id (must agree across call sites/runs).
        assert_eq!(def, effective_provider_id("", wt, Some("superzej")));
        // {worktree} ⇒ basename; {repo}/{hash} expand.
        assert_eq!(
            effective_provider_id("{worktree}", wt, Some("superzej")),
            "sz-quick-dagger"
        );
        assert_eq!(
            effective_provider_id("{repo}-{worktree}", wt, Some("superzej")),
            "superzej-sz-quick-dagger"
        );
        // {slug} ⇒ full-path slug (globally unique).
        assert!(effective_provider_id("{slug}", wt, None).contains("worktrees"));
        // A static id with no token stays shared (back-compat).
        assert_eq!(
            effective_provider_id("shared", wt, Some("superzej")),
            "shared"
        );

        // CONFLICT-FREE: two worktrees whose repo + branch basenames COINCIDE but
        // whose full paths differ get distinct ids (the {hash} disambiguates) —
        // e.g. a `main` worktree in two different checkouts.
        let a = Path::new("/home/u/code/repo-a/worktrees/x/main");
        let b = Path::new("/home/u/work/repo-b/worktrees/x/main");
        let ia = effective_provider_id("", a, Some("x"));
        let ib = effective_provider_id("", b, Some("x"));
        assert_ne!(ia, ib, "same repo+branch, different path ⇒ distinct id");

        // DNS-length clamp keeps it short and still ends in the path hash.
        let deep = Path::new(
            "/home/u/very/deeply/nested/path/that/keeps/going/superzej/worktrees/superzej/an-extremely-long-branch-name-that-overflows",
        );
        let long = effective_provider_id(
            "{repo}-{worktree}-{hash}",
            deep,
            Some("a-very-long-repo-name"),
        );
        assert!(
            long.len() <= MAX_PROVIDER_ID,
            "clamped: {} ({})",
            long,
            long.len()
        );
        assert!(
            long.ends_with(&util::short_hash(&deep.to_string_lossy(), 6)),
            "clamp preserves the path-hash suffix: {long}"
        );
    }
}
