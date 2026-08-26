//! **Projects** — a grouping layer above workspaces: several repos worked on
//! together as one feature (`api` + `web` + `shared-lib`). Existence and
//! membership live in the DB ([`crate::store::ProjectStore`]); this module is
//! the *pure* workflow logic — resolving the single linked branch name a feature
//! uses across every member repo, and planning a batched cross-repo worktree
//! creation.
//!
//! The cross-repo link is **branch-name equality**, never a persisted super-repo
//! record. A feature is created by resolving ONE final branch name once (the
//! configured prefix + slug, applied a single time) and creating that exact
//! branch verbatim in each member repo — per-repo `branch_prefix` overrides are
//! deliberately NOT re-applied, because identity must be literal. This keeps git
//! the sole source of truth per repo: a same-named branch created outside thegn
//! joins its feature automatically.

use crate::util;

/// The single, literal branch name a feature uses across every member repo:
/// `{branch_prefix}{slug(feature)}`. Resolved once and used verbatim in each
/// member — deliberately NOT per-repo deduped (dedup would make the names differ
/// across repos and break the branch-name-equality identity). Produces the same
/// string as [`crate::worktree::human_base`], sharing [`util::slugify`].
pub fn feature_branch_name(feature: &str, branch_prefix: &str) -> String {
    format!("{branch_prefix}{}", util::slugify(feature))
}

/// One member repo of a project, tagged with whether it already has the feature
/// branch (a git-derived fact the caller probes per repo — kept OUT of this pure
/// module so the planner stays table-testable without I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberBranchState {
    pub repo_root: String,
    pub repo_name: String,
    /// Whether the resolved feature branch already exists in this repo.
    pub has_branch: bool,
}

/// What the batched create will do for one member repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberPlan {
    /// The branch is absent — create it (+ a worktree) here.
    Create,
    /// The branch already exists — attach (report `exists`, skip creation). This
    /// is what makes a re-run after partial failure the recovery path.
    Exists,
}

/// A member repo paired with the action the batched create will take for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMember {
    pub repo_root: String,
    pub repo_name: String,
    pub plan: MemberPlan,
}

/// The full plan for a batched cross-repo feature creation: one resolved branch
/// name, a per-member action, and any `--repos` names that matched no member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchedCreatePlan {
    /// The single branch name created verbatim in every planned member.
    pub branch: String,
    /// Members to act on, in the order given (deterministic). Empty when the
    /// `--repos` filter excluded every member.
    pub members: Vec<PlannedMember>,
    /// `--repos` names that don't match any project member — reported to the
    /// user (a likely typo), never fatal.
    pub unknown_repos: Vec<String>,
}

impl BatchedCreatePlan {
    /// Members whose branch must actually be created (the rest already exist).
    pub fn to_create(&self) -> impl Iterator<Item = &PlannedMember> {
        self.members.iter().filter(|m| m.plan == MemberPlan::Create)
    }
}

