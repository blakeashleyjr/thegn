//! Dockerfile image builds for OCI sandboxes — split out of the (ratchet-capped)
//! `sandbox.rs`. A devcontainer `build` block (or any future `[sandbox] build`)
//! becomes a synchronous `<runtime> build -t <tag> …` run *before* the container
//! is created, driven through the same `oci_prefix` as every other runtime call
//! so it targets the right daemon (rootful podman, an `oci_host` remote, …).
//!
//! The tag binds the build declaration, safely-read Dockerfile bytes, and a
//! bounded digest of the build context. `.dockerignore` is not interpreted, so
//! ignored files can cause an extra rebuild. The argv + tag
//! construction are unit-tested here; only [`build_image`] touches a
//! subprocess (a `cov_ignore` seam exercised by `test/smoke.sh`).

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::sandbox::{SandboxSpec, oci_prefix};

/// A Dockerfile build request. Paths are **already resolved to absolute** by the
/// caller (e.g. the devcontainer overlay resolves them against the
/// `.devcontainer` directory), so this module treats them verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxBuild {
    /// Absolute path to the Dockerfile.
    pub dockerfile: String,
    /// Absolute path to the build context directory.
    pub context: String,
    /// `--build-arg` values (stable order for a deterministic argv/tag).
    pub args: BTreeMap<String, String>,
    /// `--target` stage, if any.
    pub target: Option<String>,
}

