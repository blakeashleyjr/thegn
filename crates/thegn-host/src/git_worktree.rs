//! Shared host-side worktree creation lifecycle.
//!
//! Core owns branch/path policy; this module owns the host adapter that sends
//! git mutations through GitBackend and performs best-effort submodule setup on
//! the worker that already owns creation.

use std::path::Path;

use thegn_core::config::{Config, SubmoduleMode, WorktreeMode};
use thegn_core::git_run::{GitOutcome, GitTextError};
use thegn_core::remote::GitLoc;
use thegn_core::store::RepoTrustStore;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthorityMetadata {
    text: String,
    blob: Option<String>,
}

fn authority_blob(loc: &GitLoc) -> Result<Option<String>, String> {
    let oid = match loc.git_text(&["rev-parse", "--verify", "--quiet", "HEAD:.gitmodules"]) {
        Ok(oid) => oid,
        Err(error) if probe_absent(&error) => return Ok(None),
        Err(error) => {
            return Err(format!(
                "submodule metadata unavailable at the execution authority: {error}"
            ));
        }
    };
    let valid = matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return Err("submodule metadata unavailable at the execution authority".into());
    }
    Ok(Some(oid))
}

fn probe_absent(error: &GitTextError) -> bool {
    matches!(
        error,
        GitTextError::Failed(GitOutcome::Exit {
            code: Some(1),
            stderr_tail,
        }) if stderr_tail.is_empty()
    )
}

fn authority_check(loc: &GitLoc, approved: &Option<String>) -> Result<(), String> {
    let changed = || {
        "submodules not initialized: .gitmodules changed at the execution authority after approval; retry"
            .to_string()
    };
    if authority_blob(loc)?.as_ref() != approved.as_ref() {
        return Err(changed());
    }
    let current = loc
        .git_text(&["hash-object", "--", ".gitmodules"])
        .map_err(|error| {
            format!("submodule metadata unavailable at the execution authority: {error}")
        })?;
    if Some(current.as_str()) != approved.as_deref() {
        return Err(changed());
    }
    Ok(())
}

fn authority_metadata(
    worktree: &Path,
    loc: Option<&GitLoc>,
) -> Result<Option<AuthorityMetadata>, String> {
    if let Some(location) = loc.filter(|location| location.is_remote()) {
        let Some(blob) = authority_blob(location)? else {
            match location.git_text(&["rev-parse", "--verify", "HEAD"]) {
                Ok(_) => return Ok(None),
                Err(error) => {
                    return Err(format!(
                        "submodule metadata unavailable at the execution authority: {error}"
                    ));
                }
            }
        };
        // An empty committed .gitmodules is invalid metadata and fails closed.
        let text = location
            .git_text(&["cat-file", "blob", &blob])
            .map_err(|error| {
                format!("submodule metadata unavailable at the execution authority: {error}")
            })?;
        if text.is_empty() {
            return Err(
                "submodule metadata unavailable at the execution authority: committed .gitmodules is empty"
                    .into(),
            );
        }
        return Ok(Some(AuthorityMetadata {
            text,
            blob: Some(blob),
        }));
    }

    let path = worktree.join(".gitmodules");
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("submodule metadata unreadable: {error}"))?;
    Ok(Some(AuthorityMetadata { text, blob: None }))
}

fn direct_submodule_request(
    specs: &[thegn_core::submodule::SubmoduleSpec],
) -> thegn_core::config_resolve::GatedRequest {
    thegn_core::config_resolve::GatedRequest {
        key: "git.submodules".into(),
        value: serde_json::json!(
            specs
                .iter()
                .map(|spec| serde_json::json!({"path": spec.path.trim(), "url": spec.url.trim()}))
                .collect::<Vec<_>>()
        ),
        summary: format!("initialize {} direct submodule(s)", specs.len()),
    }
}

