//! The pure fold engine for the local merge queue ("fold-actor").
//!
//! Given a starting `main` tip and an ordered list of branch tips, fold each one
//! against a *running* tip in the object database: a clean 3-way merge advances
//! the tip via a sequential 2-parent merge commit, a conflict is deferred
//! without stopping the line. This is the "kill the manual sequencing" core —
//! ordering becomes an in-memory fold instead of a checkout-per-branch ritual.
//!
//! This module is I/O-free: git is injected behind [`FoldGit`] so the sequencing
//! is exhaustively unit-tested (the crate's 95% gate). The host drives it with an
//! adapter over `thegn_svc::git::PlumbingOps` (merge-tree + commit-tree), then
//! test-gates the resulting tip and CAS-advances `main` — both of which are I/O
//! and live in the host, deliberately out of this gated crate.

use anyhow::Result;

/// Outcome of folding one branch onto the running tip. Re-declared here (rather
/// than reusing svc's `MergeTreeOutcome`) so `thegn-core` needn't depend on
/// `thegn-svc`; the host adapter converts between the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean { tree: String },
    Conflict { paths: Vec<String> },
}

/// Git operations the fold engine needs, injected so the algorithm is testable
/// without a real repo.
pub trait FoldGit {
    /// 3-way merge `theirs` onto `ours` in the object DB (no checkout).
    fn merge_tree(&self, ours: &str, theirs: &str) -> Result<MergeOutcome>;
    /// Create a merge commit from `tree` with `parents`; returns the new oid.
    fn commit_tree(&self, tree: &str, parents: &[&str], msg: &str) -> Result<String>;
}

/// A branch queued to land: its display name and current tip oid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub tip: String,
}

/// Why a deferred branch couldn't land cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// A real textual conflict the merge can't resolve — needs a human/agent.
    Textual,
    /// Conflicts confined to regenerable artifacts (lockfiles/manifests); the
    /// host can resolve these by regenerating rather than handing them back.
    Regenerable,
}

/// A branch that didn't land, with its conflicted paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deferred {
    pub branch: Branch,
    pub paths: Vec<String>,
    pub kind: ConflictKind,
}

/// A branch that landed, with the resulting merge-commit oid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub branch: Branch,
    pub commit: String,
}

/// The result of folding a queue: where `main` started, where it ended (in the
/// object DB — not yet CAS-advanced), what landed, what was deferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldPlan {
    pub original: String,
    pub final_tip: String,
    pub landed: Vec<Landed>,
    pub deferred: Vec<Deferred>,
}

impl FoldPlan {
    /// Did anything land? (The final tip moved off the original.)
    pub fn advanced(&self) -> bool {
        self.final_tip != self.original
    }
}

/// The merge-commit subject for a landed branch.
pub fn merge_msg(b: &Branch) -> String {
    format!("Merge branch '{}' (fold-actor)", b.name)
}

/// Classify a conflict by its paths: `Regenerable` iff the conflict is non-empty
/// and *every* conflicted path is a regenerate-path (matched by exact path or by
/// basename, so `Cargo.lock` matches `crates/x/Cargo.lock`); otherwise `Textual`.
pub fn classify(paths: &[String], regenerate_paths: &[String]) -> ConflictKind {
    if !paths.is_empty() && paths.iter().all(|p| is_regenerable(p, regenerate_paths)) {
        ConflictKind::Regenerable
    } else {
        ConflictKind::Textual
    }
}

fn is_regenerable(path: &str, regenerate_paths: &[String]) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    regenerate_paths.iter().any(|r| r == path || r == base)
}

