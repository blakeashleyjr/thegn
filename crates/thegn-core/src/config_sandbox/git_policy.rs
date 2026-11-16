//! Pure `[sandbox.git]` policy: ref-glob matching, pull/push ref checks, the
//! bind-mount plan for the git common dir, and config warnings (THE-2009,
//! THE-2021).
//!
//! Nothing here touches the filesystem: callers pass the paths in and act on
//! the returned plan (the bwrap wiring lives in `sandbox.rs`).

use std::path::{Path, PathBuf};

use super::{GitCredential, SandboxGitConfig, SandboxGitMode};

/// Does `refname` match `pattern`?
///
/// Supported forms: an exact ref (`refs/heads/main`), a trailing `/*` (exactly
/// one more path component: `refs/heads/tg/*` matches `refs/heads/tg/a` but not
/// `refs/heads/tg/a/b`) and a trailing `/**` (one or more components at any
/// depth). No other wildcard is recognized, so a `*` anywhere else never
/// matches. Empty inputs and anything containing `..` never match.
pub fn ref_glob_matches(pattern: &str, refname: &str) -> bool {
    if pattern.is_empty() || refname.is_empty() || pattern.contains("..") || refname.contains("..")
    {
        return false;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        if prefix.contains('*') {
            return false;
        }
        return refname
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| !rest.is_empty() && !rest.split('/').any(str::is_empty));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        if prefix.contains('*') {
            return false;
        }
        return refname
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'));
    }
    !pattern.contains('*') && pattern == refname
}

/// Why `pattern` is not a usable ref glob, or `None` when it is.
pub fn ref_glob_error(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return Some("empty ref glob".to_string());
    }
    if pattern.contains("..") {
        return Some(format!("ref glob {pattern:?} contains \"..\""));
    }
    if !pattern.starts_with("refs/") {
        return Some(format!("ref glob {pattern:?} must start with \"refs/\""));
    }
    let stem = pattern
        .strip_suffix("/**")
        .or_else(|| pattern.strip_suffix("/*"))
        .unwrap_or(pattern);
    if stem.contains('*') {
        return Some(format!(
            "ref glob {pattern:?}: only a trailing \"/*\" or \"/**\" wildcard is supported"
        ));
    }
    if stem.split('/').any(str::is_empty) {
        return Some(format!("ref glob {pattern:?} has an empty path component"));
    }
    None
}

/// The fully-qualified ref of the merge target: a bare branch name becomes
/// `refs/heads/<name>`; an already-qualified `refs/...` is kept.
fn target_ref(target_branch: &str) -> String {
    if target_branch.starts_with("refs/") {
        target_branch.to_string()
    } else {
        format!("refs/heads/{target_branch}")
    }
}

/// May `refname` be accepted back from a sandbox? Refuses a malformed ref, the
/// merge target (`refs/heads/<target>`, an implicit deny regardless of
/// `deny_refs`) and anything matching a `deny_refs` glob.
pub fn check_pull_ref(
    cfg: &SandboxGitConfig,
    refname: &str,
    target_branch: Option<&str>,
) -> Result<(), String> {
    if refname.is_empty()
        || refname.contains("..")
        || !refname.starts_with("refs/")
        || refname.split('/').any(str::is_empty)
        || refname.ends_with(".lock")
    {
        return Err(format!("refusing malformed ref {refname:?}"));
    }
    if let Some(target) = target_branch.map(str::trim).filter(|t| !t.is_empty())
        && target_ref(target) == refname
    {
        return Err(format!(
            "refusing {refname}: it is the merge target; sandbox work lands through the queue"
        ));
    }
    if let Some(pattern) = cfg
        .deny_refs
        .iter()
        .find(|pattern| ref_glob_matches(pattern, refname))
    {
        return Err(format!(
            "refusing {refname}: matches sandbox.git.deny_refs entry {pattern:?}"
        ));
    }
    Ok(())
}

