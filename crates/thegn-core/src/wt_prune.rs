//! `thegn wt prune --merged`: which worktrees are safe to remove.
//!
//! Pure policy over facts the host gathered; no I/O. The host module of the
//! same name collects the facts and performs the removals.
//!
//! Refusals are checked in a fixed order, and the first one that applies is the
//! reported reason:
//!
//! 1. [`KeepReason::PrimaryCheckout`] — the repo's main checkout.
//! 2. [`KeepReason::TargetBranch`] — the worktree holds the merge target itself.
//! 3. [`KeepReason::DetachedHead`] — no branch to prove merged.
//! 4. [`KeepReason::NotMerged`] — the branch is not an ancestor of the target
//!    (or Git could not say; unknown is never "merged").
//! 5. [`KeepReason::Dirty`] — uncommitted or untracked files.
//! 6. [`KeepReason::LiveProcess`] — a process has its cwd inside the worktree.
//! 7. [`KeepReason::LiveSession`] — held by a live session or a queue row.

/// Facts about one worktree, gathered by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneFacts {
    pub path: String,
    /// Checked-out branch; `None` for a detached HEAD.
    pub branch: Option<String>,
    /// This is the repo's primary (main) checkout.
    pub is_primary: bool,
    /// The worktree's branch is the merge target.
    pub is_target_branch: bool,
    /// The branch is an ancestor of the target (unknown counts as `false`).
    pub ancestor_of_target: bool,
    /// `git status --porcelain` is empty (unknown counts as `false`).
    pub tree_clean: bool,
    /// Pids whose cwd lies inside the worktree.
    pub live_cwd_pids: Vec<u32>,
    /// Held by a running session or an active queue row.
    pub held_by_session: bool,
}

/// Why a worktree is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    PrimaryCheckout,
    TargetBranch,
    DetachedHead,
    NotMerged,
    Dirty,
    LiveProcess,
    LiveSession,
}

impl KeepReason {
    /// Stable machine-readable token (JSON output).
    pub const fn code(self) -> &'static str {
        match self {
            Self::PrimaryCheckout => "primary-checkout",
            Self::TargetBranch => "target-branch",
            Self::DetachedHead => "detached-head",
            Self::NotMerged => "not-merged",
            Self::Dirty => "dirty",
            Self::LiveProcess => "live-process",
            Self::LiveSession => "live-session",
        }
    }

    /// Human sentence fragment.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::PrimaryCheckout => "primary checkout",
            Self::TargetBranch => "holds the target branch",
            Self::DetachedHead => "detached HEAD",
            Self::NotMerged => "not merged into the target",
            Self::Dirty => "uncommitted or untracked changes",
            Self::LiveProcess => "a process is running inside it",
            Self::LiveSession => "held by a live session",
        }
    }
}

/// The verdict for one worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneDecision {
    /// Remove the worktree; `delete_branch` also deletes its (merged) branch.
    Remove {
        delete_branch: bool,
    },
    Keep(KeepReason),
}

/// Decide one worktree. See the module docs for the refusal order.
pub fn decide(f: &PruneFacts) -> PruneDecision {
    use KeepReason::*;
    if f.is_primary {
        return PruneDecision::Keep(PrimaryCheckout);
    }
    if f.is_target_branch {
        return PruneDecision::Keep(TargetBranch);
    }
    if f.branch.as_deref().is_none_or(str::is_empty) {
        return PruneDecision::Keep(DetachedHead);
    }
    if !f.ancestor_of_target {
        return PruneDecision::Keep(NotMerged);
    }
    if !f.tree_clean {
        return PruneDecision::Keep(Dirty);
    }
    if !f.live_cwd_pids.is_empty() {
        return PruneDecision::Keep(LiveProcess);
    }
    if f.held_by_session {
        return PruneDecision::Keep(LiveSession);
    }
    PruneDecision::Remove {
        delete_branch: true,
    }
}

/// Decide every worktree, preserving input order.
pub fn plan(facts: Vec<PruneFacts>) -> Vec<(String, PruneDecision)> {
    facts
        .into_iter()
        .map(|f| {
            let d = decide(&f);
            (f.path, d)
        })
        .collect()
}

/// One summary line for a decision, e.g. `remove /p (and branch)` or
/// `keep /p: not merged into the target`.
pub fn render(path: &str, decision: &PruneDecision) -> String {
    match decision {
        PruneDecision::Remove {
            delete_branch: true,
        } => format!("remove {path} (and branch)"),
        PruneDecision::Remove { .. } => format!("remove {path}"),
        PruneDecision::Keep(r) => format!("keep   {path}: {}", r.describe()),
    }
}

