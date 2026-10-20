//! Cross-host branch ingest for the merge queue: make a queued branch's tip
//! present in the **target** repo's object store when the branch's worktree
//! lives on a different machine, so the object-DB fold can merge it.
//!
//! The merge queue folds every branch into one target store (where the target
//! branch lives). A same-host branch already shares that store — git worktrees
//! share a single `.git` — so its tip OID is already present. A branch whose
//! worktree is on **another host** (ssh/provider) has its *own* object store, so
//! its tip is absent from the target and the fold would fail on an unknown OID.
//! We bridge that with a git **bundle**: create a bundle of the branch ref on the
//! branch's host, stream it to the target host, and `git fetch` it into a
//! synthetic ref scoped to the source row and ingestion generation. That ref
//! is what the fold merges and an attempt-scoped guard removes it afterward.
//!
//! Runs off the event loop (from the merge-queue drain in `spawn_blocking` / the
//! CLI). It shells `git bundle`/`git fetch`, so the `.output()` sites carry the
//! host crate's off-loop `#[expect(clippy::disallowed_methods)]`.

use crate::platform::bundle_custody::BundleTemp;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use thegn_core::remote::GitLoc;
use thegn_svc::git::{CliGit, PlumbingOps};

/// The fenced source tip used by the fold. The ingested source ref stays owned
/// by this guard for the full integration attempt. Same-store sources have no
/// synthetic ref and no guard.
pub struct IngestedTip {
    /// Ref the fold reads from the target object store.
    pub ref_name: String,
    /// Captured remote source OID; absent for same-store branches.
    pub oid: Option<String>,
    _guard: Option<IngestRef>,
}

struct IngestRef {
    target: GitLoc,
    ref_name: String,
    oid: String,
}

impl Drop for IngestRef {
    fn drop(&mut self) {
        // best-effort: the attempt must still return its actual disposition.
        if !self
            .target
            .git_ok(&["update-ref", "-d", &self.ref_name, &self.oid])
        {
            tracing::warn!(ref_name = %self.ref_name, "failed to remove merge-queue ingest ref");
        }
    }
}

/// Ensure `branch`'s tip is available in the target store. Cross-host refs are
/// unique per source authority, worktree and random ingestion generation.
pub fn ingest_tip(target: &GitLoc, branch: &str, branch_loc: &GitLoc) -> Result<IngestedTip> {
    if !needs_ingest(target, branch_loc) {
        return Ok(IngestedTip {
            ref_name: format!("refs/heads/{branch}"),
            oid: None,
            _guard: None,
        });
    }
    ingest_tip_with_expected(target, branch, branch_loc, None)
}

fn ingest_tip_with_expected(
    target: &GitLoc,
    branch: &str,
    branch_loc: &GitLoc,
    injected_expected: Option<&str>,
) -> Result<IngestedTip> {
    if target.is_remote() {
        anyhow::bail!(
            "remote target store: run the drain on the target host (merge daemon), \
             not by pushing a bundle over ssh"
        );
    }
    let expected = match injected_expected {
        Some(oid) => oid.to_string(),
        None => CliGit.rev_parse(branch_loc, &format!("refs/heads/{branch}"))?,
    };
    prune_refs(target, branch_loc, branch)?;
    let ref_name = generation_ref(branch_loc, branch)?;
    let bundle = bundle_bytes(branch_loc, branch)
        .with_context(|| format!("branch host unreachable while bundling {branch}"))?;
    fetch_bundle(target, branch, &bundle, &ref_name, &expected)
        .with_context(|| format!("fetching {branch} into the target store"))?;
    let guard = IngestRef {
        target: target.clone(),
        ref_name: ref_name.clone(),
        oid: expected.clone(),
    };
    Ok(IngestedTip {
        ref_name: ref_name.clone(),
        oid: Some(expected.clone()),
        _guard: Some(guard),
    })
}

fn generation_ref(branch_loc: &GitLoc, branch: &str) -> Result<String> {
    let row = row_key(branch_loc, branch);
    let mut generation = [0_u8; 8];
    getrandom::fill(&mut generation).context("mint merge-queue ingest generation")?;
    Ok(format!("refs/thegn/mq/{row}/{}", hex_prefix(&generation)))
}

fn row_key(branch_loc: &GitLoc, branch: &str) -> String {
    let mut row_hash = Sha256::new();
    row_hash.update(host_id(branch_loc).as_bytes());
    row_hash.update([0]);
    row_hash.update(loc_path(branch_loc).as_bytes());
    row_hash.update([0]);
    row_hash.update(branch.as_bytes());
    hex_prefix(&row_hash.finalize()[..8])
}

