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
//! synthetic ref `refs/thegn/mq/<branch>`. That ref is what the fold merges.
//!
//! Runs off the event loop (from the merge-queue drain in `spawn_blocking` / the
//! CLI). It shells `git bundle`/`git fetch`, so the `.output()` sites carry the
//! host crate's off-loop `#[expect(clippy::disallowed_methods)]`.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use thegn_core::remote::GitLoc;
use thegn_core::util;

/// The synthetic ref a cross-host branch tip is fetched under in the target
/// store (kept out of `refs/heads/*` so it never shows up as a real branch).
pub fn mq_ref(branch: &str) -> String {
    format!("refs/thegn/mq/{branch}")
}

/// Ensure `branch`'s tip is resolvable in the target store, returning the ref
/// the fold should merge. For a branch that already shares the target's store
/// (local, or the same host as the target) that's `refs/heads/<branch>`. For a
/// branch on another host, bundle-fetch its tip into `refs/thegn/mq/<branch>`
/// and return that. Errors (host unreachable, fetch failed) bubble up so the
/// drain can defer the row with a clear reason rather than silently dropping it.
pub fn ensure_tip_in_target(
    target: &GitLoc,
    branch: &str,
    branch_loc: &GitLoc,
) -> Result<String> {
    if !needs_ingest(target, branch_loc) {
        return Ok(format!("refs/heads/{branch}"));
    }
    let bundle = bundle_bytes(branch_loc, branch)
        .with_context(|| format!("branch host unreachable while bundling {branch}"))?;
    fetch_bundle(target, branch, &bundle)
        .with_context(|| format!("fetching {branch} into the target store"))?;
    Ok(mq_ref(branch))
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

/// Materialize the bundle on the target host and fetch its branch ref into
/// `refs/thegn/mq/<branch>`. Under the co-location model the drain runs on the
/// target host, so `target` is `Local`; a remote target means the caller should
/// have dispatched to that host's drain daemon (see Milestone B), so we fail
/// loudly rather than trying to push a bundle the wrong way.
fn fetch_bundle(target: &GitLoc, branch: &str, bundle: &[u8]) -> Result<()> {
    if target.is_remote() {
        anyhow::bail!(
            "remote target store: run the drain on the target host (merge daemon), \
             not by pushing a bundle over ssh"
        );
    }
    let tmp = tmp_bundle_path();
    std::fs::write(&tmp, bundle).context("write temp bundle")?;
    let tmp_s = tmp.to_string_lossy().to_string();
    let refspec = format!("refs/heads/{branch}:{}", mq_ref(branch));
    // `git_ok` runs `.output()` inside thegn-core (off-loop by contract there).
    let ok = target.git_ok(&["fetch", &tmp_s, &refspec]);
    let _ = std::fs::remove_file(&tmp); // best-effort: temp bundle cleanup
    if !ok {
        anyhow::bail!("git fetch from bundle failed");
    }
    Ok(())
}

/// A unique local temp path for a streamed bundle (mirrors `integrate::tmp_path`:
/// `util::now()` is seconds-resolution, so a process-wide sequence disambiguates
/// two near-simultaneous ingests).
fn tmp_bundle_path() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("thegn-mq-{}-{}-{n}.bundle", std::process::id(), util::now()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::remote::{GitLoc, SshTarget};
    use std::path::PathBuf;

    fn remote(host: &str) -> GitLoc {
        GitLoc::Remote {
            ssh: SshTarget {
                host: host.to_string(),
                port: 22,
                forward_agent: false,
            },
            path: "/wt".to_string(),
        }
    }

    #[test]
    fn mq_ref_is_namespaced() {
        assert_eq!(mq_ref("feat/x"), "refs/thegn/mq/feat/x");
    }

    #[test]
    fn same_store_needs_no_ingest() {
        let local = GitLoc::Local(PathBuf::from("/repo"));
        // Two local locs share the executing host's store.
        assert!(!needs_ingest(&local, &GitLoc::Local(PathBuf::from("/wt"))));
        // Same remote host+port ⇒ same store.
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
        // No ingest for a same-store branch → plain heads ref, no I/O.
        let local = GitLoc::Local(PathBuf::from("/repo"));
        let r = ensure_tip_in_target(&local, "feat", &GitLoc::Local(PathBuf::from("/wt"))).unwrap();
        assert_eq!(r, "refs/heads/feat");
    }

    // ── real git-bundle transport across two separate object stores ──────────
    // Exercises bundle_bytes → fetch_bundle with Local locs (the ssh/provider
    // wrapping is covered by remote.rs's argv tests). Proves a tip that exists
    // ONLY in store B lands in store A under refs/thegn/mq/<branch>.
    #[expect(clippy::disallowed_methods)] // test-only git plumbing, never on the loop
    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = thegn_core::util::git_cmd(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
    }

    #[test]
    fn bundle_and_fetch_moves_a_tip_between_stores() {
        use thegn_core::util;
        let tag = std::process::id();
        let a = std::env::temp_dir().join(format!("thegn-mqr-a-{tag}"));
        let b = std::env::temp_dir().join(format!("thegn-mqr-b-{tag}"));
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
        std::fs::create_dir_all(&a).unwrap();

        // Store A (the target): main with a base commit.
        git(&a, &["init", "-q", "-b", "main"]);
        git(&a, &["config", "user.name", "t"]);
        git(&a, &["config", "user.email", "t@e"]);
        git(&a, &["config", "commit.gpgsign", "false"]);
        std::fs::write(a.join("base.txt"), "base\n").unwrap();
        git(&a, &["add", "-A"]);
        git(&a, &["commit", "-q", "-m", "c0"]);

        // Store B (the "other host"): a clone of A with a feature branch whose
        // tip does NOT exist in A yet.
        git(a.parent().unwrap(), &["clone", "-q", &a.to_string_lossy(), &b.to_string_lossy()]);
        git(&b, &["config", "user.name", "t"]);
        git(&b, &["config", "user.email", "t@e"]);
        git(&b, &["config", "commit.gpgsign", "false"]);
        git(&b, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(b.join("a.txt"), "a\n").unwrap();
        git(&b, &["add", "-A"]);
        git(&b, &["commit", "-q", "-m", "feat work"]);
        let feat_oid = util::git_out(&b, &["rev-parse", "refs/heads/feat"]).unwrap();

        // The tip's object is absent from A before ingest.
        assert!(!util::git_ok(&a, &["cat-file", "-e", &feat_oid]));

        // Bundle on B, fetch into A — the real transport.
        let bytes = bundle_bytes(&GitLoc::Local(b.clone()), "feat").unwrap();
        assert!(!bytes.is_empty());
        fetch_bundle(&GitLoc::Local(a.clone()), "feat", &bytes).unwrap();

        // A now resolves the synthetic ref to feat's tip and can reach it.
        assert_eq!(
            util::git_out(&a, &["rev-parse", "refs/thegn/mq/feat"]).as_deref(),
            Some(feat_oid.as_str())
        );

        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }
}
