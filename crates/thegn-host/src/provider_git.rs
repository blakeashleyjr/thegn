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

pub(crate) type CountedPull = (PullReport, Option<u64>);

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
    stage_dir_with(&ProviderSeed {
        provider,
        id,
        exec_env,
    })
}

fn remove_stage_dir(provider: &Provider, id: &str, dir: &str, exec_env: &[(String, String)]) {
    remove_stage_dir_with(
        &ProviderSeed {
            provider,
            id,
            exec_env,
        },
        dir,
    );
}

/// Remove a staging directory through `t`; a failure is logged, not raised.
fn remove_stage_dir_with(t: &dyn SeedTransport, dir: &str) {
    let script = format!("rm -rf -- {}", thegn_core::util::sh_quote(dir));
    match t.sh(&script) {
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

/// How the bundle seed reaches the remote side: run a `/bin/sh` script there
/// (stderr folded into the output) and write a file there. Implemented by a
/// managed provider ([`ProviderSeed`]) and by a plain ssh host
/// (`crate::ssh_seed::SshSeed`), so both seed through one code path.
pub(crate) trait SeedTransport {
    /// Run `script` remotely; returns `(exit code, combined output)`.
    fn sh(&self, script: &str) -> anyhow::Result<(i32, String)>;
    /// Write `bytes` to the remote `path` (its directory is the staging dir).
    fn write(&self, path: &str, bytes: &[u8]) -> anyhow::Result<()>;
    /// A short name for the remote side, for error messages.
    fn label(&self) -> String;
}

/// [`SeedTransport`] over a managed provider's exec + files API.
pub(crate) struct ProviderSeed<'a> {
    pub(crate) provider: &'a Provider,
    pub(crate) id: &'a str,
    pub(crate) exec_env: &'a [(String, String)],
}

impl SeedTransport for ProviderSeed<'_> {
    fn sh(&self, script: &str) -> anyhow::Result<(i32, String)> {
        sandbox_sh(self.provider, self.id, script, self.exec_env)
    }

    fn write(&self, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        block_on_provider(|| async { self.provider.write(self.id, path, bytes).await })
    }

    fn label(&self) -> String {
        "the sandbox".to_string()
    }
}

/// Create a private staging directory through `t`; returns its path.
fn stage_dir_with(t: &dyn SeedTransport) -> anyhow::Result<String> {
    let dir = crate::parity::random_staging_dir()?;
    let (code, out) = t
        .sh(&syncstate::stage_dir_script(&dir))
        .with_context(|| format!("create staging directory {dir} in {}", t.label()))?;
    anyhow::ensure!(
        code == 0,
        "staging directory {dir} could not be created privately (exit {code}): {}",
        crate::provision_recover::stderr_gist(&out)
    );
    Ok(dir)
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
    let t = ProviderSeed {
        provider,
        id,
        exec_env,
    };
    seed_from_bundle_with(&t, worktree, workdir, branch)
}

/// [`seed_from_bundle`] over any [`SeedTransport`]: bundle the local branch,
/// stage it remotely, run [`syncstate::bundle_seed_script`] and fail on
/// anything but a clean exit (the `TG-SEED-ERROR:` marker names the step).
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
pub(crate) fn seed_from_bundle_with(
    t: &dyn SeedTransport,
    worktree: &str,
    workdir: &str,
    branch: &str,
) -> anyhow::Result<()> {
    let mut tip = String::new();
    let result = (|| {
        let wt = Path::new(worktree);
        check_branch_name(wt, branch)?;
        tip = branch_tip(wt, branch)?;
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

        let dir = stage_dir_with(t)?;
        let staged = syncstate::staged_artifact_path(&dir, "seed.bundle");
        t.write(&staged, &bytes)
            .with_context(|| format!("upload the repository bundle to {staged}"))?;
        let script = syncstate::bundle_seed_script(workdir, &staged, branch, &tip);
        let (code, out) = t
            .sh(&script)
            .with_context(|| format!("seed {} repository from the bundle", t.label()))?;
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
    })();
    crate::git_audit::record(&crate::git_audit::GitAuditEvent {
        ts_ms: thegn_core::util::now_ms(),
        action: "seed",
        worktree,
        branch,
        from: "",
        to: &tip,
        outcome: if result.is_ok() { "ok" } else { "error" },
    });
    result
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
pub(crate) fn pull_back_counted(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
) -> anyhow::Result<CountedPull> {
    // Cross-process single flight (`thegn land`, `thegn env pull` and the
    // daemon's exit pull may race on one worktree). Everything below — the
    // branch and the local tip included — is read after the wait, so a second
    // caller sees the first one's fast-forward and reports `up to date`.
    let _lock = pull_lock(worktree)?;
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
        Ok((outcome, _)) => (
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
    crate::git_audit::record(&crate::git_audit::GitAuditEvent {
        ts_ms: thegn_core::util::now_ms(),
        action: "pull",
        worktree,
        branch: &branch,
        from: &local_tip,
        to: &to,
        outcome: outcome_label,
    });
    let (outcome, dirty) = result?;
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
            crate::git_audit::record(&crate::git_audit::GitAuditEvent {
                ts_ms: thegn_core::util::now_ms(),
                action: "push-lane",
                worktree,
                branch: &branch,
                from: "",
                to: "",
                outcome: if push.starts_with("pushed ") {
                    "ok"
                } else {
                    "error"
                },
            });
            Some(push)
        }
        (PullOutcome::FastForwarded { .. }, Some(Err(refusal))) => {
            Some(format!("not pushed: {refusal}"))
        }
        _ => None,
    };
    Ok((
        PullReport {
            branch,
            outcome,
            push,
        },
        dirty,
    ))
}