fn hex_prefix(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn loc_path(loc: &GitLoc) -> String {
    match loc {
        GitLoc::Local(path) => path.to_string_lossy().into_owned(),
        GitLoc::Remote { path, .. } | GitLoc::Provider { path, .. } => path.clone(),
    }
}

fn prune_refs(target: &GitLoc, branch_loc: &GitLoc, branch: &str) -> Result<()> {
    const PRUNE_LIMIT: usize = 64;
    let prefix = format!("refs/thegn/mq/{}/", row_key(branch_loc, branch));
    let refs = match enumerate_stale_refs(target, &prefix) {
        Ok(refs) => refs,
        Err(error) => {
            tracing::warn!(ref_prefix = %prefix, %error, "failed to enumerate stale merge-queue ingest refs");
            Vec::new()
        }
    };
    if refs.len() == PRUNE_LIMIT {
        tracing::warn!(
            ref_prefix = %prefix,
            count = refs.len(),
            "merge-queue ingest ref prune reached its per-attempt limit"
        );
    }
    for ref_name in refs {
        if !target.git_ok(&["update-ref", "-d", &ref_name]) {
            tracing::warn!(ref_name = %ref_name, "failed to prune stale merge-queue ingest ref");
        }
    }
    let legacy = format!("refs/thegn/mq/{branch}");
    if target
        .git_out(&["rev-parse", "--verify", &legacy])
        .is_some()
        && !target.git_ok(&["update-ref", "-d", &legacy])
    {
        tracing::warn!(ref_name = %legacy, "failed to prune legacy merge-queue ingest ref");
    }
    Ok(())
}

fn parse_ref_listing(stdout: &[u8]) -> Result<Vec<String>> {
    Ok(String::from_utf8_lossy(stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

#[expect(clippy::disallowed_methods)] // off-loop: merge-queue drain (spawn_blocking / CLI)
fn enumerate_stale_refs(target: &GitLoc, prefix: &str) -> Result<Vec<String>> {
    let output = target
        .git_command(&["for-each-ref", "--count=64", "--format=%(refname)", prefix])
        .output()
        .context("spawn git for-each-ref")?;
    if !output.status.success() {
        anyhow::bail!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_ref_listing(&output.stdout)
}

/// Result of comparing the live source host branch to the ingested OID.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceCheck {
    /// The live source still names the ingested OID.
    Unchanged,
    /// The live source moved to a different OID.
    Moved { actual: String },
    /// The live source could not be read.
    Unreachable(String),
}

fn classify_source(ingested: &str, live: Result<String>) -> SourceCheck {
    match live {
        Ok(actual) if actual == ingested => SourceCheck::Unchanged,
        Ok(actual) => SourceCheck::Moved { actual },
        Err(error) => SourceCheck::Unreachable(format!("{error:#}")),
    }
}

/// Re-read a source branch on its authority and compare it with its ingested OID.
pub fn verify_source_unchanged(branch_loc: &GitLoc, branch: &str, ingested: &str) -> SourceCheck {
    classify_source(
        ingested,
        CliGit.rev_parse(branch_loc, &format!("refs/heads/{branch}")),
    )
}

/// A stable identity for the *store* a loc points at: `local` for the executing
/// host, `ssh:<host>:<port>` for an ssh remote, `prov:<prefix>` for a provider
/// env. Two locs share an object store iff their host ids match (worktrees of
/// one repo on one host share its `.git`; membership already scopes to one repo).
fn host_id(loc: &GitLoc) -> String {
    match loc {
        GitLoc::Local(_) => "local".to_string(),
        GitLoc::Remote { ssh, .. } => format!("ssh:{}:{}", ssh.host, ssh.port),
        GitLoc::Provider { control_prefix, .. } => format!("prov:{}", control_prefix.join(" ")),
    }
}

/// Whether `branch`'s tip must be fetched into `target`'s store — true exactly
/// when the two live on different hosts.
fn needs_ingest(target: &GitLoc, branch: &GitLoc) -> bool {
    host_id(target) != host_id(branch)
}

/// Bundle the branch ref on its own host and return the bundle bytes. `git
/// bundle create -` writes the bundle to stdout (progress goes to stderr, which
/// we keep separate), so this works verbatim over ssh/provider.
#[expect(clippy::disallowed_methods)] // off-loop: merge-queue drain (spawn_blocking / CLI)
fn bundle_bytes(branch_loc: &GitLoc, branch: &str) -> Result<Vec<u8>> {
    let src = format!("refs/heads/{branch}");
    let out = branch_loc
        .git_command(&["bundle", "create", "-", &src])
        .output()
        .context("spawn git bundle create")?;
    if !out.status.success() {
        anyhow::bail!(
            "git bundle create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if out.stdout.is_empty() {
        anyhow::bail!("git bundle create produced no data");
    }
    Ok(out.stdout)
}

/// Materialize the bundle on the target host and fetch its branch ref into the
/// supplied generation-scoped ref. Under the co-location model the drain runs on the
/// target host, so `target` is `Local`; a remote target means the caller should
/// have dispatched to that host's drain daemon (see Milestone B), so we fail
/// loudly rather than trying to push a bundle the wrong way.
fn fetch_bundle(
    target: &GitLoc,
    branch: &str,
    bundle: &[u8],
    target_ref: &str,
    expected: &str,
) -> Result<()> {
    fetch_bundle_with_root(target, branch, bundle, target_ref, expected, None)
}

fn fetch_bundle_with_root(
    target: &GitLoc,
    branch: &str,
    bundle: &[u8],
    target_ref: &str,
    expected: &str,
    temp_root: Option<&Path>,
) -> Result<()> {
    if target.is_remote() {
        anyhow::bail!(
            "remote target store: run the drain on the target host (merge daemon), \
             not by pushing a bundle over ssh"
        );
    }
    let mut tmp = match temp_root {
        Some(root) => BundleTemp::new_in(root),
        None => BundleTemp::new(),
    }
    .context("create private temp bundle custody")?;
    if let Err(error) = tmp.write(bundle) {
        let cleanup = tmp.finish();
        return match cleanup {
            Ok(()) => Err(anyhow::Error::new(error).context("write temp bundle")),
            Err(cleanup) => Err(anyhow::Error::new(error)
                .context(format!("write temp bundle; cleanup failed: {cleanup}"))),
        };
    }
    if let Err(error) = tmp.verify() {
        let cleanup = tmp.finish();
        return match cleanup {
            Ok(()) => {
                Err(anyhow::Error::new(error).context("verify temp bundle custody before fetch"))
            }
            Err(cleanup) => Err(anyhow::Error::new(error).context(format!(
                "verify temp bundle custody before fetch; cleanup failed: {cleanup}"
            ))),
        };
    }
    let tmp_s = tmp.path().to_string_lossy().to_string();
    let staging_ref = format!("{target_ref}-staging");
    // The random generation makes this an isolated fetch destination. The final
    // ref is published separately with an all-zero old OID, making it create-only.
    let refspec = format!("refs/heads/{branch}:{staging_ref}");
    let ok = target.git_ok(&["fetch", &tmp_s, &refspec]);
    if !ok {
        match tmp.finish() {
            Ok(()) => anyhow::bail!("git fetch from bundle failed"),
            Err(cleanup) => {
                anyhow::bail!("git fetch from bundle failed; cleanup failed: {cleanup}")
            }
        };
    }
    let fetch_head = match CliGit.rev_parse(target, &staging_ref) {
        Ok(oid) => oid,
        Err(error) => {
            if let Some(actual) = target.git_out(&["rev-parse", &staging_ref])
                && !target.git_ok(&["update-ref", "-d", &staging_ref, &actual])
            {
                tracing::warn!(ref_name = %staging_ref, "failed to remove unreadable bundle staging ref");
            }
            let cleanup = tmp.finish();
            return match cleanup {
                Ok(()) => Err(error.context("resolve fetched bundle tip")),
                Err(cleanup) => Err(error.context(format!(
                    "resolve fetched bundle tip; cleanup failed: {cleanup}"
                ))),
            };
        }
    };
    if fetch_head != expected {
        if !target.git_ok(&["update-ref", "-d", &staging_ref, &fetch_head]) {
            tracing::warn!(ref_name = %staging_ref, "failed to remove mismatched bundle staging ref");
        }
        tmp.finish().context("cleanup mismatched fetched bundle")?;
        anyhow::bail!("bundled tip differs from the branch-host tip; retry");
    }
    let zero_oid = "0".repeat(expected.len());
    if !target.git_ok(&["update-ref", target_ref, expected, &zero_oid]) {
        if !target.git_ok(&["update-ref", "-d", &staging_ref, expected]) {
            tracing::warn!(ref_name = %staging_ref, "failed to remove staging ref after create-only publication failure");
        }
        tmp.finish()
            .context("cleanup after create-only ref publication failure")?;
        anyhow::bail!("create-only merge-queue ingest ref publication failed");
    }
    if !target.git_ok(&["update-ref", "-d", &staging_ref, expected]) {
        tracing::warn!(ref_name = %staging_ref, "failed to remove merge-queue bundle staging ref");
    }
    if let Err(error) = tmp.finish() {
        if !target.git_ok(&["update-ref", "-d", target_ref, expected]) {
            tracing::warn!(ref_name = %target_ref, "failed to remove ingest ref after bundle cleanup failure");
        }
        return Err(error).context("cleanup fetched bundle");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use thegn_core::remote::{GitLoc, SshTarget};

    fn remote(host: &str) -> GitLoc {
        GitLoc::Remote {
            ssh: SshTarget::plain(host.to_string(), 22, false),
            path: "/wt".to_string(),
        }
    }

    #[test]
    fn mq_ref_is_namespaced() {
        let first = generation_ref(&GitLoc::Local(PathBuf::from("/src")), "feat/x").unwrap();
        let second = generation_ref(&GitLoc::Local(PathBuf::from("/src")), "feat/x").unwrap();
        assert!(first.starts_with("refs/thegn/mq/"));
        assert_ne!(first, second);
        let authority_a = generation_ref(&remote("a"), "feat").unwrap();
        let authority_b = generation_ref(&remote("b"), "feat").unwrap();
        assert_ne!(authority_a, authority_b);
    }

    #[test]
    fn empty_stale_ref_listing_is_successful_and_empty() {
        assert_eq!(parse_ref_listing(b"").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn same_store_needs_no_ingest() {
        let local = GitLoc::Local(PathBuf::from("/repo"));
        assert!(!needs_ingest(&local, &GitLoc::Local(PathBuf::from("/wt"))));
        assert!(!needs_ingest(&remote("box"), &remote("box")));
    }

    #[test]
    fn different_host_needs_ingest() {
        let local = GitLoc::Local(PathBuf::from("/repo"));
        assert!(needs_ingest(&local, &remote("box")));
        assert!(needs_ingest(&remote("a"), &remote("b")));
    }

    #[test]
    fn ensure_tip_returns_heads_ref_when_same_store() {
        let local = GitLoc::Local(PathBuf::from("/repo"));
        let r = ingest_tip(&local, "feat", &GitLoc::Local(PathBuf::from("/wt"))).unwrap();
        assert_eq!(r.ref_name, "refs/heads/feat");
        assert!(r.oid.is_none());
    }

    #[expect(clippy::disallowed_methods)] // test-only git plumbing, never on the loop
    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = thegn_core::util::git_cmd(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
    }

    struct Stores {
        _root: tempfile::TempDir,
        target: PathBuf,
        source: PathBuf,
        oid: String,
    }

    fn stores(source_name: &str) -> Stores {
        use thegn_core::util;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let source = root.path().join(source_name);
        std::fs::create_dir_all(&target).unwrap();
        git(&target, &["init", "-q", "-b", "main"]);
        git(&target, &["config", "user.name", "t"]);
        git(&target, &["config", "user.email", "t@e"]);
        git(&target, &["config", "commit.gpgsign", "false"]);
        std::fs::write(target.join("base.txt"), "base\n").unwrap();
        git(&target, &["add", "-A"]);
        git(&target, &["commit", "-q", "-m", "base"]);
        git(
            root.path(),
            &[
                "clone",
                "-q",
                &target.to_string_lossy(),
                &source.to_string_lossy(),
            ],
        );
        git(&source, &["config", "user.name", "t"]);
        git(&source, &["config", "user.email", "t@e"]);
        git(&source, &["config", "commit.gpgsign", "false"]);
        git(&source, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(source.join("feat.txt"), "feature\n").unwrap();
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-q", "-m", "feature"]);
        let oid = util::git_out(&source, &["rev-parse", "refs/heads/feat"]).unwrap();
        Stores {
            _root: root,
            target,
            source,
            oid,
        }
    }

    fn ingest(fixture: &Stores, expected: Option<&str>) -> Result<IngestedTip> {
        ingest_tip_with_expected(
            &GitLoc::Local(fixture.target.clone()),
            "feat",
            &GitLoc::Local(fixture.source.clone()),
            expected,
        )
    }

    fn list_refs(target: &std::path::Path) -> Vec<String> {
        thegn_core::util::git_out_allow_empty(
            target,
            &["for-each-ref", "--format=%(refname)", "refs/thegn/mq/"],
        )
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn bundle_and_fetch_moves_a_tip_between_stores() {
        use thegn_core::util;
        let fixture = stores("source");
        let temp_root = fixture._root.path().join("bundle-temp");
        std::fs::create_dir(&temp_root).unwrap();
        let generated = generation_ref(&GitLoc::Local(fixture.source.clone()), "feat").unwrap();
        assert!(
            fetch_bundle_with_root(
                &GitLoc::Local(fixture.target.clone()),
                "feat",
                b"not a bundle",
                &generated,
                &fixture.oid,
                Some(&temp_root),
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
        assert!(!util::git_ok(
            &fixture.target,
            &["cat-file", "-e", &fixture.oid]
        ));
        let bundle = bundle_bytes(&GitLoc::Local(fixture.source.clone()), "feat").unwrap();
        fetch_bundle_with_root(
            &GitLoc::Local(fixture.target.clone()),
            "feat",
            &bundle,
            &generated,
            &fixture.oid,
            Some(&temp_root),
        )
        .unwrap();
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
        assert_eq!(
            util::git_out(&fixture.target, &["rev-parse", &generated]).as_deref(),
            Some(fixture.oid.as_str())
        );
        let ingested = ingest(&fixture, None).unwrap();
        assert_eq!(ingested.oid.as_deref(), Some(fixture.oid.as_str()));
        assert_eq!(
            util::git_out(&fixture.target, &["rev-parse", &ingested.ref_name]).as_deref(),
            Some(fixture.oid.as_str())
        );
        drop(ingested);
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn ingest_ref_is_row_scoped_and_unique_per_generation() {
        let first = stores("source-one");
        let other = stores("source-two");
        let one = ingest(&first, None).unwrap();
        let one_ref = one.ref_name.clone();
        let two = ingest(&other, None).unwrap();
        let two_ref = two.ref_name.clone();
        assert_ne!(one_ref, two_ref);
        drop(one);
        drop(two);
        let again = ingest(&first, None).unwrap();
        assert_ne!(one_ref, again.ref_name);
    }

    #[test]
    fn ingest_force_pushed_tip_ingests_as_new_generation() {
        use thegn_core::util;
        let fixture = stores("source");
        let first = ingest(&fixture, None).unwrap();
        let old_ref = first.ref_name.clone();
        std::mem::forget(first);
        git(
            &fixture.source,
            &["commit", "--amend", "-m", "rewritten feature"],
        );
        let new_oid = util::git_out(&fixture.source, &["rev-parse", "refs/heads/feat"]).unwrap();
        let next = ingest(&fixture, None).unwrap();
        assert_ne!(old_ref, next.ref_name);
        assert_eq!(
            util::git_out(&fixture.target, &["rev-parse", &next.ref_name]).as_deref(),
            Some(new_oid.as_str())
        );
        assert!(!list_refs(&fixture.target).contains(&old_ref));
    }

    #[test]
    fn ingest_ref_is_deleted_when_the_guard_drops() {
        let fixture = stores("source");
        let ingested = ingest(&fixture, None).unwrap();
        assert_eq!(list_refs(&fixture.target), vec![ingested.ref_name.clone()]);
        drop(ingested);
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn ingest_prunes_legacy_branch_ref() {
        let fixture = stores("source");
        git(
            &fixture.target,
            &["update-ref", "refs/thegn/mq/feat", &fixture.oid],
        );
        let ingested = ingest(&fixture, None).unwrap();
        assert!(!list_refs(&fixture.target).contains(&"refs/thegn/mq/feat".to_string()));
        drop(ingested);
    }

    #[test]
    fn ingest_refuses_bundle_that_differs_from_expected_tip() {
        let fixture = stores("source");
        let result = ingest(&fixture, Some("0000000000000000000000000000000000000000"));
        assert!(result.is_err());
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn classify_source_table() {
        let cases = [
            ("abc", Ok("abc".to_string()), SourceCheck::Unchanged),
            (
                "abc",
                Ok("def".to_string()),
                SourceCheck::Moved {
                    actual: "def".to_string(),
                },
            ),
            (
                "abc",
                Err(anyhow::anyhow!("offline")),
                SourceCheck::Unreachable("offline".to_string()),
            ),
        ];
        for (ingested, live, expected) in cases {
            assert_eq!(classify_source(ingested, live), expected);
        }
    }

    #[test]
    fn verify_source_unchanged_reads_the_live_branch_tip() {
        let fixture = stores("source");
        assert_eq!(
            verify_source_unchanged(&GitLoc::Local(fixture.source.clone()), "feat", &fixture.oid),
            SourceCheck::Unchanged
        );
        git(
            &fixture.source,
            &["commit", "--amend", "-m", "moved source"],
        );
        let actual =
            thegn_core::util::git_out(&fixture.source, &["rev-parse", "refs/heads/feat"]).unwrap();
        assert_eq!(
            verify_source_unchanged(&GitLoc::Local(fixture.source.clone()), "feat", &fixture.oid),
            SourceCheck::Moved { actual }
        );
    }
}