/// Fold `branches` onto `start_tip` in order. Each clean merge advances the
/// running tip (so a later branch is merged against the *folded* result, not the
/// original base — this is what catches a branch that only conflicts with an
/// earlier-landed one). Each conflict is deferred without aborting the rest.
pub fn fold(
    git: &impl FoldGit,
    start_tip: &str,
    branches: Vec<Branch>,
    regenerate_paths: &[String],
) -> Result<FoldPlan> {
    let mut tip = start_tip.to_string();
    let mut landed = Vec::new();
    let mut deferred = Vec::new();
    for b in branches {
        match git.merge_tree(&tip, &b.tip)? {
            MergeOutcome::Clean { tree } => {
                let commit = git.commit_tree(&tree, &[&tip, &b.tip], &merge_msg(&b))?;
                tip = commit.clone();
                landed.push(Landed { branch: b, commit });
            }
            MergeOutcome::Conflict { paths } => {
                let kind = classify(&paths, regenerate_paths);
                deferred.push(Deferred {
                    branch: b,
                    paths,
                    kind,
                });
            }
        }
    }
    Ok(FoldPlan {
        original: start_tip.to_string(),
        final_tip: tip,
        landed,
        deferred,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeSet, HashMap};

    /// How a given branch tip behaves when folded.
    enum Rule {
        Conflict(Vec<String>),
        /// Conflicts only once the named branch has already landed (models a
        /// branch that's clean against base but collides with an earlier fold).
        ConflictIfLanded(&'static str, Vec<String>),
    }

    /// A scripted `FoldGit`: branches are clean unless a rule says otherwise.
    /// The running set of landed branch names mirrors the real running tip (the
    /// fold is sequential + single-threaded), so `ConflictIfLanded` is exact.
    struct Fake {
        rules: HashMap<String, Rule>,   // theirs tip -> rule
        names: HashMap<String, String>, // theirs tip -> branch name
        landed: RefCell<BTreeSet<String>>,
        n: Cell<u32>,
        merge_calls: RefCell<Vec<(String, String)>>,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                rules: HashMap::new(),
                names: HashMap::new(),
                landed: RefCell::new(BTreeSet::new()),
                n: Cell::new(0),
                merge_calls: RefCell::new(Vec::new()),
            }
        }
        /// Register `name` with tip `t<name>` and an optional rule.
        fn branch(mut self, name: &str, rule: Option<Rule>) -> Self {
            let tip = format!("t{name}");
            self.names.insert(tip.clone(), name.to_string());
            if let Some(r) = rule {
                self.rules.insert(tip, r);
            }
            self
        }
    }

    impl FoldGit for Fake {
        fn merge_tree(&self, ours: &str, theirs: &str) -> Result<MergeOutcome> {
            self.merge_calls
                .borrow_mut()
                .push((ours.to_string(), theirs.to_string()));
            let conflict = match self.rules.get(theirs) {
                Some(Rule::Conflict(p)) => Some(p.clone()),
                Some(Rule::ConflictIfLanded(name, p)) => {
                    self.landed.borrow().contains(*name).then(|| p.clone())
                }
                None => None,
            };
            Ok(match conflict {
                Some(paths) => MergeOutcome::Conflict { paths },
                None => MergeOutcome::Clean {
                    tree: format!("tree_{theirs}_on_{ours}"),
                },
            })
        }
        fn commit_tree(&self, _tree: &str, parents: &[&str], _msg: &str) -> Result<String> {
            self.n.set(self.n.get() + 1);
            // parents[1] is the branch tip just folded; record it as landed.
            if let Some(name) = parents.get(1).and_then(|t| self.names.get(*t)) {
                self.landed.borrow_mut().insert(name.clone());
            }
            Ok(format!("M{}", self.n.get()))
        }
    }

    fn br(name: &str) -> Branch {
        Branch {
            name: name.to_string(),
            tip: format!("t{name}"),
        }
    }

    #[test]
    fn all_clean_lands_all_and_advances_running_tip() {
        let git = Fake::new()
            .branch("b1", None)
            .branch("b2", None)
            .branch("b3", None);
        let plan = fold(&git, "base", vec![br("b1"), br("b2"), br("b3")], &[]).unwrap();

        assert!(plan.deferred.is_empty());
        let names: Vec<&str> = plan.landed.iter().map(|l| l.branch.name.as_str()).collect();
        assert_eq!(names, ["b1", "b2", "b3"], "land order preserved");
        assert_eq!(
            plan.landed
                .iter()
                .map(|l| l.commit.as_str())
                .collect::<Vec<_>>(),
            ["M1", "M2", "M3"]
        );
        assert_eq!(plan.final_tip, "M3");
        assert!(plan.advanced());

        // Each branch folds against the *running* tip, not the base.
        let calls = git.merge_calls.borrow();
        assert_eq!(calls[0], ("base".into(), "tb1".into()));
        assert_eq!(calls[1], ("M1".into(), "tb2".into()));
        assert_eq!(calls[2], ("M2".into(), "tb3".into()));
    }

    #[test]
    fn conflicts_deferred_clean_still_land() {
        let git = Fake::new()
            .branch("b1", None)
            .branch("b2", Some(Rule::Conflict(vec!["src/x.rs".into()])))
            .branch("b3", None);
        let plan = fold(&git, "base", vec![br("b1"), br("b2"), br("b3")], &[]).unwrap();

        assert_eq!(
            plan.landed
                .iter()
                .map(|l| l.branch.name.as_str())
                .collect::<Vec<_>>(),
            ["b1", "b3"]
        );
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].branch.name, "b2");
        assert_eq!(plan.deferred[0].paths, ["src/x.rs"]);
        assert_eq!(plan.deferred[0].kind, ConflictKind::Textual);
        assert!(plan.advanced());
    }

    #[test]
    fn empty_queue_is_a_noop() {
        let git = Fake::new();
        let plan = fold(&git, "base", vec![], &[]).unwrap();
        assert_eq!(plan.original, "base");
        assert_eq!(plan.final_tip, "base");
        assert!(plan.landed.is_empty() && plan.deferred.is_empty());
        assert!(!plan.advanced());
    }

    #[test]
    fn single_branch_lands() {
        let git = Fake::new().branch("solo", None);
        let plan = fold(&git, "base", vec![br("solo")], &[]).unwrap();
        assert_eq!(plan.landed.len(), 1);
        assert_eq!(plan.final_tip, "M1");
    }

    #[test]
    fn branch_conflicting_only_with_an_earlier_landed_one_is_deferred() {
        // b2 is clean against base but collides once b1 lands.
        let git = Fake::new().branch("b1", None).branch(
            "b2",
            Some(Rule::ConflictIfLanded("b1", vec!["shared.rs".into()])),
        );

        // With b1 first, b2 must defer.
        let plan = fold(&git, "base", vec![br("b1"), br("b2")], &[]).unwrap();
        assert_eq!(
            plan.landed
                .iter()
                .map(|l| l.branch.name.as_str())
                .collect::<Vec<_>>(),
            ["b1"]
        );
        assert_eq!(
            plan.deferred
                .iter()
                .map(|d| d.branch.name.as_str())
                .collect::<Vec<_>>(),
            ["b2"]
        );

        // b2 alone (b1 never landed) folds clean — proves the dependence is on
        // the *running tip*, not an intrinsic property of b2.
        let git2 = Fake::new().branch(
            "b2",
            Some(Rule::ConflictIfLanded("b1", vec!["shared.rs".into()])),
        );
        let plan2 = fold(&git2, "base", vec![br("b2")], &[]).unwrap();
        assert_eq!(plan2.landed.len(), 1);
        assert!(plan2.deferred.is_empty());
    }

    #[test]
    fn lockfile_only_conflict_is_classified_regenerable() {
        let regen = vec!["Cargo.lock".to_string()];
        // Nested path still matches by basename.
        let git = Fake::new()
            .branch(
                "b1",
                Some(Rule::Conflict(vec!["crates/x/Cargo.lock".into()])),
            )
            .branch(
                "b2",
                Some(Rule::Conflict(vec!["Cargo.lock".into(), "src/x.rs".into()])),
            );
        let plan = fold(&git, "base", vec![br("b1"), br("b2")], &regen).unwrap();

        assert_eq!(
            plan.deferred[0].kind,
            ConflictKind::Regenerable,
            "lockfile-only"
        );
        assert_eq!(
            plan.deferred[1].kind,
            ConflictKind::Textual,
            "mixed → textual"
        );
    }

    #[test]
    fn classify_edges() {
        let regen = vec!["Cargo.lock".to_string(), "flake.lock".to_string()];
        assert_eq!(classify(&[], &regen), ConflictKind::Textual, "empty");
        assert_eq!(
            classify(&["Cargo.lock".into()], &regen),
            ConflictKind::Regenerable
        );
        assert_eq!(
            classify(&["flake.lock".into(), "Cargo.lock".into()], &regen),
            ConflictKind::Regenerable
        );
        assert_eq!(
            classify(&["a/b/Cargo.lock".into()], &regen),
            ConflictKind::Regenerable
        );
        assert_eq!(
            classify(&["Cargo.lock".into(), "x.rs".into()], &regen),
            ConflictKind::Textual
        );
        assert_eq!(classify(&["x.rs".into()], &regen), ConflictKind::Textual);
        assert_eq!(
            classify(&["Cargo.lock".into()], &[]),
            ConflictKind::Textual,
            "no regen list"
        );
    }

    #[test]
    fn merge_msg_names_the_branch() {
        assert_eq!(
            merge_msg(&br("feat-x")),
            "Merge branch 'feat-x' (fold-actor)"
        );
    }
}