pub(crate) fn pull_back(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
) -> anyhow::Result<PullReport> {
    pull_back_counted(cfg, worktree, env_name).map(|(report, _)| report)
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

/// How long a [`pull_back`] waits for another process's pull of the same
/// worktree before failing.
pub(crate) const PULL_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// The cross-process claim on one worktree's pull (an exclusive advisory file
/// lock); released when dropped. [`try_begin_pull`] stays the in-process guard.
#[derive(Debug)]
pub(crate) struct PullLock {
    _file: std::fs::File,
}

/// `$XDG_RUNTIME_DIR/thegn`, or the state home's lock dir without one.
fn pull_lock_dir() -> std::path::PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => std::path::PathBuf::from(dir).join("thegn"),
        None => thegn_core::util::xdg_state_home()
            .join("thegn")
            .join("locks"),
    }
}

/// `pull-<sha12>.lock`: the first 12 hex digits of the worktree path's SHA-256.
fn pull_lock_path_in(dir: &Path, worktree: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(worktree.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("pull-{hex}.lock"))
}

/// Take `worktree`'s cross-process pull lock, waiting up to
/// [`PULL_LOCK_WAIT`] for another process's pull to finish.
// off-loop: CLI land/pull and the daemon's blocking-pool exit pull.
pub(crate) fn pull_lock(worktree: &str) -> anyhow::Result<PullLock> {
    pull_lock_in(&pull_lock_dir(), worktree, PULL_LOCK_WAIT)
}

fn pull_lock_in(dir: &Path, worktree: &str, wait: std::time::Duration) -> anyhow::Result<PullLock> {
    if !dir.is_dir() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        crate::platform::restrict_dir_owner_only(dir);
    }
    let path = pull_lock_path_in(dir, worktree);
    let file = crate::platform::open_lock_file(&path)
        .with_context(|| format!("open the pull lock {}", path.display()))?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(PullLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    anyhow::bail!(
                        "another pull of {worktree} is running (waited {}s for {})",
                        wait.as_secs(),
                        path.display()
                    );
                }
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(now)
                        .min(std::time::Duration::from_millis(100)),
                );
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("lock the pull lock {}", path.display()));
            }
        }
    }
}

/// What `thegn land` may do with a worktree before folding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreLand {
    /// Not a provider worktree: land exactly as before.
    NotProvider,
    /// The local branch holds everything the sandbox had: land it locally.
    /// `report` is `None` when no pull was due (a `read-only` git policy, or an
    /// env that no longer resolves to a provider placement).
    Ready { report: Option<PullReport> },
    /// A complete provider listing proves the sandbox is gone: land the local
    /// branch.
    SandboxGone,
    /// The sandbox head was kept in a scratch ref; landing would drop it.
    Refused(String),
}

/// The I/O [`pull_before_land`] needs, as a seam for tests.
pub(crate) trait PreLandOps {
    /// Is the worktree bound to a provider sandbox for `thegn land`? This is
    /// true for a stored provider location, or for a resolved provider env
    /// whose sandbox exists locally or in a complete provider inventory.
    fn is_provider(&self, worktree: &str) -> anyhow::Result<bool>;
    /// [`pull_back_if_provider`].
    fn pull(&self, worktree: &str) -> anyhow::Result<Option<PullReport>>;
    /// [`Self::pull`] plus the sandbox's dirty-path count (`None` = unknown).
    fn pull_counted(&self, worktree: &str) -> anyhow::Result<Option<CountedPull>> {
        self.pull(worktree)
            .map(|report| report.map(|report| (report, None)))
    }
    /// Is the worktree's sandbox in the provider's listing? `Some(false)` only
    /// when a complete listing lacks it; `None` when that cannot be proven.
    fn sandbox_listed(&self, worktree: &str) -> Option<bool>;
}

struct LivePreLand<'a> {
    cfg: &'a Config,
}

