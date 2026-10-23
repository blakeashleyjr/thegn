//! The pure fold engine for the local merge queue ("fold-actor").
//!
//! Given a starting `main` tip and an ordered list of branch tips, fold each one
//! against a *running* tip in the object database under a configurable
//! [`LandStrategy`]:
//!
//! - **merge** — a clean 3-way merge advances the tip via a sequential 2-parent
//!   merge commit (today's behaviour, unchanged);
//! - **squash** — the same merged tree, committed with a single parent;
//! - **rebase** — the branch's own commits replayed one at a time (linear
//!   history), preserving each commit's author.
//!
//! A conflict is deferred without stopping the line, and no partial replay ever
//! lands. This is the "kill the manual sequencing" core — ordering becomes an
//! in-memory fold instead of a checkout-per-branch ritual.
//!
//! This module is I/O-free: git is injected behind [`FoldGit`] so the sequencing
//! is exhaustively unit-tested (the crate's 95% gate). The host drives it with an
//! adapter over `thegn_svc::git::PlumbingOps` (merge-tree + commit-tree), then
//! test-gates the resulting tip and CAS-advances `main` — both of which are I/O
//! and live in the host, deliberately out of this gated crate.

use crate::config::LandStrategy;
use anyhow::Result;

/// Outcome of folding one branch onto the running tip. Re-declared here (rather
/// than reusing svc's `MergeTreeOutcome`) so `thegn-core` needn't depend on
/// `thegn-svc`; the host adapter converts between the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean {
        tree: String,
    },
    Conflict {
        paths: Vec<String>,
    },
    /// A gitlink conflict carries raw paths for git operations and typed
    /// pointer details for reporting/prompt rendering.
    SubmoduleConflict {
        paths: Vec<String>,
        conflicts: Vec<crate::submodule::SubmoduleConflict>,
    },
}

/// The author of a commit, preserved when a `rebase` land replays it. The
/// committer is intentionally left to the ambient git identity — same as
/// `git rebase`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Author {
    pub name: String,
    pub email: String,
    /// Author date in git's `@<unix> <tz>` / RFC2822 form; empty ⇒ "now".
    pub date: String,
}

/// A commit considered for landing: its oid, first parent (the merge base for
/// replaying just this commit's delta), all parents in their original order,
/// the full message, and the author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    pub oid: String,
    pub parent: String,
    /// Every parent oid in commit-header order; empty for a root commit.
    pub parents: Vec<String>,
    pub message: String,
    pub author: Author,
}

impl CommitMeta {
    /// The first line of the message (the commit subject).
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim()
    }
}

/// Git operations the fold engine needs, injected so the algorithm is testable
/// without a real repo. The first two power `merge`/`squash`; the rest are used
/// only by `rebase` and by `{subjects}` message rendering.
pub trait FoldGit {
    /// 3-way merge `theirs` onto `ours` in the object DB (no checkout), letting
    /// git find the merge base itself.
    fn merge_tree(&self, ours: &str, theirs: &str) -> Result<MergeOutcome>;
    /// Create a commit from `tree` with `parents` and `msg`; returns the new oid.
    fn commit_tree(&self, tree: &str, parents: &[&str], msg: &str) -> Result<String>;
    /// 3-way merge with an *explicit* base — a plumbing cherry-pick that replays
    /// exactly `theirs`'s delta (`base..theirs`) onto `ours`. Used by `rebase`.
    fn merge_tree_base(&self, base: &str, ours: &str, theirs: &str) -> Result<MergeOutcome>;
    /// Create a commit preserving `author` (committer stays ambient). Used by
    /// `rebase` to keep replayed commits' original authorship.
    fn commit_tree_author(
        &self,
        tree: &str,
        parents: &[&str],
        msg: &str,
        author: &Author,
    ) -> Result<String>;
    /// Merge base of `a` and `b` (`None` when unrelated).
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>>;
    /// Commits in `base_excl..tip`, ancestor-first — each with its ordered
    /// parent list, first parent, full message, and author. Used by `rebase`
    /// (replay list) and by `{subjects}` rendering.
    fn commits(&self, base_excl: &str, tip: &str) -> Result<Vec<CommitMeta>>;
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
    /// The branch has unrelated history or root/merge commits in the replay
    /// range, which the `rebase` land strategy cannot replay; held, not a
    /// content conflict.
    UnsupportedMergeHistory,
}

/// A branch that didn't land, with its conflicted paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deferred {
    pub branch: Branch,
    pub paths: Vec<String>,
    pub kind: ConflictKind,
    pub submodule_conflicts: Vec<crate::submodule::SubmoduleConflict>,
}

/// A branch that landed, with the resulting tip oid (a merge/squash commit, or
/// the last replayed commit under `rebase`).
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

