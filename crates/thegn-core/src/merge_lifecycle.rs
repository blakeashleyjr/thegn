//! Merge-queue → sidebar-folder lifecycle policy (the pure half).
//!
//! As a worktree branch moves through the local merge queue (see
//! [`crate::fold`] + the host `integrate`/`merge_driver`), the shell can
//! optionally reorganize its worktree in the sidebar: file it into a "Merging"
//! folder when queued, then move it to "Merged" (or clean it up entirely) when
//! it lands, and shunt it to a "Needs attention" folder when it fails.
//!
//! This module is the **pure decision** — it maps a lifecycle event to an action
//! given `[merge_queue]` config, with no DB or git I/O — so it is exhaustively
//! unit-tested (the core coverage gate). The host executes the returned action in
//! `crates/thegn-host/src/merge_lifecycle.rs`.

use crate::config::{MergeQueueConfig, OnLanded};

/// A settled transition in a branch's merge-queue lifecycle. Only settled states
/// emit an event; transient ones (`folding`/`verifying`/`agent_running`/`ready`)
/// don't, so a branch mid-flight structurally stays in the queued folder until it
/// either lands or fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// The branch was (re-)enqueued (`queued`).
    Enqueued,
    /// The branch folded cleanly and advanced the target (`landed`).
    Landed,
    /// The branch could not land — conflict, red gate, or the agent gave up
    /// (`deferred` / `gate_failed` / `needs_human`).
    Failed,
}

/// What the host should do for a worktree in response to a [`LifecycleEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleAction {
    /// Do nothing.
    Noop,
    /// File the worktree into the named sidebar folder (find-or-create).
    FileInto(String),
    /// Remove the worktree; also delete its branch when `delete_branch`.
    RemoveWorktree { delete_branch: bool },
}

/// Map a lifecycle event to an action under the current config. The master
/// toggle is checked first, so the whole feature is inert when
/// `organize_folders = false`. An empty folder name means "don't file".
pub fn decide(cfg: &MergeQueueConfig, event: LifecycleEvent) -> LifecycleAction {
    if !cfg.organize_folders {
        return LifecycleAction::Noop;
    }
    match event {
        LifecycleEvent::Enqueued => file_into(&cfg.queued_folder),
        LifecycleEvent::Failed => file_into(&cfg.failed_folder),
        LifecycleEvent::Landed => match cfg.on_landed {
            OnLanded::Off => LifecycleAction::Noop,
            OnLanded::Move => file_into(&cfg.merged_folder),
            OnLanded::Detach => LifecycleAction::RemoveWorktree {
                delete_branch: false,
            },
            OnLanded::Remove => LifecycleAction::RemoveWorktree {
                delete_branch: true,
            },
        },
    }
}

/// `FileInto` unless the name is blank/whitespace, in which case `Noop`.
fn file_into(name: &str) -> LifecycleAction {
    if name.trim().is_empty() {
        LifecycleAction::Noop
    } else {
        LifecycleAction::FileInto(name.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MergeQueueConfig {
        MergeQueueConfig {
            organize_folders: true,
            queued_folder: "Merging".into(),
            on_landed: OnLanded::Move,
            merged_folder: "Merged".into(),
            failed_folder: "Needs attention".into(),
            ..MergeQueueConfig::default()
        }
    }

    #[test]
    fn master_toggle_off_is_inert() {
        let mut c = cfg();
        c.organize_folders = false;
        for ev in [
            LifecycleEvent::Enqueued,
            LifecycleEvent::Landed,
            LifecycleEvent::Failed,
        ] {
            assert_eq!(decide(&c, ev), LifecycleAction::Noop);
        }
    }

    #[test]
    fn enqueue_files_into_queued_folder() {
        assert_eq!(
            decide(&cfg(), LifecycleEvent::Enqueued),
            LifecycleAction::FileInto("Merging".into())
        );
    }

    #[test]
    fn failure_files_into_failed_folder() {
        assert_eq!(
            decide(&cfg(), LifecycleEvent::Failed),
            LifecycleAction::FileInto("Needs attention".into())
        );
    }

    #[test]
    fn landed_move_files_into_merged_folder() {
        assert_eq!(
            decide(&cfg(), LifecycleEvent::Landed),
            LifecycleAction::FileInto("Merged".into())
        );
    }

    #[test]
    fn landed_off_is_noop() {
        let mut c = cfg();
        c.on_landed = OnLanded::Off;
        assert_eq!(decide(&c, LifecycleEvent::Landed), LifecycleAction::Noop);
    }

    #[test]
    fn landed_detach_keeps_branch() {
        let mut c = cfg();
        c.on_landed = OnLanded::Detach;
        assert_eq!(
            decide(&c, LifecycleEvent::Landed),
            LifecycleAction::RemoveWorktree {
                delete_branch: false
            }
        );
    }

    #[test]
    fn landed_remove_deletes_branch() {
        let mut c = cfg();
        c.on_landed = OnLanded::Remove;
        assert_eq!(
            decide(&c, LifecycleEvent::Landed),
            LifecycleAction::RemoveWorktree {
                delete_branch: true
            }
        );
    }

    #[test]
    fn empty_folder_name_is_noop() {
        let mut c = cfg();
        c.queued_folder = "  ".into();
        c.failed_folder = String::new();
        assert_eq!(decide(&c, LifecycleEvent::Enqueued), LifecycleAction::Noop);
        assert_eq!(decide(&c, LifecycleEvent::Failed), LifecycleAction::Noop);
    }

    #[test]
    fn folder_name_is_trimmed() {
        let mut c = cfg();
        c.queued_folder = "  Merging  ".into();
        assert_eq!(
            decide(&c, LifecycleEvent::Enqueued),
            LifecycleAction::FileInto("Merging".into())
        );
    }

    #[test]
    fn on_landed_enum_parse_roundtrip() {
        assert_eq!(
            OnLanded::from_str_validated("move").unwrap(),
            OnLanded::Move
        );
        assert_eq!(
            OnLanded::from_str_validated("folder").unwrap(),
            OnLanded::Move
        );
        assert_eq!(
            OnLanded::from_str_validated("cleanup").unwrap(),
            OnLanded::Remove
        );
        assert_eq!(OnLanded::from_str_validated("none").unwrap(), OnLanded::Off);
        assert!(OnLanded::from_str_validated("bogus").is_err());
        assert_eq!(OnLanded::Detach.as_str(), "detach");
        assert_eq!(OnLanded::default(), OnLanded::Off);
    }
}
