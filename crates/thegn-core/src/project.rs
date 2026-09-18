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
/// `{branch_prefix}{feature}`, with the feature name used EXACTLY (THE-516).
///
/// Membership is branch-name equality, so the name must be lossless: it used
/// to be `slugify(feature)`, which federated `payments/retry`,
/// `payments-retry`, `payments_retry` and `Payments-Retry` into one branch
/// across every member repo. A name that is not already a valid Git branch
/// name is refused with a suggested literal, never silently normalized —
/// a display alias may propose a branch but cannot define identity.
///
/// Resolved once and used verbatim in each member — deliberately NOT per-repo
/// deduped (dedup would make the names differ across repos).
pub fn feature_branch_name(feature: &str, branch_prefix: &str) -> Result<String, String> {
    let branch = format!("{branch_prefix}{feature}");
    if feature.is_empty() || !is_valid_branch_name(&branch) {
        let slug = util::slugify(feature);
        let hint = if slug.is_empty() {
            String::new()
        } else {
            format!(" (did you mean {slug:?}?)")
        };
        return Err(format!(
            "feature name {feature:?} is not a literal Git branch name{hint}; \
             feature identity is the exact branch, so it is never normalized"
        ));
    }
    Ok(branch)
}

/// Pure approximation of `git check-ref-format --branch` for the rules that
/// matter to a user-typed name. Conservative: anything it accepts Git accepts.
pub fn is_valid_branch_name(name: &str) -> bool {
    if name.is_empty()
        || name == "@"
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
    {
        return false;
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return false;
    }
    name.split('/')
        .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
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
    fn feature_names_are_literal_and_never_normalized_together() {
        let names = [
            "payments/retry",
            "payments-retry",
            "payments_retry",
            "Payments-Retry",
        ];
        let branches: Vec<String> = names
            .iter()
            .map(|n| feature_branch_name(n, "tg/").unwrap())
            .collect();
        for (i, b) in branches.iter().enumerate() {
            assert_eq!(b, &format!("tg/{}", names[i]));
            assert!(branches[i + 1..].iter().all(|o| o != b), "{b} federated");
        }
    }

    #[test]
    fn non_branch_feature_names_are_refused_with_a_hint() {
        let err = feature_branch_name("payments retry", "tg/").unwrap_err();
        assert!(err.contains("\"payments-retry\""), "{err}");
        for bad in ["", "a..b", "x~1", "a:b", "a/", ".hidden", "x.lock", "a//b"] {
            assert!(feature_branch_name(bad, "tg/").is_err(), "{bad:?} accepted");
        }
        // With no prefix, a leading dash would read as a git option.
        assert!(feature_branch_name("-x", "").is_err());
    }

    #[test]
    fn literal_feature_branch_matches_worktree_human_base_for_slug_names() {
        // A name that is already its own slug resolves identically on both
        // paths, so a hand-made worktree still joins its project feature.
        let cfg = crate::config::Config {
            branch_prefix: "tg/".into(),
            ..Default::default()
        };
        assert_eq!(
            feature_branch_name("payments-retry", &cfg.branch_prefix).unwrap(),
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
