//! The repository identities used by every pull-request lookup.
//!
//! Git has three different notions of a remote for a checked-out branch: the
//! repository cloned as `origin`, the branch's merge remote, and the remote
//! which receives its pushes.  Resolving those independently at each provider
//! call made fork checkouts especially easy to misidentify.  This module
//! captures the complete scope once, before a provider request, and callers
//! carry that value through number resolution and the subsequent fetch.

use super::ForgeError;
use super::model::{ForgeRepoIdentity, repo_identity_from_remote_url};
use crate::remote::GitLoc;

/// The repository scope captured for one checkout operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForgeCheckoutScope {
    pub origin: ForgeRepoIdentity,
    pub base: ForgeRepoIdentity,
    pub head: ForgeRepoIdentity,
    pub branch: String,
}

/// Capture branch, origin, merge-base, and push-head repository identity from
/// one worktree. Missing branch configuration keeps the historical `origin`
/// default; a configured remote which cannot be read or parsed is an explicit
/// configuration error and is never silently replaced with `origin`.
pub fn checkout_scope(loc: &GitLoc) -> Result<ForgeCheckoutScope, ForgeError> {
    let branch = loc
        .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
        .ok_or(ForgeError::NotConfigured("checkout branch".into()))?;
    let branch = branch.trim().to_string();
    checkout_scope_for_branch(loc, &branch)
}

/// Capture repository scope for an explicitly named branch. This is used by
/// branch cleanup/browser actions whose requested branch can differ from the
/// worktree's current branch; current-status callers use [`checkout_scope`].
pub fn checkout_scope_for_branch(
    loc: &GitLoc,
    branch: &str,
) -> Result<ForgeCheckoutScope, ForgeError> {
    if branch.is_empty() || branch == "HEAD" || has_control(branch) {
        return Err(ForgeError::NoPr);
    }

    let origin = remote_identity(loc, "origin", false, "origin repository")?;
    let base_remote = configured_remote(loc, &format!("branch.{branch}.remote"))
        .map(|remote| local_remote(&remote))
        .unwrap_or_else(|| "origin".to_string());
    let base = remote_identity(loc, &base_remote, false, "base repository")?;

    let push_remote = configured_remote(loc, &format!("branch.{branch}.pushRemote"))
        .or_else(|| configured_remote(loc, "remote.pushDefault"))
        .or_else(|| configured_remote(loc, &format!("branch.{branch}.remote")))
        .map(|remote| local_remote(&remote))
        .unwrap_or_else(|| "origin".to_string());
    let head = remote_identity(loc, &push_remote, true, "push repository")?;

    if !origin.host.eq_ignore_ascii_case(&base.host)
        || !origin.host.eq_ignore_ascii_case(&head.host)
    {
        return Err(ForgeError::NotConfigured(
            "checkout repositories use different hosts".into(),
        ));
    }

    Ok(ForgeCheckoutScope {
        origin,
        base,
        head,
        branch: branch.to_string(),
    })
}

fn configured_remote(loc: &GitLoc, key: &str) -> Option<String> {
    let output = loc.git_command(&["config", "--get", key]).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn local_remote(remote: &str) -> String {
    if remote == "." {
        "origin".to_string()
    } else {
        remote.to_string()
    }
}

fn remote_identity(
    loc: &GitLoc,
    remote: &str,
    push: bool,
    what: &'static str,
) -> Result<ForgeRepoIdentity, ForgeError> {
    if remote.is_empty() || has_control(remote) {
        return Err(ForgeError::NotConfigured(what.into()));
    }
    let url = if push {
        loc.git_out(&["remote", "get-url", "--push", remote])
    } else {
        loc.git_out(&["remote", "get-url", remote])
    }
    .ok_or(ForgeError::NotConfigured(what.into()))?;
    repo_identity_from_remote_url(url.trim()).ok_or(ForgeError::NotConfigured(what.into()))
}

fn has_control(value: &str) -> bool {
    value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{ForgeCheckoutScope, checkout_scope, local_remote};
    use crate::forge::model::ForgeRepoIdentity;
    use crate::remote::GitLoc;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::util::git_cmd(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed: {status}");
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "scope-test"]);
        git(
            dir.path(),
            &["config", "user.email", "scope@example.invalid"],
        );
        std::fs::write(dir.path().join("README"), "scope\n").unwrap();
        git(dir.path(), &["add", "README"]);
        git(dir.path(), &["commit", "-qm", "scope"]);
        dir
    }

    #[test]
    fn local_branch_remote_maps_to_origin() {
        assert_eq!(local_remote("."), "origin");
        assert_eq!(local_remote("upstream"), "upstream");
    }

    #[test]
    fn scope_is_lossless_and_serializable() {
        let scope = ForgeCheckoutScope {
            origin: ForgeRepoIdentity {
                host: "github.example".into(),
                path: "user/fork".into(),
            },
            base: ForgeRepoIdentity {
                host: "github.example".into(),
                path: "org/base".into(),
            },
            head: ForgeRepoIdentity {
                host: "github.example".into(),
                path: "user/fork".into(),
            },
            branch: "123".into(),
        };
        let json = serde_json::to_string(&scope).expect("scope serializes");
        assert_eq!(
            serde_json::from_str::<ForgeCheckoutScope>(&json).unwrap(),
            scope
        );
    }

    #[test]
    fn captures_numeric_branch_fork_base_and_push_remote_precedence() {
        let dir = fixture();
        git(dir.path(), &["branch", "-M", "123"]);
        git(
            dir.path(),
            &["remote", "add", "origin", "git@github.com:user/fork.git"],
        );
        git(
            dir.path(),
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/org/base.git",
            ],
        );
        git(dir.path(), &["config", "branch.123.remote", "upstream"]);
        git(dir.path(), &["config", "branch.123.pushRemote", "origin"]);
        let scope = checkout_scope(&GitLoc::Local(dir.path().to_path_buf())).unwrap();
        assert_eq!(scope.branch, "123");
        assert_eq!(scope.origin.path, "user/fork");
        assert_eq!(scope.base.path, "org/base");
        assert_eq!(scope.head.path, "user/fork");
    }

    #[test]
    fn explicit_invalid_push_remote_is_not_replaced_by_origin() {
        let dir = fixture();
        git(dir.path(), &["branch", "-M", "topic"]);
        git(
            dir.path(),
            &["remote", "add", "origin", "https://github.com/org/base.git"],
        );
        git(
            dir.path(),
            &["config", "branch.topic.pushRemote", "missing"],
        );
        let result = checkout_scope(&GitLoc::Local(dir.path().to_path_buf()));
        assert!(matches!(
            &result,
            Err(crate::forge::ForgeError::NotConfigured(message))
                if message.to_string() == "push repository"
        ));
    }

    #[test]
    fn separate_push_url_is_the_head_identity_and_origin_tracking_is_base() {
        let dir = fixture();
        git(dir.path(), &["branch", "-M", "topic"]);
        git(
            dir.path(),
            &["remote", "add", "origin", "https://github.com/org/base.git"],
        );
        git(
            dir.path(),
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                "git@github.com:user/fork.git",
            ],
        );
        git(dir.path(), &["config", "branch.topic.remote", "origin"]);
        let scope = checkout_scope(&GitLoc::Local(dir.path().to_path_buf())).unwrap();
        assert_eq!(scope.base.path, "org/base");
        assert_eq!(scope.head.path, "user/fork");
    }
}
