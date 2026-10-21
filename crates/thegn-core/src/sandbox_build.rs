//! Dockerfile image builds for OCI sandboxes — split out of the (ratchet-capped)
//! `sandbox.rs`. A devcontainer `build` block (or any future `[sandbox] build`)
//! becomes a synchronous `<runtime> build -t <tag> …` run *before* the container
//! is created, driven through the same `oci_prefix` as every other runtime call
//! so it targets the right daemon (rootful podman, an `oci_host` remote, …).
//!
//! The tag binds the build declaration and safely-read Dockerfile bytes; files
//! under the build context are deliberately outside its digest. The argv + tag
//! construction are unit-tested here; only [`build_image`] touches a
//! subprocess (a `cov_ignore` seam exercised by `test/smoke.sh`).

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

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

/// A stable, local image tag for a build. The versioned SHA-256 covers the
/// build declaration and the Dockerfile bytes when they can be read safely;
/// it does not cover files in the build context. An absent digest marks a
/// declaration-only tag. `basename` is a human hint (e.g. the repo name); it
/// is slugified.
pub fn content_tag(basename: &str, b: &SandboxBuild, dockerfile_sha256: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"thegn.dc-build-tag.v2\0");
    hash_field(&mut hasher, b.dockerfile.as_bytes());
    hash_field(&mut hasher, b.context.as_bytes());
    hasher.update((b.args.len() as u64).to_be_bytes());
    for (key, value) in &b.args {
        hash_field(&mut hasher, key.as_bytes());
        hash_field(&mut hasher, value.as_bytes());
    }
    hash_optional_field(&mut hasher, b.target.as_deref());
    hash_optional_field(&mut hasher, dockerfile_sha256);
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
        let t1 = content_tag("my-repo", &build(), Some("dockerfile-digest"));
        assert_eq!(
            t1,
            content_tag("my-repo", &build(), Some("dockerfile-digest"))
        );
        assert!(t1.starts_with("thegn-dc-my-repo:"));

        // A changed arg → different tag.
        let mut b2 = build();
        b2.args.insert("VARIANT".into(), "22".into());
        assert_ne!(t1, content_tag("my-repo", &b2, Some("dockerfile-digest")));

        // A changed target → different tag.
        let mut b3 = build();
        b3.target = Some("prod".into());
        assert_ne!(t1, content_tag("my-repo", &b3, Some("dockerfile-digest")));

        let mut b4 = build();
        b4.dockerfile.push_str(".changed");
        assert_ne!(t1, content_tag("my-repo", &b4, Some("dockerfile-digest")));
        let mut b5 = build();
        b5.context.push_str("-changed");
        assert_ne!(t1, content_tag("my-repo", &b5, Some("dockerfile-digest")));
    }

    #[test]
    fn tag_slugifies_basename() {
        let t = content_tag("My Repo/App!", &build(), Some("dockerfile-digest"));
        // Only the tag prefix is slugified; the ':' + hash follow.
        let prefix = t.split(':').next().unwrap();
        assert_eq!(prefix, "thegn-dc-my-repo-app");
        // Empty/garbage basename falls back to a placeholder.
        assert!(
            content_tag("///", &build(), Some("dockerfile-digest")).starts_with("thegn-dc-img:")
        );
    }

    #[test]
    fn dockerfile_bytes_change_the_tag() {
        let declaration = build();
        let first = content_tag("my-repo", &declaration, Some("sha256-one"));
        let tag_digest = first.split_once(':').unwrap().1;
        assert_eq!(tag_digest.len(), 64, "tag must contain a full SHA-256");
        assert!(tag_digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(
            first,
            content_tag("my-repo", &declaration, Some("sha256-two"))
        );
        assert_eq!(
            first,
            content_tag("my-repo", &declaration, Some("sha256-one"))
        );
        assert_ne!(
            first,
            content_tag("my-repo", &declaration, None),
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
}