/// `(removable, kept)` counts of a plan.
pub fn counts(plan: &[(String, PruneDecision)]) -> (usize, usize) {
    let removable = plan
        .iter()
        .filter(|(_, d)| matches!(d, PruneDecision::Remove { .. }))
        .count();
    (removable, plan.len() - removable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn removable() -> PruneFacts {
        PruneFacts {
            path: "/w/a".into(),
            branch: Some("tg/a".into()),
            is_primary: false,
            is_target_branch: false,
            ancestor_of_target: true,
            tree_clean: true,
            live_cwd_pids: vec![],
            held_by_session: false,
        }
    }

    #[test]
    fn merged_clean_idle_is_removed_with_branch() {
        assert_eq!(
            decide(&removable()),
            PruneDecision::Remove {
                delete_branch: true
            }
        );
    }

    #[test]
    fn each_reason_fires_alone() {
        let cases: Vec<(Box<dyn Fn(&mut PruneFacts)>, KeepReason)> = vec![
            (
                Box::new(|f| f.is_primary = true),
                KeepReason::PrimaryCheckout,
            ),
            (
                Box::new(|f| f.is_target_branch = true),
                KeepReason::TargetBranch,
            ),
            (Box::new(|f| f.branch = None), KeepReason::DetachedHead),
            (
                Box::new(|f| f.branch = Some(String::new())),
                KeepReason::DetachedHead,
            ),
            (
                Box::new(|f| f.ancestor_of_target = false),
                KeepReason::NotMerged,
            ),
            (Box::new(|f| f.tree_clean = false), KeepReason::Dirty),
            (
                Box::new(|f| f.live_cwd_pids = vec![7]),
                KeepReason::LiveProcess,
            ),
            (
                Box::new(|f| f.held_by_session = true),
                KeepReason::LiveSession,
            ),
        ];
        for (mutate, want) in cases {
            let mut f = removable();
            mutate(&mut f);
            assert_eq!(decide(&f), PruneDecision::Keep(want));
        }
    }

    #[test]
    fn refusal_order_is_fixed() {
        let mut f = removable();
        f.is_primary = true;
        f.is_target_branch = true;
        f.branch = None;
        f.ancestor_of_target = false;
        f.tree_clean = false;
        f.live_cwd_pids = vec![1];
        f.held_by_session = true;
        let order = [
            KeepReason::PrimaryCheckout,
            KeepReason::TargetBranch,
            KeepReason::DetachedHead,
            KeepReason::NotMerged,
            KeepReason::Dirty,
            KeepReason::LiveProcess,
            KeepReason::LiveSession,
        ];
        for want in order {
            assert_eq!(decide(&f), PruneDecision::Keep(want));
            match want {
                KeepReason::PrimaryCheckout => f.is_primary = false,
                KeepReason::TargetBranch => f.is_target_branch = false,
                KeepReason::DetachedHead => f.branch = Some("b".into()),
                KeepReason::NotMerged => f.ancestor_of_target = true,
                KeepReason::Dirty => f.tree_clean = true,
                KeepReason::LiveProcess => f.live_cwd_pids.clear(),
                KeepReason::LiveSession => f.held_by_session = false,
            }
        }
        assert!(matches!(decide(&f), PruneDecision::Remove { .. }));
    }

    #[test]
    fn plan_preserves_order_and_counts() {
        let mut dirty = removable();
        dirty.path = "/w/b".into();
        dirty.tree_clean = false;
        let p = plan(vec![removable(), dirty]);
        assert_eq!(p[0].0, "/w/a");
        assert_eq!(p[1].0, "/w/b");
        assert_eq!(counts(&p), (1, 1));
    }

    #[test]
    fn render_and_codes_cover_every_variant() {
        assert_eq!(
            render(
                "/p",
                &PruneDecision::Remove {
                    delete_branch: true
                }
            ),
            "remove /p (and branch)"
        );
        assert_eq!(
            render(
                "/p",
                &PruneDecision::Remove {
                    delete_branch: false
                }
            ),
            "remove /p"
        );
        let all = [
            KeepReason::PrimaryCheckout,
            KeepReason::TargetBranch,
            KeepReason::DetachedHead,
            KeepReason::NotMerged,
            KeepReason::Dirty,
            KeepReason::LiveProcess,
            KeepReason::LiveSession,
        ];
        let mut codes = std::collections::HashSet::new();
        for r in all {
            assert!(codes.insert(r.code()));
            assert!(!r.describe().is_empty());
            assert!(render("/p", &PruneDecision::Keep(r)).contains(r.describe()));
        }
    }
}