/// May a sandbox push `refname`? Every pull refusal applies; `read-only` and
/// `commit-only` never push; `push-lane` additionally requires a `push_refs`
/// match; `full` is bounded by the deny list only.
pub fn check_push_ref(
    cfg: &SandboxGitConfig,
    refname: &str,
    target_branch: Option<&str>,
) -> Result<(), String> {
    check_pull_ref(cfg, refname, target_branch)?;
    match cfg.mode {
        SandboxGitMode::ReadOnly | SandboxGitMode::CommitOnly => Err(format!(
            "refusing push of {refname}: sandbox.git.mode = \"{}\" does not push",
            cfg.mode
        )),
        SandboxGitMode::PushLane => {
            if cfg
                .push_refs
                .iter()
                .any(|pattern| ref_glob_matches(pattern, refname))
            {
                Ok(())
            } else {
                Err(format!(
                    "refusing push of {refname}: no sandbox.git.push_refs entry matches"
                ))
            }
        }
        SandboxGitMode::Full => Ok(()),
    }
}

/// Bind plan for the git common dir of a sandboxed linked worktree.
///
/// Apply in order: every `ro` bind, then every `rw` carve-out, then every
/// `ro_after` re-pin (bwrap applies binds in argument order, so a later bind
/// over a subpath wins). `ro_after` must be applied after the worktree's own
/// bind (it re-pins `<worktree>/.git` over it). Before launch the host creates each `mkdirs` entry
/// (a bind target must exist) and must create any missing `ro_after` file
/// empty, so the re-pin always applies and the sandbox cannot create it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitMountPlan {
    pub ro: Vec<PathBuf>,
    pub rw: Vec<PathBuf>,
    pub ro_after: Vec<PathBuf>,
    pub mkdirs: Vec<PathBuf>,
    /// The caller must also bind the worktree itself read-only.
    pub worktree_ro: bool,
    /// The mode wants a git credential (`full`). Only `repo-token` /
    /// `user-token` hand one over today (the host GitHub token, see
    /// [`SandboxGitConfig::admits_git_token`]); other credentials hand none.
    pub credential_wanted: bool,
}

/// The directory under `refs/heads/` holding `branch`'s loose ref, or an error
/// for a branch that cannot be confined (top level, malformed).
fn branch_parent(branch: &str) -> Result<&str, String> {
    let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    if branch.is_empty()
        || branch.contains("..")
        || branch.contains('*')
        || branch.split('/').any(str::is_empty)
    {
        return Err(format!("malformed lane branch {branch:?}"));
    }
    match branch.rsplit_once('/') {
        Some((parent, _)) => Ok(parent),
        None => Err(format!(
            "commit-only needs a namespaced lane branch (e.g. tg/<name>); {branch} would expose every top-level branch"
        )),
    }
}

/// Does the writable `carve` (`refs/heads/<parent>`) also contain the merge
/// target or anything a `deny_refs` entry protects? Returns what it exposes.
fn carve_out_conflict(
    cfg: &SandboxGitConfig,
    carve: &str,
    target_branch: Option<&str>,
) -> Option<String> {
    let dir = format!("{carve}/");
    if let Some(target) = target_branch.map(str::trim).filter(|t| !t.is_empty()) {
        let target = target_ref(target);
        if target.starts_with(&dir) {
            return Some(format!("the merge target {target}"));
        }
    }
    cfg.deny_refs.iter().find_map(|p| {
        let conflict = if let Some(s) = p.strip_suffix("/**") {
            s == carve || s.starts_with(&dir) || carve.starts_with(&format!("{s}/"))
        } else if let Some(s) = p.strip_suffix("/*") {
            s == carve || s.starts_with(&dir)
        } else {
            p.starts_with(&dir)
        };
        conflict.then(|| format!("sandbox.git.deny_refs entry {p:?}"))
    })
}

