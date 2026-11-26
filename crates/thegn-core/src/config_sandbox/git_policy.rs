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

/// The fully-qualified branch ref of a merge target / default branch: a
/// leading `refs/heads/` is normalised away, then `refs/heads/<name>` is
/// rebuilt. Blank names yield `None` (unknown).
fn branch_ref(branch: Option<&str>) -> Option<String> {
    let name = branch.map(str::trim).filter(|b| !b.is_empty())?;
    let name = name.strip_prefix("refs/heads/").unwrap_or(name);
    (!name.is_empty()).then(|| format!("refs/heads/{name}"))
}

/// The implicitly protected branch refs: the merge target and the default
/// branch (each when known), labelled for error messages.
fn protected_refs(
    target_branch: Option<&str>,
    default_branch: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(target) = branch_ref(target_branch) {
        out.push(("merge target", target));
    }
    if let Some(default) = branch_ref(default_branch)
        && !out.iter().any(|(_, r)| *r == default)
    {
        out.push(("default branch", default));
    }
    out
}

/// Is `refname` a well-formed `refs/heads/<branch>` ref? Applies git's
/// `check-ref-format` rules: no empty component, no component starting with
/// `.` or ending in `.lock`, no `..`, `@{`, control characters or any of
/// `` ~^:?*[\`` and space, and no trailing `.` or `/`.
fn is_branch_ref(refname: &str) -> bool {
    let Some(branch) = refname.strip_prefix("refs/heads/") else {
        return false;
    };
    !branch.is_empty()
        && !refname.contains("..")
        && !refname.contains("@{")
        && !refname.ends_with('.')
        && !refname.ends_with('/')
        && !refname.chars().any(|c| {
            c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
        && refname
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

/// May `refname` be accepted back from a sandbox? Only a well-formed
/// `refs/heads/` branch is; the merge target and the default branch are an
/// implicit deny regardless of `deny_refs`, and anything matching a
/// `deny_refs` glob is refused.
pub fn check_pull_ref(
    cfg: &SandboxGitConfig,
    refname: &str,
    target_branch: Option<&str>,
    default_branch: Option<&str>,
) -> Result<(), String> {
    if !is_branch_ref(refname) {
        return Err(format!(
            "refusing ref {refname:?}: only well-formed refs/heads/ branches are accepted from a sandbox"
        ));
    }
    if let Some((what, _)) = protected_refs(target_branch, default_branch)
        .into_iter()
        .find(|(_, r)| r == refname)
    {
        return Err(format!(
            "refusing {refname}: it is the {what}; sandbox work lands through the queue"
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
    default_branch: Option<&str>,
) -> Result<(), String> {
    check_pull_ref(cfg, refname, target_branch, default_branch)?;
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
    /// The mode wants a git credential (`full`). `repo-token` / `user-token`
    /// hand over the host GitHub token (see
    /// [`SandboxGitConfig::admits_git_token`]); `deploy-key` binds the
    /// configured key read-only on local placements (see
    /// [`SandboxGitConfig::wants_deploy_key`]); `none` and `github-app` hand none.
    pub credential_wanted: bool,
}

/// Filesystem facts used to validate one git control pin without making the
/// policy planner perform I/O.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PinTargetFacts {
    pub directory: bool,
    pub regular_file: bool,
    pub symlink: bool,
    pub inside_common: bool,
}

/// Whether a git control pin target is safe to bind read-only.
pub fn pin_target_ok(path: &Path, common: &Path, facts: PinTargetFacts) -> bool {
    if facts.symlink {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    match name {
        "hooks" | "info" => facts.directory,
        "config.worktree" => facts.regular_file && facts.inside_common,
        "config" | "commondir" | "gitdir" | ".git" => {
            facts.regular_file || (name == ".git" && path == common && facts.directory)
        }
        _ => false,
    }
}

/// Git control paths that must be re-pinned read-only after git carve-outs.
///
/// When `admin` is the common dir itself (read-only mode, a main checkout or
/// any structural fallback) there is no per-worktree admin dir: the
/// `commondir`/`gitdir`/`config.worktree` pins are skipped, because the common
/// dir is bound read-only already and a caller that creates missing pin
/// targets must never plant an empty `commondir` in the main git dir (git
/// refuses to run on an unreadable one). A main checkout's `<worktree>/.git`
/// is the common dir itself and is skipped too.
pub fn control_pins(common: &Path, admin: &Path, worktree: &Path) -> Vec<PathBuf> {
    let gitfile = worktree.join(".git");
    let mut pins = vec![
        common.join("hooks"),
        common.join("config"),
        common.join("info"),
    ];
    if admin != common {
        pins.extend(
            ["config.worktree", "commondir", "gitdir"]
                .iter()
                .map(|name| admin.join(name)),
        );
    }
    if gitfile != common {
        pins.push(gitfile);
    }
    pins
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
/// target, the default branch or anything a `deny_refs` entry protects?
/// Returns what it exposes. An unknown merge target fails closed: the carve-out
/// cannot be shown not to contain it.
fn carve_out_conflict(
    cfg: &SandboxGitConfig,
    carve: &str,
    target_branch: Option<&str>,
    default_branch: Option<&str>,
) -> Option<String> {
    let dir = format!("{carve}/");
    if branch_ref(target_branch).is_none() {
        return Some("an unknown merge target".to_string());
    }
    if let Some((what, r)) = protected_refs(target_branch, default_branch)
        .into_iter()
        .find(|(_, r)| r.starts_with(&dir))
    {
        return Some(format!("the {what} {r}"));
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
/// Every mode re-pins git control paths read-only in `ro_after`, including the
/// shared hooks/config/info and the per-worktree gitfile and control files.
/// Callers validate pin targets without following symlinks; if any target is
/// unsafe or cannot be created, they fail closed by keeping the common dir
/// read-only.
///
/// A top-level branch (no `/`) is refused: its loose ref lives directly in
/// `refs/heads/`, so the carve-out would expose every top-level branch. The
/// caller warns and keeps today's read-write common dir.
///
/// The writable `refs/heads/<parent>/` carve-out is also refused when it would
/// contain the merge target, the default branch or a `deny_refs` entry, and
/// when `target_branch` is unknown (`None` or blank: fail closed). For that
/// error the caller must fall back to read-only git, not read-write.
pub fn plan_git_mounts(
    cfg: &SandboxGitConfig,
    common_dir: &Path,
    admin_dir: &Path,
    worktree: &Path,
    branch: &str,
    target_branch: Option<&str>,
    default_branch: Option<&str>,
) -> Result<GitMountPlan, String> {
    if cfg.mode == SandboxGitMode::ReadOnly {
        return Ok(GitMountPlan {
            ro: vec![common_dir.to_path_buf()],
            ro_after: control_pins(common_dir, admin_dir, worktree),
            worktree_ro: true,
            ..GitMountPlan::default()
        });
    }
    let parent = branch_parent(branch)?;
    let carve = format!("refs/heads/{parent}");
    if let Some(what) = carve_out_conflict(cfg, &carve, target_branch, default_branch) {
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
        ro_after: control_pins(common_dir, admin_dir, worktree),
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
        match cfg.credential {
            GitCredential::DeployKey if cfg.deploy_key.trim().is_empty() => out.push(
                "sandbox.git.credential = \"deploy-key\" needs sandbox.git.deploy_key (a private key path); the sandbox gets no git credential"
                    .to_string(),
            ),
            GitCredential::GithubApp => out.push(
                "sandbox.git.credential = \"github-app\" is not implemented yet; the sandbox gets no git credential"
                    .to_string(),
            ),
            _ => {}
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

/// Fixed in-sandbox path a `credential = "deploy-key"` key is bound to on
/// backends with their own mount namespace (bwrap, OCI).
pub const DEPLOY_KEY_DEST: &str = "/run/thegn-deploy-key";

/// What a deploy key file is on the host, read with `symlink_metadata` (never
/// following a symlink). `mode` is `None` on platforms without mode bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeployKeyFacts {
    pub regular: bool,
    pub symlink: bool,
    pub owned_by_user: bool,
    pub mode: Option<u32>,
}

/// Is a key with these facts safe to hand a sandbox? A regular file (not a
/// symlink, not a directory) owned by the user with no group/world bits.
/// `Err` carries the reason, worded for the launch warning.
pub fn check_deploy_key(facts: Option<DeployKeyFacts>) -> Result<(), String> {
    let Some(facts) = facts else {
        return Err("the key file does not exist or cannot be read".to_string());
    };
    if facts.symlink {
        return Err("the key path is a symlink (point deploy_key at the file itself)".to_string());
    }
    if !facts.regular {
        return Err("the key path is not a regular file".to_string());
    }
    if !facts.owned_by_user {
        return Err("the key file is not owned by the current user".to_string());
    }
    if facts.mode.is_some_and(|mode| mode & 0o077 != 0) {
        return Err("the key file is group/world accessible (chmod 600 it)".to_string());
    }
    Ok(())
}

/// The `GIT_SSH_COMMAND` selecting the key at `dest` inside the sandbox.
pub fn deploy_key_ssh_command(dest: &str) -> String {
    format!(
        "ssh -i {} -o IdentitiesOnly=yes",
        crate::util::sh_quote(dest)
    )
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
        assert!(check_pull_ref(&cfg, "refs/heads/tg/lane", Some("main"), None).is_ok());
        assert!(check_pull_ref(&cfg, "refs/heads/tg/lane", None, None).is_ok());
        // Implicit target deny, even when the deny list is empty.
        let open = SandboxGitConfig {
            deny_refs: Vec::new(),
            ..SandboxGitConfig::default()
        };
        let err = check_pull_ref(&open, "refs/heads/develop", Some("develop"), None).unwrap_err();
        assert!(err.contains("merge target"), "{err}");
        assert!(
            check_pull_ref(
                &open,
                "refs/heads/develop",
                Some("refs/heads/develop"),
                None
            )
            .is_err()
        );
        assert!(check_pull_ref(&open, "refs/heads/develop", Some("  "), None).is_ok());
        // Default deny list.
        let err = check_pull_ref(&cfg, "refs/heads/main", None, None).unwrap_err();
        assert!(err.contains("deny_refs"), "{err}");
        assert!(check_pull_ref(&cfg, "refs/tags/v1", None, None).is_err());
        // Malformed refs.
        for bad in [
            "",
            "heads/x",
            "refs/heads/../main",
            "refs/heads//main",
            "refs/heads/tg/a.lock",
            "refs/heads/",
            "refs/remotes/origin/x",
            "refs/replace/x",
            "refs/notes/x",
            "refs/stash",
            "refs/tags/x",
            "refs/heads/a b",
            "refs/heads/a~1",
            "refs/heads/a^",
            "refs/heads/a:b",
            "refs/heads/a?",
            "refs/heads/a*",
            "refs/heads/a[b",
            "refs/heads/a\\b",
            "refs/heads/a\tb",
            "refs/heads/a\u{7f}",
            "refs/heads/a@{1}",
            "refs/heads/a.",
            "refs/heads/a/",
            "refs/heads/.hidden",
            "refs/heads/tg/.x",
        ] {
            assert!(check_pull_ref(&cfg, bad, None, None).is_err(), "{bad:?}");
        }
        assert!(check_pull_ref(&open, "refs/heads/tg/a.b@c", None, None).is_ok());
    }

    #[test]
    fn pull_denies_default_branch_distinct_from_target() {
        let open = SandboxGitConfig {
            deny_refs: Vec::new(),
            ..SandboxGitConfig::default()
        };
        let err =
            check_pull_ref(&open, "refs/heads/trunk", Some("develop"), Some("trunk")).unwrap_err();
        assert!(err.contains("default branch"), "{err}");
        // A `refs/heads/` prefix on either name is normalised.
        assert!(check_pull_ref(&open, "refs/heads/trunk", None, Some("refs/heads/trunk")).is_err());
        assert!(
            check_pull_ref(
                &open,
                "refs/heads/develop",
                Some("refs/heads/develop"),
                Some("trunk")
            )
            .is_err()
        );
        assert!(check_pull_ref(&open, "refs/heads/tg/a", Some("develop"), Some("trunk")).is_ok());
        let lane = SandboxGitConfig {
            mode: SandboxGitMode::PushLane,
            push_refs: vec!["refs/heads/*".into()],
            deny_refs: Vec::new(),
            ..SandboxGitConfig::default()
        };
        assert!(check_push_ref(&lane, "refs/heads/trunk", Some("develop"), Some("trunk")).is_err());
        assert!(check_push_ref(&lane, "refs/heads/other", Some("develop"), Some("trunk")).is_ok());
    }

    #[test]
    fn push_rules_per_mode() {
        let with = |mode| SandboxGitConfig {
            mode,
            ..SandboxGitConfig::default()
        };
        for mode in [SandboxGitMode::ReadOnly, SandboxGitMode::CommitOnly] {
            let err =
                check_push_ref(&with(mode), "refs/heads/tg/a", Some("main"), None).unwrap_err();
            assert!(err.contains("does not push"), "{err}");
        }
        let lane = with(SandboxGitMode::PushLane);
        assert!(check_push_ref(&lane, "refs/heads/tg/a", Some("main"), None).is_ok());
        let err = check_push_ref(&lane, "refs/heads/feature", Some("main"), None).unwrap_err();
        assert!(err.contains("push_refs"), "{err}");
        assert!(check_push_ref(&lane, "refs/heads/main", Some("main"), None).is_err());
        let full = with(SandboxGitMode::Full);
        assert!(check_push_ref(&full, "refs/heads/feature", Some("main"), None).is_ok());
        assert!(check_push_ref(&full, "refs/tags/v1", Some("main"), None).is_err());
        assert!(check_push_ref(&full, "refs/heads/main", None, None).is_err());
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
        plan_git_mounts(cfg, &common, &admin, Path::new("/wt"), branch, target, None)
    }

    #[test]
    fn read_only_plan() {
        let (common, _) = fixture();
        let plan =
            plan_for(&mode_cfg(SandboxGitMode::ReadOnly), "main", Some("main")).expect("plan");
        assert_eq!(plan.ro, [common]);
        assert!(plan.rw.is_empty());
        assert_eq!(
            plan.ro_after,
            [
                PathBuf::from("/repo/.git/hooks"),
                PathBuf::from("/repo/.git/config"),
                PathBuf::from("/repo/.git/info"),
                PathBuf::from("/repo/.git/worktrees/lane/config.worktree"),
                PathBuf::from("/repo/.git/worktrees/lane/commondir"),
                PathBuf::from("/repo/.git/worktrees/lane/gitdir"),
                PathBuf::from("/wt/.git"),
            ]
        );
        assert!(plan.mkdirs.is_empty());
        assert!(plan.worktree_ro);
        assert!(!plan.credential_wanted);
    }

    #[test]
    fn legacy_mounts_pin_hooks_and_config_worktree() {
        let (common, admin) = fixture();
        let pins = control_pins(&common, &admin, Path::new("/wt"));
        assert!(pins.contains(&common.join("hooks")));
        assert!(pins.contains(&common.join("config")));
        assert!(pins.contains(&admin.join("config.worktree")));
    }

    #[test]
    fn full_fallback_pins_hooks() {
        let (common, admin) = fixture();
        let plan = plan_git_mounts(
            &mode_cfg(SandboxGitMode::Full),
            &common,
            &admin,
            Path::new("/wt"),
            "tg/lane",
            Some("main"),
            None,
        )
        .expect("plan");
        assert!(plan.ro_after.contains(&common.join("hooks")));
    }

    #[test]
    fn remote_placement_pins_hooks() {
        let common = Path::new("/remote/repo/.git");
        let admin = common.join("worktrees/lane");
        let pins = control_pins(common, &admin, Path::new("/remote/repo/wt"));
        assert!(pins.contains(&common.join("hooks")));
    }

    #[test]
    fn read_only_mode_pins_info() {
        let (common, admin) = fixture();
        let plan = plan_git_mounts(
            &mode_cfg(SandboxGitMode::ReadOnly),
            &common,
            &admin,
            Path::new("/wt"),
            "tg/lane",
            None,
            None,
        )
        .expect("plan");
        assert!(plan.ro_after.contains(&common.join("info")));
    }

    #[test]
    fn control_pins_skip_admin_files_without_an_admin_dir() {
        let common = Path::new("/repo/.git");
        // Read-only mode / structural fallback: admin is the common dir.
        let pins = control_pins(common, common, Path::new("/wt"));
        assert_eq!(
            pins,
            [
                common.join("hooks"),
                common.join("config"),
                common.join("info"),
                PathBuf::from("/wt/.git"),
            ]
        );
        // Main checkout: `<worktree>/.git` is the common dir, not re-bound.
        let pins = control_pins(common, common, Path::new("/repo"));
        assert!(!pins.iter().any(|p| p == common), "{pins:?}");
        assert!(!pins.iter().any(|p| p.ends_with("commondir")), "{pins:?}");
    }

    #[test]
    fn pin_target_requires_real_type_and_containment() {
        let common = Path::new("/repo/.git");
        let hooks = common.join("hooks");
        assert!(pin_target_ok(
            &hooks,
            common,
            PinTargetFacts {
                directory: true,
                ..PinTargetFacts::default()
            }
        ));
        assert!(!pin_target_ok(
            &hooks,
            common,
            PinTargetFacts {
                directory: true,
                symlink: true,
                ..PinTargetFacts::default()
            }
        ));
        assert!(!pin_target_ok(
            &common.join("worktrees/lane/config.worktree"),
            common,
            PinTargetFacts {
                regular_file: true,
                inside_common: false,
                ..PinTargetFacts::default()
            }
        ));
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
            assert_eq!(plan.ro, std::slice::from_ref(&common), "{mode}");
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
                    common.join("hooks"),
                    common.join("config"),
                    common.join("info"),
                    admin.join("config.worktree"),
                    admin.join("commondir"),
                    admin.join("gitdir"),
                    PathBuf::from("/wt/.git"),
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
    fn carve_out_fails_closed_on_unknown_target() {
        for mode in [
            SandboxGitMode::CommitOnly,
            SandboxGitMode::PushLane,
            SandboxGitMode::Full,
        ] {
            for target in [None, Some(""), Some("  ")] {
                let err = plan_for(&mode_cfg(mode), "tg/lane", target).unwrap_err();
                assert!(
                    err.contains("unknown merge target"),
                    "{mode} {target:?}: {err}"
                );
            }
        }
        // read-only makes nothing writable, so an unknown target is fine.
        assert!(plan_for(&mode_cfg(SandboxGitMode::ReadOnly), "tg/lane", None).is_ok());
    }

    #[test]
    fn carve_out_refuses_default_branch() {
        let (common, admin) = fixture();
        let open = SandboxGitConfig {
            deny_refs: Vec::new(),
            ..SandboxGitConfig::default()
        };
        let plan = |default| {
            plan_git_mounts(
                &open,
                &common,
                &admin,
                Path::new("/wt"),
                "tg/lane",
                Some("main"),
                default,
            )
        };
        assert!(plan(Some("trunk")).is_ok());
        let err = plan(Some("tg/release")).unwrap_err();
        assert!(
            err.contains("default branch refs/heads/tg/release"),
            "{err}"
        );
        assert!(plan(Some("refs/heads/tg/release")).is_err());
        // A qualified merge target is normalised the same way.
        assert!(
            plan_git_mounts(
                &open,
                &common,
                &admin,
                Path::new("/wt"),
                "tg/lane",
                Some("refs/heads/tg/b139-integration"),
                None,
            )
            .is_err()
        );
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
        assert!(warns[1].contains("needs sandbox.git.deploy_key"));
        let warns = validate(&SandboxGitConfig {
            mode: SandboxGitMode::Full,
            credential: GitCredential::GithubApp,
            push_refs: vec!["refs/heads/x*".into()],
            deny_refs: vec![String::new()],
            ..SandboxGitConfig::default()
        });
        assert_eq!(warns.len(), 3, "{warns:?}");
        assert!(warns[0].contains("github-app") && warns[0].contains("not implemented yet"));
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
    fn validate_deploy_key_table() {
        // (mode, credential, deploy_key, expected warning substrings)
        let cases: [(SandboxGitMode, GitCredential, &str, &[&str]); 6] = [
            (
                SandboxGitMode::Full,
                GitCredential::DeployKey,
                "",
                &["needs sandbox.git.deploy_key"],
            ),
            (
                SandboxGitMode::Full,
                GitCredential::DeployKey,
                "   ",
                &["needs sandbox.git.deploy_key"],
            ),
            (
                SandboxGitMode::Full,
                GitCredential::DeployKey,
                "~/.ssh/deploy",
                &[],
            ),
            (SandboxGitMode::Full, GitCredential::DeployKey, "/k", &[]),
            (
                SandboxGitMode::PushLane,
                GitCredential::DeployKey,
                "/k",
                &["only used in mode = \"full\""],
            ),
            (
                SandboxGitMode::Full,
                GitCredential::GithubApp,
                "/k",
                &["not implemented yet"],
            ),
        ];
        for (mode, credential, key, want) in cases {
            let warns = validate(&SandboxGitConfig {
                mode,
                credential,
                deploy_key: key.to_string(),
                ..SandboxGitConfig::default()
            });
            assert_eq!(
                warns.len(),
                want.len(),
                "{mode} {credential} {key:?}: {warns:?}"
            );
            for (warn, needle) in warns.iter().zip(want.iter()) {
                assert!(warn.contains(needle), "{warn}");
            }
            assert!(
                !warns.iter().any(
                    |w| credential == GitCredential::DeployKey && w.contains("not implemented")
                ),
                "{warns:?}"
            );
        }
    }

    #[test]
    fn deploy_key_file_checks() {
        let good = DeployKeyFacts {
            regular: true,
            symlink: false,
            owned_by_user: true,
            mode: Some(0o600),
        };
        assert_eq!(check_deploy_key(Some(good)), Ok(()));
        assert_eq!(
            check_deploy_key(Some(DeployKeyFacts {
                mode: Some(0o400),
                ..good
            })),
            Ok(())
        );
        // No mode bits (non-unix): ownership + kind decide.
        assert_eq!(
            check_deploy_key(Some(DeployKeyFacts { mode: None, ..good })),
            Ok(())
        );
        let cases = [
            (None, "does not exist"),
            (
                Some(DeployKeyFacts {
                    symlink: true,
                    regular: false,
                    ..good
                }),
                "symlink",
            ),
            (
                Some(DeployKeyFacts {
                    regular: false,
                    ..good
                }),
                "not a regular file",
            ),
            (
                Some(DeployKeyFacts {
                    owned_by_user: false,
                    ..good
                }),
                "not owned",
            ),
            (
                Some(DeployKeyFacts {
                    mode: Some(0o644),
                    ..good
                }),
                "group/world",
            ),
            (
                Some(DeployKeyFacts {
                    mode: Some(0o640),
                    ..good
                }),
                "group/world",
            ),
            (
                Some(DeployKeyFacts {
                    mode: Some(0o602),
                    ..good
                }),
                "group/world",
            ),
        ];
        for (facts, want) in cases {
            let err = check_deploy_key(facts).expect_err(want);
            assert!(err.contains(want), "{err}");
        }
    }

    #[test]
    fn deploy_key_ssh_command_quotes_the_path() {
        assert_eq!(
            deploy_key_ssh_command(DEPLOY_KEY_DEST),
            "ssh -i /run/thegn-deploy-key -o IdentitiesOnly=yes"
        );
        assert_eq!(
            deploy_key_ssh_command("/h/my key"),
            "ssh -i '/h/my key' -o IdentitiesOnly=yes"
        );
    }

    #[test]
    fn deploy_key_wanted_and_path_expansion() {
        let mut cfg = SandboxGitConfig {
            mode: SandboxGitMode::Full,
            credential: GitCredential::DeployKey,
            ..SandboxGitConfig::default()
        };
        assert!(!cfg.wants_deploy_key());
        assert_eq!(cfg.deploy_key_path(), None);
        cfg.deploy_key = "/abs/key".into();
        assert!(cfg.wants_deploy_key());
        assert_eq!(cfg.deploy_key_path(), Some(PathBuf::from("/abs/key")));
        cfg.deploy_key = "~/.ssh/deploy".into();
        assert_eq!(
            cfg.deploy_key_path(),
            Some(crate::util::home().join(".ssh/deploy"))
        );
        // Deploy key never admits the host token families.
        assert!(!cfg.admits_git_token());
        assert!(!cfg.admits_env("GH_TOKEN"));
        cfg.mode = SandboxGitMode::PushLane;
        assert!(!cfg.wants_deploy_key());
        cfg.mode = SandboxGitMode::Full;
        cfg.credential = GitCredential::RepoToken;
        assert!(!cfg.wants_deploy_key());
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
            deploy_key: Some("/k".into()),
        });
        assert_eq!(all.deploy_key, "/k");
        assert_eq!(merged.deploy_key, "");
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

        let minified = "sandbox={git={mode=\"push_lane\",transport=\"clone\",credential=\"deploy_key\",push_refs=[\"refs/heads/x/*\"],deny_refs=[],deploy_key=\"~/.ssh/d\"}}\n";
        let cfg: Config = toml::from_str(minified).expect("minified");
        assert_eq!(cfg.sandbox.git.mode, SandboxGitMode::PushLane);
        assert_eq!(cfg.sandbox.git.transport, GitTransport::Clone);
        assert_eq!(cfg.sandbox.git.credential, GitCredential::DeployKey);
        assert_eq!(cfg.sandbox.git.push_refs, ["refs/heads/x/*"]);
        assert!(cfg.sandbox.git.deny_refs.is_empty());
        assert_eq!(cfg.sandbox.git.deploy_key, "~/.ssh/d");

        let pretty_key = "[sandbox.git]\nmode = \"full\"\ncredential = \"deploy-key\"\ndeploy_key = \"/srv/keys/deploy\"\n";
        let keyed: Config = toml::from_str(pretty_key).expect("pretty deploy key");
        assert_eq!(keyed.sandbox.git.deploy_key, "/srv/keys/deploy");
        assert!(keyed.sandbox.git.wants_deploy_key());
        let overlay = "[env.ci.sandbox.git]\ndeploy_key = \"/k\"\n";
        let env_cfg: Config = toml::from_str(overlay).expect("overlay deploy key");
        assert_eq!(
            env_cfg.env["ci"]
                .sandbox
                .git
                .as_ref()
                .and_then(|g| g.deploy_key.as_deref()),
            Some("/k")
        );

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
