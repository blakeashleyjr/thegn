//! Dockerfile image builds for OCI sandboxes — split out of the (ratchet-capped)
//! `sandbox.rs`. A devcontainer `build` block (or any future `[sandbox] build`)
//! becomes a synchronous `<runtime> build -t <tag> …` run *before* the container
//! is created, driven through the same `oci_prefix` as every other runtime call
//! so it targets the right daemon (rootful podman, an `oci_host` remote, …).
//!
//! The argv + tag construction are pure and unit-tested here; only
//! [`build_image`] touches a subprocess (a `cov_ignore` seam exercised by
//! `test/smoke.sh`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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

/// A stable, local image tag for a build. Content-addressed over the build
/// *declaration* (dockerfile/context/args/target) so two worktrees with the same
/// devcontainer reuse one built image, and a changed declaration gets a fresh
/// tag. `basename` is a human hint (e.g. the repo name); it is slugified.
pub fn content_tag(basename: &str, b: &SandboxBuild) -> String {
    let mut sig = format!("{}\u{1f}{}", b.dockerfile, b.context);
    for (k, v) in &b.args {
        sig.push('\u{1f}');
        sig.push_str(k);
        sig.push('=');
        sig.push_str(v);
    }
    if let Some(t) = &b.target {
        sig.push('\u{1f}');
        sig.push_str(t);
    }
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
    format!("superzej-dc-{slug}:{}", crate::util::short_hash(&sig, 12))
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
        anyhow::bail!("{rt} build of {tag} failed: {last}");
    }
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
        let t1 = content_tag("my-repo", &build());
        assert_eq!(t1, content_tag("my-repo", &build()));
        assert!(t1.starts_with("superzej-dc-my-repo:"));

        // A changed arg → different tag.
        let mut b2 = build();
        b2.args.insert("VARIANT".into(), "22".into());
        assert_ne!(t1, content_tag("my-repo", &b2));

        // A changed target → different tag.
        let mut b3 = build();
        b3.target = Some("prod".into());
        assert_ne!(t1, content_tag("my-repo", &b3));
    }

    #[test]
    fn tag_slugifies_basename() {
        let t = content_tag("My Repo/App!", &build());
        // Only the tag prefix is slugified; the ':' + hash follow.
        let prefix = t.split(':').next().unwrap();
        assert_eq!(prefix, "superzej-dc-my-repo-app");
        // Empty/garbage basename falls back to a placeholder.
        assert!(content_tag("///", &build()).starts_with("superzej-dc-img:"));
    }
}