/// Plan the git common-dir binds for `cfg.mode`.
///
/// - `read-only`: the common dir read-only and `worktree_ro` set.
/// - `commit-only`: the common dir read-only, then read-write `objects/`,
///   `admin_dir` (`<common>/worktrees/<name>`: per-worktree HEAD, index,
///   logs/HEAD, ORIG_HEAD), `refs/heads/<branch parent>/` and
///   `logs/refs/heads/<branch parent>/`; then `commondir`, `gitdir` and
///   `config.worktree` inside `admin_dir` re-pinned read-only (a writable
///   `commondir` would point host git at a sandbox-planted common dir with its
///   own config and hooks). `hooks/`, `config`, `packed-refs`,
///   `refs/heads/main` and every other ref stay read-only.
/// - `push-lane`: the same binds (pushes go through thegn, not `.git` writes).
/// - `full`: the same binds plus `credential_wanted`.
///
/// Residuals (accepted, documented for the sandbox.rs caller):
/// - lanes share the `refs/heads/tg/` directory, so commit-only protects
///   `main`, tags and non-`tg/` branches, not sibling lanes; a sibling ref can
///   even be rewritten into a symref (`ref: refs/heads/main`), so host code
///   that later updates a lane ref must not dereference symrefs;
/// - the object store is shared and writable, so ref protection is not
///   content integrity for objects the host later reads;
/// - deleting a branch whose ref is packed fails (`packed-refs` is read-only);
/// - `admin_dir/HEAD` is writable, so the sandbox can make it a symref to
///   `refs/heads/main`; host code that later commits, merges or fast-forwards
///   in that worktree must verify `HEAD` is the lane branch first;
/// - the rw index plus the rw worktree let the sandbox plant a gitlink plus
///   `<sub>/.git` pointing at its own git dir; host `git status` recurses into
///   populated submodules and would run that dir's `core.fsmonitor`, so host
///   git in a sandboxed worktree must run with `--ignore-submodules` and
///   `-c core.fsmonitor=false` (or equivalent);
/// - the object store is rw, so existing object files (including main's
///   blobs) can be rewritten in place.
///
/// Every mode re-pins `<worktree>/.git` read-only first in `ro_after`: a
/// writable gitfile could be pointed at a planted git dir whose `config` runs
/// `core.fsmonitor`/`core.hooksPath` on the next host `git -C <worktree>`.
///
/// A top-level branch (no `/`) is refused: its loose ref lives directly in
/// `refs/heads/`, so the carve-out would expose every top-level branch. The
/// caller warns and keeps today's read-write common dir.
///
/// The writable `refs/heads/<parent>/` carve-out is also refused when it would
/// contain the merge target or a `deny_refs` entry. For that error the caller
/// must fall back to read-only git, not read-write.
pub fn plan_git_mounts(
    cfg: &SandboxGitConfig,
    common_dir: &Path,
    admin_dir: &Path,
    worktree: &Path,
    branch: &str,
    target_branch: Option<&str>,
) -> Result<GitMountPlan, String> {
    let gitfile = worktree.join(".git");
    if cfg.mode == SandboxGitMode::ReadOnly {
        return Ok(GitMountPlan {
            ro: vec![common_dir.to_path_buf()],
            ro_after: vec![gitfile],
            worktree_ro: true,
            ..GitMountPlan::default()
        });
    }
    let parent = branch_parent(branch)?;
    let carve = format!("refs/heads/{parent}");
    if let Some(what) = carve_out_conflict(cfg, &carve, target_branch) {
        return Err(format!(
            "commit-only cannot confine {branch}: the writable {carve}/ would also expose {what}"
        ));
    }
    let heads = common_dir.join("refs/heads").join(parent);
    let logs = common_dir.join("logs/refs/heads").join(parent);
    Ok(GitMountPlan {
        ro: vec![common_dir.to_path_buf()],
        rw: vec![
            common_dir.join("objects"),
            admin_dir.to_path_buf(),
            heads.clone(),
            logs.clone(),
        ],
        ro_after: std::iter::once(gitfile)
            .chain(
                ["commondir", "gitdir", "config.worktree"]
                    .iter()
                    .map(|name| admin_dir.join(name)),
            )
            .collect(),
        mkdirs: vec![heads, logs],
        worktree_ro: false,
        credential_wanted: cfg.mode == SandboxGitMode::Full,
    })
}

