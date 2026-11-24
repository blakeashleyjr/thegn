//! The THE-791 workspace modes (alignment T4): how a sandbox's workload gets
//! the files it works on. Config `[sandbox.workspace] mode`, written
//! `workspace.mode` in prose.
//!
//! Stub landed by WS00-P1-14: the enum only, no config key and no caller.
//! Owner after WS00: WS11-P3-06c (the config key, the `worktrees.workspace_mode`
//! column, and the private-copy machinery with WS11-P3-06a..P3-19). Consumers:
//! WS02-P5-09 (private-copy refuses ephemeral storage), WS09-P6-01 (remote
//! worktrees), WS11.
//!
//! `git-worktree` is today's linked worktree and the interactive default;
//! untrusted launch classes default to `private-copy` (T2). The private-copy
//! gate is `Feature::PrivateCopy`. WS11's earlier `[sandbox.git] workspace`
//! key is replaced by this vocabulary before it ships.

use serde::{Deserialize, Serialize};

/// How a sandbox's workload gets its files (T4). Stub (WS00-P1-14); owner
/// WS11-P3-06c.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceMode {
    /// The workload edits its own private repository and only reviewed file
    /// changes return to the host.
    PrivateCopy,
    /// The workload edits a linked worktree that shares the host repository's
    /// objects and refs.
    #[default]
    GitWorktree,
    /// The workload edits the host directory in place.
    LiveMount,
}

impl WorkspaceMode {
    /// Every mode, in a stable order.
    pub const ALL: [WorkspaceMode; 3] = [
        WorkspaceMode::PrivateCopy,
        WorkspaceMode::GitWorktree,
        WorkspaceMode::LiveMount,
    ];

    /// One factual sentence on the files the workload can edit.
    pub const fn exposure_note(&self) -> &'static str {
        match *self {
            WorkspaceMode::PrivateCopy => {
                "The workload edits its own private repository and only reviewed file changes return to the host."
            }
            WorkspaceMode::GitWorktree => {
                "The workload edits a linked worktree that shares the host repository's objects and refs."
            }
            WorkspaceMode::LiveMount => "The workload edits the host directory in place.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_mode_kebab_roundtrip() {
        for (mode, text) in [
            (WorkspaceMode::PrivateCopy, "private-copy"),
            (WorkspaceMode::GitWorktree, "git-worktree"),
            (WorkspaceMode::LiveMount, "live-mount"),
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(serde_json::from_str::<WorkspaceMode>(&json).unwrap(), mode);
        }
        assert!(serde_json::from_str::<WorkspaceMode>("\"private-clone\"").is_err());
        assert!(serde_json::from_str::<WorkspaceMode>("\"linked\"").is_err());
    }

    #[test]
    fn workspace_mode_default_is_git_worktree() {
        assert_eq!(WorkspaceMode::default(), WorkspaceMode::GitWorktree);
        let notes: std::collections::HashSet<&str> = WorkspaceMode::ALL
            .iter()
            .map(|mode| mode.exposure_note())
            .collect();
        assert_eq!(notes.len(), WorkspaceMode::ALL.len());
        assert!(
            WorkspaceMode::PrivateCopy
                .exposure_note()
                .starts_with("The workload edits its own private repository")
        );
        assert!(
            WorkspaceMode::ALL
                .iter()
                .all(|mode| mode.exposure_note().ends_with('.')),
            "each note is one plain sentence"
        );
    }

    #[test]
    fn class_defaults_name_a_workspace_mode() {
        // The T2 table stores the mode as its config string; keep the two in step.
        let private_copy = serde_json::to_value(WorkspaceMode::PrivateCopy).unwrap();
        for (kind, defaults) in crate::launch_class::CLASS_DEFAULTS {
            assert_eq!(private_copy, defaults.workspace, "{kind:?}");
        }
    }
}