impl PreLandOps for LivePreLand<'_> {
    fn is_provider(&self, worktree: &str) -> anyhow::Result<bool> {
        use thegn_core::store::WorkspaceStore;
        let stored = thegn_core::db::Db::open()
            .and_then(|db| db.location_for(worktree))
            .with_context(|| format!("read the stored location of {worktree}"))?;
        let stored = is_provider_location(worktree, stored.as_deref());
        if stored {
            return Ok(true);
        }
        let env = crate::cmd::env::resolve_for(self.cfg, Some(worktree.to_string()))?;
        let name = env
            .placement
            .is_provider()
            .then(|| {
                crate::provider_factory::provider_sandbox_name_checked(
                    self.cfg, worktree, &env.name,
                )
            })
            .transpose()?
            .flatten()
            .filter(|name| !name.trim().is_empty());
        let provisioned = name
            .as_deref()
            .is_some_and(crate::provider_workdir::is_provisioned_locally);
        land_binding(stored, name.as_deref(), provisioned, &mut || {
            self.sandbox_listed(worktree)
        })
        .map_err(anyhow::Error::msg)
    }

    fn pull(&self, worktree: &str) -> anyhow::Result<Option<PullReport>> {
        pull_back_resolved(self.cfg, worktree)
    }

    fn pull_counted(&self, worktree: &str) -> anyhow::Result<Option<CountedPull>> {
        pull_back_resolved_counted(self.cfg, worktree)
    }

    fn sandbox_listed(&self, worktree: &str) -> Option<bool> {
        let env = crate::cmd::env::resolve_for(self.cfg, Some(worktree.to_string())).ok()?;
        let pc = &self.cfg.env.get(&env.name)?.provider;
        let id =
            crate::provider_factory::provider_sandbox_name_checked(self.cfg, worktree, &env.name)
                .ok()??;
        if id.is_empty() {
            return None;
        }
        let owner = crate::provider_factory::provider_sandbox_owner(self.cfg, worktree, &env.name);
        let provider =
            crate::provider_factory::provider_for_named_owned(pc, &id, owner.as_deref())?;
        let report = block_on_provider(|| async { provider.inventory().await }).ok()?;
        sandbox_in_inventory(&report, &id)
    }
}

/// Whether `id` is in a provider inventory: a complete listing proves
/// absence; a partial one only presence. Pure.
pub(crate) fn sandbox_in_inventory(
    report: &thegn_svc::provider::InventoryReport,
    id: &str,
) -> Option<bool> {
    use thegn_svc::provider::InventoryReport;
    let matches =
        |info: &thegn_svc::provider::SandboxInfo| info.id == id || info.name.as_deref() == Some(id);
    match report {
        InventoryReport::Complete(rows) => Some(rows.iter().any(matches)),
        InventoryReport::Partial { rows, .. } => rows.iter().any(matches).then_some(true),
    }
}

/// Whether `thegn land` must treat `worktree` as bound to a provider sandbox.
/// `resolved` is the sandbox name when the worktree's RESOLVED env is
/// provider-placed (`None` = not provider-placed, or no name). A headless
/// worktree (`wt new --env` + `env up`) stores no location, so the stored row
/// alone is not proof of "no sandbox". `listed` is called only when the local
/// provisioned marker is missing; `None` from it refuses. Pure.
pub(crate) fn land_binding(
    stored_is_provider: bool,
    resolved: Option<&str>,
    provisioned_locally: bool,
    listed: &mut dyn FnMut() -> Option<bool>,
) -> Result<bool, String> {
    if stored_is_provider {
        return Ok(true);
    }
    let Some(name) = resolved else {
        return Ok(false);
    };
    if provisioned_locally {
        return Ok(true);
    }
    match listed() {
        Some(bound) => Ok(bound),
        None => Err(format!(
            "cannot tell whether sandbox {name} exists (the provider listing is incomplete or unreachable); land refused so no sandbox commit is skipped: retry when the provider answers, or move the worktree to a local env with `thegn env set <env> --worktree <wt>`"
        )),
    }
}

/// Bring a provider worktree's sandbox commits home before `thegn land`
/// folds the local branch. An explicit action: the pull may wake or resume
/// the sandbox.
// off-loop: CLI path (`thegn land`).
pub(crate) fn pull_before_land(cfg: &Config, worktree: &str) -> anyhow::Result<PreLand> {
    pull_before_land_with(&LivePreLand { cfg }, worktree)
}

pub(crate) fn pull_before_land_with(
    ops: &dyn PreLandOps,
    worktree: &str,
) -> anyhow::Result<PreLand> {
    if !ops.is_provider(worktree)? {
        return Ok(PreLand::NotProvider);
    }
    match ops.pull(worktree) {
        Ok(None) => Ok(PreLand::Ready { report: None }),
        Ok(Some(report)) => {
            let kept = matches!(
                report.outcome,
                PullOutcome::Diverged { .. } | PullOutcome::LocalDirty { .. }
            );
            if kept {
                Ok(PreLand::Refused(format!(
                    "{} ; land refused: merge the kept ref first",
                    report.summary()
                )))
            } else {
                Ok(PreLand::Ready {
                    report: Some(report),
                })
            }
        }
        // Only a complete listing that lacks the sandbox turns a failed pull
        // into "gone"; anything else (a timeout, a partial listing) keeps the
        // pull's own error so nothing is landed over unseen sandbox commits.
        Err(error) => match ops.sandbox_listed(worktree) {
            Some(false) => Ok(PreLand::SandboxGone),
            Some(true) | None => {
                Err(error.context(format!("pull {worktree} from its sandbox before landing")))
            }
        },
    }
}

/// What a destroy, recycle or release learned about the sandbox first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeforeDestroy {
    /// No provider sandbox is bound (the land binding said no).
    NotBound,
    /// A complete listing proves the sandbox gone.
    Gone,
    /// The pull ran (`report: None` = the read-only policy: no pull is due).
    Pulled {
        report: Option<PullReport>,
        sandbox_dirty: Option<u64>,
    },
}

pub(crate) fn pull_before_destroy(cfg: &Config, worktree: &str) -> anyhow::Result<BeforeDestroy> {
    pull_before_destroy_with(&LivePreLand { cfg }, worktree)
}