/// Config warnings for `[sandbox.git]` (wired into `config validate` by the
/// sandbox.rs lane).
pub fn validate(cfg: &SandboxGitConfig) -> Vec<String> {
    let mut out = Vec::new();
    if cfg.credential != GitCredential::None {
        if cfg.mode != SandboxGitMode::Full {
            out.push(format!(
                "sandbox.git.credential = \"{}\" is only used in mode = \"full\"",
                cfg.credential
            ));
        }
        if !matches!(
            cfg.credential,
            GitCredential::RepoToken | GitCredential::UserToken
        ) {
            out.push(
                "sandbox.git.credential modes are not implemented yet; the sandbox gets no git credential"
                    .to_string(),
            );
        }
    }
    for (key, list) in [("push_refs", &cfg.push_refs), ("deny_refs", &cfg.deny_refs)] {
        out.extend(
            list.iter()
                .filter_map(|pattern| ref_glob_error(pattern))
                .map(|e| format!("sandbox.git.{key}: {e}")),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, GitTransport, SandboxConfig, SandboxGitOverlay, SandboxOverlay};

    #[test]
    fn glob_table() {
        let cases = [
            ("refs/heads/tg/*", "refs/heads/tg/a", true),
            ("refs/heads/tg/*", "refs/heads/tg/a/b", false),
            ("refs/heads/tg/*", "refs/heads/tgx", false),
            ("refs/heads/tg/*", "refs/heads/tg/", false),
            ("refs/heads/tg/*", "refs/heads/tg", false),
            ("refs/tags/**", "refs/tags/v1", true),
            ("refs/tags/**", "refs/tags/a/b/c", true),
            ("refs/tags/**", "refs/tags", false),
            ("refs/tags/**", "refs/tags/", false),
            ("refs/tags/**", "refs/tags/a//b", false),
            ("refs/tags/**", "refs/tagsx/a", false),
            ("refs/heads/main", "refs/heads/main", true),
            ("refs/heads/main", "refs/heads/main2", false),
            ("refs/heads/m*", "refs/heads/main", false),
            ("refs/*/x/*", "refs/a/x/b", false),
            ("refs/*/**", "refs/a/b", false),
            ("", "refs/heads/main", false),
            ("refs/heads/main", "", false),
            ("refs/heads/**", "refs/heads/../main", false),
            ("refs/heads/../x/*", "refs/heads/../x/a", false),
        ];
        for (pattern, refname, want) in cases {
            assert_eq!(
                ref_glob_matches(pattern, refname),
                want,
                "{pattern} vs {refname}"
            );
        }
    }

    #[test]
    fn glob_errors() {
        assert_eq!(ref_glob_error("refs/heads/tg/*"), None);
        assert_eq!(ref_glob_error("refs/tags/**"), None);
        assert_eq!(ref_glob_error("refs/heads/main"), None);
        for bad in [
            "",
            "refs/../x",
            "heads/main",
            "refs/heads/m*",
            "refs/*/x",
            "refs//x/*",
        ] {
            assert!(ref_glob_error(bad).is_some(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn pull_denies_implicit_target_and_deny_list() {
        let cfg = SandboxGitConfig::default();
        assert!(check_pull_ref(&cfg, "refs/heads/tg/lane", Some("main")).is_ok());
        assert!(check_pull_ref(&cfg, "refs/heads/tg/lane", None).is_ok());
        // Implicit target deny, even when the deny list is empty.
        let open = SandboxGitConfig {
            deny_refs: Vec::new(),
            ..SandboxGitConfig::default()
        };
        let err = check_pull_ref(&open, "refs/heads/develop", Some("develop")).unwrap_err();
        assert!(err.contains("merge target"), "{err}");
        assert!(check_pull_ref(&open, "refs/heads/develop", Some("refs/heads/develop")).is_err());
        assert!(check_pull_ref(&open, "refs/heads/develop", Some("  ")).is_ok());
        // Default deny list.
        let err = check_pull_ref(&cfg, "refs/heads/main", None).unwrap_err();
        assert!(err.contains("deny_refs"), "{err}");
        assert!(check_pull_ref(&cfg, "refs/tags/v1", None).is_err());
        // Malformed refs.
        for bad in [
            "",
            "heads/x",
            "refs/heads/../main",
            "refs/heads//main",
            "refs/heads/tg/a.lock",
        ] {
            assert!(check_pull_ref(&cfg, bad, None).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn push_rules_per_mode() {
        let with = |mode| SandboxGitConfig {
            mode,
            ..SandboxGitConfig::default()
        };
        for mode in [SandboxGitMode::ReadOnly, SandboxGitMode::CommitOnly] {
            let err = check_push_ref(&with(mode), "refs/heads/tg/a", Some("main")).unwrap_err();
            assert!(err.contains("does not push"), "{err}");
        }
        let lane = with(SandboxGitMode::PushLane);
        assert!(check_push_ref(&lane, "refs/heads/tg/a", Some("main")).is_ok());
        let err = check_push_ref(&lane, "refs/heads/feature", Some("main")).unwrap_err();
        assert!(err.contains("push_refs"), "{err}");
        assert!(check_push_ref(&lane, "refs/heads/main", Some("main")).is_err());
        let full = with(SandboxGitMode::Full);
        assert!(check_push_ref(&full, "refs/heads/feature", Some("main")).is_ok());
        assert!(check_push_ref(&full, "refs/tags/v1", Some("main")).is_err());
        assert!(check_push_ref(&full, "refs/heads/main", None).is_err());
    }

    fn fixture() -> (PathBuf, PathBuf) {
        let common = PathBuf::from("/repo/.git");
        let admin = common.join("worktrees/lane");
        (common, admin)
    }

    fn mode_cfg(mode: SandboxGitMode) -> SandboxGitConfig {
        SandboxGitConfig {
            mode,
            ..SandboxGitConfig::default()
        }
    }

    fn plan_for(
        cfg: &SandboxGitConfig,
        branch: &str,
        target: Option<&str>,
    ) -> Result<GitMountPlan, String> {
        let (common, admin) = fixture();
        plan_git_mounts(cfg, &common, &admin, Path::new("/wt"), branch, target)
    }

    #[test]
    fn read_only_plan() {
        let (common, _) = fixture();
        let plan =
            plan_for(&mode_cfg(SandboxGitMode::ReadOnly), "main", Some("main")).expect("plan");
        assert_eq!(plan.ro, [common]);
        assert!(plan.rw.is_empty());
        assert_eq!(plan.ro_after, [PathBuf::from("/wt/.git")]);
        assert!(plan.mkdirs.is_empty());
        assert!(plan.worktree_ro);
        assert!(!plan.credential_wanted);
    }

    #[test]
    fn writable_mode_plans() {
        let (common, admin) = fixture();
        for mode in [
            SandboxGitMode::CommitOnly,
            SandboxGitMode::PushLane,
            SandboxGitMode::Full,
        ] {
            let plan = plan_for(&mode_cfg(mode), "tg/lane", Some("main")).expect("plan");
            // The common dir is bound read-only first; carve-outs come after.
            assert_eq!(plan.ro, [common.clone()], "{mode}");
            assert_eq!(
                plan.rw,
                [
                    PathBuf::from("/repo/.git/objects"),
                    admin.clone(),
                    PathBuf::from("/repo/.git/refs/heads/tg"),
                    PathBuf::from("/repo/.git/logs/refs/heads/tg"),
                ],
                "{mode}"
            );
            assert!(
                plan.rw.iter().all(|p| p.starts_with(&common)),
                "every carve-out sits beneath the ro common dir"
            );
            assert_eq!(
                plan.ro_after,
                [
                    PathBuf::from("/wt/.git"),
                    admin.join("commondir"),
                    admin.join("gitdir"),
                    admin.join("config.worktree"),
                ],
                "{mode}"
            );
            assert_eq!(
                plan.mkdirs,
                [
                    PathBuf::from("/repo/.git/refs/heads/tg"),
                    PathBuf::from("/repo/.git/logs/refs/heads/tg"),
                ]
            );
            // hooks, config and packed-refs are never carved out.
            for blocked in ["hooks", "config", "packed-refs", "refs/heads/main"] {
                let path = common.join(blocked);
                assert!(
                    !plan.rw.iter().any(|rw| path.starts_with(rw)),
                    "{blocked} must stay read-only"
                );
            }
            assert!(!plan.worktree_ro);
            assert_eq!(plan.credential_wanted, mode == SandboxGitMode::Full);
        }
    }

    #[test]
    fn nested_and_qualified_branches_use_their_parent_dir() {
        let plan = plan_for(
            &mode_cfg(SandboxGitMode::CommitOnly),
            "refs/heads/tg/b139/lane",
            Some("main"),
        )
        .expect("plan");
        assert_eq!(
            plan.mkdirs[0],
            PathBuf::from("/repo/.git/refs/heads/tg/b139")
        );
    }

    #[test]
    fn top_level_and_malformed_branches_are_refused() {
        let err =
            plan_for(&mode_cfg(SandboxGitMode::CommitOnly), "main", Some("main")).unwrap_err();
        assert_eq!(
            err,
            "commit-only needs a namespaced lane branch (e.g. tg/<name>); main would expose every top-level branch"
        );
        for bad in ["", "tg//x", "tg/../main", "tg/*", "/tg/x", "tg/x/"] {
            assert!(
                plan_for(&mode_cfg(SandboxGitMode::PushLane), bad, Some("main")).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn carve_out_refuses_merge_target_and_deny_refs() {
        let cfg = SandboxGitConfig::default();
        assert!(plan_for(&cfg, "tg/lane", Some("main")).is_ok());
        let err = plan_for(&cfg, "tg/lane", Some("tg/b139-integration")).unwrap_err();
        assert!(err.contains("merge target"), "{err}");
        let deny = |refs: &[&str]| SandboxGitConfig {
            deny_refs: refs.iter().map(|r| (*r).to_string()).collect(),
            ..SandboxGitConfig::default()
        };
        for refused in ["refs/heads/tg/release", "refs/heads/**", "refs/heads/tg/*"] {
            let err = plan_for(&deny(&[refused]), "tg/lane", Some("main")).unwrap_err();
            assert!(err.contains("deny_refs"), "{refused}: {err}");
        }
        // One level below refs/heads never reaches refs/heads/tg/x.
        assert!(plan_for(&deny(&["refs/heads/*"]), "tg/lane", Some("main")).is_ok());
    }

    #[test]
    fn validate_warnings() {
        assert!(validate(&SandboxGitConfig::default()).is_empty());
        let warns = validate(&SandboxGitConfig {
            credential: GitCredential::DeployKey,
            ..SandboxGitConfig::default()
        });
        assert_eq!(warns.len(), 2, "{warns:?}");
        assert!(warns[0].contains("only used in mode = \"full\""));
        assert!(warns[1].contains("not implemented yet"));
        let warns = validate(&SandboxGitConfig {
            mode: SandboxGitMode::Full,
            credential: GitCredential::GithubApp,
            push_refs: vec!["refs/heads/x*".into()],
            deny_refs: vec![String::new()],
            ..SandboxGitConfig::default()
        });
        assert_eq!(warns.len(), 3, "{warns:?}");
        assert!(warns[1].starts_with("sandbox.git.push_refs:"));
        assert!(warns[2].starts_with("sandbox.git.deny_refs:"));
        assert!(
            validate(&SandboxGitConfig {
                mode: SandboxGitMode::Full,
                credential: GitCredential::RepoToken,
                ..SandboxGitConfig::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn git_token_withheld_unless_full_with_token_credential() {
        let default = SandboxGitConfig::default();
        assert!(!default.admits_git_token());
        assert!(!default.admits_env("GH_TOKEN") && !default.admits_env("GITHUB_TOKEN"));
        assert!(default.admits_env("ANTHROPIC_API_KEY") && default.admits_env("TERM"));
        // A token credential outside `full`, or `full` without one, still withholds.
        for (mode, credential) in [
            (SandboxGitMode::PushLane, GitCredential::RepoToken),
            (SandboxGitMode::Full, GitCredential::None),
            (SandboxGitMode::Full, GitCredential::DeployKey),
            (SandboxGitMode::Full, GitCredential::GithubApp),
        ] {
            let cfg = SandboxGitConfig {
                mode,
                credential,
                ..SandboxGitConfig::default()
            };
            assert!(!cfg.admits_env("GH_TOKEN"), "{mode} {credential}");
        }
        for credential in [GitCredential::RepoToken, GitCredential::UserToken] {
            let cfg = SandboxGitConfig {
                mode: SandboxGitMode::Full,
                credential,
                ..SandboxGitConfig::default()
            };
            assert!(cfg.admits_git_token(), "{credential}");
            assert!(cfg.admits_env("GH_TOKEN") && cfg.admits_env("GITHUB_TOKEN"));
        }
    }

    #[test]
    fn passthrough_env_drops_github_token_by_default() {
        let _env = crate::testenv::EnvGuard::set(&[
            ("GH_TOKEN", "gh-secret"),
            ("GITHUB_TOKEN", "github-secret"),
            ("ANTHROPIC_API_KEY", "agent-key"),
        ]);
        let firewall = crate::profile::CredentialFirewall::inactive();
        let keys = |sb: &SandboxConfig| -> Vec<String> {
            sb.passthrough_env_for(&firewall)
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        // Default `[sandbox.git] credential = "none"`: no GitHub token, even
        // though the default env_passthrough lists both names.
        let default = SandboxConfig::default();
        assert!(default.env_passthrough.iter().any(|k| k == "GH_TOKEN"));
        let got = keys(&default);
        assert!(
            !got.iter().any(|k| k == "GH_TOKEN" || k == "GITHUB_TOKEN"),
            "{got:?}"
        );
        assert!(got.iter().any(|k| k == "ANTHROPIC_API_KEY"), "{got:?}");
        // `full` + `repo-token` re-enables them.
        let mut granted = SandboxConfig::default();
        granted.git.mode = SandboxGitMode::Full;
        granted.git.credential = GitCredential::RepoToken;
        let got = keys(&granted);
        assert!(got.iter().any(|k| k == "GH_TOKEN"), "{got:?}");
        assert!(got.iter().any(|k| k == "GITHUB_TOKEN"), "{got:?}");
        assert!(got.iter().any(|k| k == "ANTHROPIC_API_KEY"), "{got:?}");
    }

    #[test]
    fn overlay_merge_overrides_present_keys_only() {
        let base = SandboxGitConfig::default();
        let merged = base.merged(&SandboxGitOverlay {
            mode: Some(SandboxGitMode::ReadOnly),
            ..SandboxGitOverlay::default()
        });
        assert_eq!(merged.mode, SandboxGitMode::ReadOnly);
        assert_eq!(merged.transport, GitTransport::Bundle);
        assert_eq!(merged.credential, GitCredential::None);
        assert_eq!(merged.push_refs, base.push_refs);
        assert_eq!(merged.deny_refs, base.deny_refs);
        let all = base.merged(&SandboxGitOverlay {
            mode: Some(SandboxGitMode::Full),
            transport: Some(GitTransport::Clone),
            credential: Some(GitCredential::UserToken),
            push_refs: Some(vec!["refs/heads/x/*".into()]),
            deny_refs: Some(Vec::new()),
        });
        assert_eq!(all.transport, GitTransport::Clone);
        assert_eq!(all.credential, GitCredential::UserToken);
        assert_eq!(all.push_refs, ["refs/heads/x/*"]);
        assert!(all.deny_refs.is_empty());
        assert!(SandboxGitOverlay::default().is_empty());
        assert!(
            !SandboxGitOverlay {
                deny_refs: Some(Vec::new()),
                ..SandboxGitOverlay::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn sandbox_overlay_applies_and_reports_git() {
        let overlay = SandboxOverlay {
            git: Some(SandboxGitOverlay {
                mode: Some(SandboxGitMode::PushLane),
                ..SandboxGitOverlay::default()
            }),
            ..SandboxOverlay::default()
        };
        assert!(!overlay.is_empty());
        let mut base = SandboxConfig::default();
        overlay.apply(&mut base);
        assert_eq!(base.git.mode, SandboxGitMode::PushLane);
        assert_eq!(base.git.deny_refs, SandboxGitConfig::default().deny_refs);
        let empty = SandboxOverlay {
            git: Some(SandboxGitOverlay::default()),
            ..SandboxOverlay::default()
        };
        assert!(empty.is_empty());
    }

    #[test]
    fn defaults_and_toml_round_trip() {
        let cfg: Config = toml::from_str("").expect("empty config");
        assert_eq!(cfg.sandbox.git, SandboxGitConfig::default());
        assert_eq!(cfg.sandbox.git.mode, SandboxGitMode::CommitOnly);
        assert_eq!(cfg.sandbox.git.push_refs, ["refs/heads/tg/*"]);
        assert_eq!(
            cfg.sandbox.git.deny_refs,
            ["refs/heads/main", "refs/tags/*"]
        );

        let pretty = "[sandbox.git]\nmode = \"read-only\"\n";
        let cfg: Config = toml::from_str(pretty).expect("pretty");
        assert_eq!(cfg.sandbox.git.mode, SandboxGitMode::ReadOnly);
        assert_eq!(cfg.sandbox.git.transport, GitTransport::Bundle);

        let minified = "sandbox={git={mode=\"push_lane\",transport=\"clone\",credential=\"deploy_key\",push_refs=[\"refs/heads/x/*\"],deny_refs=[]}}\n";
        let cfg: Config = toml::from_str(minified).expect("minified");
        assert_eq!(cfg.sandbox.git.mode, SandboxGitMode::PushLane);
        assert_eq!(cfg.sandbox.git.transport, GitTransport::Clone);
        assert_eq!(cfg.sandbox.git.credential, GitCredential::DeployKey);
        assert_eq!(cfg.sandbox.git.push_refs, ["refs/heads/x/*"]);
        assert!(cfg.sandbox.git.deny_refs.is_empty());

        // Serialize back to the canonical spellings and re-parse identically.
        let text = toml::to_string(&cfg.sandbox.git).expect("serialize");
        assert!(text.contains("mode = \"push-lane\""), "{text}");
        assert!(text.contains("credential = \"deploy-key\""), "{text}");
        let back: SandboxGitConfig = toml::from_str(&text).expect("reparse");
        assert_eq!(back, cfg.sandbox.git);
    }

    #[test]
    fn alias_spellings_parse() {
        let cases = [
            ("read-only", SandboxGitMode::ReadOnly),
            ("read_only", SandboxGitMode::ReadOnly),
            ("readonly", SandboxGitMode::ReadOnly),
            ("commit-only", SandboxGitMode::CommitOnly),
            ("commit_only", SandboxGitMode::CommitOnly),
            ("push-lane", SandboxGitMode::PushLane),
            ("push_lane", SandboxGitMode::PushLane),
            ("full", SandboxGitMode::Full),
        ];
        for (text, want) in cases {
            assert_eq!(SandboxGitMode::from_str_validated(text), Ok(want), "{text}");
        }
        assert!(SandboxGitMode::from_str_validated("rw").is_err());
        assert_eq!(
            GitTransport::from_str_validated("clone"),
            Ok(GitTransport::Clone)
        );
        for (text, want) in [
            ("off", GitCredential::None),
            ("repo_token", GitCredential::RepoToken),
            ("github-app", GitCredential::GithubApp),
            ("user_token", GitCredential::UserToken),
        ] {
            assert_eq!(GitCredential::from_str_validated(text), Ok(want), "{text}");
        }
    }

    #[test]
    fn env_overlay_mode_only_keeps_defaults() {
        let src = "[sandbox.git]\nmode = \"full\"\n\n[env.ci.sandbox.git]\nmode = \"read-only\"\n";
        let cfg: Config = toml::from_str(src).expect("parse");
        let mut base = cfg.sandbox.clone();
        cfg.env["ci"].sandbox.clone().apply(&mut base);
        assert_eq!(base.git.mode, SandboxGitMode::ReadOnly);
        assert_eq!(base.git.push_refs, SandboxGitConfig::default().push_refs);
        assert_eq!(base.git.deny_refs, SandboxGitConfig::default().deny_refs);
    }
}