/// How a fold commits each branch — the [`LandStrategy`] plus the message
/// template and target-branch name the message template can reference.
#[derive(Debug, Clone, Copy)]
pub struct LandOpts<'a> {
    pub strategy: LandStrategy,
    /// `[merge_queue] land_message`; empty ⇒ the built-in per-strategy default.
    pub message_template: &'a str,
    /// The target branch name, bound to `{target}` in the message template.
    pub target: &'a str,
}

impl LandOpts<'_> {
    /// The default `merge` strategy with the built-in message (today's
    /// behaviour) — used by call sites that don't customize the land.
    pub fn merge_default() -> LandOpts<'static> {
        LandOpts {
            strategy: LandStrategy::Merge,
            message_template: "",
            target: "",
        }
    }
}

/// The built-in merge-commit subject for a landed branch (`merge` strategy,
/// empty template). Kept byte-identical so the default land is unchanged.
pub fn merge_msg(b: &Branch) -> String {
    format!("Merge branch '{}' (fold-actor)", b.name)
}

/// The built-in squash-commit subject for a landed branch (`squash` strategy,
/// empty template).
pub fn squash_msg(b: &Branch) -> String {
    format!("Squash branch '{}' (fold-actor)", b.name)
}

/// Classify a conflict by its paths: `Regenerable` iff the conflict is non-empty
/// and *every* conflicted path is a regenerate-path. An entry without `*` matches
/// only its exact repo-relative path; `**/<path>` matches that path at the root or
/// under any directory. Other wildcard entries match nothing. Otherwise `Textual`.
pub fn classify(paths: &[String], regenerate_paths: &[String]) -> ConflictKind {
    if !paths.is_empty() && paths.iter().all(|p| is_regenerable(p, regenerate_paths)) {
        ConflictKind::Regenerable
    } else {
        ConflictKind::Textual
    }
}

fn is_regenerable(path: &str, regenerate_paths: &[String]) -> bool {
    regenerate_paths.iter().any(|entry| {
        if let Some(rest) = entry.strip_prefix("**/") {
            !rest.is_empty()
                && !rest.contains('*')
                && (path == rest
                    || path
                        .strip_suffix(rest)
                        .is_some_and(|prefix| prefix.ends_with('/')))
        } else {
            !entry.contains('*') && entry == path
        }
    })
}

/// Outcome of folding one branch under a strategy.
enum One {
    Landed {
        commit: String,
    },
    Deferred {
        paths: Vec<String>,
        submodule_conflicts: Vec<crate::submodule::SubmoduleConflict>,
    },
    UnsupportedMergeHistory,
}

