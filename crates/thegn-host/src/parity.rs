//! Local→remote worktree parity, extracted from `agent.rs` (pinned by the
//! keep-god-files-flat guidance): bring a provider sandbox's clone to full parity with
//! the LOCAL worktree by capturing the artifact triple on the host and
//! replaying it in the sandbox. The script text lives in
//! [`thegn_core::syncstate`] so the hibernator's reverse capture shares it.

use std::path::Path;

use anyhow::Context;

use crate::agent::block_on_provider;

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 0x0f) as usize] as char,
            ]
        })
        .collect()
}

fn random_staging_dir() -> anyhow::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)?;
    Ok(format!("/tmp/tg-parity-{}", lower_hex(&random)))
}

fn manifest_for_artifacts(
    artifacts: &[(String, Vec<u8>)],
) -> Vec<thegn_core::syncstate::CaptureArtifact> {
    use sha2::{Digest, Sha256};

    artifacts
        .iter()
        .map(|(name, bytes)| {
            let digest = Sha256::digest(bytes);
            let sha256 = lower_hex(&digest);
            thegn_core::syncstate::CaptureArtifact {
                name: name.clone(),
                bytes: bytes.len() as u64,
                sha256,
            }
        })
        .collect()
}

fn nonempty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
}

fn output_tail(output: &str) -> &str {
    const MAX_BYTES: usize = 2 * 1024;
    let mut start = output.len().saturating_sub(MAX_BYTES);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    &output[start..]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayStage {
    Exec,
    Checkout,
    ArtifactValidation,
    Fetch,
    Reset,
    Patch,
    Extraction,
    Cleanup,
}

impl ReplayStage {
    fn from_marker(output: &str) -> Option<Self> {
        output.lines().find_map(|line| match line.trim() {
            "TG-PARITY-ERROR:checkout" => Some(Self::Checkout),
            "TG-PARITY-ERROR:artifact_validation" => Some(Self::ArtifactValidation),
            "TG-PARITY-ERROR:fetch" => Some(Self::Fetch),
            "TG-PARITY-ERROR:reset" => Some(Self::Reset),
            "TG-PARITY-ERROR:patch" => Some(Self::Patch),
            "TG-PARITY-ERROR:extraction" => Some(Self::Extraction),
            "TG-PARITY-ERROR:cleanup" => Some(Self::Cleanup),
            _ => None,
        })
    }
}

impl std::fmt::Display for ReplayStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug)]
struct ReplayFailure {
    stage: ReplayStage,
    detail: String,
}

impl std::fmt::Display for ReplayFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "local parity replay failed at {}: {}",
            self.stage, self.detail
        )
    }
}

impl std::error::Error for ReplayFailure {}

