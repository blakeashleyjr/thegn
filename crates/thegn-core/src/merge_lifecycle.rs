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
    /// A fold-actor land (`thegn land`) succeeded, but — unlike `Landed` — the
    /// worktree must stay exactly where it is. `thegn land` is routinely scripted
    /// from *inside* the worktree being landed (CI, the fold-actor, a sandboxed
    /// agent whose cwd it is), so its contract is leave-in-place: file the
    /// worktree into `merged_folder` under every `on_landed` arm that would keep
    /// it, and degrade the destructive `remove`/`detach` arms to that same filing
    /// rather than deleting the caller's working directory. Under `off` (the user
    /// has opted out of a Merged folder) it clears a stale lifecycle-folder
    /// membership (`Unfile`) so a worktree stranded in "Merging" is still healed.
    /// Records no queue row, so its filing is never an expiry-sweep candidate.
    LandedInPlace,
    /// The branch left the queue WITHOUT landing or failing — a plain dequeue
    /// (`merge rm` / `merge clear`, or the in-app remove/clear). Unlike
    /// `Landed`/`Failed` there is no new home for the worktree, so it should
    /// simply leave the lifecycle folder its enqueue filed it into and return to
    /// the ungrouped repo root. Without emitting this, a dequeued worktree is
    /// stranded in "Merging"/"Needs attention" forever — the sidebar/queue
    /// de-sync this event closes.
    Dequeued,
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
    /// Un-file the worktree from its lifecycle folder back to the ungrouped repo
    /// root, IF it currently sits in one. The host guards on the worktree's
    /// present folder so a folder the user filed it into by hand is left alone.
    Unfile,
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
        LifecycleEvent::Dequeued => LifecycleAction::Unfile,
        // A land-in-place keeps the worktree: file into `merged_folder` under
        // every arm that would retain it, and degrade the destructive
        // `remove`/`detach` arms to that same filing — `thegn land`'s
        // leave-in-place contract forbids deleting a worktree that is typically
        // the caller's own cwd. `off` still clears a stranded "Merging" membership.
        LifecycleEvent::LandedInPlace => match cfg.on_landed {
            OnLanded::Off => LifecycleAction::Unfile,
            OnLanded::Move | OnLanded::Expire | OnLanded::Detach | OnLanded::Remove => {
                file_into(&cfg.merged_folder)
            }
        },
        LifecycleEvent::Landed => match cfg.on_landed {
            OnLanded::Off => LifecycleAction::Noop,
            // `Expire` is `Move` at landing time — the difference is entirely in
            // the future, when `merge_sweep` collects it. Deciding it here would
            // mean deleting immediately, which is the behavior it exists to avoid.
            OnLanded::Move | OnLanded::Expire => file_into(&cfg.merged_folder),
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
            LifecycleEvent::LandedInPlace,
            LifecycleEvent::Failed,
            LifecycleEvent::Dequeued,
        ] {
            assert_eq!(decide(&c, ev), LifecycleAction::Noop);
        }
    }

    #[test]
    fn dequeue_unfiles_from_lifecycle_folder() {
        // A plain dequeue (rm/clear) has no land/fail destination — it just pulls
        // the worktree back out of the folder its enqueue filed it into. The host
        // guards which folders are actually un-filed; the decision is unconditional
        // once organizing is on. This is the fix for the sidebar/queue de-sync.
        assert_eq!(
            decide(&cfg(), LifecycleEvent::Dequeued),
            LifecycleAction::Unfile
        );
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
    fn landed_in_place_files_into_merged_for_all_keep_arms() {
        // Every non-off arm — including the destructive `remove`/`detach` — files
        // the worktree into Merged rather than removing it: `thegn land` must
        // never delete the (typically cwd) worktree it was scripted from.
        for on_landed in [
            OnLanded::Move,
            OnLanded::Expire,
            OnLanded::Detach,
            OnLanded::Remove,
        ] {
            let mut c = cfg();
            c.on_landed = on_landed;
            assert_eq!(
                decide(&c, LifecycleEvent::LandedInPlace),
                LifecycleAction::FileInto("Merged".into()),
                "LandedInPlace must file into Merged under {on_landed:?}"
            );
        }
    }

    #[test]
    fn landed_in_place_off_unfiles() {
        // With no Merged folder configured, a land-in-place still clears a stale
        // "Merging" membership rather than doing nothing (the stranding heal).
        let mut c = cfg();
        c.on_landed = OnLanded::Off;
        assert_eq!(
            decide(&c, LifecycleEvent::LandedInPlace),
            LifecycleAction::Unfile
        );
    }

    #[test]
    fn landed_in_place_never_removes_worktree() {
        // The leave-in-place contract as a table property: no `on_landed` value
        // can make a land-in-place delete the worktree or its branch.
        for on_landed in [
            OnLanded::Off,
            OnLanded::Move,
            OnLanded::Expire,
            OnLanded::Detach,
            OnLanded::Remove,
        ] {
            let mut c = cfg();
            c.on_landed = on_landed;
            assert!(
                !matches!(
                    decide(&c, LifecycleEvent::LandedInPlace),
                    LifecycleAction::RemoveWorktree { .. }
                ),
                "LandedInPlace must never remove ({on_landed:?})"
            );
        }
    }

    #[test]
    fn landed_in_place_empty_merged_folder_is_noop() {
        // A blank `merged_folder` degrades to the same empty-name Noop guard the
        // other filing events use — nowhere to file, so leave the worktree be.
        let mut c = cfg();
        c.merged_folder = "  ".into();
        assert_eq!(
            decide(&c, LifecycleEvent::LandedInPlace),
            LifecycleAction::Noop
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

    #[test]
    fn default_config_enables_full_lifecycle() {
        // The shipped default is "whole lifecycle on": file in-flight work into
        // folders, and on a clean land file the worktree into `merged_folder`.
        // Locks the default flip (organize_folders + on_landed) so a regression
        // back to inert is caught here, not in the field.
        let c = MergeQueueConfig::default();
        assert!(c.organize_folders);
        assert_eq!(
            decide(&c, LifecycleEvent::Enqueued),
            LifecycleAction::FileInto("Merging".into())
        );
        // Landing must NOT remove anything by default. The default is `expire`,
        // whose removal is the sweep's job once `merged_ttl_secs` is up — a
        // `RemoveWorktree` here would delete the worktree the instant the branch
        // landed, which is exactly the grace period's absence.
        assert_eq!(
            decide(&c, LifecycleEvent::Landed),
            LifecycleAction::FileInto("Merged".into()),
            "the default must not delete a worktree at landing time"
        );
        assert!(
            c.merged_ttl_secs > 0,
            "expire with no ttl would keep merged worktrees forever"
        );
        // A fold-actor land (`thegn land`) files into Merged too under the default
        // `expire` — leave-in-place, never a RemoveWorktree.
        assert_eq!(
            decide(&c, LifecycleEvent::LandedInPlace),
            LifecycleAction::FileInto("Merged".into()),
            "a default land-in-place files into Merged, it does not remove"
        );
        assert_eq!(
            decide(&c, LifecycleEvent::Failed),
            LifecycleAction::FileInto("Needs attention".into())
        );
    }
}
