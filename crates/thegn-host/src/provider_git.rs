//! Credential-free git transport for provider sandboxes (THE-2021).
//!
//! **In:** provisioning seeds the sandbox repository from a host-side
//! `git bundle` of the worktree's branch, uploaded over the provider files API
//! ([`seed_from_bundle`], the plan's `bundle_seed` step). The sandbox needs no
//! ssh key or token to get the code, and no vendor CLI is involved.
//!
//! **Back:** `thegn env pull` ([`pull_back`]) bundles the commits made inside
//! the sandbox, downloads the bundle, fetches it into a scratch ref
//! (`refs/thegn/pull/<worktree>`) and fast-forwards the lane's own branch when
//! that is safe. Anything else (a diverged sandbox branch, a dirty local
//! checkout) stays in the scratch ref for the user to merge; pull-back never
//! writes any other branch and never forces.
//!
//! Script text lives in [`thegn_core::syncstate`] (pure, unit-tested). Every
//! function here blocks on provider calls and git, so it runs only on the CLI
//! or the off-loop provisioning path.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::Context;
use thegn_core::config::{Config, SandboxGitConfig, SandboxGitMode};
use thegn_core::config_sandbox::git_policy;
use thegn_core::syncstate;
use thegn_svc::provider::Provider;

use crate::agent::block_on_provider;

/// Largest repository bundle the seed step uploads.
pub(crate) const MAX_SEED_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// Largest sandbox bundle `env pull` downloads.
pub(crate) const MAX_PULL_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// Scratch-ref namespace pull-back fetches into (never a branch).
pub(crate) const PULL_REF_PREFIX: &str = "refs/thegn/pull/";

const MIB: u64 = 1024 * 1024;

/// The seed-cap refusal.
pub(crate) fn seed_cap_message(bytes: u64) -> String {
    format!(
        "the repository bundle is {} MiB, over the {} MiB seed cap; set `[sandbox.git] transport = \"clone\"` (and a credential) for this env",
        bytes.div_ceil(MIB),
        MAX_SEED_BUNDLE_BYTES / MIB
    )
}

/// The open-path refusal when the sandbox repository is not provisioned and
/// nothing can run a command in the sandbox (THE-2014).
pub(crate) fn no_exec_channel_message(cli: &str) -> String {
    format!(
        "the sandbox repository is not provisioned and no provider exec channel is available (the `{cli}` CLI is not on PATH); run `thegn env up` first"
    )
}

/// What one `thegn env pull` did to the local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PullOutcome {
    /// The sandbox has nothing the local branch lacks.
    UpToDate,
    /// The local branch (and its checkout) moved forward to the sandbox head.
    FastForwarded {
        from: String,
        to: String,
        commits: u64,
    },
    /// The sandbox head is not a descendant of the local tip; it is kept in
    /// `kept_ref` for the user to merge.
    Diverged { kept_ref: String, vm_head: String },
    /// A fast-forward was possible but the local checkout has uncommitted
    /// tracked changes; the sandbox head is kept in `kept_ref`.
    LocalDirty { kept_ref: String },
}

impl PullOutcome {
    fn label(&self) -> &'static str {
        match self {
            PullOutcome::UpToDate => "up_to_date",
            PullOutcome::FastForwarded { .. } => "fast_forwarded",
            PullOutcome::Diverged { .. } => "diverged",
            PullOutcome::LocalDirty { .. } => "local_dirty",
        }
    }
}

/// A [`PullOutcome`] with the branch it applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullReport {
    pub branch: String,
    pub outcome: PullOutcome,
    /// `push-lane` only: what happened to the push of the fast-forwarded
    /// branch to `origin` (`None` when no push was due).
    pub push: Option<String>,
}