/// Bring the sandbox clone to full parity with the LOCAL worktree at `wt_host`:
/// replay unpushed commits (a thin `git bundle … HEAD --not --remotes`), restore
/// uncommitted tracked changes (`git diff HEAD --binary`), and lay down untracked
/// non-ignored files (a tar). Host git reads use the GIT_*-scrubbed `git_cmd`
/// wrapper; the three artifacts are written into the sandbox `/tmp` and a single
/// replay script applies them in `workdir`. Replay failures are surfaced and
/// leave sandbox artifacts in place for diagnosis. Capture coherence and staged
/// checkout promotion remain deferred.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
pub(crate) fn apply_local_parity(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    wt_host: &str,
    workdir: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    use thegn_core::syncstate::{replay_script_verified, stage_dir_script, staged_artifact_path};
    use thegn_core::util::git_cmd;
    let wt = Path::new(wt_host);
    if !wt.join(".git").exists() {
        // No git metadata (a bare directory) — nothing to mirror.
        return Ok(());
    }
    let host_head = git_cmd(wt)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty());

    let tmp = tempfile::Builder::new().prefix("tg-parity-").tempdir()?;
    let bundle_file = tempfile::Builder::new()
        .prefix("bundle-")
        .suffix(".bundle")
        .tempfile_in(tmp.path())?;
    let tar_file = tempfile::Builder::new()
        .prefix("untracked-")
        .suffix(".tar")
        .tempfile_in(tmp.path())?;
    let list_file = tempfile::Builder::new()
        .prefix("untracked-")
        .suffix(".list")
        .tempfile_in(tmp.path())?;

    // 1. Unpushed commits → a thin bundle (prerequisites = the remote-tracking
    //    tips the sandbox clone already has). An empty bundle (nothing unpushed)
    //    exits non-zero; treat that as "no commits to carry".
    let has_bundle = git_cmd(wt)
        .args(["bundle", "create"])
        .arg(bundle_file.path())
        .args(["HEAD", "--not", "--remotes"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        && nonempty(bundle_file.path());

    // 2. Uncommitted tracked changes (staged + unstaged vs HEAD), incl. deletions.
    let patch = git_cmd(wt)
        .args(["diff", "HEAD", "--binary"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .filter(|p| !p.is_empty());

    // 3. Untracked, non-ignored files → a tar (paths relative to the worktree).
    let untracked = git_cmd(wt)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .filter(|l| !l.is_empty());
    let has_tar = if let Some(list) = &untracked {
        std::fs::write(list_file.path(), list).is_ok()
            && std::process::Command::new("tar")
                .arg("-C")
                .arg(wt)
                .arg("--null")
                .arg("--files-from")
                .arg(list_file.path())
                .arg("-cf")
                .arg(tar_file.path())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            && nonempty(tar_file.path())
    } else {
        false
    };

    if !has_bundle && patch.is_none() && !has_tar {
        // Clean working tree with nothing unpushed — the origin clone is parity.
        return Ok(());
    }

    let mut artifacts = Vec::new();
    if has_bundle {
        let bytes = std::fs::read(bundle_file.path())?;
        artifacts.push(("bundle".to_string(), bytes));
    }
    if let Some(p) = &patch {
        artifacts.push(("patch".to_string(), p.clone()));
    }
    if has_tar {
        let bytes = std::fs::read(tar_file.path())?;
        artifacts.push(("tar".to_string(), bytes));
    }

    let dir = random_staging_dir()?;
    let create_script = stage_dir_script(&dir);
    let create_argv = vec!["/bin/sh".to_string(), "-lc".to_string(), create_script];
    let (code, output) =
        block_on_provider(|| async { provider.run_exec(id, &create_argv, None, exec_env).await })
            .map_err(|error| {
            anyhow::Error::new(ReplayFailure {
                stage: ReplayStage::ArtifactValidation,
                detail: format!(
                    "staging directory {dir} could not be created privately: {}",
                    output_tail(&error.to_string())
                ),
            })
        })?;
    if code != 0 {
        return Err(anyhow::Error::new(ReplayFailure {
            stage: ReplayStage::ArtifactValidation,
            detail: format!(
                "staging directory {dir} could not be created privately: {}",
                output_tail(&output)
            ),
        }));
    }

    let manifest = manifest_for_artifacts(&artifacts);
    for (name, bytes) in &artifacts {
        let dst = staged_artifact_path(&dir, name);
        block_on_provider(|| async { provider.write(id, &dst, bytes).await })
            .with_context(|| format!("upload to staging directory {dir} failed"))?;
    }

    // Replay over the clone, in the workdir. Failed stages preserve remote artifacts.
    let script = replay_script_verified(
        workdir,
        &dir,
        &manifest,
        host_head.as_deref().filter(|_| has_bundle),
        patch.is_some(),
        has_tar,
        "local parity applied",
    );
    let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
    let (code, output) =
        block_on_provider(|| async { provider.run_exec(id, &argv, None, exec_env).await })
            .map_err(|e| {
                let error_detail = e.to_string();
                anyhow::Error::new(ReplayFailure {
                    stage: ReplayStage::Exec,
                    detail: format!(
                        "{}; uploaded artifacts retained in {dir}",
                        output_tail(&error_detail)
                    ),
                })
            })?;
    if code != 0 {
        return Err(anyhow::Error::new(ReplayFailure {
            stage: ReplayStage::from_marker(&output).unwrap_or(ReplayStage::Exec),
            detail: format!(
                "{}{}; uploaded artifacts retained in {dir}",
                ReplayStage::from_marker(&output)
                    .and_then(|_| {
                        output
                            .lines()
                            .find(|line| line.trim().starts_with("TG-PARITY-ERROR:"))
                    })
                    .map(|line| format!("{line}\n"))
                    .unwrap_or_default(),
                output_tail(&output)
            ),
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{manifest_for_artifacts, nonempty, random_staging_dir};

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture invokes git to exercise bundle path metadata"
    )]
    fn bundle_path_metadata_sees_git_atomic_replace() {
        let repo = tempfile::tempdir().unwrap();
        let run_git = |args: &[&str]| {
            thegn_core::util::git_cmd(repo.path())
                .arg("-c")
                .arg("commit.gpgsign=false")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .output()
                .unwrap()
        };
        assert!(run_git(&["init", "-q"]).status.success());
        assert!(
            run_git(&["config", "user.name", "Parity Test"])
                .status
                .success()
        );
        assert!(
            run_git(&["config", "user.email", "parity@example.invalid"])
                .status
                .success()
        );
        std::fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
        assert!(run_git(&["add", "tracked.txt"]).status.success());
        assert!(run_git(&["commit", "-m", "initial"]).status.success());

        let bundle = tempfile::Builder::new()
            .suffix(".bundle")
            .tempfile_in(repo.path())
            .unwrap();
        assert!(
            run_git(&[
                "bundle",
                "create",
                bundle.path().to_str().unwrap(),
                "HEAD",
                "--not",
                "--remotes"
            ])
            .status
            .success()
        );
        assert!(nonempty(bundle.path()));
    }

    #[test]
    fn replay_failure_marker_identifies_its_typed_stage() {
        use super::{ReplayStage, ReplayStage::Patch};
        assert_eq!(
            ReplayStage::from_marker("TG-PARITY-ERROR:patch\n"),
            Some(Patch)
        );
        assert_eq!(ReplayStage::from_marker("unrelated output"), None);
    }

    #[test]
    fn output_tail_is_bounded_on_a_character_boundary() {
        let output = format!("{}🦀{}", "a".repeat(2047), "b".repeat(2045));
        let tail = super::output_tail(&output);
        assert!(tail.len() <= 2 * 1024);
        assert!(tail.starts_with('b'));
    }

    #[test]
    fn staging_dirs_are_random_and_never_the_fixed_stem() {
        let first = random_staging_dir().unwrap();
        let second = random_staging_dir().unwrap();
        for dir in [&first, &second] {
            let suffix = dir.strip_prefix("/tmp/tg-parity-").unwrap();
            assert_eq!(suffix.len(), 32);
            assert!(
                suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            assert_ne!(dir.as_str(), "/tmp/tg-parity");
        }
        assert_ne!(first, second);
    }

    #[test]
    fn manifest_digests_match_uploaded_bytes() {
        use sha2::{Digest, Sha256};

        let artifacts = vec![
            ("patch".to_string(), b"patch bytes".to_vec()),
            ("tar".to_string(), b"tar bytes".to_vec()),
        ];
        let manifest = manifest_for_artifacts(&artifacts);
        assert_eq!(manifest.len(), artifacts.len());
        for ((name, bytes), entry) in artifacts.iter().zip(manifest) {
            assert_eq!(&entry.name, name);
            assert_eq!(entry.bytes, bytes.len() as u64);
            let expected = format!("{:x}", Sha256::digest(bytes));
            assert_eq!(entry.sha256, expected);
        }
    }
}
