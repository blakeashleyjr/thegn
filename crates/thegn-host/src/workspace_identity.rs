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

#[cfg(test)]
mod tests {
    use super::workspace_identity;

    #[test]
    fn known_kind_short_circuits_without_probing() {
        // Nonexistent path: a probe would say "dir", so "repo" proves the shortcut.
        let (name, kind) = workspace_identity("/nonexistent/thegn-test/app", Some("repo"));
        assert_eq!((name.as_str(), kind), ("app", "repo"));
        let (_, kind) = workspace_identity("/nonexistent/thegn-test/app", Some("dir"));
        assert_eq!(kind, "dir");
    }

    #[test]
    fn name_falls_back_to_workspace_without_basename() {
        let (name, kind) = workspace_identity("/", Some("dir"));
        assert_eq!((name.as_str(), kind), ("workspace", "dir"));
    }
}