pub(crate) fn pull_before_destroy_with(
    ops: &dyn PreLandOps,
    worktree: &str,
) -> anyhow::Result<BeforeDestroy> {
    if !ops
        .is_provider(worktree)
        .map_err(|e| anyhow::anyhow!("cannot tell whether {worktree} still has a provider sandbox: the provider listing is incomplete or unreachable ({e:#})"))?
    {
        return Ok(BeforeDestroy::NotBound);
    }
    match ops.pull_counted(worktree) {
        Ok(None) => Ok(BeforeDestroy::Pulled {
            report: None,
            sandbox_dirty: None,
        }),
        Ok(Some((report, sandbox_dirty))) => Ok(BeforeDestroy::Pulled {
            report: Some(report),
            sandbox_dirty,
        }),
        Err(error) => match ops.sandbox_listed(worktree) {
            Some(false) => Ok(BeforeDestroy::Gone),
            Some(true) | None => Err(error.context(format!(
                "pull {worktree} from its sandbox before destroying it"
            ))),
        },
    }
}

/// One line a destroy path prints after the guard (`warn` = loud).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardNote {
    pub warn: bool,
    pub text: String,
}

/// Pure: may the sandbox be destroyed after `pre`? `Err` is the refusal.
pub(crate) fn destroy_guard(
    pre: anyhow::Result<BeforeDestroy>,
    sandbox: &str,
    accept_loss: bool,
) -> Result<Vec<GuardNote>, String> {
    let pre = match pre {
        Ok(pre) => pre,
        Err(error) if !accept_loss => {
            return Err(format!(
                "could not pull sandbox {sandbox}'s commits before destroying it: {error:#}; nothing was destroyed. Pull them with `thegn env pull`, or force the destroy (`thegn wt rm --force`, the TUI force delete, `thegn env down --force`)"
            ));
        }
        Err(error) => {
            return Ok(vec![GuardNote {
                warn: true,
                text: format!(
                    "could not pull sandbox {sandbox}: {error:#}; destroying it anyway (forced): commits made inside it that were not pulled back are lost"
                ),
            }]);
        }
    };
    match pre {
        BeforeDestroy::NotBound | BeforeDestroy::Gone => Ok(Vec::new()),
        BeforeDestroy::Pulled { report: None, .. } => Ok(vec![GuardNote {
            warn: false,
            text: format!(
                "sandbox {sandbox}: no pull (read-only git policy); commits made inside it are not kept"
            ),
        }]),
        BeforeDestroy::Pulled {
            report: Some(report),
            sandbox_dirty,
        } => {
            let mut notes = vec![GuardNote {
                warn: false,
                text: format!("pulled {}: {}", report.branch, report.summary()),
            }];
            if matches!(
                &report.outcome,
                &PullOutcome::Diverged { .. } | &PullOutcome::LocalDirty { .. }
            ) {
                if !accept_loss {
                    return Err(format!(
                        "sandbox {sandbox}: {}; its commits are not on {} and nothing was destroyed. Merge the kept ref first, then retry, or force the destroy",
                        report.summary(),
                        report.branch
                    ));
                }
                notes.push(GuardNote {
                    warn: true,
                    text: format!(
                        "sandbox {sandbox} destroyed (forced): its commits are only in the kept ref ({}); merge it before another pull of this worktree overwrites it",
                        report.summary()
                    ),
                });
            }
            match sandbox_dirty {
                Some(0) => Ok(notes),
                Some(n) if !accept_loss => Err(format!(
                    "sandbox {sandbox} has {n} uncommitted or untracked path(s) that a pull cannot bring home; nothing was destroyed. Commit them inside the sandbox and retry, or force the destroy"
                )),
                None if !accept_loss => Err(format!(
                    "could not tell whether sandbox {sandbox} has uncommitted changes; nothing was destroyed. Retry, or force the destroy"
                )),
                Some(n) => {
                    notes.push(GuardNote {
                        warn: true,
                        text: format!(
                            "sandbox {sandbox}: {n} uncommitted or untracked path(s) are lost with it (forced)"
                        ),
                    });
                    Ok(notes)
                }
                None => {
                    notes.push(GuardNote {
                        warn: true,
                        text: format!(
                            "sandbox {sandbox}: an unknown number of uncommitted or untracked path(s) are lost with it (forced)"
                        ),
                    });
                    Ok(notes)
                }
            }
        }
    }
}

/// The offline answer to "is a provider sandbox bound to this worktree"
/// ([`land_binding`] with no listing; never wakes a sandbox).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OfflineBinding {
    Unbound,
    /// Bound, or cannot be proven unbound without a provider read.
    Bound {
        sandbox: String,
    },
    /// The stored location is a provider location but the resolved env names no sandbox.
    BoundUnknown,
}

