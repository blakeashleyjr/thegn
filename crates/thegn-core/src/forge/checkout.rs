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
///
/// A LOCAL worktree answers from a memo keyed on every input this reads (config
/// files and their includes, HEAD and the refs its short name could be ambiguous
/// with; see [`crate::git_memo::checkout_fingerprint`]) -- hydration asks several
/// times per tick, each time forking `rev-parse` and up to a dozen `remote
/// get-url` / `config --get` (THE-718). Only a deterministic answer is stored; a
/// `git` that could not be spawned is not.
pub fn checkout_scope(loc: &GitLoc) -> Result<ForgeCheckoutScope, ForgeError> {
    type Memo = crate::git_memo::Memo<Result<ForgeCheckoutScope, ForgeError>>;
    static MEMO: std::sync::Mutex<Option<Memo>> = std::sync::Mutex::new(None);
    let GitLoc::Local(dir) = loc else {
        return checkout_scope_uncached(loc, &std::cell::Cell::new(false));
    };
    let fp = crate::git_memo::checkout_fingerprint(dir, &|k| std::env::var(k).ok());
    crate::git_memo::memoised_checked(&MEMO, 256, dir, fp, || {
        let transient = std::cell::Cell::new(false);
        let r = checkout_scope_uncached(loc, &transient);
        (r, !transient.get())
    })
}

/// `git -C <loc> <args>` trimmed stdout, `None` on a non-zero exit or empty
/// output; flags `transient` when git could not even be run.
fn out(loc: &GitLoc, args: &[&str], transient: &std::cell::Cell<bool>) -> Option<String> {
    match loc.git_command(args).output() {
        Err(_) => {
            transient.set(true);
            None
        }
        Ok(o) if !o.status.success() => None,
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (!s.is_empty()).then_some(s)
        }
    }
}

fn checkout_scope_uncached(
    loc: &GitLoc,
    transient: &std::cell::Cell<bool>,
) -> Result<ForgeCheckoutScope, ForgeError> {
    let branch = out(loc, &["rev-parse", "--abbrev-ref", "HEAD"], transient)
        .ok_or(ForgeError::NotConfigured("checkout branch".into()))?;
    let branch = branch.trim().to_string();
    scope_for_branch(loc, &branch, transient)
}

/// Capture repository scope for an explicitly named branch. This is used by
/// branch cleanup/browser actions whose requested branch can differ from the
/// worktree's current branch; current-status callers use [`checkout_scope`].
pub fn checkout_scope_for_branch(
    loc: &GitLoc,
    branch: &str,
) -> Result<ForgeCheckoutScope, ForgeError> {
    scope_for_branch(loc, branch, &std::cell::Cell::new(false))
}

fn scope_for_branch(
    loc: &GitLoc,
    branch: &str,
    transient: &std::cell::Cell<bool>,
) -> Result<ForgeCheckoutScope, ForgeError> {
    if branch.is_empty() || branch == "HEAD" || has_control(branch) {
        return Err(ForgeError::NoPr);
    }

    let origin = remote_identity(loc, "origin", false, "origin repository", transient)?;
    let base_remote = configured_remote(loc, &format!("branch.{branch}.remote"), transient)
        .map(|remote| local_remote(&remote))
        .unwrap_or_else(|| "origin".to_string());
    let base = remote_identity(loc, &base_remote, false, "base repository", transient)?;

    let push_remote = configured_remote(loc, &format!("branch.{branch}.pushRemote"), transient)
        .or_else(|| configured_remote(loc, "remote.pushDefault", transient))
        .or_else(|| configured_remote(loc, &format!("branch.{branch}.remote"), transient))
        .map(|remote| local_remote(&remote))
        .unwrap_or_else(|| "origin".to_string());
    let head = remote_identity(loc, &push_remote, true, "push repository", transient)?;

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

fn configured_remote(loc: &GitLoc, key: &str, transient: &std::cell::Cell<bool>) -> Option<String> {
    let output = match loc.git_command(&["config", "--get", key]).output() {
        Ok(o) => o,
        Err(_) => {
            transient.set(true);
            return None;
        }
    };
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
    transient: &std::cell::Cell<bool>,
) -> Result<ForgeRepoIdentity, ForgeError> {
    if remote.is_empty() || has_control(remote) {
        return Err(ForgeError::NotConfigured(what.into()));
    }
    let url = if push {
        out(loc, &["remote", "get-url", "--push", remote], transient)
    } else {
        out(loc, &["remote", "get-url", remote], transient)
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
        // Never inherit the developer's signing config. With `commit.gpgSign =
        // true` in a real ~/.gitconfig these fixtures block on gpg-agent and
        // fail ~60s later, so the suite passes or fails depending on whether a
        // passphrase happens to be cached — which is exactly how this reached
        // the merge gate green from one machine and red from another.
        let status = crate::util::git_cmd(dir)
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .status()
            .unwrap();
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
                if message.as_ref() == "push repository"
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

    /// THE-718: the scope is memoised per worktree, so every input it reads must
    /// invalidate it. Walk one repo through each kind of change, asking between
    /// every step (the memo is hot on each repeat ask).
    #[test]
    fn memoised_scope_follows_branch_remote_and_config_changes() {
        let dir = fixture();
        let loc = GitLoc::Local(dir.path().to_path_buf());
        git(dir.path(), &["branch", "-M", "topic"]);
        // No origin: a stable, memoisable error.
        assert!(checkout_scope(&loc).is_err());
        assert!(checkout_scope(&loc).is_err());
        git(
            dir.path(),
            &["remote", "add", "origin", "https://github.com/org/base.git"],
        );
        let scope = checkout_scope(&loc).unwrap();
        assert_eq!(
            (scope.branch.as_str(), scope.head.path.as_str()),
            ("topic", "org/base")
        );
        assert_eq!(checkout_scope(&loc).unwrap(), scope, "hot repeat");
        // Push URL retargets the head identity.
        git(
            dir.path(),
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                "git@github.com:me/fork.git",
            ],
        );
        assert_eq!(checkout_scope(&loc).unwrap().head.path, "me/fork");
        // Origin URL retarget.
        git(
            dir.path(),
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/org/moved.git",
            ],
        );
        assert_eq!(checkout_scope(&loc).unwrap().origin.path, "org/moved");
        // Branch switch changes the branch (and its per-branch config).
        git(dir.path(), &["checkout", "-qb", "other"]);
        assert_eq!(checkout_scope(&loc).unwrap().branch, "other");
        git(dir.path(), &["config", "branch.other.remote", "extra"]);
        git(
            dir.path(),
            &["remote", "add", "extra", "https://github.com/org/extra.git"],
        );
        assert_eq!(checkout_scope(&loc).unwrap().base.path, "org/extra");
        // A tag named like the branch makes `--abbrev-ref` print `heads/other`.
        git(dir.path(), &["tag", "other"]);
        assert_eq!(
            checkout_scope(&loc).unwrap().branch,
            "heads/other",
            "ambiguity is part of the answer, so it is part of the print"
        );
        // Detached HEAD has no checkout scope.
        git(dir.path(), &["tag", "-d", "other"]);
        git(dir.path(), &["checkout", "-q", "--detach"]);
        assert!(checkout_scope(&loc).is_err());
        // The directory going away is an error, not the last good answer.
        let gone = GitLoc::Local(dir.path().join("nope"));
        assert!(checkout_scope(&gone).is_err());
    }
}