/// Fold `branches` onto `start_tip` in order under `opts.strategy`. Each clean
/// land advances the running tip (so a later branch folds against the *folded*
/// result, not the original base — this is what catches a branch that only
/// conflicts with an earlier-landed one). Each conflict is deferred without
/// aborting the rest, and no partial replay ever lands.
pub fn fold(
    git: &impl FoldGit,
    start_tip: &str,
    branches: Vec<Branch>,
    regenerate_paths: &[String],
    opts: &LandOpts,
) -> Result<FoldPlan> {
    let mut tip = start_tip.to_string();
    let mut landed = Vec::new();
    let mut deferred = Vec::new();
    for b in branches {
        match fold_one(git, &tip, &b, opts)? {
            One::Landed { commit } => {
                tip = commit.clone();
                landed.push(Landed { branch: b, commit });
            }
            One::Deferred {
                paths,
                submodule_conflicts,
            } => {
                let kind = if submodule_conflicts.is_empty() {
                    classify(&paths, regenerate_paths)
                } else {
                    ConflictKind::Textual
                };
                deferred.push(Deferred {
                    branch: b,
                    paths,
                    kind,
                    submodule_conflicts,
                });
            }
            One::UnsupportedMergeHistory => {
                deferred.push(Deferred {
                    branch: b,
                    paths: Vec::new(),
                    kind: ConflictKind::UnsupportedMergeHistory,
                    submodule_conflicts: Vec::new(),
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

/// Fold one branch onto `tip` under the chosen strategy.
fn fold_one(git: &impl FoldGit, tip: &str, b: &Branch, opts: &LandOpts) -> Result<One> {
    match opts.strategy {
        LandStrategy::Merge => match git.merge_tree(tip, &b.tip)? {
            MergeOutcome::Clean { tree } => {
                let msg = land_message(git, tip, b, opts, false)?;
                Ok(One::Landed {
                    commit: git.commit_tree(&tree, &[tip, &b.tip], &msg)?,
                })
            }
            MergeOutcome::Conflict { paths } => Ok(One::Deferred {
                paths,
                submodule_conflicts: Vec::new(),
            }),
            MergeOutcome::SubmoduleConflict { paths, conflicts } => Ok(One::Deferred {
                paths,
                submodule_conflicts: conflicts,
            }),
        },
        LandStrategy::Squash => match git.merge_tree(tip, &b.tip)? {
            MergeOutcome::Clean { tree } => {
                let msg = land_message(git, tip, b, opts, true)?;
                // Single parent — the whole branch collapses to one commit.
                Ok(One::Landed {
                    commit: git.commit_tree(&tree, &[tip], &msg)?,
                })
            }
            MergeOutcome::Conflict { paths } => Ok(One::Deferred {
                paths,
                submodule_conflicts: Vec::new(),
            }),
            MergeOutcome::SubmoduleConflict { paths, conflicts } => Ok(One::Deferred {
                paths,
                submodule_conflicts: conflicts,
            }),
        },
        LandStrategy::Rebase => rebase_replay(git, tip, b),
    }
}

/// The rebase commit walk was not exactly the linear chain `base..tip`; nothing was replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncoherentCommitWalk {
    pub branch: String,
    pub reason: String,
}

impl std::fmt::Display for IncoherentCommitWalk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "commit walk for {} is not the linear chain from the merge base to its tip ({}); nothing was replayed",
            self.branch, self.reason
        )
    }
}

impl std::error::Error for IncoherentCommitWalk {}

/// Verify `commits` is exactly the ancestor-first chain from `base` (exclusive)
/// to `tip` (inclusive), with each commit having one parent.
fn verify_linear_walk(
    base: &str,
    tip: &str,
    commits: &[CommitMeta],
) -> std::result::Result<(), String> {
    if commits.is_empty() {
        return if base.eq_ignore_ascii_case(tip) {
            Ok(())
        } else {
            Err("empty walk but the merge base is not the tip".to_string())
        };
    }

    let first = &commits[0];
    if !matches!(first.parents.as_slice(), [parent] if parent.eq_ignore_ascii_case(base)) {
        return Err(format!(
            "commit at index 0 expected sole parent {base}, found [{}]",
            first.parents.join(", ")
        ));
    }
    if !first.parent.eq_ignore_ascii_case(base) {
        return Err(format!(
            "commit at index 0 expected replay parent {base}, found {}",
            first.parent
        ));
    }

    for (index, pair) in commits.windows(2).enumerate() {
        let previous = &pair[0];
        let current = &pair[1];
        if !matches!(current.parents.as_slice(), [parent] if parent.eq_ignore_ascii_case(&previous.oid))
        {
            return Err(format!(
                "commit at index {} expected sole parent {}, found [{}]",
                index + 1,
                previous.oid,
                current.parents.join(", ")
            ));
        }
        if !current.parent.eq_ignore_ascii_case(&previous.oid) {
            return Err(format!(
                "commit at index {} expected replay parent {}, found {}",
                index + 1,
                previous.oid,
                current.parent
            ));
        }
    }

    let last_index = commits.len() - 1;
    let last = &commits[last_index];
    if !last.oid.eq_ignore_ascii_case(tip) {
        return Err(format!(
            "commit at index {last_index} expected tip {tip}, found {}",
            last.oid
        ));
    }
    Ok(())
}

/// Replay a linear branch's commits one at a time onto `tip` (a plumbing
/// cherry-pick per commit). Unrelated history and histories containing root
/// or merge commits are held before replay; the walk must then be the complete
/// ancestor-first chain from the merge base through the branch tip. Any
/// conflicting step also defers the WHOLE branch — no commit lands until every
/// one replays clean, exactly like a conflicting merge fold.
fn rebase_replay(git: &impl FoldGit, tip: &str, b: &Branch) -> Result<One> {
    let Some(base) = git.merge_base(tip, &b.tip)? else {
        return Ok(One::UnsupportedMergeHistory);
    };
    let commits = git.commits(&base, &b.tip)?;
    if commits
        .iter()
        .any(|commit| commit.parents.is_empty() || commit.parents.len() > 1)
    {
        return Ok(One::UnsupportedMergeHistory);
    }
    if let Err(reason) = verify_linear_walk(&base, &b.tip, &commits) {
        return Err(anyhow::Error::new(IncoherentCommitWalk {
            branch: b.name.clone(),
            reason,
        }));
    }
    let mut running = tip.to_string();
    for c in commits {
        match git.merge_tree_base(&c.parent, &running, &c.oid)? {
            MergeOutcome::Clean { tree } => {
                running = git.commit_tree_author(&tree, &[&running], &c.message, &c.author)?;
            }
            // Stop at the first conflicting commit; nothing this branch would
            // have replayed lands (the caller keeps the running tip pre-branch).
            MergeOutcome::Conflict { paths } => {
                return Ok(One::Deferred {
                    paths,
                    submodule_conflicts: Vec::new(),
                });
            }
            MergeOutcome::SubmoduleConflict { paths, conflicts } => {
                return Ok(One::Deferred {
                    paths,
                    submodule_conflicts: conflicts,
                });
            }
        }
    }
    Ok(One::Landed { commit: running })
}

/// Render the land-commit message for `merge`/`squash`. Empty template ⇒ the
/// built-in per-strategy default (with no extra git calls on the default merge
/// path). A template referencing `{subjects}` triggers a merge-base + commit
/// walk to list the landed commits' subjects.
fn land_message(
    git: &impl FoldGit,
    tip: &str,
    b: &Branch,
    opts: &LandOpts,
    squash: bool,
) -> Result<String> {
    let tmpl = opts.message_template.trim();
    if tmpl.is_empty() {
        // Built-in defaults. Squash lists the folded subjects as the body since a
        // single commit otherwise loses them; merge keeps its terse subject.
        if squash {
            let subjects = subjects(git, tip, &b.tip)?;
            let mut msg = squash_msg(b);
            if !subjects.is_empty() {
                msg.push_str("\n\n");
                msg.push_str(&subjects_block(&subjects));
            }
            return Ok(msg);
        }
        return Ok(merge_msg(b));
    }

    let mut vars = crate::agent_task::TaskVars::new()
        .set("branch", b.name.clone())
        .set("target", opts.target);
    // Only pay for the commit walk when the template actually asks for subjects.
    if tmpl.contains("subjects") {
        vars = vars.set("subjects", subjects_block(&subjects(git, tip, &b.tip)?));
    }
    // Validated at config load; on any render error fall back to the default so a
    // land is never blocked by a message-template edge case.
    Ok(crate::agent_task::render_prompt(tmpl, &vars)
        .unwrap_or_else(|_| if squash { squash_msg(b) } else { merge_msg(b) }))
}

/// The subjects of the commits `tip..branch_tip` would land, ancestor-first.
fn subjects(git: &impl FoldGit, tip: &str, branch_tip: &str) -> Result<Vec<String>> {
    let base = git
        .merge_base(tip, branch_tip)?
        .unwrap_or_else(|| tip.to_string());
    Ok(git
        .commits(&base, branch_tip)?
        .into_iter()
        .map(|c| c.subject().to_string())
        .collect())
}

/// Format subjects as one `- <subject>` line each (the `{subjects}` expansion).
fn subjects_block(subjects: &[String]) -> String {
    subjects
        .iter()
        .map(|s| format!("- {s}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeSet, HashMap};

    fn merge_opts() -> LandOpts<'static> {
        LandOpts::merge_default()
    }

    /// How a given branch tip behaves when folded.
    enum Rule {
        Conflict(Vec<String>),
        SubmoduleConflict(Vec<String>, Vec<crate::submodule::SubmoduleConflict>),
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
        // Rebase scripting: branch tip -> its commits (ancestor-first); and a
        // per-commit-oid conflict on replay.
        commits: HashMap<String, Vec<CommitMeta>>,
        replay_conflict: HashMap<String, Vec<String>>,
        no_merge_base: bool,
        landed: RefCell<BTreeSet<String>>,
        n: Cell<u32>,
        merge_calls: RefCell<Vec<(String, String)>>,
        commit_msgs: RefCell<Vec<String>>,
        commit_parents: RefCell<Vec<usize>>,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                rules: HashMap::new(),
                names: HashMap::new(),
                commits: HashMap::new(),
                replay_conflict: HashMap::new(),
                no_merge_base: false,
                landed: RefCell::new(BTreeSet::new()),
                n: Cell::new(0),
                merge_calls: RefCell::new(Vec::new()),
                commit_msgs: RefCell::new(Vec::new()),
                commit_parents: RefCell::new(Vec::new()),
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
        /// Register a branch's commit list (for rebase / subjects), each entry
        /// `(oid, subject)`; `parent` chains oid_{i-1} → "base" for the first.
        fn with_commits(mut self, name: &str, commits: &[(&str, &str)]) -> Self {
            let tip = format!("t{name}");
            let mut metas = Vec::new();
            let mut parent = "base".to_string();
            for (oid, subject) in commits {
                metas.push(CommitMeta {
                    oid: (*oid).to_string(),
                    parent: parent.clone(),
                    parents: vec![parent.clone()],
                    message: (*subject).to_string(),
                    author: Author {
                        name: "orig".into(),
                        email: "o@e".into(),
                        date: String::new(),
                    },
                });
                parent = (*oid).to_string();
            }
            self.commits.insert(tip, metas);
            self
        }
        fn with_merge_commit(mut self, name: &str, oid: &str, second_parent: &str) -> Self {
            let tip = format!("t{name}");
            let commit = self
                .commits
                .get_mut(&tip)
                .and_then(|commits| commits.iter_mut().find(|commit| commit.oid == oid))
                .expect("fixture merge commit exists");
            commit.parents.push(second_parent.to_string());
            self
        }
        fn unrelated(mut self) -> Self {
            self.no_merge_base = true;
            self
        }
        /// Mark commit `oid` as conflicting when replayed (rebase).
        fn replay_conflicts(mut self, oid: &str, paths: &[&str]) -> Self {
            self.replay_conflict.insert(
                oid.to_string(),
                paths.iter().map(|s| s.to_string()).collect(),
            );
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
                Some(Rule::SubmoduleConflict(_, _)) => None,
                None => None,
            };
            if let Some(Rule::SubmoduleConflict(paths, conflicts)) = self.rules.get(theirs) {
                return Ok(MergeOutcome::SubmoduleConflict {
                    paths: paths.clone(),
                    conflicts: conflicts.clone(),
                });
            }
            Ok(match conflict {
                Some(paths) => MergeOutcome::Conflict { paths },
                None => MergeOutcome::Clean {
                    tree: format!("tree_{theirs}_on_{ours}"),
                },
            })
        }
        fn commit_tree(&self, _tree: &str, parents: &[&str], msg: &str) -> Result<String> {
            self.n.set(self.n.get() + 1);
            self.commit_msgs.borrow_mut().push(msg.to_string());
            self.commit_parents.borrow_mut().push(parents.len());
            // parents[1] is the branch tip just folded; record it as landed.
            if let Some(name) = parents.get(1).and_then(|t| self.names.get(*t)) {
                self.landed.borrow_mut().insert(name.clone());
            }
            Ok(format!("M{}", self.n.get()))
        }
        fn merge_tree_base(&self, _base: &str, ours: &str, theirs: &str) -> Result<MergeOutcome> {
            self.merge_calls
                .borrow_mut()
                .push((ours.to_string(), theirs.to_string()));
            if let Some(paths) = self.replay_conflict.get(theirs) {
                return Ok(MergeOutcome::Conflict {
                    paths: paths.clone(),
                });
            }
            Ok(MergeOutcome::Clean {
                tree: format!("tree_{theirs}_on_{ours}"),
            })
        }
        fn commit_tree_author(
            &self,
            _tree: &str,
            parents: &[&str],
            msg: &str,
            _author: &Author,
        ) -> Result<String> {
            self.n.set(self.n.get() + 1);
            self.commit_msgs.borrow_mut().push(msg.to_string());
            self.commit_parents.borrow_mut().push(parents.len());
            Ok(format!("R{}", self.n.get()))
        }
        fn merge_base(&self, _a: &str, _b: &str) -> Result<Option<String>> {
            Ok((!self.no_merge_base).then(|| "base".to_string()))
        }
        fn commits(&self, _base_excl: &str, tip: &str) -> Result<Vec<CommitMeta>> {
            Ok(self.commits.get(tip).cloned().unwrap_or_default())
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
        let plan = fold(
            &git,
            "base",
            vec![br("b1"), br("b2"), br("b3")],
            &[],
            &merge_opts(),
        )
        .unwrap();

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
    fn default_merge_uses_two_parents_and_builtin_message() {
        let git = Fake::new().branch("feat-x", None);
        let plan = fold(&git, "base", vec![br("feat-x")], &[], &merge_opts()).unwrap();
        assert_eq!(plan.landed.len(), 1);
        assert_eq!(git.commit_parents.borrow().as_slice(), &[2]);
        assert_eq!(
            git.commit_msgs.borrow().as_slice(),
            &["Merge branch 'feat-x' (fold-actor)".to_string()]
        );
    }

    #[test]
    fn conflicts_deferred_clean_still_land() {
        let git = Fake::new()
            .branch("b1", None)
            .branch("b2", Some(Rule::Conflict(vec!["src/x.rs".into()])))
            .branch("b3", None);
        let plan = fold(
            &git,
            "base",
            vec![br("b1"), br("b2"), br("b3")],
            &[],
            &merge_opts(),
        )
        .unwrap();

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
    fn typed_submodule_conflicts_are_preserved_and_textual() {
        let conflicts = vec![crate::submodule::SubmoduleConflict {
            path: "vendor/lib".into(),
            ours_sha: "1111111".into(),
            theirs_sha: "2222222".into(),
        }];
        let git = Fake::new().branch(
            "submodule",
            Some(Rule::SubmoduleConflict(
                vec!["vendor/lib".into()],
                conflicts.clone(),
            )),
        );

        let plan = fold(
            &git,
            "base",
            vec![br("submodule")],
            &["vendor/lib".into()],
            &merge_opts(),
        )
        .unwrap();

        assert_eq!(plan.deferred[0].paths, ["vendor/lib"]);
        assert_eq!(plan.deferred[0].kind, ConflictKind::Textual);
        assert_eq!(plan.deferred[0].submodule_conflicts, conflicts);
    }

    #[test]
    fn empty_queue_is_a_noop() {
        let git = Fake::new();
        let plan = fold(&git, "base", vec![], &[], &merge_opts()).unwrap();
        assert_eq!(plan.original, "base");
        assert_eq!(plan.final_tip, "base");
        assert!(plan.landed.is_empty() && plan.deferred.is_empty());
        assert!(!plan.advanced());
    }

    #[test]
    fn single_branch_lands() {
        let git = Fake::new().branch("solo", None);
        let plan = fold(&git, "base", vec![br("solo")], &[], &merge_opts()).unwrap();
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
        let plan = fold(&git, "base", vec![br("b1"), br("b2")], &[], &merge_opts()).unwrap();
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
        let plan2 = fold(&git2, "base", vec![br("b2")], &[], &merge_opts()).unwrap();
        assert_eq!(plan2.landed.len(), 1);
        assert!(plan2.deferred.is_empty());
    }

    #[test]
    fn lockfile_only_conflict_is_classified_regenerable() {
        let regen = vec!["**/Cargo.lock".to_string()];
        // Nested path matches through the explicit `**/` pattern.
        let git = Fake::new()
            .branch(
                "b1",
                Some(Rule::Conflict(vec!["crates/x/Cargo.lock".into()])),
            )
            .branch(
                "b2",
                Some(Rule::Conflict(vec!["Cargo.lock".into(), "src/x.rs".into()])),
            );
        let plan = fold(
            &git,
            "base",
            vec![br("b1"), br("b2")],
            &regen,
            &merge_opts(),
        )
        .unwrap();

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
            ConflictKind::Textual
        );
        assert_eq!(
            classify(&["a/b/Cargo.lock".into()], &["**/Cargo.lock".into()]),
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
    fn regen_bare_name_does_not_match_nested_path() {
        assert_eq!(
            classify(&["crates/x/Cargo.lock".into()], &["Cargo.lock".into()]),
            ConflictKind::Textual
        );
    }

    #[test]
    fn regen_double_star_matches_root_and_nested_only_by_component() {
        let regen = ["**/Cargo.lock".into()];
        for path in ["Cargo.lock", "a/b/Cargo.lock"] {
            assert_eq!(
                classify(&[path.into()], &regen),
                ConflictKind::Regenerable,
                "{path}"
            );
        }
        for path in ["xCargo.lock", "a/xCargo.lock", "Cargo.lock.bak"] {
            assert_eq!(
                classify(&[path.into()], &regen),
                ConflictKind::Textual,
                "{path}"
            );
        }
    }

    #[test]
    fn regen_other_wildcards_match_nothing() {
        // Positive control: these cases are also Textual with the old matcher.
        for entry in ["*.lock", "**/*.lock", "**/", "crates/**/Cargo.lock"] {
            for path in ["Cargo.lock", "crates/x/Cargo.lock"] {
                assert_eq!(
                    classify(&[path.into()], &[entry.into()]),
                    ConflictKind::Textual,
                    "{entry} against {path}"
                );
            }
        }
    }

    #[test]
    fn regen_shipped_defaults_keep_nested_lockfiles() {
        let regen = crate::config::MergeQueueConfig::default().regenerate_paths;
        assert_eq!(
            classify(&["crates/x/Cargo.lock".into()], &regen),
            ConflictKind::Regenerable
        );
        assert_eq!(
            classify(&["web/package-lock.json".into()], &regen),
            ConflictKind::Regenerable
        );
        assert_eq!(
            classify(&["src/x.rs".into()], &regen),
            ConflictKind::Textual
        );
    }

    #[test]
    fn merge_msg_names_the_branch() {
        assert_eq!(
            merge_msg(&br("feat-x")),
            "Merge branch 'feat-x' (fold-actor)"
        );
    }

    // ---- squash strategy ---------------------------------------------------

    fn opts(strategy: LandStrategy, template: &'static str) -> LandOpts<'static> {
        LandOpts {
            strategy,
            message_template: template,
            target: "main",
        }
    }

    #[test]
    fn squash_lands_one_single_parent_commit() {
        let git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "add a"), ("c2", "add b"), ("c3", "add c")]);
        let plan = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Squash, ""),
        )
        .unwrap();
        assert_eq!(plan.landed.len(), 1);
        // Exactly one commit, single parent (the running tip).
        assert_eq!(git.commit_parents.borrow().as_slice(), &[1]);
        // Default squash message lists the folded subjects.
        let msg = &git.commit_msgs.borrow()[0];
        assert!(
            msg.starts_with("Squash branch 'feat' (fold-actor)"),
            "{msg}"
        );
        assert!(msg.contains("- add a") && msg.contains("- add c"), "{msg}");
    }

    #[test]
    fn squash_conflict_defers_like_a_merge() {
        let git = Fake::new().branch("feat", Some(Rule::Conflict(vec!["x.rs".into()])));
        let plan = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Squash, ""),
        )
        .unwrap();
        assert!(plan.landed.is_empty());
        assert_eq!(plan.deferred[0].paths, ["x.rs"]);
        assert!(!plan.advanced());
    }

    #[test]
    fn land_message_template_renders_vars() {
        let git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "first"), ("c2", "second")]);
        let plan = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(
                LandStrategy::Squash,
                "Land {branch} onto {target}\n\n{subjects}",
            ),
        )
        .unwrap();
        assert_eq!(plan.landed.len(), 1);
        let msg = &git.commit_msgs.borrow()[0];
        assert_eq!(msg, "Land feat onto main\n\n- first\n- second");
    }

    // ---- rebase strategy ---------------------------------------------------

    fn assert_incoherent_walk(error: &anyhow::Error, branch: &str) {
        let walk_error = error
            .downcast_ref::<IncoherentCommitWalk>()
            .expect("incoherent walk error is typed");
        assert_eq!(walk_error.branch, branch);
        assert!(!walk_error.reason.is_empty());
    }

    #[test]
    fn rebase_walk_out_of_order_fails_before_any_replay() {
        let mut git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "one"), ("tfeat", "two")]);
        git.commits.get_mut("tfeat").unwrap().swap(0, 1);

        let result = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        );
        let error = match result {
            Ok(_) => panic!("out-of-order walk must fail"),
            Err(error) => error,
        };
        assert_incoherent_walk(&error, "feat");
        assert!(git.merge_calls.borrow().is_empty());
        assert!(git.commit_parents.borrow().is_empty());
    }

    #[test]
    fn rebase_walk_with_a_gap_fails_before_any_replay() {
        let mut git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "one"), ("c2", "two"), ("tfeat", "three")]);
        git.commits.get_mut("tfeat").unwrap().remove(1);

        let result = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        );
        let error = match result {
            Ok(_) => panic!("gapped walk must fail"),
            Err(error) => error,
        };
        assert_incoherent_walk(&error, "feat");
        assert!(git.merge_calls.borrow().is_empty());
        assert!(git.commit_parents.borrow().is_empty());
    }

    #[test]
    fn rebase_walk_not_ending_at_the_tip_fails() {
        let git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "one"), ("c2", "two")]);

        let result = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        );
        let error = match result {
            Ok(_) => panic!("walk not ending at the tip must fail"),
            Err(error) => error,
        };
        assert_incoherent_walk(&error, "feat");
        assert!(git.merge_calls.borrow().is_empty());
        assert!(git.commit_parents.borrow().is_empty());
    }

    #[test]
    fn rebase_walk_not_starting_at_the_merge_base_fails() {
        let mut git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "one"), ("tfeat", "two")]);
        let first = &mut git.commits.get_mut("tfeat").unwrap()[0];
        first.parent = "elsewhere".to_string();
        first.parents = vec!["elsewhere".to_string()];

        let result = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        );
        let error = match result {
            Ok(_) => panic!("walk not starting at the merge base must fail"),
            Err(error) => error,
        };
        assert_incoherent_walk(&error, "feat");
        assert!(git.merge_calls.borrow().is_empty());
        assert!(git.commit_parents.borrow().is_empty());
    }

    #[test]
    fn rebase_walk_replay_parent_mismatch_fails() {
        for index in [0, 1] {
            let mut git = Fake::new()
                .branch("feat", None)
                .with_commits("feat", &[("c1", "one"), ("tfeat", "two")]);
            git.commits.get_mut("tfeat").expect("fixture commits exist")[index].parent =
                "elsewhere".to_string();

            let result = fold(
                &git,
                "base",
                vec![br("feat")],
                &[],
                &opts(LandStrategy::Rebase, ""),
            );
            let error = match result {
                Ok(_) => panic!("replay parent mismatch at index {index} must fail"),
                Err(error) => error,
            };
            assert_incoherent_walk(&error, "feat");
            let expected_reason = if index == 0 {
                "commit at index 0 expected replay parent base, found elsewhere"
            } else {
                "commit at index 1 expected replay parent c1, found elsewhere"
            };
            assert_eq!(
                error
                    .downcast_ref::<IncoherentCommitWalk>()
                    .expect("incoherent walk error is typed")
                    .reason,
                expected_reason
            );
            assert!(git.merge_calls.borrow().is_empty());
            assert!(git.commit_parents.borrow().is_empty());
        }
    }

    #[test]
    fn rebase_walk_empty_but_tip_differs_fails() {
        let git = Fake::new().branch("feat", None);
        let result = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        );
        let error = match result {
            Ok(_) => panic!("empty walk away from the merge base must fail"),
            Err(error) => error,
        };
        assert_incoherent_walk(&error, "feat");
        assert!(git.merge_calls.borrow().is_empty());
        assert!(git.commit_parents.borrow().is_empty());
    }

    #[test]
    fn verify_linear_walk_accepts_an_empty_walk_at_the_tip() {
        assert_eq!(verify_linear_walk("AbCd", "aBcD", &[]), Ok(()));
    }

    #[test]
    fn incoherent_commit_walk_display_names_branch_and_reason() {
        let error = IncoherentCommitWalk {
            branch: "feat".to_string(),
            reason: "walk ends at c2, expected tip tfeat".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "commit walk for feat is not the linear chain from the merge base to its tip (walk ends at c2, expected tip tfeat); nothing was replayed"
        );
    }

    #[test]
    fn rebase_replays_each_commit_single_parent_preserving_message() {
        let git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "one"), ("tfeat", "two")]);
        let plan = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        )
        .unwrap();
        assert_eq!(plan.landed.len(), 1);
        // Two replayed commits, each with a single parent, original messages kept.
        assert_eq!(git.commit_parents.borrow().as_slice(), &[1, 1]);
        assert_eq!(
            git.commit_msgs.borrow().as_slice(),
            &["one".to_string(), "two".to_string()]
        );
        // Final tip is the last replayed commit.
        assert_eq!(plan.final_tip, "R2");
    }

    #[test]
    fn rebase_merge_history_is_held_before_any_replay_object() {
        let git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "one"), ("tfeat", "merge")])
            .with_merge_commit("feat", "tfeat", "side-parent");
        let plan = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        )
        .unwrap();

        assert!(plan.landed.is_empty());
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].kind, ConflictKind::UnsupportedMergeHistory);
        assert!(plan.deferred[0].paths.is_empty());
        assert_eq!(plan.final_tip, "base");
        assert!(
            git.commit_parents.borrow().is_empty(),
            "no replay commit created"
        );
        assert!(
            git.merge_calls.borrow().is_empty(),
            "no replay merge attempted"
        );
    }

    #[test]
    fn rebase_unrelated_history_is_held_before_any_replay_object() {
        let git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("c1", "one"), ("tfeat", "two")])
            .unrelated();
        let plan = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        )
        .unwrap();

        assert!(plan.landed.is_empty());
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].kind, ConflictKind::UnsupportedMergeHistory);
        assert_eq!(plan.final_tip, "base");
        assert!(git.commit_parents.borrow().is_empty());
        assert!(git.merge_calls.borrow().is_empty());
    }

    #[test]
    fn rebase_root_commit_in_range_is_held() {
        let mut git = Fake::new()
            .branch("feat", None)
            .with_commits("feat", &[("root", "root"), ("tfeat", "child")]);
        let commits = git.commits.get_mut("tfeat").expect("fixture commits exist");
        commits[0].parent.clear();
        commits[0].parents.clear();
        commits[1].parent = "root".to_string();
        commits[1].parents = vec!["root".to_string()];
        let plan = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        )
        .unwrap();

        assert!(plan.landed.is_empty());
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].kind, ConflictKind::UnsupportedMergeHistory);
        assert_eq!(plan.final_tip, "base");
        assert!(git.commit_parents.borrow().is_empty());
        assert!(git.merge_calls.borrow().is_empty());
    }

    #[test]
    fn rebase_merge_history_does_not_affect_merge_or_squash() {
        for strategy in [LandStrategy::Merge, LandStrategy::Squash] {
            let git = Fake::new()
                .branch("feat", None)
                .with_commits("feat", &[("c1", "one"), ("tfeat", "merge")])
                .with_merge_commit("feat", "tfeat", "side-parent");
            let plan = fold(&git, "base", vec![br("feat")], &[], &opts(strategy, "")).unwrap();

            assert_eq!(plan.landed.len(), 1, "{strategy:?}");
            assert!(plan.deferred.is_empty(), "{strategy:?}");
        }
    }

    #[test]
    fn rebase_replay_stops_on_conflict_and_lands_nothing() {
        // Second commit conflicts on replay → the whole branch defers, no commit
        // lands (the running tip never moved off base).
        let git = Fake::new()
            .branch("feat", None)
            .with_commits(
                "feat",
                &[("c1", "ok"), ("c2", "boom"), ("tfeat", "unreached")],
            )
            .replay_conflicts("c2", &["src/x.rs"]);
        let plan = fold(
            &git,
            "base",
            vec![br("feat")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        )
        .unwrap();
        assert!(plan.landed.is_empty(), "no partial replay lands");
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].paths, ["src/x.rs"]);
        assert!(!plan.advanced());
        // The branch tip was never attempted after the conflict at c2.
        let calls = git.merge_calls.borrow();
        assert!(!calls.iter().any(|(_, t)| t == "tfeat"));
        assert_eq!(plan.final_tip, "base");
    }

    #[test]
    fn rebase_defers_but_a_later_clean_branch_still_lands() {
        let git = Fake::new()
            .branch("bad", None)
            .with_commits("bad", &[("tbad", "boom")])
            .replay_conflicts("tbad", &["x.rs"])
            .branch("good", None)
            .with_commits("good", &[("tgood", "fine")]);
        let plan = fold(
            &git,
            "base",
            vec![br("bad"), br("good")],
            &[],
            &opts(LandStrategy::Rebase, ""),
        )
        .unwrap();
        assert_eq!(
            plan.landed
                .iter()
                .map(|l| l.branch.name.as_str())
                .collect::<Vec<_>>(),
            ["good"]
        );
        assert_eq!(plan.deferred[0].branch.name, "bad");
    }
}