/// Create a linked worktree through the service seam. Directory preparation and
/// the in-repo exclude are host policy; the git mutation is never a core
/// subprocess at this call site.
#[allow(dead_code)]
pub(crate) fn add_checked(
    root: &Path,
    branch: &str,
    base: &str,
    path: &Path,
    cfg: &Config,
) -> Result<(), String> {
    if cfg.worktree_mode == WorktreeMode::InRepo {
        let exclude = root.join(".git/info/exclude");
        if let Ok(contents) = std::fs::read_to_string(&exclude)
            && !contents.lines().any(|line| line == ".worktrees/")
        {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&exclude) {
                let _ = writeln!(file, ".worktrees/");
            }
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let loc = GitLoc::for_worktree(root);
    thegn_svc::git::GitBackend::add_worktree(&*crate::git_handle::get(), root, branch, base, path)
        .map_err(|error| {
            format!(
                "git worktree add failed (branch={branch} base={base} at {}): {error}",
                loc.path()
            )
        })
}

#[allow(dead_code)]
pub(crate) fn remove(root: &Path, path: &Path, delete_branch: bool) -> bool {
    let removed = crate::git_handle::get()
        .remove_worktree(root, path, delete_branch)
        .is_ok();
    if removed {
        let _ = std::fs::remove_dir_all(path);
    }
    removed
}

/// Initialize approved direct submodules after a checkout exists. Nested
/// submodules are left untouched until their metadata closure can be approved.
/// Ok(false) means policy disabled or there was no root-level .gitmodules;
/// failures are non-fatal to worktree creation and are reported by the caller.
pub(crate) fn initialize(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    loc: Option<&GitLoc>,
) -> Result<bool, String> {
    if cfg.repo_git(repo_root).submodules == SubmoduleMode::Off {
        return Ok(false);
    }
    let Some(metadata) = authority_metadata(worktree, loc)? else {
        return Ok(false);
    };
    let specs = thegn_core::submodule::parse_gitmodules(&metadata.text)
        .map_err(|e| format!("submodule metadata invalid: {e}"))?;
    if specs.is_empty() {
        return Ok(false);
    }
    let request = direct_submodule_request(&specs);
    let root_s = repo_root.to_string_lossy();
    let approved = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_trust_approved(&root_s).ok())
        .is_some_and(|approved| thegn_core::repo_trust::is_approved(&request, &approved));
    if !approved {
        let id = thegn_core::repo_trust::request_id(&request.canonical());
        return Err(format!(
            "submodules not initialized: trust approval required (repo trust request {id})"
        ));
    }
    if let Some(location) = loc.filter(|location| location.is_remote()) {
        authority_check(location, &metadata.blob)?;
    }
    let location = loc
        .cloned()
        .unwrap_or_else(|| GitLoc::for_worktree(worktree));
    crate::git_handle::get()
        .init_submodules(&location, false)
        .map(|_| true)
        .map_err(|e| format!("submodules not initialized: {e}"))
}

/// Whether a remote clone may initialize the approved direct submodules. This is
/// kept host-side because the remote script must not make an independent trust
/// decision or enable a git transport override. Nested initialization remains
/// disabled until its metadata closure can be pinned and approved.
pub(crate) fn recursive_submodules_allowed(cfg: &Config, repo_root: &Path) -> bool {
    if cfg.repo_git(repo_root).submodules == SubmoduleMode::Off {
        return false;
    }
    let Ok(metadata) = std::fs::read_to_string(repo_root.join(".gitmodules")) else {
        return false;
    };
    let Ok(specs) = thegn_core::submodule::parse_gitmodules(&metadata) else {
        return false;
    };
    if specs.is_empty() {
        return false;
    }
    let request = direct_submodule_request(&specs);
    let root_s = repo_root.to_string_lossy();
    thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_trust_approved(&root_s).ok())
        .is_some_and(|approved| thegn_core::repo_trust::is_approved(&request, &approved))
}