/// Hash a Dockerfile without following a symlink or blocking on a FIFO.
pub fn dockerfile_sha256(path: &Path) -> io::Result<String> {
    let mut file = crate::fsperm::open_regular_file_nofollow(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Maximum build-context entries visited when computing an image tag.
pub const CONTEXT_DIGEST_MAX_ENTRIES: usize = 2048;
/// Maximum regular-file bytes read when computing an image tag.
pub const CONTEXT_DIGEST_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Why a build context could not be digested safely within its bounds.
#[derive(Debug)]
pub enum ContextDigestError {
    TooManyEntries { limit: usize },
    TooManyBytes { limit: u64 },
    Io { path: PathBuf, error: io::Error },
}

impl std::fmt::Display for ContextDigestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyEntries { limit } => write!(f, "more than {limit} entries"),
            Self::TooManyBytes { limit } => write!(f, "more than {limit} bytes of files"),
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for ContextDigestError {}

struct ContextDigestState {
    hasher: Sha256,
    entries: usize,
    bytes: u64,
    max_entries: usize,
    max_bytes: u64,
}

fn check_context_file_size(
    path: &Path,
    initial_len: u64,
    bytes_read: u64,
) -> Result<(), ContextDigestError> {
    if bytes_read != initial_len {
        return Err(ContextDigestError::Io {
            path: path.to_path_buf(),
            error: io::Error::new(
                io::ErrorKind::InvalidData,
                "file changed while computing the build-context digest",
            ),
        });
    }
    Ok(())
}

/// Hash a bounded build context without following symlinks. `.dockerignore` is
/// not interpreted, so an ignored file may cause an extra rebuild.
pub fn context_sha256(root: &Path) -> Result<String, ContextDigestError> {
    context_sha256_bounded(root, CONTEXT_DIGEST_MAX_ENTRIES, CONTEXT_DIGEST_MAX_BYTES)
}

pub(crate) fn context_sha256_bounded(
    root: &Path,
    max_entries: usize,
    max_bytes: u64,
) -> Result<String, ContextDigestError> {
    const MAX_DEPTH: usize = 32;
    fn walk(
        dir: &Path,
        relative: &Path,
        depth: usize,
        state: &mut ContextDigestState,
    ) -> Result<(), ContextDigestError> {
        if depth > MAX_DEPTH {
            return Err(ContextDigestError::TooManyEntries {
                limit: state.max_entries,
            });
        }
        let entries = std::fs::read_dir(dir).map_err(|error| ContextDigestError::Io {
            path: dir.to_path_buf(),
            error,
        })?;
        let mut sorted_entries = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| ContextDigestError::Io {
                path: dir.to_path_buf(),
                error,
            })?;
            let name = entry.file_name();
            if depth == 0 && name == ".git" {
                continue;
            }
            state.entries += 1;
            if state.entries > state.max_entries {
                return Err(ContextDigestError::TooManyEntries {
                    limit: state.max_entries,
                });
            }
            sorted_entries.push((name, entry.path()));
        }
        sorted_entries
            .sort_by(|left, right| left.0.as_encoded_bytes().cmp(right.0.as_encoded_bytes()));

        for (name, path) in sorted_entries {
            let relative_path = relative.join(name);
            let metadata =
                std::fs::symlink_metadata(&path).map_err(|error| ContextDigestError::Io {
                    path: path.clone(),
                    error,
                })?;
            let relative_bytes = relative_path.as_os_str().as_encoded_bytes();
            if metadata.file_type().is_dir() {
                state.hasher.update([b'd']);
                hash_field(&mut state.hasher, relative_bytes);
                walk(&path, &relative_path, depth + 1, state)?;
            } else if metadata.file_type().is_file() {
                state.hasher.update([b'f']);
                hash_field(&mut state.hasher, relative_bytes);
                let remaining = state.max_bytes.saturating_sub(state.bytes);
                let mut file =
                    crate::fsperm::open_regular_file_nofollow(&path).map_err(|error| {
                        ContextDigestError::Io {
                            path: path.clone(),
                            error,
                        }
                    })?;
                let initial_len = file
                    .metadata()
                    .map_err(|error| ContextDigestError::Io {
                        path: path.clone(),
                        error,
                    })?
                    .len();
                let mut file_hasher = Sha256::new();
                let mut bounded = (&mut file).take(remaining.saturating_add(1));
                let read = io::copy(&mut bounded, &mut file_hasher).map_err(|error| {
                    ContextDigestError::Io {
                        path: path.clone(),
                        error,
                    }
                })?;
                if read > remaining {
                    return Err(ContextDigestError::TooManyBytes {
                        limit: state.max_bytes,
                    });
                }
                check_context_file_size(&path, initial_len, read)?;
                state.bytes += read;
                state.hasher.update(read.to_be_bytes());
                hash_field(
                    &mut state.hasher,
                    format!("{:x}", file_hasher.finalize()).as_bytes(),
                );
            } else if metadata.file_type().is_symlink() {
                state.hasher.update([b'l']);
                hash_field(&mut state.hasher, relative_bytes);
                let target = std::fs::read_link(&path).map_err(|error| ContextDigestError::Io {
                    path: path.clone(),
                    error,
                })?;
                hash_field(&mut state.hasher, target.as_os_str().as_encoded_bytes());
            } else {
                state.hasher.update([b'o']);
                hash_field(&mut state.hasher, relative_bytes);
            }
        }
        Ok(())
    }

    let mut state = ContextDigestState {
        hasher: Sha256::new(),
        entries: 0,
        bytes: 0,
        max_entries,
        max_bytes,
    };
    state.hasher.update(b"thegn.dc-build-context.v1\0");
    let root_metadata =
        std::fs::symlink_metadata(root).map_err(|error| ContextDigestError::Io {
            path: root.to_path_buf(),
            error,
        })?;
    if !root_metadata.file_type().is_dir() {
        return Err(ContextDigestError::Io {
            path: root.to_path_buf(),
            error: io::Error::new(
                io::ErrorKind::InvalidInput,
                "build context is not a directory",
            ),
        });
    }
    walk(root, Path::new(""), 0, &mut state)?;
    Ok(format!("{:x}", state.hasher.finalize()))
}