/// Plan a batched cross-repo create over a project's members.
///
/// `branch` is the already-resolved feature branch (see [`feature_branch_name`]).
/// `members` is every member repo with its probed branch-existence. When
/// `repos_filter` is `Some`, only members whose `repo_name` appears in the filter
/// are planned, and filter names matching no member are returned in
/// `unknown_repos`. Order is preserved from `members` (deterministic).
pub fn plan_batched_create(
    branch: &str,
    members: &[MemberBranchState],
    repos_filter: Option<&[String]>,
) -> BatchedCreatePlan {
    let selected: Vec<&MemberBranchState> = match repos_filter {
        None => members.iter().collect(),
        Some(filter) => members
            .iter()
            .filter(|m| filter.iter().any(|f| f == &m.repo_name))
            .collect(),
    };

    let unknown_repos = match repos_filter {
        None => Vec::new(),
        Some(filter) => filter
            .iter()
            .filter(|f| !members.iter().any(|m| &m.repo_name == *f))
            .cloned()
            .collect(),
    };

    let planned = selected
        .into_iter()
        .map(|m| PlannedMember {
            repo_root: m.repo_root.clone(),
            repo_name: m.repo_name.clone(),
            plan: if m.has_branch {
                MemberPlan::Exists
            } else {
                MemberPlan::Create
            },
        })
        .collect();

    BatchedCreatePlan {
        branch: branch.to_string(),
        members: planned,
        unknown_repos,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(root: &str, name: &str, has: bool) -> MemberBranchState {
        MemberBranchState {
            repo_root: root.into(),
            repo_name: name.into(),
            has_branch: has,
        }
    }

    #[test]
    fn branch_name_is_prefix_plus_slug_applied_once() {
        assert_eq!(
            feature_branch_name("payments retry", "tg/"),
            "tg/payments-retry"
        );
        // A name that already looks branch-shaped still slugs, once.
        assert_eq!(
            feature_branch_name("Fix Bug #42", "feat/"),
            "feat/fix-bug-42"
        );
    }

    #[test]
    fn branch_name_matches_worktree_human_base() {
        // The batched path must resolve the SAME name the single-repo pipeline
        // would, so a project feature and a hand-made worktree share identity.
        let cfg = crate::config::Config {
            branch_prefix: "tg/".into(),
            ..Default::default()
        };
        assert_eq!(
            feature_branch_name("payments-retry", &cfg.branch_prefix),
            crate::worktree::human_base("payments-retry", &cfg),
        );
    }

    #[test]
    fn plans_create_for_absent_and_exists_for_present() {
        let members = vec![member("/api", "api", false), member("/web", "web", true)];
        let plan = plan_batched_create("tg/x", &members, None);
        assert_eq!(plan.branch, "tg/x");
        assert_eq!(plan.members.len(), 2);
        assert_eq!(plan.members[0].plan, MemberPlan::Create);
        assert_eq!(plan.members[1].plan, MemberPlan::Exists);
        assert!(plan.unknown_repos.is_empty());
        // Only the absent one needs creation.
        let create: Vec<&str> = plan.to_create().map(|m| m.repo_name.as_str()).collect();
        assert_eq!(create, vec!["api"]);
    }

    #[test]
    fn re_run_after_partial_failure_attaches_existing() {
        // First run created api + web, then failed at shared-lib. Re-run: api/web
        // report Exists (attach), shared-lib is created.
        let members = vec![
            member("/api", "api", true),
            member("/web", "web", true),
            member("/lib", "shared-lib", false),
        ];
        let plan = plan_batched_create("tg/feat", &members, None);
        assert_eq!(plan.members[0].plan, MemberPlan::Exists);
        assert_eq!(plan.members[1].plan, MemberPlan::Exists);
        assert_eq!(plan.members[2].plan, MemberPlan::Create);
    }

    #[test]
    fn subset_filter_restricts_and_reports_unknown() {
        let members = vec![
            member("/api", "api", false),
            member("/web", "web", false),
            member("/lib", "shared-lib", false),
        ];
        let filter = vec!["api".to_string(), "web".to_string(), "typo".to_string()];
        let plan = plan_batched_create("tg/x", &members, Some(&filter));
        let names: Vec<&str> = plan.members.iter().map(|m| m.repo_name.as_str()).collect();
        assert_eq!(names, vec!["api", "web"]);
        assert_eq!(plan.unknown_repos, vec!["typo".to_string()]);
    }

    #[test]
    fn order_is_preserved_and_sparse_sets_allowed() {
        // Members keep their given order; a filter can select a single repo
        // (a sparse feature that touches only one member).
        let members = vec![
            member("/lib", "shared-lib", false),
            member("/api", "api", false),
            member("/web", "web", false),
        ];
        let filter = vec!["web".to_string()];
        let plan = plan_batched_create("tg/x", &members, Some(&filter));
        assert_eq!(plan.members.len(), 1);
        assert_eq!(plan.members[0].repo_name, "web");
        assert!(plan.unknown_repos.is_empty());
    }

    #[test]
    fn empty_filter_selects_nothing() {
        let members = vec![member("/api", "api", false)];
        let empty: Vec<String> = Vec::new();
        let plan = plan_batched_create("tg/x", &members, Some(&empty));
        assert!(plan.members.is_empty());
        assert!(plan.unknown_repos.is_empty());
    }
}