#[cfg(test)]
mod tests {
    use super::{authority_blob, authority_check, authority_metadata, direct_submodule_request};
    use std::path::Path;
    use thegn_core::remote::GitLoc;

    const MODULES_A: &str =
        "[submodule \"a\"]\n\tpath = a\n\turl = https://example.invalid/a.git\n";
    const MODULES_B: &str =
        "[submodule \"b\"]\n\tpath = b\n\turl = https://example.invalid/b.git\n";

    #[expect(clippy::disallowed_methods, reason = "test fixture git command")]
    fn git(dir: &Path, args: &[&str]) {
        let mut command = thegn_core::util::git_cmd(dir);
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
            ])
            .args(args);
        let output = command.output().expect("git fixture command starts");
        assert!(
            output.status.success(),
            "git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn provider(path: &Path) -> GitLoc {
        GitLoc::Provider {
            control_prefix: vec!["env".into()],
            path: path.to_string_lossy().into_owned(),
        }
    }

    fn failing_provider(wrapper_dir: &Path, path: &Path, needle: &str, exit_code: u8) -> GitLoc {
        let wrapper = wrapper_dir.join("git-wrapper.sh");
        std::fs::write(
            &wrapper,
            format!(
                "case \"$3\" in *'{needle}'*) echo 'ssh: connect to host fixture: Connection refused' >&2; exit {exit_code};; esac\nexec \"$@\"\n"
            ),
        )
        .expect("write provider wrapper");
        GitLoc::Provider {
            control_prefix: vec!["sh".into(), wrapper.to_string_lossy().into_owned()],
            path: path.to_string_lossy().into_owned(),
        }
    }

    fn committed_repo(dir: &Path, modules: Option<&str>) {
        git(dir, &["init", "-q"]);
        if let Some(contents) = modules {
            std::fs::write(dir.join(".gitmodules"), contents).expect("write fixture metadata");
            git(dir, &["add", ".gitmodules"]);
        }
        git(dir, &["commit", "-q", "--allow-empty", "-m", "fixture"]);
    }

    #[test]
    fn submodule_authority_provider_reads_metadata_at_the_authority() {
        let host = tempfile::tempdir().expect("host fixture dir");
        std::fs::write(host.path().join(".gitmodules"), MODULES_A).expect("write host metadata");
        let provider_dir = tempfile::tempdir().expect("provider fixture dir");
        committed_repo(provider_dir.path(), Some(MODULES_B));
        let location = provider(provider_dir.path());
        let impossible_host_worktree = provider_dir.path().join("missing-host-worktree");

        let metadata = authority_metadata(&impossible_host_worktree, Some(&location))
            .expect("provider metadata is readable")
            .expect("provider has committed metadata");
        assert_eq!(metadata.text, MODULES_B.trim());
        assert!(metadata.blob.is_some());
    }

    #[test]
    fn submodule_authority_never_falls_back_to_the_host_root() {
        let host = tempfile::tempdir().expect("host fixture dir");
        std::fs::write(host.path().join(".gitmodules"), MODULES_A).expect("write host metadata");
        let provider_dir = tempfile::tempdir().expect("provider fixture dir");
        committed_repo(provider_dir.path(), None);

        let metadata = authority_metadata(
            &host.path().join("not-the-provider-worktree"),
            Some(&provider(provider_dir.path())),
        )
        .expect("provider commit has no metadata");
        assert!(metadata.is_none());
    }

    #[test]
    fn submodule_authority_unreadable_provider_is_an_error() {
        let missing = tempfile::tempdir().expect("provider fixture dir");
        let absent_provider = missing.path().join("absent");
        let result = authority_metadata(
            &missing.path().join("unreadable"),
            Some(&provider(&absent_provider)),
        );
        assert!(matches!(result, Err(message) if message.contains("execution authority")));
    }

    #[test]
    fn submodule_authority_dirty_worktree_metadata_is_refused() {
        let provider_dir = tempfile::tempdir().expect("provider fixture dir");
        committed_repo(provider_dir.path(), Some(MODULES_A));
        let location = provider(provider_dir.path());
        let metadata = authority_metadata(provider_dir.path(), Some(&location))
            .expect("provider metadata is readable")
            .expect("fixture has metadata");

        std::fs::write(provider_dir.path().join(".gitmodules"), MODULES_B)
            .expect("write uncommitted metadata");

        let result = authority_check(&location, &metadata.blob);
        assert!(
            matches!(result, Err(message) if message.contains("changed at the execution authority"))
        );
    }

    #[test]
    fn submodule_authority_request_is_unchanged_for_existing_approvals() {
        let specs =
            thegn_core::submodule::parse_gitmodules(MODULES_A).expect("valid fixture metadata");
        let request = direct_submodule_request(&specs);

        assert_eq!(
            request.canonical(),
            "{\"key\":\"git.submodules\",\"value\":[{\"path\":\"a\",\"url\":\"https://example.invalid/a.git\"}]}"
        );
    }

    #[test]
    fn submodule_authority_blob_change_is_detected() {
        let provider_dir = tempfile::tempdir().expect("provider fixture dir");
        committed_repo(provider_dir.path(), Some(MODULES_A));
        let location = provider(provider_dir.path());
        let approved_blob = authority_blob(&location)
            .expect("metadata probe succeeds")
            .expect("fixture has metadata");

        std::fs::write(provider_dir.path().join(".gitmodules"), MODULES_B)
            .expect("write changed metadata");
        git(provider_dir.path(), &["add", ".gitmodules"]);
        git(
            provider_dir.path(),
            &["commit", "-q", "-m", "changed fixture"],
        );

        let current_blob = authority_blob(&location)
            .expect("changed metadata probe succeeds")
            .expect("changed fixture has metadata");
        assert_ne!(approved_blob, current_blob);
    }

    #[test]
    fn submodule_authority_transport_failure_is_not_absent() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let wrapper_dir = tempfile::tempdir().expect("wrapper dir");
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path(), Some(MODULES_A));
        let location = failing_provider(wrapper_dir.path(), repo.path(), "HEAD:.gitmodules", 255);

        let result = authority_metadata(repo.path(), Some(&location));
        assert!(matches!(result, Err(message)
            if message.contains("execution authority")
                && message.contains("Connection refused")));
    }

    #[test]
    fn submodule_authority_hash_failure_reports_git_reason() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path(), Some(MODULES_A));
        let approved = authority_blob(&provider(repo.path()))
            .expect("metadata probe succeeds")
            .expect("fixture has metadata");
        let wrapper_dir = tempfile::tempdir().expect("wrapper dir");
        let location = failing_provider(wrapper_dir.path(), repo.path(), "hash-object", 255);

        let result = authority_check(&location, &Some(approved));
        assert!(matches!(result, Err(message) if message.contains("Connection refused")));
    }

    #[test]
    fn submodule_authority_absent_metadata_is_still_none() {
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path(), None);

        let result = authority_metadata(repo.path(), Some(&provider(repo.path())))
            .expect("exit 1 means absent metadata");
        assert!(result.is_none());
    }

    #[test]
    fn submodule_authority_exit_one_with_stderr_is_not_absent() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let wrapper_dir = tempfile::tempdir().expect("wrapper dir");
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path(), Some(MODULES_A));
        let location = failing_provider(wrapper_dir.path(), repo.path(), "HEAD:.gitmodules", 1);

        let result = authority_metadata(repo.path(), Some(&location));
        assert!(matches!(result, Err(message)
            if message.contains("execution authority")
                && message.contains("Connection refused")));
    }

    #[test]
    fn submodule_authority_empty_committed_metadata_is_refused() {
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path(), Some(""));

        let result = authority_metadata(repo.path(), Some(&provider(repo.path())));
        assert!(matches!(result, Err(message)
            if message.contains("execution authority")
                && message.contains("empty")));
    }
}
