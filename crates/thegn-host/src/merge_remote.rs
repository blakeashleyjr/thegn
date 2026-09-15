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
use std::io;
use std::path::{Path, PathBuf};
use thegn_core::remote::GitLoc;

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
pub fn ensure_tip_in_target(target: &GitLoc, branch: &str, branch_loc: &GitLoc) -> Result<String> {
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
    fetch_bundle_with_root(target, branch, bundle, None)
}

fn fetch_bundle_with_root(
    target: &GitLoc,
    branch: &str,
    bundle: &[u8],
    temp_root: Option<&Path>,
) -> Result<()> {
    if target.is_remote() {
        anyhow::bail!(
            "remote target store: run the drain on the target host (merge daemon), \
             not by pushing a bundle over ssh"
        );
    }
    let mut tmp = match temp_root {
        Some(root) => BundleTemp::new_in(root, false),
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
    let refspec = format!("refs/heads/{branch}:{}", mq_ref(branch));
    // `git_ok` runs `.output()` inside thegn-core (off-loop by contract there).
    let ok = target.git_ok(&["fetch", &tmp_s, &refspec]);
    if !ok {
        return match tmp.finish() {
            Ok(()) => anyhow::bail!("git fetch from bundle failed"),
            Err(cleanup) => {
                anyhow::bail!("git fetch from bundle failed; cleanup failed: {cleanup}")
            }
        };
    }
    tmp.finish().context("cleanup fetched bundle")
}

/// Own a private directory and its exclusively-created bundle leaf until the
/// fetch has consumed it. Drop only removes identities that still match the
/// retained handles; a replacement is preserved for inspection. Unix uses the
/// descriptor-relative gate seam. Windows keeps the fetch path available with
/// CREATE_NEW, delete-sharing, and final reparse-point refusal.
struct BundleTemp {
    parent_path: PathBuf,
    path: PathBuf,
    #[cfg(test)]
    fail_write: bool,
    #[cfg(test)]
    fail_flush: bool,
    #[cfg(unix)]
    parent: Option<crate::platform::gate_path::Directory>,
    #[cfg(unix)]
    file: Option<crate::platform::gate_path::Regular>,
    #[cfg(windows)]
    parent: Option<std::fs::File>,
    #[cfg(windows)]
    file: Option<std::fs::File>,
}

impl BundleTemp {
    fn new() -> io::Result<Self> {
        Self::new_in(&std::env::temp_dir(), false)
    }

    #[cfg(all(test, unix))]
    fn new_for_test(root: &Path, precreate_leaf: bool) -> io::Result<Self> {
        Self::new_in(root, precreate_leaf)
    }

    fn new_in(root: &Path, precreate_leaf: bool) -> io::Result<Self> {
        #[cfg(not(test))]
        let _ = precreate_leaf;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut builder = tempfile::Builder::new();
            builder
                .prefix("thegn-mq-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .disable_cleanup(true);
            let temp = builder.tempdir_in(std::fs::canonicalize(root)?)?;
            let temp_path = temp.path().to_owned();
            #[cfg(test)]
            if precreate_leaf {
                if let Err(error) = std::fs::write(temp_path.join("bundle"), b"foreign fixture") {
                    return Err(match std::fs::remove_dir(&temp_path) {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "seed hostile bundle leaf failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            }
            let parent = match crate::platform::gate_path::Directory::open(
                &temp_path,
                Some(&temp_path),
            ) {
                Ok(parent) => parent,
                Err(error) => {
                    return Err(match std::fs::remove_dir(&temp_path) {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "open private bundle parent failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            };
            let parent_path = temp.keep();
            let path = parent_path.join("bundle");
            let mut custody = Self {
                parent_path,
                path,
                parent: Some(parent),
                file: None,
                #[cfg(test)]
                fail_write: false,
                #[cfg(test)]
                fail_flush: false,
            };
            custody.file = match crate::platform::gate_path::Regular::create_exclusive_at(
                custody.parent.as_ref().expect("bundle parent retained"),
                &custody.path,
            ) {
                Ok(file) => Some(file),
                Err(error) => {
                    return Err(match custody.cleanup() {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "create private bundle leaf failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            };
            return Ok(custody);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            // CreateDirectoryW receives a SECURITY_ATTRIBUTES containing the
            // current process's user SID, so the private parent has its
            // owner-only DACL at creation. The follow-up ACL readback and
            // retained no-follow handle still guard the seam before fetch.
            let temp_path = crate::platform::create_private_directory(root)?;
            #[cfg(test)]
            if precreate_leaf {
                if let Err(error) = std::fs::write(temp_path.join("bundle"), b"foreign fixture") {
                    return Err(match std::fs::remove_dir(&temp_path) {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "seed hostile bundle leaf failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            }
            if let Err(error) = crate::platform::secure_private_directory(&temp_path) {
                return Err(match std::fs::remove_dir(&temp_path) {
                    Ok(()) => error,
                    Err(cleanup) => io::Error::other(format!(
                        "secure private bundle parent failed: {error}; nonrecursive cleanup failed: {cleanup}"
                    )),
                });
            }
            let parent = match crate::platform::open_directory_nofollow(&temp_path) {
                Ok(parent) => parent,
                Err(error) => {
                    return Err(match std::fs::remove_dir(&temp_path) {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "open private bundle parent failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            };
            let parent_path = temp_path;
            let path = parent_path.join("bundle");
            let mut custody = Self {
                parent_path,
                path,
                parent: Some(parent),
                file: None,
                #[cfg(test)]
                fail_write: false,
                #[cfg(test)]
                fail_flush: false,
            };
            custody.file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&custody.path)
            {
                Ok(file) => Some(file),
                Err(error) => {
                    return Err(match custody.cleanup() {
                        Ok(()) => error,
                        Err(cleanup) => io::Error::other(format!(
                            "create private bundle leaf failed: {error}; nonrecursive cleanup failed: {cleanup}"
                        )),
                    });
                }
            };
            return Ok(custody);
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private bundle custody is unsupported on this platform",
            ))
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.parent
                .as_ref()
                .expect("bundle parent retained")
                .verify()?;
            self.file.as_ref().expect("bundle file retained").verify()
        }
        #[cfg(windows)]
        {
            self.verify_windows_parent()?;
            let current = crate::platform::open_read_nofollow(&self.path)?;
            if !windows_same_identity(self.file.as_ref().expect("bundle file retained"), &current) {
                return Err(io::Error::other("bundle file identity changed"));
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private bundle custody is unsupported on this platform",
            ))
        }
    }

    fn finish(mut self) -> io::Result<()> {
        self.cleanup()
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            #[cfg(test)]
            if self.fail_write {
                return Err(io::Error::other("injected bundle write failure"));
            }
            let file = self.file.as_mut().expect("bundle file retained");
            file.write_all(bytes)?;
            #[cfg(test)]
            if self.fail_flush {
                return Err(io::Error::other("injected bundle flush failure"));
            }
            file.flush()?;
            file.verify()
        }
        #[cfg(windows)]
        {
            use std::io::Write;
            #[cfg(test)]
            if self.fail_write {
                return Err(io::Error::other("injected bundle write failure"));
            }
            self.verify_windows_parent()?;
            let file = self.file.as_mut().expect("bundle file retained");
            file.write_all(bytes)?;
            #[cfg(test)]
            if self.fail_flush {
                return Err(io::Error::other("injected bundle flush failure"));
            }
            file.flush()?;
            self.verify()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = bytes;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private bundle custody is unsupported on this platform",
            ))
        }
    }

    #[cfg(unix)]
    fn cleanup(&mut self) -> io::Result<()> {
        let mut failure = None;
        if let Some(file) = self.file.take() {
            if let Err(error) = file.remove_verified() {
                failure = Some(error);
            }
        }
        if let Some(parent) = self.parent.take() {
            if parent.verify().is_ok() {
                if let Err(error) = std::fs::remove_dir(&self.parent_path) {
                    failure.get_or_insert(error);
                }
            } else if failure.is_none() {
                failure = Some(io::Error::other("bundle parent identity changed"));
            }
        }
        failure.map_or(Ok(()), Err)
    }

    #[cfg(windows)]
    fn cleanup(&mut self) -> io::Result<()> {
        let had_file = self.file.is_some();
        let same_identity = self.file.as_ref().and_then(|owned| {
            let current = crate::platform::open_read_nofollow(&self.path).ok()?;
            Some(windows_same_identity(owned, &current))
        }) == Some(true);
        let parent_identity = self.parent.as_ref().and_then(|owned| {
            let current = crate::platform::open_directory_nofollow(&self.parent_path).ok()?;
            Some(windows_same_identity(owned, &current))
        }) == Some(true);
        let path_missing = matches!(
            std::fs::symlink_metadata(&self.path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        );
        let mut failure = None;
        if same_identity && parent_identity {
            if let Err(error) = std::fs::remove_file(&self.path) {
                failure = Some(error);
            }
        } else if had_file {
            failure = Some(io::Error::other("bundle identity changed before cleanup"));
        }
        drop(self.file.take());
        drop(self.parent.take());
        if same_identity && parent_identity {
            if let Err(error) = std::fs::remove_dir(&self.parent_path) {
                failure.get_or_insert(error);
            }
        } else if !had_file && parent_identity && path_missing {
            if let Err(error) = std::fs::remove_dir(&self.parent_path) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    #[cfg(not(any(unix, windows)))]
    fn cleanup(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private bundle custody is unsupported on this platform",
        ))
    }

    #[cfg(windows)]
    fn verify_windows_parent(&self) -> io::Result<()> {
        let current = crate::platform::open_directory_nofollow(&self.parent_path)?;
        if !windows_same_identity(
            self.parent.as_ref().expect("bundle parent retained"),
            &current,
        ) {
            return Err(io::Error::other("bundle parent identity changed"));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn windows_same_identity(a: &std::fs::File, b: &std::fs::File) -> bool {
    let (Ok(a), Ok(b)) = (
        crate::platform::handle_identity(a),
        crate::platform::handle_identity(b),
    ) else {
        return false;
    };
    a == b
}

impl Drop for BundleTemp {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = self.cleanup();
        #[cfg(windows)]
        let _ = self.cleanup();
    }
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
        let fixture = tempfile::tempdir().unwrap();
        let temp_root = fixture.path().join("bundle-temp");
        std::fs::create_dir(&temp_root).unwrap();
        let a = fixture.path().join("store-a");
        let b = fixture.path().join("store-b");
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
        git(
            a.parent().unwrap(),
            &["clone", "-q", &a.to_string_lossy(), &b.to_string_lossy()],
        );
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
        assert!(
            fetch_bundle_with_root(
                &GitLoc::Local(a.clone()),
                "bad",
                b"not a bundle",
                Some(&temp_root)
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
        fetch_bundle_with_root(&GitLoc::Local(a.clone()), "feat", &bytes, Some(&temp_root))
            .unwrap();
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);

        // A now resolves the synthetic ref to feat's tip and can reach it.
        assert_eq!(
            util::git_out(&a, &["rev-parse", "refs/thegn/mq/feat"]).as_deref(),
            Some(feat_oid.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn bundle_cleanup_preserves_an_observed_replacement() {
        let temp = BundleTemp::new().unwrap();
        let replacement = temp.path().with_file_name("replacement");
        std::fs::rename(temp.path(), &replacement).unwrap();
        std::fs::write(temp.path(), b"foreign fixture").unwrap();
        let parent = temp.parent_path.clone();
        drop(temp);
        assert_eq!(
            std::fs::read(parent.join("bundle")).unwrap(),
            b"foreign fixture"
        );
        std::fs::remove_file(parent.join("bundle")).unwrap();
        std::fs::remove_file(replacement).unwrap();
        std::fs::remove_dir(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bundle_custody_rejects_special_replacements_and_keeps_modes() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let initial = BundleTemp::new().unwrap();
        assert_eq!(
            std::fs::metadata(initial.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&initial.parent_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(initial);

        for kind in ["symlink", "fifo", "hardlink", "directory"] {
            let temp = BundleTemp::new().unwrap();
            let parent = temp.parent_path.clone();
            let path = temp.path().to_path_buf();
            let retained = parent.join("retained");
            std::fs::rename(&path, &retained).unwrap();
            match kind {
                "symlink" => {
                    std::fs::write(parent.join("target"), b"foreign").unwrap();
                    std::os::unix::fs::symlink(parent.join("target"), &path).unwrap();
                }
                "fifo" => {
                    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                "hardlink" => {
                    std::fs::write(&retained, b"foreign").unwrap();
                    std::fs::hard_link(&retained, &path).unwrap();
                }
                "directory" => std::fs::create_dir(&path).unwrap(),
                _ => unreachable!(),
            }
            drop(temp);
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            match kind {
                "symlink" => assert!(metadata.file_type().is_symlink()),
                "fifo" => assert!(metadata.file_type().is_fifo()),
                "hardlink" => assert_eq!(metadata.nlink(), 2),
                "directory" => assert!(metadata.is_dir()),
                _ => unreachable!(),
            }
            if kind == "directory" {
                std::fs::remove_dir(&path).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            std::fs::remove_file(&retained).unwrap();
            if kind == "symlink" {
                std::fs::remove_file(parent.join("target")).unwrap();
            }
            std::fs::remove_dir(parent).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn bundle_write_revalidation_failure_preserves_replacement() {
        let mut temp = BundleTemp::new().unwrap();
        let parent = temp.parent_path.clone();
        let path = temp.path().to_path_buf();
        std::fs::rename(&path, parent.join("retained")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(temp.write(b"must not publish").is_err());
        drop(temp);
        assert!(path.is_dir());
        std::fs::remove_dir(path).unwrap();
        std::fs::remove_file(parent.join("retained")).unwrap();
        std::fs::remove_dir(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bundle_creation_collision_preserves_preexisting_leaf() {
        let root = tempfile::tempdir().unwrap();
        assert!(BundleTemp::new_for_test(root.path(), true).is_err());
        let children: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        assert_eq!(children.len(), 1);
        let parent = children[0].as_ref().unwrap().path();
        assert_eq!(
            std::fs::read(parent.join("bundle")).unwrap(),
            b"foreign fixture"
        );
        std::fs::remove_file(parent.join("bundle")).unwrap();
        std::fs::remove_dir(parent).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn bundle_write_and_flush_failures_finish_without_recursive_cleanup() {
        for (fail_write, fail_flush) in [(true, false), (false, true)] {
            let mut temp = BundleTemp::new().unwrap();
            #[cfg(test)]
            {
                temp.fail_write = fail_write;
                temp.fail_flush = fail_flush;
            }
            let parent = temp.parent_path.clone();
            assert!(temp.write(b"injected failure").is_err());
            temp.finish().unwrap();
            assert!(!parent.exists());
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn bundle_unwind_after_injected_write_failure_cleans_owned_fixture() {
        use std::sync::{Arc, Mutex};
        let parent_slot = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&parent_slot);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut temp = BundleTemp::new().unwrap();
            *slot.lock().unwrap() = Some(temp.parent_path.clone());
            #[cfg(test)]
            {
                temp.fail_write = true;
            }
            let _ = temp.write(b"injected failure");
            panic!("injected bundle unwind");
        }));
        assert!(result.is_err());
        let parent = parent_slot.lock().unwrap().take().unwrap();
        assert!(!parent.exists());
    }
}
