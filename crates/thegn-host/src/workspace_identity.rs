//! Shared display identity for registering a workspace from a path.

use std::path::Path;

/// Match hydration's workspace registration rule for folder bookkeeping and
/// other path-driven callers: basename (or `workspace`) and repo/dir kind.
pub(crate) fn workspace_identity(
    repo_path: &str,
    known_kind: Option<&str>,
) -> (String, &'static str) {
    let path = Path::new(repo_path);
    let name = path
        .file_name()
        .map(|part| part.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());
    let kind = match known_kind {
        Some("repo") => "repo",
        Some("dir") => "dir",
        _ if thegn_core::repo::main_worktree(path).is_some() => "repo",
        _ => "dir",
    };
    (name, kind)
}
