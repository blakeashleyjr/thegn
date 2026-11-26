//! Worktree files that the host may execute or source after an agent run.
//!
//! This module is a pure path policy. Callers supply filesystem facts and
//! perform the resulting read-only binds.

use std::path::{Path, PathBuf};

/// Worktree paths used by host-executed tools and configuration.
pub const HOST_EXECUTED_WORKTREE_PATHS: &[&str] =
    &[".claude/", ".mcp.json", ".envrc", ".vscode/tasks.json"];

/// Facts collected without following symlinks for a host-executed path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostExecPathFacts {
    pub exists: bool,
    pub directory: bool,
    pub regular_file: bool,
    pub symlink: bool,
}

/// Existing, correctly typed host-executed paths beneath `worktree`.
pub fn existing_host_exec_pins<F>(worktree: &Path, mut facts: F) -> Vec<PathBuf>
where
    F: FnMut(&Path) -> HostExecPathFacts,
{
    HOST_EXECUTED_WORKTREE_PATHS
        .iter()
        .filter_map(|relative| {
            let path = worktree.join(relative.trim_end_matches('/'));
            let entry = facts(&path);
            let valid_type = if relative.ends_with('/') {
                entry.directory
            } else {
                entry.regular_file
            };
            (entry.exists && !entry.symlink && valid_type).then_some(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_existing_real_host_exec_entries_are_pinned() {
        let worktree = Path::new("/repo/wt");
        let pins = existing_host_exec_pins(worktree, |path| match path.file_name().unwrap() {
            name if name == ".claude" => HostExecPathFacts {
                exists: true,
                directory: true,
                ..HostExecPathFacts::default()
            },
            name if name == ".mcp.json" => HostExecPathFacts {
                exists: true,
                regular_file: true,
                ..HostExecPathFacts::default()
            },
            name if name == ".envrc" => HostExecPathFacts {
                exists: true,
                regular_file: true,
                symlink: true,
                ..HostExecPathFacts::default()
            },
            _ => HostExecPathFacts::default(),
        });
        assert_eq!(pins, [worktree.join(".claude"), worktree.join(".mcp.json")]);
    }
}