/// A stable, local image tag for a build. The versioned SHA-256 covers the
/// build declaration, Dockerfile bytes, and bounded build-context digest when
/// safely available. If both digests are absent, the tag covers only the build
/// declaration. The v3 domain causes one rebuild after upgrading from v2.
/// `basename` is a human hint (e.g. the repo name); it is slugified.
pub fn content_tag(
    basename: &str,
    b: &SandboxBuild,
    dockerfile_sha256: Option<&str>,
    context_sha256: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"thegn.dc-build-tag.v3\0");
    hash_field(&mut hasher, b.dockerfile.as_bytes());
    hash_field(&mut hasher, b.context.as_bytes());
    hasher.update((b.args.len() as u64).to_be_bytes());
    for (key, value) in &b.args {
        hash_field(&mut hasher, key.as_bytes());
        hash_field(&mut hasher, value.as_bytes());
    }
    hash_optional_field(&mut hasher, b.target.as_deref());
    hash_optional_field(&mut hasher, dockerfile_sha256);
    hash_optional_field(&mut hasher, context_sha256);
    let digest = hasher.finalize();
    let slug: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "img" } else { slug };
    format!("thegn-dc-{slug}:{digest:x}")
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn hash_optional_field(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update([1]);
            hash_field(hasher, value.as_bytes());
        }
        None => hasher.update([0]),
    }
}

/// The `build …` subcommand argv (the runtime binary + any daemon flags come
/// from `oci_prefix`, so this starts at `build`). Pure.
pub fn build_argv(tag: &str, b: &SandboxBuild) -> Vec<String> {
    let mut argv = vec![
        "build".into(),
        "-t".into(),
        tag.to_string(),
        "-f".into(),
        b.dockerfile.clone(),
    ];
    for (k, v) in &b.args {
        argv.push("--build-arg".into());
        argv.push(format!("{k}={v}"));
    }
    if let Some(t) = &b.target {
        argv.push("--target".into());
        argv.push(t.clone());
    }
    argv.push(b.context.clone());
    argv
}