fn short(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

impl PullReport {
    /// The one-line CLI summary.
    pub(crate) fn summary(&self) -> String {
        match &self.push {
            Some(push) => format!("{}; {push}", self.outcome_summary()),
            None => self.outcome_summary(),
        }
    }

    fn outcome_summary(&self) -> String {
        let branch = &self.branch;
        match &self.outcome {
            PullOutcome::UpToDate => "up to date".to_string(),
            PullOutcome::FastForwarded { from, to, commits } => format!(
                "fast-forwarded {branch} {}..{} ({commits} commit{})",
                short(from),
                short(to),
                if *commits == 1 { "" } else { "s" }
            ),
            PullOutcome::Diverged { kept_ref, vm_head } => format!(
                "kept {kept_ref}: the sandbox head {} is not a descendant of {branch}; merge it yourself",
                short(vm_head)
            ),
            PullOutcome::LocalDirty { kept_ref } => format!(
                "kept {kept_ref}: {branch} has uncommitted changes in the worktree; merge it yourself"
            ),
        }
    }
}

/// Decide what to do with a fetched sandbox head. Pure.
pub(crate) fn classify_pull(
    local_tip: &str,
    fetched_head: &str,
    is_ancestor: bool,
    clean: bool,
    kept_ref: &str,
    commits: u64,
) -> PullOutcome {
    if local_tip == fetched_head {
        PullOutcome::UpToDate
    } else if !is_ancestor {
        PullOutcome::Diverged {
            kept_ref: kept_ref.to_string(),
            vm_head: fetched_head.to_string(),
        }
    } else if !clean {
        PullOutcome::LocalDirty {
            kept_ref: kept_ref.to_string(),
        }
    } else {
        PullOutcome::FastForwarded {
            from: local_tip.to_string(),
            to: fetched_head.to_string(),
            commits,
        }
    }
}

/// Branches a sandbox pull-back never writes: `main`, the repository's
/// default branch and the configured merge target.
pub(crate) fn is_protected_branch(
    branch: &str,
    default_branch: Option<&str>,
    merge_target: &str,
) -> bool {
    let branch = short_branch(branch);
    let target = short_branch(merge_target);
    branch == "main"
        || default_branch.is_some_and(|d| {
            let d = short_branch(d);
            d != "HEAD" && d == branch
        })
        || (!target.is_empty() && target != "auto" && target == branch)
}

/// A branch name with surrounding whitespace and a leading `refs/heads/`
/// removed, so `refs/heads/x` and `x` compare equal.
fn short_branch(name: &str) -> &str {
    let name = name.trim();
    name.strip_prefix("refs/heads/").unwrap_or(name)
}

/// Gate a pull-back into `branch`. Fails closed when the default branch is
/// unknown: a read failure, or the `HEAD` fallback
/// [`thegn_core::worktree::default_branch_checked`] returns when neither
/// `origin/HEAD` nor `main`/`master` exists. Pure.
pub(crate) fn check_pull_target(
    branch: &str,
    default_branch: Result<&str, &str>,
    merge_target: &str,
) -> Result<(), String> {
    let default_branch = match default_branch {
        Ok(d) if short_branch(d) != "HEAD" && !short_branch(d).is_empty() => d,
        Ok(_) => {
            return Err(format!(
                "refusing to pull sandbox commits into {branch}: the repository's default branch is unknown; set origin/HEAD (`git remote set-head origin --auto`) or `merge_queue.target_branch`"
            ));
        }
        Err(error) => {
            return Err(format!(
                "refusing to pull sandbox commits into {branch}: the repository's default branch could not be read ({error}); set origin/HEAD (`git remote set-head origin --auto`) or `merge_queue.target_branch`"
            ));
        }
    };
    if is_protected_branch(branch, Some(default_branch), merge_target) {
        return Err(format!(
            "refusing to pull sandbox commits into {branch}: it is the repository's default branch or merge target, which a sandbox never writes; check out the lane branch first"
        ));
    }
    Ok(())
}

/// The merge target as the `[sandbox.git]` policy checks see it: `auto` (or
/// blank) is the default branch, `None` when that is unknown too.
fn policy_target<'a>(merge_target: &'a str, default_branch: Option<&'a str>) -> Option<&'a str> {
    let target = merge_target.trim();
    if target.is_empty() || target == "auto" {
        default_branch
    } else {
        Some(target)
    }
}

/// The `[sandbox.git]` gate on a pull-back into `branch` (THE-2021): a
/// `read-only` sandbox has nothing to pull, and the ref must pass
/// [`git_policy::check_pull_ref`] (merge target, default branch, `deny_refs`).
/// `default_branch` is `None` when unknown. Pure.
pub(crate) fn check_pull_policy(
    git: &SandboxGitConfig,
    branch: &str,
    default_branch: Option<&str>,
    merge_target: &str,
) -> Result<(), String> {
    if git.mode == SandboxGitMode::ReadOnly {
        return Err("mode = read-only: the sandbox cannot produce commits to pull".to_string());
    }
    git_policy::check_pull_ref(
        git,
        &format!("refs/heads/{}", short_branch(branch)),
        policy_target(merge_target, default_branch),
        default_branch,
    )
}

/// Should a pull-back that fast-forwarded `branch` push it to `origin`?
/// `None` outside `push-lane`; otherwise [`git_policy::check_push_ref`]'s
/// verdict (`push_refs` match, never the merge target / default branch /
/// `deny_refs`). Pure.
pub(crate) fn push_decision(
    git: &SandboxGitConfig,
    branch: &str,
    default_branch: Option<&str>,
    merge_target: &str,
) -> Option<Result<(), String>> {
    (git.mode == SandboxGitMode::PushLane).then(|| {
        git_policy::check_push_ref(
            git,
            &format!("refs/heads/{}", short_branch(branch)),
            policy_target(merge_target, default_branch),
            default_branch,
        )
    })
}

/// The worktree env's effective `[sandbox.git]`: the global table with the
/// `[env.<name>.sandbox.git]` overlay applied.
fn env_git_policy(cfg: &Config, env_name: &str) -> SandboxGitConfig {
    match cfg
        .env
        .get(env_name)
        .and_then(|env| env.sandbox.git.as_ref())
    {
        Some(overlay) => cfg.sandbox.git.merged(overlay),
        None => cfg.sandbox.git.clone(),
    }
}

/// `git push origin refs/heads/<branch>:refs/heads/<branch>` from the host
/// (`push-lane`, after a fast-forward); the one-line result for the report.
fn push_lane_branch(worktree: &Path, branch: &str) -> String {
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    match git_write(
        worktree,
        ["push", "--porcelain", "origin", refspec.as_str()],
        &format!("git push origin {branch}"),
    ) {
        Ok(_) => format!("pushed {branch} to origin"),
        Err(error) => format!("push of {branch} to origin failed: {error:#}"),
    }
}

/// The scratch ref a worktree's pull-back fetches into: the worktree's
/// directory name reduced to `[A-Za-z0-9_-]` plus a short hash of the full
/// worktree path (so it is always a valid, single-component ref name, and
/// two worktrees sharing a directory name never share a scratch ref).
pub(crate) fn pull_ref_for(worktree: &Path) -> String {
    use sha2::{Digest, Sha256};
    let base = worktree
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mapped: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let key = mapped.trim_matches('-');
    let key = if key.is_empty() { "worktree" } else { key };
    let digest = Sha256::digest(worktree.as_os_str().as_encoded_bytes());
    let hash: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("{PULL_REF_PREFIX}{key}-{hash}")
}

/// Run a mutating git command in `dir`, failing with its stderr tail.
#[expect(
    clippy::disallowed_methods,
    reason = "off-loop CLI/provisioning path: git writes are not bounded reads"
)]
fn git_write<I, S>(dir: &Path, args: I, what: &str) -> anyhow::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = thegn_core::util::git_cmd(dir)
        .args(args)
        .output()
        .with_context(|| format!("{what}: could not run git"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "{what} failed ({}): {}",
            out.status,
            crate::provision_recover::stderr_gist(&stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git <args>` read on the bounded policy, with a readable error.
fn git_read(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    thegn_core::util::git_out_result(dir, args)
        .map_err(|e| anyhow::anyhow!("git {} in {}: {e}", args.join(" "), dir.display()))
}

/// The commit `refs/heads/<branch>` points at in `worktree`.
fn branch_tip(worktree: &Path, branch: &str) -> anyhow::Result<String> {
    let spec = format!("refs/heads/{branch}^{{commit}}");
    let tip = git_read(worktree, &["rev-parse", "--verify", "-q", &spec])?;
    anyhow::ensure!(!tip.is_empty(), "branch {branch} has no commit");
    Ok(tip)
}

fn check_branch_name(worktree: &Path, branch: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !branch.is_empty() && !branch.starts_with('-'),
        "refusing to bundle branch {branch:?}"
    );
    git_read(worktree, &["check-ref-format", "--branch", branch])
        .with_context(|| format!("{branch:?} is not a valid branch name"))?;
    Ok(())
}

/// Bundle `refs/heads/<branch>` of `worktree` into `out`.
fn bundle_branch(worktree: &Path, branch: &str, out: &Path) -> anyhow::Result<()> {
    let refname = format!("refs/heads/{branch}");
    git_write(
        worktree,
        [
            OsStr::new("bundle"),
            OsStr::new("create"),
            out.as_os_str(),
            OsStr::new(&refname),
        ],
        "git bundle create",
    )
    .map(|_| ())
}

/// Run one `/bin/sh` script in the sandbox (login PATH, stderr folded).
fn sandbox_sh(
    provider: &Provider,
    id: &str,
    script: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<(i32, String)> {
    let argv = crate::provision_recover::exec_login_argv(script);
    let to = crate::agent::provision_step_timeout("bundle_seed");
    block_on_provider(|| async {
        match tokio::time::timeout(to, provider.run_exec(id, &argv, None, exec_env)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("exec timed out after {}s", to.as_secs())),
        }
    })
}

/// Create a private staging directory in the sandbox; returns its path.
fn stage_dir(
    provider: &Provider,
    id: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<String> {
    let dir = crate::parity::random_staging_dir()?;
    let (code, out) = sandbox_sh(provider, id, &syncstate::stage_dir_script(&dir), exec_env)
        .with_context(|| format!("create staging directory {dir} in the sandbox"))?;
    anyhow::ensure!(
        code == 0,
        "staging directory {dir} could not be created privately (exit {code}): {}",
        crate::provision_recover::stderr_gist(&out)
    );
    Ok(dir)
}

fn remove_stage_dir(provider: &Provider, id: &str, dir: &str, exec_env: &[(String, String)]) {
    let script = format!("rm -rf -- {}", thegn_core::util::sh_quote(dir));
    match sandbox_sh(provider, id, &script, exec_env) {
        Ok((0, _)) => {}
        Ok((code, out)) => tracing::warn!(
            target: "thegn::git_audit",
            dir,
            code,
            output = %crate::provision_recover::stderr_gist(&out),
            "could not remove the sandbox staging directory"
        ),
        Err(error) => tracing::warn!(
            target: "thegn::git_audit",
            dir,
            error = %format!("{error:#}"),
            "could not remove the sandbox staging directory"
        ),
    }
}

/// Seed the sandbox repository at `workdir` from a bundle of the local
/// `branch` in `worktree` (the plan's `bundle_seed` step). NOT best-effort:
/// any failure fails the step, since without it the sandbox has no repository.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
pub(crate) fn seed_from_bundle(
    provider: &Provider,
    id: &str,
    worktree: &str,
    workdir: &str,
    branch: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    let wt = Path::new(worktree);
    check_branch_name(wt, branch)?;
    let tip = branch_tip(wt, branch)?;
    let tmp = tempfile::Builder::new().prefix("tg-seed-").tempdir()?;
    let bundle = tmp.path().join("seed.bundle");
    bundle_branch(wt, branch, &bundle)?;
    let size = std::fs::metadata(&bundle)
        .with_context(|| format!("stat {}", bundle.display()))?
        .len();
    if size > MAX_SEED_BUNDLE_BYTES {
        anyhow::bail!(seed_cap_message(size));
    }
    let bytes = std::fs::read(&bundle).with_context(|| format!("read {}", bundle.display()))?;

    let dir = stage_dir(provider, id, exec_env)?;
    let staged = syncstate::staged_artifact_path(&dir, "seed.bundle");
    block_on_provider(|| async { provider.write(id, &staged, &bytes).await })
        .with_context(|| format!("upload the repository bundle to {staged}"))?;
    let script = syncstate::bundle_seed_script(workdir, &staged, branch, &tip);
    let (code, out) = sandbox_sh(provider, id, &script, exec_env)
        .context("seed the sandbox repository from the bundle")?;
    if code != 0 {
        let marker = out
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("TG-SEED-ERROR:"))
            .unwrap_or("TG-SEED-ERROR:exec");
        anyhow::bail!(
            "seeding {workdir} from the bundle failed ({marker}, exit {code}): {}; staged bundle retained in {dir}",
            crate::provision_recover::stderr_gist(&out)
        );
    }
    tracing::info!(
        target: "thegn::git_audit",
        worktree,
        branch,
        to = %tip,
        bytes = size,
        outcome = "seeded",
        "sandbox repository seeded from a git bundle"
    );
    Ok(())
}

/// Fetch a downloaded sandbox bundle into the worktree's scratch ref and
/// fast-forward `branch` when that is safe. Host-only (no provider).
fn apply_pulled_bundle(
    worktree: &Path,
    branch: &str,
    local_tip: &str,
    bundle: &Path,
    vm_head: &str,
) -> anyhow::Result<PullOutcome> {
    git_write(
        worktree,
        [
            OsStr::new("bundle"),
            OsStr::new("verify"),
            bundle.as_os_str(),
        ],
        "git bundle verify",
    )?;
    let kept_ref = pull_ref_for(worktree);
    let refspec = format!("+HEAD:{kept_ref}");
    git_write(
        worktree,
        [
            OsStr::new("-c"),
            OsStr::new("transfer.fsckObjects=true"),
            OsStr::new("-c"),
            OsStr::new("fetch.fsckObjects=true"),
            OsStr::new("fetch"),
            OsStr::new("--no-tags"),
            OsStr::new("-q"),
            bundle.as_os_str(),
            OsStr::new(&refspec),
        ],
        "git fetch from the sandbox bundle",
    )?;
    let fetched = git_read(worktree, &["rev-parse", "--verify", "-q", &kept_ref])?;
    anyhow::ensure!(
        fetched.eq_ignore_ascii_case(vm_head),
        "the sandbox bundle's head {fetched} does not match the reported sandbox HEAD {vm_head}; kept {kept_ref}"
    );
    let is_ancestor =
        thegn_core::util::git_is_ancestor(worktree, local_tip, &fetched).ok_or_else(|| {
            anyhow::anyhow!("could not compare {local_tip} with the sandbox head; kept {kept_ref}")
        })?;
    let status = git_read(worktree, &["status", "--porcelain", "--untracked-files=no"])?;
    let commits = if is_ancestor {
        let range = format!("{local_tip}..{fetched}");
        git_read(worktree, &["rev-list", "--count", &range])?
            .parse::<u64>()
            .unwrap_or(0)
    } else {
        0
    };
    let outcome = classify_pull(
        local_tip,
        &fetched,
        is_ancestor,
        status.is_empty(),
        &kept_ref,
        commits,
    );
    match &outcome {
        PullOutcome::FastForwarded { to, .. } => {
            // The download took a while: re-read the checkout right before
            // writing so a branch switch (or a commit) in the meantime
            // aborts instead of fast-forwarding the wrong branch.
            let current =
                git_read(worktree, &["symbolic-ref", "--short", "-q", "HEAD"]).unwrap_or_default();
            anyhow::ensure!(
                current == branch,
                "{} switched from {branch} to {} during the pull; the sandbox head is kept in {kept_ref}",
                worktree.display(),
                if current.is_empty() {
                    "a detached HEAD"
                } else {
                    current.as_str()
                }
            );
            let tip_now = branch_tip(worktree, branch)?;
            anyhow::ensure!(
                tip_now.eq_ignore_ascii_case(local_tip),
                "{branch} moved from {local_tip} to {tip_now} during the pull; the sandbox head is kept in {kept_ref}"
            );
            git_write(
                worktree,
                ["merge", "--ff-only", "-q", to.as_str()],
                &format!("fast-forward {branch}"),
            )
            .with_context(|| format!("the sandbox head is kept in {kept_ref}"))?;
            git_write(
                worktree,
                ["update-ref", "-d", kept_ref.as_str()],
                "drop the pull ref",
            )?;
        }
        PullOutcome::UpToDate => {
            git_write(
                worktree,
                ["update-ref", "-d", kept_ref.as_str()],
                "drop the pull ref",
            )?;
        }
        PullOutcome::Diverged { .. } | PullOutcome::LocalDirty { .. } => {}
    }
    Ok(outcome)
}

/// Resolve `(provider, sandbox id, workdir)` for a provider worktree.
fn provider_target(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
) -> anyhow::Result<(Provider, String, String)> {
    let pc = &cfg
        .env
        .get(env_name)
        .ok_or_else(|| anyhow::anyhow!("env {env_name} is not configured"))?
        .provider;
    let id = crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, env_name)?
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("env {env_name} has no sandbox for {worktree}"))?;
    let owner = crate::provider_factory::provider_sandbox_owner(cfg, worktree, env_name);
    let provider = crate::provider_factory::provider_for_named_owned(pc, &id, owner.as_deref())
        .ok_or_else(|| anyhow::anyhow!("env {env_name}: the provider API token is not set"))?;
    anyhow::ensure!(
        provider.caps().files,
        "env {env_name}: the provider has no files API to download a bundle with"
    );
    let workdir = crate::provider_workdir::resolve(pc, &id);
    Ok((provider, id, workdir))
}

/// The explicit merge target for the repo whose main checkout is `repo_root`,
/// resolved as [`pull_back`] does (per-repo workspace overlay over the global
/// `merge_queue.target_branch`). `refs/heads/` is stripped; `auto`/empty is
/// `None` so the sandbox mount plan falls back to the default-branch rule.
fn explicit_merge_target(cfg: &Config, repo_root: &Path) -> Option<String> {
    let target = cfg.repo_merge_queue(repo_root).target_branch;
    let target = target.trim();
    let target = target.strip_prefix("refs/heads/").unwrap_or(target);
    (!target.is_empty() && target != "auto").then(|| target.to_string())
}

/// The `[sandbox.git]` merge-target lookup for this process. Reads the
/// currently published admitted config (hot reloads included) and falls back
/// to `boot` when the process has no admission store. Non-blocking: a short
/// store lock plus a lexical overlay match, no file or DB I/O.
pub(crate) fn merge_target_lookup(boot: &Config) -> thegn_core::sandbox::MergeTargetLookup {
    let snapshot = boot.clone();
    Box::new(move |repo_root: &Path| {
        let current = crate::config_startup::display();
        let cfg = current
            .as_deref()
            .map_or(&snapshot, |admitted| admitted.config());
        explicit_merge_target(cfg, repo_root)
    })
}

/// Register [`merge_target_lookup`] with core once, at process start, so every
/// entry point that resolves sandboxes protects an explicit merge target.
pub(crate) fn install_merge_target_lookup(boot: &Config) {
    thegn_core::sandbox::set_merge_target_lookup(merge_target_lookup(boot));
}

/// Fetch the commits made inside the worktree's provider sandbox back into its
/// branch (fast-forward only). The caller has already established that the
/// worktree's env is a provider env named `env_name`.
// off-loop: CLI path (`thegn env pull`).
pub(crate) fn pull_back(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
) -> anyhow::Result<PullReport> {
    let wt = Path::new(worktree);
    let branch = git_read(wt, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{worktree} is not on a branch; nothing to pull into"))?;
    let default_branch = thegn_core::worktree::default_branch_checked(wt);
    // The per-repo merge target (workspace overlay) as `thegn land` resolves
    // it; the global value when the main checkout cannot be found.
    let merge_target = crate::integrate::main_checkout(wt)
        .map(|root| cfg.repo_merge_queue(&root).target_branch)
        .unwrap_or_else(|| cfg.merge_queue.target_branch.clone());
    check_pull_target(
        &branch,
        default_branch.as_deref().map_err(String::as_str),
        &merge_target,
    )
    .map_err(anyhow::Error::msg)?;
    // check_pull_target has failed closed on an unknown default branch.
    let known_default = default_branch.as_deref().ok();
    let git = env_git_policy(cfg, env_name);
    check_pull_policy(&git, &branch, known_default, &merge_target).map_err(anyhow::Error::msg)?;
    let local_tip = branch_tip(wt, &branch)?;
    let (provider, id, workdir) = provider_target(cfg, worktree, env_name)?;

    let result = pull_from_sandbox(&provider, &id, &workdir, wt, &branch, &local_tip);
    let (outcome_label, to) = match &result {
        Ok(outcome) => (
            outcome.label(),
            match outcome {
                PullOutcome::FastForwarded { to, .. } => to.clone(),
                PullOutcome::Diverged { vm_head, .. } => vm_head.clone(),
                PullOutcome::UpToDate | PullOutcome::LocalDirty { .. } => String::new(),
            },
        ),
        Err(_) => ("error", String::new()),
    };
    tracing::info!(
        target: "thegn::git_audit",
        worktree,
        branch = %branch,
        from = %local_tip,
        to = %to,
        outcome = outcome_label,
        "env pull"
    );
    let outcome = result?;
    let push = match (
        &outcome,
        push_decision(&git, &branch, known_default, &merge_target),
    ) {
        (PullOutcome::FastForwarded { .. }, Some(Ok(()))) => {
            let push = push_lane_branch(wt, &branch);
            tracing::info!(
                target: "thegn::git_audit",
                worktree,
                branch = %branch,
                result = %push,
                "env pull push-lane"
            );
            Some(push)
        }
        (PullOutcome::FastForwarded { .. }, Some(Err(refusal))) => {
            Some(format!("not pushed: {refusal}"))
        }
        _ => None,
    };
    Ok(PullReport {
        branch,
        outcome,
        push,
    })
}

/// Whether a stored `location` value is a managed-provider location. Ssh
/// locations are not: their worktree path lives on the remote, so there is no
/// local checkout to fall back to.
pub(crate) fn is_provider_location(wt: &str, location: Option<&str>) -> bool {
    matches!(
        thegn_core::remote::GitLoc::from_db(wt, location),
        thegn_core::remote::GitLoc::Provider { .. }
    )
}

/// Should a session exit trigger the automatic pull-back (THE-2021)? Only for
/// a provider-located worktree, only when it was the worktree's last live
/// session, and never while a pull for it is already running. Pure.
pub(crate) fn should_pull_on_exit(
    location_is_provider: bool,
    other_live_sessions: usize,
    in_flight: bool,
) -> bool {
    location_is_provider && other_live_sessions == 0 && !in_flight
}

/// Worktrees with an automatic pull-back running in this process.
fn pulls_in_flight() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static IN_FLIGHT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// The single-flight claim on one worktree's automatic pull-back; released
/// when dropped.
#[derive(Debug)]
pub(crate) struct PullFlight {
    worktree: String,
}

impl Drop for PullFlight {
    fn drop(&mut self) {
        pulls_in_flight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.worktree);
    }
}

/// Claim `worktree`'s pull-back; `None` when one is already running.
pub(crate) fn try_begin_pull(worktree: &str) -> Option<PullFlight> {
    pulls_in_flight()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(worktree.to_string())
        .then(|| PullFlight {
            worktree: worktree.to_string(),
        })
}

/// [`pull_back`] for a worktree whose recorded location is a provider
/// location and whose env resolves to a provider placement; `Ok(None)` for
/// anything else (nothing to pull). Blocks on the DB, git and the provider:
/// the daemon runs it on the blocking pool after the last session exits.
// off-loop: daemon `spawn_blocking` on session exit.
pub(crate) fn pull_back_if_provider(
    cfg: &Config,
    worktree: &str,
) -> anyhow::Result<Option<PullReport>> {
    use thegn_core::store::WorkspaceStore;
    let stored = thegn_core::db::Db::open()
        .and_then(|db| db.location_for(worktree))
        .with_context(|| format!("read the stored location of {worktree}"))?;
    if !is_provider_location(worktree, stored.as_deref()) {
        return Ok(None);
    }
    let env = crate::cmd::env::resolve_for(cfg, Some(worktree.to_string()))?;
    if !env.placement.is_provider() {
        return Ok(None);
    }
    // `read-only`: the owner chose not to pull from this sandbox. Nothing to
    // pull and nothing to report on every session exit.
    if env_git_policy(cfg, &env.name).mode == SandboxGitMode::ReadOnly {
        return Ok(None);
    }
    pull_back(cfg, worktree, &env.name).map(Some)
}

fn pull_from_sandbox(
    provider: &Provider,
    id: &str,
    workdir: &str,
    wt: &Path,
    branch: &str,
    local_tip: &str,
) -> anyhow::Result<PullOutcome> {
    let dir = stage_dir(provider, id, &[])?;
    let result = download_and_apply(provider, id, workdir, &dir, wt, branch, local_tip);
    remove_stage_dir(provider, id, &dir, &[]);
    result
}

fn download_and_apply(
    provider: &Provider,
    id: &str,
    workdir: &str,
    dir: &str,
    wt: &Path,
    branch: &str,
    local_tip: &str,
) -> anyhow::Result<PullOutcome> {
    let out_path = syncstate::staged_artifact_path(dir, "pull.bundle");
    let script = syncstate::pull_back_script(workdir, local_tip, &out_path);
    let (code, out) =
        sandbox_sh(provider, id, &script, &[]).context("bundle the sandbox commits")?;
    let markers = syncstate::parse_pull_markers(&out).map_err(|reason| {
        anyhow::anyhow!(
            "{reason} (exit {code}): {}",
            crate::provision_recover::stderr_gist(&out)
        )
    })?;
    anyhow::ensure!(
        code == 0,
        "the sandbox pull-back script exited {code}: {}",
        crate::provision_recover::stderr_gist(&out)
    );
    let Some(size) = markers.bundle_bytes else {
        return Ok(PullOutcome::UpToDate);
    };
    anyhow::ensure!(
        size <= MAX_PULL_BUNDLE_BYTES,
        "the sandbox bundle is {} MiB, over the {} MiB pull cap",
        size.div_ceil(MIB),
        MAX_PULL_BUNDLE_BYTES / MIB
    );
    let bytes = block_on_provider(|| async { provider.read(id, &out_path).await })
        .with_context(|| format!("download {out_path} from the sandbox"))?;
    anyhow::ensure!(
        bytes.len() as u64 == size,
        "the downloaded bundle is {} bytes, the sandbox reported {size}",
        bytes.len()
    );
    let tmp = tempfile::Builder::new().prefix("tg-pull-").tempdir()?;
    let local_bundle = tmp.path().join("pull.bundle");
    std::fs::write(&local_bundle, &bytes)
        .with_context(|| format!("write {}", local_bundle.display()))?;
    apply_pulled_bundle(wt, branch, local_tip, &local_bundle, &markers.head)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SEED_BUNDLE_BYTES, PULL_REF_PREFIX, PullOutcome, PullReport, apply_pulled_bundle,
        branch_tip, bundle_branch, check_pull_policy, check_pull_target, classify_pull,
        is_protected_branch, no_exec_channel_message, pull_ref_for, push_decision,
        seed_cap_message, should_pull_on_exit, try_begin_pull,
    };
    use std::path::Path;
    use thegn_core::config::{SandboxGitConfig, SandboxGitMode};

    const A: &str = "1111111111111111111111111111111111111111";

    #[test]
    fn merge_target_lookup_returns_the_per_repo_target() {
        use thegn_core::config::{
            Config, MergeQueueConfig, MergeQueueOverlay, WorkspaceConfig, workspace_slug,
        };
        let dir = std::env::temp_dir().join(format!("thegn-mtl-{}", std::process::id()));
        let repo = dir.join("Repo");
        let other = dir.join("Other");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut cfg = Config {
            merge_queue: MergeQueueConfig {
                target_branch: "auto".into(),
                ..MergeQueueConfig::default()
            },
            ..Config::default()
        };
        cfg.workspace.insert(
            workspace_slug(&repo),
            WorkspaceConfig {
                merge_queue: MergeQueueOverlay {
                    target_branch: Some("refs/heads/tg/b139-integration".into()),
                    ..MergeQueueOverlay::default()
                },
                ..WorkspaceConfig::default()
            },
        );
        let lookup = super::merge_target_lookup(&cfg);
        assert_eq!(lookup(&repo).as_deref(), Some("tg/b139-integration"));
        // `auto` defers to the default-branch rule.
        assert_eq!(lookup(&other), None);
        let _ = std::fs::remove_dir_all(&dir); // best-effort: temp cleanup
    }

    #[test]
    fn protected_branches_are_never_pull_targets() {
        assert!(is_protected_branch("main", None, "auto"));
        assert!(is_protected_branch("develop", Some("develop"), "auto"));
        assert!(is_protected_branch(
            "tg/b1-integration",
            Some("main"),
            "tg/b1-integration"
        ));
        assert!(!is_protected_branch("tg/lane", Some("main"), "auto"));
        assert!(!is_protected_branch("tg/lane", Some("HEAD"), ""));
    }

    #[test]
    fn merge_target_comparison_ignores_refs_heads_prefix() {
        assert!(is_protected_branch("x", Some("main"), "refs/heads/x"));
        assert!(is_protected_branch("refs/heads/x", Some("main"), "x"));
        assert!(is_protected_branch("dev", Some("refs/heads/dev"), "auto"));
        assert!(!is_protected_branch("y", Some("main"), "refs/heads/x"));
    }

    #[test]
    fn pull_target_check_fails_closed_on_unknown_default_branch() {
        let unknown = check_pull_target("tg/lane", Ok("HEAD"), "auto").unwrap_err();
        assert!(unknown.contains("default branch is unknown"), "{unknown}");
        assert!(unknown.contains("origin/HEAD"), "{unknown}");
        assert!(unknown.contains("merge_queue.target_branch"), "{unknown}");
        let failed = check_pull_target("tg/lane", Err("boom"), "auto").unwrap_err();
        assert!(failed.contains("boom"), "{failed}");
        assert!(failed.contains("merge_queue.target_branch"), "{failed}");
        assert!(check_pull_target("main", Ok("main"), "auto").is_err());
        assert!(check_pull_target("x", Ok("main"), "refs/heads/x").is_err());
        assert_eq!(check_pull_target("tg/lane", Ok("main"), "auto"), Ok(()));
    }
    const B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn pull_policy_denies_main_and_read_only_mode() {
        let git = SandboxGitConfig::default();
        let main = check_pull_policy(&git, "main", Some("main"), "auto").unwrap_err();
        assert!(main.contains("refs/heads/main"), "{main}");
        // An explicit merge target is denied even when it is not the default.
        assert!(check_pull_policy(&git, "release", Some("main"), "release").is_err());
        assert!(check_pull_policy(&git, "refs/heads/main", Some("main"), "auto").is_err());
        assert_eq!(
            check_pull_policy(&git, "tg/lane", Some("main"), "auto"),
            Ok(())
        );
        let read_only = SandboxGitConfig {
            mode: SandboxGitMode::ReadOnly,
            ..SandboxGitConfig::default()
        };
        assert_eq!(
            check_pull_policy(&read_only, "tg/lane", Some("main"), "auto"),
            Err("mode = read-only: the sandbox cannot produce commits to pull".to_string())
        );
    }

    #[test]
    fn push_lane_pushes_only_tg_lanes() {
        let commit_only = SandboxGitConfig::default();
        assert_eq!(
            push_decision(&commit_only, "tg/lane", Some("main"), "auto"),
            None
        );
        let push_lane = SandboxGitConfig {
            mode: SandboxGitMode::PushLane,
            push_refs: vec!["refs/heads/tg/*".into()],
            ..SandboxGitConfig::default()
        };
        assert_eq!(
            push_decision(&push_lane, "tg/lane", Some("main"), "auto"),
            Some(Ok(()))
        );
        assert!(matches!(
            push_decision(&push_lane, "feat/x", Some("main"), "auto"),
            Some(Err(_))
        ));
        assert!(matches!(
            push_decision(&push_lane, "main", Some("main"), "auto"),
            Some(Err(_))
        ));
    }

    #[test]
    fn classifier_covers_every_outcome() {
        let r = "refs/thegn/pull/wt";
        assert_eq!(
            classify_pull(A, A, true, false, r, 0),
            PullOutcome::UpToDate
        );
        assert_eq!(
            classify_pull(A, B, true, true, r, 3),
            PullOutcome::FastForwarded {
                from: A.into(),
                to: B.into(),
                commits: 3
            }
        );
        assert_eq!(
            classify_pull(A, B, true, false, r, 3),
            PullOutcome::LocalDirty { kept_ref: r.into() }
        );
        for clean in [true, false] {
            assert_eq!(
                classify_pull(A, B, false, clean, r, 0),
                PullOutcome::Diverged {
                    kept_ref: r.into(),
                    vm_head: B.into()
                }
            );
        }
    }

    #[test]
    fn report_summaries_read_as_documented() {
        let report = |outcome| PullReport {
            branch: "feat".into(),
            outcome,
            push: None,
        };
        assert_eq!(report(PullOutcome::UpToDate).summary(), "up to date");
        assert_eq!(
            report(PullOutcome::FastForwarded {
                from: A.into(),
                to: B.into(),
                commits: 1
            })
            .summary(),
            "fast-forwarded feat 111111111111..222222222222 (1 commit)"
        );
        let kept = report(PullOutcome::LocalDirty {
            kept_ref: "refs/thegn/pull/x".into(),
        })
        .summary();
        assert!(kept.starts_with("kept refs/thegn/pull/x: "), "{kept}");
        assert!(kept.ends_with("; merge it yourself"), "{kept}");
        let diverged = report(PullOutcome::Diverged {
            kept_ref: "refs/thegn/pull/x".into(),
            vm_head: B.into(),
        })
        .summary();
        assert!(diverged.contains("222222222222"), "{diverged}");
        assert!(diverged.ends_with("; merge it yourself"), "{diverged}");
    }

    #[test]
    fn seed_cap_message_names_size_cap_and_opt_out() {
        let msg = seed_cap_message(MAX_SEED_BUNDLE_BYTES + 1);
        assert_eq!(
            msg,
            "the repository bundle is 513 MiB, over the 512 MiB seed cap; set `[sandbox.git] transport = \"clone\"` (and a credential) for this env"
        );
    }

    #[test]
    fn session_exit_pulls_only_after_the_last_provider_session() {
        for (provider, others, in_flight, expected) in [
            (true, 0, false, true),
            (true, 1, false, false),
            (true, 0, true, false),
            (false, 0, false, false),
            (false, 2, true, false),
        ] {
            assert_eq!(
                should_pull_on_exit(provider, others, in_flight),
                expected,
                "provider={provider} others={others} in_flight={in_flight}"
            );
        }
    }

    #[test]
    fn pull_flight_is_single_flight_per_worktree() {
        let wt = "/wt/single-flight-test-a";
        let first = try_begin_pull(wt).expect("first claim");
        assert!(try_begin_pull(wt).is_none(), "a second claim must wait");
        let other = try_begin_pull("/wt/single-flight-test-b").expect("another worktree");
        drop(first);
        let again = try_begin_pull(wt).expect("released on drop");
        drop(again);
        drop(other);
    }

    #[test]
    fn no_exec_channel_message_is_the_open_refusal() {
        assert_eq!(
            no_exec_channel_message("sprite"),
            "the sandbox repository is not provisioned and no provider exec channel is available (the `sprite` CLI is not on PATH); run `thegn env up` first"
        );
    }

    #[test]
    fn pull_refs_are_single_safe_components() {
        let key = |p: &str| {
            pull_ref_for(Path::new(p))
                .strip_prefix(PULL_REF_PREFIX)
                .expect("prefixed")
                .to_string()
        };
        for (path, base) in [
            ("/w/tg-b1.x lock", "tg-b1-x-lock"),
            ("/w/..", "worktree"),
            ("/", "worktree"),
        ] {
            let k = key(path);
            let (name, hash) = k.rsplit_once('-').expect("hash suffix");
            assert_eq!(name, base, "{path}");
            assert_eq!(hash.len(), 8, "{path}");
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{path}");
            assert!(
                k.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{k}"
            );
        }
        // Same directory name, different parents: distinct scratch refs.
        assert_ne!(key("/a/lane"), key("/b/lane"));
        assert_eq!(key("/a/lane"), key("/a/lane"));
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture runs git and the generated sh scripts directly"
    )]
    fn run(cmd: &mut std::process::Command) -> String {
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_CEILING_DIRECTORIES",
        ] {
            cmd.env_remove(var);
        }
        let out = cmd
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{cmd:?} failed: {text}");
        text
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let mut cmd = thegn_core::util::git_cmd(dir);
        cmd.args(["-c", "commit.gpgsign=false"]).args(args);
        run(&mut cmd).trim().to_string()
    }

    fn sh(script: &str) -> String {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        run(&mut cmd)
    }

    #[test]
    fn bundle_seed_then_pull_back_fast_forwards_the_branch() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("host wt");
        std::fs::create_dir_all(&host).unwrap();
        git(&host, &["init", "-q", "-b", "lane"]);
        std::fs::create_dir_all(host.join(".git/info")).unwrap();
        std::fs::write(host.join(".git/info/exclude"), "").unwrap();
        git(&host, &["config", "user.name", "Seed Test"]);
        git(&host, &["config", "user.email", "seed@example.invalid"]);
        std::fs::write(host.join("a.txt"), "one\n").unwrap();
        git(&host, &["add", "a.txt"]);
        git(&host, &["commit", "-q", "-m", "first"]);
        let tip = branch_tip(&host, "lane").unwrap();

        // Seed a "sandbox" dir from the bundle with the generated script.
        let stage = root.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let staged = stage.join("seed.bundle");
        bundle_branch(&host, "lane", &staged).unwrap();
        let vm = root.path().join("vm's repo");
        let seed = thegn_core::syncstate::bundle_seed_script(
            vm.to_str().unwrap(),
            staged.to_str().unwrap(),
            "lane",
            &tip,
        );
        let out = sh(&seed);
        assert!(out.contains("TG-SEED-OK seeded"), "{out}");
        assert!(!staged.exists(), "the staged bundle is removed");
        assert_eq!(git(&vm, &["rev-parse", "HEAD"]), tip);
        assert_eq!(git(&vm, &["symbolic-ref", "--short", "HEAD"]), "lane");
        assert_eq!(git(&vm, &["remote"]), "", "the bundle origin is removed");
        assert_eq!(std::fs::read_to_string(vm.join("a.txt")).unwrap(), "one\n");
        // Idempotent: a second run is a no-op.
        assert!(sh(&seed).contains("TG-SEED-OK present"));

        // Nothing new yet.
        let out_path = root.path().join("pull.bundle");
        let script = thegn_core::syncstate::pull_back_script(
            vm.to_str().unwrap(),
            &tip,
            out_path.to_str().unwrap(),
        );
        let markers = thegn_core::syncstate::parse_pull_markers(&sh(&script)).unwrap();
        assert_eq!(markers.bundle_bytes, None);

        // Commit inside the "sandbox", then pull it back.
        git(&vm, &["config", "user.name", "Seed Test"]);
        git(&vm, &["config", "user.email", "seed@example.invalid"]);
        std::fs::write(vm.join("a.txt"), "two\n").unwrap();
        git(&vm, &["commit", "-q", "-am", "in the sandbox"]);
        let vm_head = git(&vm, &["rev-parse", "HEAD"]);
        let markers = thegn_core::syncstate::parse_pull_markers(&sh(&script)).unwrap();
        assert_eq!(markers.head, vm_head);
        let size = markers.bundle_bytes.expect("a bundle");
        assert_eq!(std::fs::metadata(&out_path).unwrap().len(), size);

        let outcome = apply_pulled_bundle(&host, "lane", &tip, &out_path, &vm_head).unwrap();
        assert_eq!(
            outcome,
            PullOutcome::FastForwarded {
                from: tip.clone(),
                to: vm_head.clone(),
                commits: 1
            }
        );
        assert_eq!(git(&host, &["rev-parse", "refs/heads/lane"]), vm_head);
        assert_eq!(
            std::fs::read_to_string(host.join("a.txt")).unwrap(),
            "two\n",
            "the checkout moved with the branch"
        );
        assert_eq!(
            git(&host, &["for-each-ref", PULL_REF_PREFIX]),
            "",
            "the scratch ref is dropped after a fast-forward"
        );
    }

    #[test]
    fn pull_back_keeps_a_diverged_head_in_the_scratch_ref() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("wt");
        std::fs::create_dir_all(&host).unwrap();
        git(&host, &["init", "-q", "-b", "lane"]);
        git(&host, &["config", "user.name", "Seed Test"]);
        git(&host, &["config", "user.email", "seed@example.invalid"]);
        std::fs::write(host.join("a.txt"), "one\n").unwrap();
        git(&host, &["add", "a.txt"]);
        git(&host, &["commit", "-q", "-m", "first"]);
        let base = branch_tip(&host, "lane").unwrap();
        let stage = root.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let staged = stage.join("seed.bundle");
        bundle_branch(&host, "lane", &staged).unwrap();
        let vm = root.path().join("vm");
        sh(&thegn_core::syncstate::bundle_seed_script(
            vm.to_str().unwrap(),
            staged.to_str().unwrap(),
            "lane",
            &base,
        ));
        git(&vm, &["config", "user.name", "Seed Test"]);
        git(&vm, &["config", "user.email", "seed@example.invalid"]);
        std::fs::write(vm.join("b.txt"), "vm\n").unwrap();
        git(&vm, &["add", "b.txt"]);
        git(&vm, &["commit", "-q", "-m", "vm side"]);
        // The host moves on independently.
        std::fs::write(host.join("c.txt"), "host\n").unwrap();
        git(&host, &["add", "c.txt"]);
        git(&host, &["commit", "-q", "-m", "host side"]);
        let host_tip = branch_tip(&host, "lane").unwrap();

        let out_path = root.path().join("pull.bundle");
        let markers = thegn_core::syncstate::parse_pull_markers(&sh(
            &thegn_core::syncstate::pull_back_script(
                vm.to_str().unwrap(),
                &host_tip,
                out_path.to_str().unwrap(),
            ),
        ))
        .unwrap();
        assert!(markers.bundle_bytes.is_some(), "full-bundle fallback");
        let outcome =
            apply_pulled_bundle(&host, "lane", &host_tip, &out_path, &markers.head).unwrap();
        let kept = pull_ref_for(&host);
        assert_eq!(
            outcome,
            PullOutcome::Diverged {
                kept_ref: kept.clone(),
                vm_head: markers.head.clone()
            }
        );
        assert_eq!(git(&host, &["rev-parse", "refs/heads/lane"]), host_tip);
        assert_eq!(git(&host, &["rev-parse", &kept]), markers.head);
    }
}