pub(crate) fn bound_offline(cfg: &Config, worktree: &str) -> anyhow::Result<OfflineBinding> {
    use thegn_core::store::WorkspaceStore;
    let stored = thegn_core::db::Db::open()
        .and_then(|db| db.location_for(worktree))
        .with_context(|| format!("read the stored location of {worktree}"))?;
    let stored = is_provider_location(worktree, stored.as_deref());
    let env = crate::cmd::env::resolve_for(cfg, Some(worktree.to_string()))?;
    let name = env
        .placement
        .is_provider()
        .then(|| crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, &env.name))
        .transpose()?
        .flatten()
        .filter(|name| !name.trim().is_empty());
    let provisioned = name
        .as_deref()
        .is_some_and(crate::provider_workdir::is_provisioned_locally);
    match (
        land_binding(stored, name.as_deref(), provisioned, &mut || None),
        name,
    ) {
        (Ok(false), _) => Ok(OfflineBinding::Unbound),
        (Ok(true) | Err(_), Some(name)) => Ok(OfflineBinding::Bound { sandbox: name }),
        (Ok(true) | Err(_), None) => Ok(OfflineBinding::BoundUnknown),
    }
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
    pull_back_resolved(cfg, worktree)
}

/// [`pull_back`] for a worktree whose resolved env is provider-placed,
/// without the stored-location gate (`thegn land` has already proven the
/// sandbox exists); `Ok(None)` for a non-provider env or a `read-only` policy.
pub(crate) fn pull_back_resolved(
    cfg: &Config,
    worktree: &str,
) -> anyhow::Result<Option<PullReport>> {
    pull_back_resolved_counted(cfg, worktree).map(|report| report.map(|(report, _)| report))
}

pub(crate) fn pull_back_resolved_counted(
    cfg: &Config,
    worktree: &str,
) -> anyhow::Result<Option<CountedPull>> {
    let env = crate::cmd::env::resolve_for(cfg, Some(worktree.to_string()))?;
    if !env.placement.is_provider() {
        return Ok(None);
    }
    // `read-only`: the owner chose not to pull from this sandbox. Nothing to
    // pull and nothing to report on every session exit.
    if env_git_policy(cfg, &env.name).mode == SandboxGitMode::ReadOnly {
        return Ok(None);
    }
    pull_back_counted(cfg, worktree, &env.name).map(Some)
}