/// Build `spec`'s image synchronously (no-op when `spec.build` is `None` or the
/// backend isn't OCI). The tag is `spec.image` — the overlay sets it to
/// [`content_tag`] so the subsequent `image exists` probe hits the freshly-built
/// image instead of attempting a registry pull. Subprocess seam (cov_ignore).
pub fn build_image(spec: &SandboxSpec) -> anyhow::Result<()> {
    let Some(build) = &spec.build else {
        return Ok(());
    };
    if !spec.backend.is_oci() {
        return Ok(());
    }
    let tag = spec
        .image
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("devcontainer build: no image tag on spec"))?;
    let rt = spec.backend.binary();
    let mut argv = oci_prefix(spec);
    argv.extend(build_argv(tag, build));
    let argv = spec.placement.control_argv(&argv);
    crate::progress::emit(crate::progress::SandboxPhase::ImageBuild {
        tag: tag.to_string(),
    });
    // Run via `output()` — it drains stdout+stderr, so a verbose build doesn't
    // deadlock a fixed-size pipe (which is why we don't reuse the timeout-based
    // `run_control_owned` here). Builds are legitimately slow; no artificial cap.
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("{rt} build spawn failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("build failed");
        crate::progress::emit(crate::progress::SandboxPhase::PhaseFailed {
            err: last.to_string(),
        });
        anyhow::bail!("{rt} build of {tag} failed: {last}");
    }
    crate::progress::emit(crate::progress::SandboxPhase::PhaseDone);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build() -> SandboxBuild {
        SandboxBuild {
            dockerfile: "/w/.devcontainer/Dockerfile".into(),
            context: "/w/.devcontainer".into(),
            args: BTreeMap::from([("VARIANT".into(), "20".into()), ("A".into(), "b".into())]),
            target: Some("dev".into()),
        }
    }

    #[test]
    fn argv_is_ordered_and_complete() {
        // No leading binary — `oci_prefix` supplies it (a doubled binary is the
        // bug this guards against).
        let argv = build_argv("tag:1", &build());
        assert_eq!(
            argv,
            vec![
                "build",
                "-t",
                "tag:1",
                "-f",
                "/w/.devcontainer/Dockerfile",
                // BTreeMap order: A before VARIANT
                "--build-arg",
                "A=b",
                "--build-arg",
                "VARIANT=20",
                "--target",
                "dev",
                "/w/.devcontainer",
            ]
        );
    }

    #[test]
    fn argv_without_args_or_target() {
        let b = SandboxBuild {
            dockerfile: "/w/Dockerfile".into(),
            context: "/w".into(),
            ..Default::default()
        };
        let argv = build_argv("t", &b);
        assert_eq!(argv, vec!["build", "-t", "t", "-f", "/w/Dockerfile", "/w"]);
    }

    #[test]
    fn tag_is_stable_and_declaration_sensitive() {
        let t1 = content_tag("my-repo", &build(), Some("dockerfile-digest"), None);
        assert_eq!(
            t1,
            content_tag("my-repo", &build(), Some("dockerfile-digest"), None)
        );
        assert!(t1.starts_with("thegn-dc-my-repo:"));

        // A changed arg → different tag.
        let mut b2 = build();
        b2.args.insert("VARIANT".into(), "22".into());
        assert_ne!(
            t1,
            content_tag("my-repo", &b2, Some("dockerfile-digest"), None)
        );

        // A changed target → different tag.
        let mut b3 = build();
        b3.target = Some("prod".into());
        assert_ne!(
            t1,
            content_tag("my-repo", &b3, Some("dockerfile-digest"), None)
        );

        let mut b4 = build();
        b4.dockerfile.push_str(".changed");
        assert_ne!(
            t1,
            content_tag("my-repo", &b4, Some("dockerfile-digest"), None)
        );
        let mut b5 = build();
        b5.context.push_str("-changed");
        assert_ne!(
            t1,
            content_tag("my-repo", &b5, Some("dockerfile-digest"), None)
        );
    }

    #[test]
    fn tag_slugifies_basename() {
        let t = content_tag("My Repo/App!", &build(), Some("dockerfile-digest"), None);
        // Only the tag prefix is slugified; the ':' + hash follow.
        let prefix = t.split(':').next().unwrap();
        assert_eq!(prefix, "thegn-dc-my-repo-app");
        // Empty/garbage basename falls back to a placeholder.
        assert!(
            content_tag("///", &build(), Some("dockerfile-digest"), None)
                .starts_with("thegn-dc-img:")
        );
    }

    #[test]
    fn dockerfile_bytes_change_the_tag() {
        let declaration = build();
        let first = content_tag("my-repo", &declaration, Some("sha256-one"), None);
        let tag_digest = first.split_once(':').unwrap().1;
        assert_eq!(tag_digest.len(), 64, "tag must contain a full SHA-256");
        assert!(tag_digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(
            first,
            content_tag("my-repo", &declaration, Some("sha256-two"), None)
        );
        assert_eq!(
            first,
            content_tag("my-repo", &declaration, Some("sha256-one"), None)
        );
        assert_ne!(
            first,
            content_tag("my-repo", &declaration, None, None),
            "unhashed declaration-only tags must be distinct"
        );
    }

    #[test]
    fn dockerfile_sha256_streams_regular_files_and_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let bytes = b"FROM scratch\n";
        std::fs::write(&dockerfile, bytes).unwrap();
        assert_eq!(
            dockerfile_sha256(&dockerfile).unwrap(),
            format!("{:x}", Sha256::digest(bytes))
        );

        if !cfg!(unix) {
            return;
        }
        let link = dir.path().join("Dockerfile-link");
        crate::fsperm::create_symlink(&dockerfile, &link, false).unwrap();
        let result = dockerfile_sha256(&link);
        assert!(result.is_err(), "a symlinked Dockerfile must be refused");
    }

    #[test]
    fn context_digest_changes_when_a_file_body_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let first = context_sha256(dir.path()).unwrap();
        std::fs::write(&file, "two").unwrap();
        let second = context_sha256(dir.path()).unwrap();
        assert_ne!(
            first, second,
            "file contents must affect the context digest"
        );
        std::fs::write(&file, "two").unwrap();
        assert_eq!(second, context_sha256(dir.path()).unwrap());
    }

    #[test]
    fn context_digest_changes_when_a_file_is_added_or_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let first_file = dir.path().join("a.txt");
        std::fs::write(&first_file, "same bytes").unwrap();
        let first = context_sha256(dir.path()).unwrap();
        std::fs::write(dir.path().join("b.txt"), "new file").unwrap();
        assert_ne!(first, context_sha256(dir.path()).unwrap());
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();
        std::fs::rename(&first_file, dir.path().join("c.txt")).unwrap();
        assert_ne!(first, context_sha256(dir.path()).unwrap());
    }

    #[test]
    fn context_digest_is_independent_of_creation_order() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        for (name, contents) in [("a", "one"), ("b", "two")] {
            std::fs::write(first_dir.path().join(name), contents).unwrap();
        }
        for (name, contents) in [("b", "two"), ("a", "one")] {
            std::fs::write(second_dir.path().join(name), contents).unwrap();
        }
        assert_eq!(
            context_sha256(first_dir.path()).unwrap(),
            context_sha256(second_dir.path()).unwrap()
        );
    }

    #[test]
    fn context_digest_skips_only_the_top_level_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "content").unwrap();
        let first = context_sha256(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "top-level").unwrap();
        assert_eq!(first, context_sha256(dir.path()).unwrap());
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::create_dir(dir.path().join("sub/.git")).unwrap();
        std::fs::write(dir.path().join("sub/.git/HEAD"), "nested").unwrap();
        assert_ne!(first, context_sha256(dir.path()).unwrap());
    }

    #[test]
    fn context_digest_records_a_symlink_without_following_it() {
        if !cfg!(unix) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let context = dir.path().join("context");
        std::fs::create_dir(&context).unwrap();
        let outside_a = dir.path().join("outside-a");
        let outside_b = dir.path().join("outside-b");
        std::fs::write(&outside_a, "one").unwrap();
        std::fs::write(&outside_b, "two").unwrap();
        let link = context.join("link");
        crate::fsperm::create_symlink(&outside_a, &link, false).unwrap();
        let first = context_sha256(&context).unwrap();
        std::fs::write(&outside_a, "changed outside").unwrap();
        assert_eq!(first, context_sha256(&context).unwrap());
        std::fs::remove_file(&link).unwrap();
        crate::fsperm::create_symlink(&outside_b, &link, false).unwrap();
        assert_ne!(first, context_sha256(&context).unwrap());
    }

    #[test]
    fn context_digest_refuses_over_the_entry_bound() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..4 {
            std::fs::write(dir.path().join(format!("file-{index}")), "").unwrap();
        }
        assert!(matches!(
            context_sha256_bounded(dir.path(), 3, 1024),
            Err(ContextDigestError::TooManyEntries { limit: 3 })
        ));
    }

    #[test]
    fn context_digest_refuses_over_the_byte_bound() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "123456789").unwrap();
        assert!(matches!(
            context_sha256_bounded(dir.path(), 4, 8),
            Err(ContextDigestError::TooManyBytes { limit: 8 })
        ));
        std::fs::write(&file, "12345678").unwrap();
        assert!(context_sha256_bounded(dir.path(), 4, 8).is_ok());
    }

    #[test]
    fn context_digest_error_for_a_missing_root_is_io() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            context_sha256(&dir.path().join("missing")),
            Err(ContextDigestError::Io { .. })
        ));
    }

    #[test]
    fn context_digest_changes_the_content_tag() {
        let declaration = build();
        let first = content_tag("r", &declaration, Some("d"), Some("c1"));
        assert_ne!(first, content_tag("r", &declaration, Some("d"), Some("c2")));
        assert_ne!(first, content_tag("r", &declaration, Some("d"), None));
        assert_eq!(first, content_tag("r", &declaration, Some("d"), Some("c1")));
    }

    #[test]
    fn context_digest_refuses_file_size_change_during_hashing() {
        let file = Path::new("context/file");
        assert!(matches!(
            check_context_file_size(file, 9, 8),
            Err(ContextDigestError::Io { error, .. })
                if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(check_context_file_size(file, 8, 8).is_ok());
    }
}