fn pull_from_sandbox(
    provider: &Provider,
    id: &str,
    workdir: &str,
    wt: &Path,
    branch: &str,
    local_tip: &str,
) -> anyhow::Result<(PullOutcome, Option<u64>)> {
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
) -> anyhow::Result<(PullOutcome, Option<u64>)> {
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
        return Ok((PullOutcome::UpToDate, markers.dirty));
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
        .map(|outcome| (outcome, markers.dirty))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SEED_BUNDLE_BYTES, PULL_REF_PREFIX, PreLandOps, PullOutcome, PullReport, SeedTransport,
        apply_pulled_bundle, branch_tip, bundle_branch, check_pull_policy, check_pull_target,
        classify_pull, is_protected_branch, land_binding, no_exec_channel_message, pull_ref_for,
        push_decision, seed_cap_message, seed_from_bundle_with, should_pull_on_exit,
        try_begin_pull,
    };
    use std::path::Path;
    use thegn_core::config::{SandboxGitConfig, SandboxGitMode};
    use thegn_core::testenv::IsolatedState;

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

    struct FakeSeedTransport;

    impl SeedTransport for FakeSeedTransport {
        fn sh(&self, _script: &str) -> anyhow::Result<(i32, String)> {
            Ok((0, String::new()))
        }

        fn write(&self, _path: &str, _bytes: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }

        fn label(&self) -> String {
            "fake transport".to_string()
        }
    }

    #[test]
    fn seed_with_fake_transport_writes_a_seed_line() {
        let state = IsolatedState::new();
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        git(&worktree, &["init", "-q", "-b", "lane"]);
        git(&worktree, &["config", "user.name", "Seed Test"]);
        git(&worktree, &["config", "user.email", "seed@example.invalid"]);
        std::fs::write(worktree.join("seed.txt"), "seed\n").unwrap();
        git(&worktree, &["add", "seed.txt"]);
        git(&worktree, &["commit", "-q", "-m", "seed"]);
        let tip = branch_tip(&worktree, "lane").unwrap();

        seed_from_bundle_with(
            &FakeSeedTransport,
            worktree.to_str().unwrap(),
            "/workspace",
            "lane",
        )
        .unwrap();

        let path = state.state_home().join("thegn/provider/git-audit.jsonl");
        let value: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(path).unwrap().trim()).unwrap();
        assert_eq!(value["action"], "seed");
        assert_eq!(value["branch"], "lane");
        assert_eq!(value["to"], tip);
        assert_eq!(value["outcome"], "ok");
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
    fn pull_back_script_counts_dirty_paths() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("sandbox");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "lane"]);
        git(&repo, &["config", "user.name", "Dirty Test"]);
        git(&repo, &["config", "user.email", "dirty@example.invalid"]);
        std::fs::write(repo.join("tracked.txt"), "clean\n").unwrap();
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        let tip = branch_tip(&repo, "lane").unwrap();
        std::fs::write(repo.join("tracked.txt"), "edited\n").unwrap();
        std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();

        let out_path = root.path().join("pull.bundle");
        let markers = thegn_core::syncstate::parse_pull_markers(&sh(
            &thegn_core::syncstate::pull_back_script(
                repo.to_str().unwrap(),
                &tip,
                out_path.to_str().unwrap(),
            ),
        ))
        .unwrap();
        assert_eq!(markers.dirty, Some(2));
        assert_eq!(markers.bundle_bytes, None);
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

    // ---- b162: pull lock + pull_before_land ---------------------------------

    #[test]
    fn land_binding_table() {
        let calls = std::cell::Cell::new(0);
        let mut listed = || {
            calls.set(calls.get() + 1);
            Some(false)
        };
        assert_eq!(land_binding(true, None, false, &mut listed), Ok(true));
        assert_eq!(calls.get(), 0);

        assert_eq!(land_binding(false, None, false, &mut listed), Ok(false));
        assert_eq!(calls.get(), 0);

        assert_eq!(
            land_binding(false, Some("tg-rd-x-sandbox"), true, &mut listed),
            Ok(true)
        );
        assert_eq!(calls.get(), 0);

        let mut present = || Some(true);
        assert_eq!(
            land_binding(false, Some("tg-rd-x-sandbox"), false, &mut present),
            Ok(true)
        );
        let mut absent = || Some(false);
        assert_eq!(
            land_binding(false, Some("tg-rd-x-sandbox"), false, &mut absent),
            Ok(false)
        );
        let mut unreachable = || None;
        let error =
            land_binding(false, Some("tg-rd-x-sandbox"), false, &mut unreachable).unwrap_err();
        assert!(error.contains("tg-rd-x-sandbox"), "{error}");
        assert!(error.contains("land refused"), "{error}");
    }

    #[test]
    fn headless_provider_worktree_is_a_provider_for_land() {
        use thegn_core::config::{Config, EnvConfig, EnvProviderConfig, PlacementMode};
        use thegn_core::db::Db;
        use thegn_core::store::WorkspaceStore;

        let _state = IsolatedState::new();
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        let worktree = parent.path().join("wt");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Land Test"]);
        git(&repo, &["config", "user.email", "land@example.invalid"]);
        std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
        git(&repo, &["add", "seed.txt"]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        let worktree_s = worktree.to_string_lossy().into_owned();
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "tg/rd-x", &worktree_s],
        );

        let root_s = repo.to_string_lossy().into_owned();
        let db = Db::open().unwrap();
        db.put_worktree("repo/rd-x", &root_s, &worktree_s, "tg/rd-x", None, None)
            .unwrap();
        db.put_workspace(&root_s, "repo", "repo").unwrap();
        db.set_workspace_env(&root_s, "sprites").unwrap();

        let mut cfg = Config::default();
        cfg.env.insert(
            "sprites".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                provider: EnvProviderConfig {
                    provider: "sprites".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let name =
            crate::provider_factory::provider_sandbox_name_checked(&cfg, &worktree_s, "sprites")
                .unwrap()
                .unwrap();
        assert_eq!(
            super::bound_offline(&cfg, &worktree_s).unwrap(),
            super::OfflineBinding::Bound {
                sandbox: name.clone()
            }
        );
        crate::provider_workdir::mark_provisioned(&name);
        assert!(
            super::LivePreLand { cfg: &cfg }
                .is_provider(&worktree_s)
                .unwrap()
        );

        crate::provider_workdir::clear_provisioned(&name);
        cfg.env.get_mut("sprites").unwrap().placement = PlacementMode::Local;
        assert!(
            !super::LivePreLand { cfg: &cfg }
                .is_provider(&worktree_s)
                .unwrap()
        );
        assert_eq!(
            super::bound_offline(&cfg, &worktree_s).unwrap(),
            super::OfflineBinding::Unbound
        );

        let recorded = thegn_core::remote::GitLoc::provider_db_string(
            &["sprite".into(), "exec".into()],
            "/workspace",
        );
        db.put_worktree(
            "repo/rd-x",
            &root_s,
            &worktree_s,
            "tg/rd-x",
            Some(&recorded),
            None,
        )
        .unwrap();
        assert_eq!(
            super::bound_offline(&cfg, &worktree_s).unwrap(),
            super::OfflineBinding::BoundUnknown
        );
    }

    struct FakePreLand {
        provider: bool,
        pull: std::cell::RefCell<Option<anyhow::Result<Option<PullReport>>>>,
        dirty: Option<u64>,
        listed: Option<bool>,
        pulls: std::cell::Cell<usize>,
    }

    impl FakePreLand {
        fn new(
            provider: bool,
            pull: anyhow::Result<Option<PullReport>>,
            listed: Option<bool>,
        ) -> Self {
            FakePreLand {
                provider,
                pull: std::cell::RefCell::new(Some(pull)),
                dirty: Some(0),
                listed,
                pulls: std::cell::Cell::new(0),
            }
        }
    }

    impl super::PreLandOps for FakePreLand {
        fn is_provider(&self, _worktree: &str) -> anyhow::Result<bool> {
            Ok(self.provider)
        }
        fn pull(&self, _worktree: &str) -> anyhow::Result<Option<PullReport>> {
            self.pulls.set(self.pulls.get() + 1);
            self.pull.borrow_mut().take().expect("pulled once")
        }
        fn pull_counted(&self, _worktree: &str) -> anyhow::Result<Option<super::CountedPull>> {
            self.pulls.set(self.pulls.get() + 1);
            self.pull
                .borrow_mut()
                .take()
                .expect("pulled once")
                .map(|report| report.map(|report| (report, self.dirty)))
        }
        fn sandbox_listed(&self, _worktree: &str) -> Option<bool> {
            self.listed
        }
    }

    fn report(outcome: PullOutcome) -> PullReport {
        PullReport {
            branch: "tg/lane".into(),
            outcome,
            push: None,
        }
    }

    #[test]
    fn pull_before_land_table() {
        use super::{PreLand, pull_before_land_with};
        let wt = "/wt/pre-land";

        // Not a provider worktree: nothing is pulled.
        let fake = FakePreLand::new(false, Ok(None), None);
        assert_eq!(
            pull_before_land_with(&fake, wt).unwrap(),
            PreLand::NotProvider
        );
        assert_eq!(fake.pulls.get(), 0);

        // Up to date / fast-forwarded / no pull due: Ready.
        let up = report(PullOutcome::UpToDate);
        let fake = FakePreLand::new(true, Ok(Some(up.clone())), None);
        assert_eq!(
            pull_before_land_with(&fake, wt).unwrap(),
            PreLand::Ready { report: Some(up) }
        );
        let ff = report(PullOutcome::FastForwarded {
            from: "a".repeat(40),
            to: "b".repeat(40),
            commits: 2,
        });
        let fake = FakePreLand::new(true, Ok(Some(ff.clone())), None);
        assert_eq!(
            pull_before_land_with(&fake, wt).unwrap(),
            PreLand::Ready { report: Some(ff) }
        );
        let fake = FakePreLand::new(true, Ok(None), None);
        assert_eq!(
            pull_before_land_with(&fake, wt).unwrap(),
            PreLand::Ready { report: None }
        );

        // Diverged / local-dirty: refused, naming the kept ref.
        let kept = format!("{PULL_REF_PREFIX}abc");
        for outcome in [
            PullOutcome::Diverged {
                kept_ref: kept.clone(),
                vm_head: "c".repeat(40),
            },
            PullOutcome::LocalDirty {
                kept_ref: kept.clone(),
            },
        ] {
            let fake = FakePreLand::new(true, Ok(Some(report(outcome))), Some(true));
            let PreLand::Refused(message) = pull_before_land_with(&fake, wt).unwrap() else {
                panic!("a kept sandbox head must refuse the land");
            };
            assert!(message.contains(&kept), "{message}");
            assert!(
                message.ends_with(" ; land refused: merge the kept ref first"),
                "{message}"
            );
        }

        // A failed pull whose sandbox a complete listing lacks: gone.
        let fake = FakePreLand::new(true, Err(anyhow::anyhow!("404 not found")), Some(false));
        assert_eq!(
            pull_before_land_with(&fake, wt).unwrap(),
            PreLand::SandboxGone
        );

        // Any other failure (listed, or absence unprovable) stays an error.
        for listed in [Some(true), None] {
            let fake = FakePreLand::new(true, Err(anyhow::anyhow!("exec timed out")), listed);
            let error = pull_before_land_with(&fake, wt).unwrap_err();
            assert!(format!("{error:#}").contains("exec timed out"), "{error:#}");
        }
    }

    #[test]
    fn pull_before_destroy_table() {
        use super::{BeforeDestroy, pull_before_destroy_with};
        let wt = "/wt/pre-destroy";

        let fake = FakePreLand::new(false, Ok(None), None);
        assert_eq!(
            pull_before_destroy_with(&fake, wt).unwrap(),
            BeforeDestroy::NotBound
        );
        assert_eq!(fake.pulls.get(), 0);

        let fake = FakePreLand::new(true, Ok(None), None);
        assert_eq!(
            pull_before_destroy_with(&fake, wt).unwrap(),
            BeforeDestroy::Pulled {
                report: None,
                sandbox_dirty: None,
            }
        );

        let mut fake = FakePreLand::new(true, Ok(Some(report(PullOutcome::UpToDate))), None);
        fake.dirty = Some(2);
        let pulled = report(PullOutcome::UpToDate);
        assert_eq!(
            pull_before_destroy_with(&fake, wt).unwrap(),
            BeforeDestroy::Pulled {
                report: Some(pulled),
                sandbox_dirty: Some(2),
            }
        );

        let fake = FakePreLand::new(true, Err(anyhow::anyhow!("404 not found")), Some(false));
        assert_eq!(
            pull_before_destroy_with(&fake, wt).unwrap(),
            BeforeDestroy::Gone
        );
        for listed in [Some(true), None] {
            let fake = FakePreLand::new(true, Err(anyhow::anyhow!("exec timed out")), listed);
            let error = pull_before_destroy_with(&fake, wt).unwrap_err();
            assert!(format!("{error:#}").contains("before destroying it"));
        }
    }

    #[test]
    fn destroy_guard_table() {
        use super::{BeforeDestroy, destroy_guard};
        let clean = report(PullOutcome::FastForwarded {
            from: "a".repeat(40),
            to: "b".repeat(40),
            commits: 1,
        });
        for forced in [false, true] {
            let notes = destroy_guard(Ok(BeforeDestroy::NotBound), "tg-box", forced).unwrap();
            assert!(notes.is_empty());
            let notes = destroy_guard(Ok(BeforeDestroy::Gone), "tg-box", forced).unwrap();
            assert!(notes.is_empty());
        }
        let notes = destroy_guard(
            Ok(BeforeDestroy::Pulled {
                report: None,
                sandbox_dirty: None,
            }),
            "tg-box",
            false,
        )
        .unwrap();
        assert!(!notes[0].warn);
        assert!(notes[0].text.contains("no pull"));

        let notes = destroy_guard(
            Ok(BeforeDestroy::Pulled {
                report: Some(clean.clone()),
                sandbox_dirty: Some(0),
            }),
            "tg-box",
            false,
        )
        .unwrap();
        assert!(notes[0].text.contains("pulled tg/lane:"));

        let diverged = report(PullOutcome::Diverged {
            kept_ref: "refs/thegn/pull/tg-box".into(),
            vm_head: "c".repeat(40),
        });
        let refusal = destroy_guard(
            Ok(BeforeDestroy::Pulled {
                report: Some(diverged.clone()),
                sandbox_dirty: Some(0),
            }),
            "tg-box",
            false,
        )
        .unwrap_err();
        assert!(refusal.contains("nothing was destroyed"), "{refusal}");
        let notes = destroy_guard(
            Ok(BeforeDestroy::Pulled {
                report: Some(diverged),
                sandbox_dirty: Some(0),
            }),
            "tg-box",
            true,
        )
        .unwrap();
        assert!(
            notes
                .iter()
                .any(|note| note.warn && note.text.contains("kept ref"))
        );

        for dirty in [Some(2), None] {
            let refusal = destroy_guard(
                Ok(BeforeDestroy::Pulled {
                    report: Some(clean.clone()),
                    sandbox_dirty: dirty,
                }),
                "tg-box",
                false,
            )
            .unwrap_err();
            assert!(refusal.contains("nothing was destroyed"), "{refusal}");
            let notes = destroy_guard(
                Ok(BeforeDestroy::Pulled {
                    report: Some(clean.clone()),
                    sandbox_dirty: dirty,
                }),
                "tg-box",
                true,
            )
            .unwrap();
            assert!(
                notes
                    .iter()
                    .any(|note| note.warn && note.text.contains("lost"))
            );
        }

        let error = destroy_guard(
            Err::<BeforeDestroy, _>(anyhow::anyhow!("provider unavailable")),
            "tg-box",
            false,
        )
        .unwrap_err();
        assert!(error.contains("nothing was destroyed"), "{error}");
        let notes =
            destroy_guard(Err(anyhow::anyhow!("provider unavailable")), "tg-box", true).unwrap();
        assert!(notes[0].warn && notes[0].text.contains("lost"));
    }

    #[test]
    fn sandbox_absence_needs_a_complete_listing() {
        use super::sandbox_in_inventory;
        use thegn_svc::provider::{InventoryReport, SandboxInfo};
        let complete = InventoryReport::Complete(vec![
            SandboxInfo::id_only("tg-a"),
            SandboxInfo {
                id: "machine-1".into(),
                name: Some("tg-b".into()),
                ..SandboxInfo::default()
            },
        ]);
        assert_eq!(sandbox_in_inventory(&complete, "tg-a"), Some(true));
        assert_eq!(sandbox_in_inventory(&complete, "tg-b"), Some(true));
        assert_eq!(sandbox_in_inventory(&complete, "tg-c"), Some(false));
        let partial = InventoryReport::Partial {
            rows: vec![SandboxInfo::id_only("tg-a")],
            reason: "page 2 failed".into(),
        };
        assert_eq!(sandbox_in_inventory(&partial, "tg-a"), Some(true));
        assert_eq!(sandbox_in_inventory(&partial, "tg-c"), None);
    }

    #[test]
    fn pull_lock_waits_for_the_holder_then_succeeds() {
        use super::pull_lock_in;
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let wt = "/wt/pull-lock-wait";
        let first = pull_lock_in(dir.path(), wt, Duration::from_secs(5)).expect("first lock");
        let path = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let waiter = std::thread::spawn(move || {
            let lock = pull_lock_in(&path, wt, Duration::from_secs(30));
            tx.send(()).unwrap();
            (lock.map(drop), started.elapsed())
        });
        // The second caller is still waiting while the first holds the lock.
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "the second pull must wait for the first"
        );
        drop(first);
        let (result, waited) = waiter.join().unwrap();
        result.expect("the second pull gets the lock once the first drops it");
        assert!(waited >= Duration::from_millis(400), "{waited:?}");
        // Another worktree never contends.
        let _other = pull_lock_in(dir.path(), "/wt/pull-lock-other", Duration::ZERO).unwrap();
    }

    #[test]
    fn pull_lock_times_out_with_the_message() {
        use super::{pull_lock_in, pull_lock_path_in};
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let wt = "/wt/pull-lock-timeout";
        let _held = pull_lock_in(dir.path(), wt, Duration::ZERO).expect("first lock");
        let error = std::thread::scope(|scope| {
            scope
                .spawn(|| pull_lock_in(dir.path(), wt, Duration::from_millis(200)))
                .join()
                .unwrap()
        })
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("another pull of /wt/pull-lock-timeout is running"),
            "{error:#}"
        );
        let name = pull_lock_path_in(dir.path(), wt);
        let name = name.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("pull-") && name.ends_with(".lock"),
            "{name}"
        );
        assert_eq!(name.len(), "pull-".len() + 12 + ".lock".len(), "{name}");
    }
}
