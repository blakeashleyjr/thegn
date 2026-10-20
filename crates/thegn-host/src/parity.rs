//! Local→remote worktree parity, extracted from `agent.rs` (pinned by the
//! keep-god-files-flat guidance): bring a provider sandbox's clone to full parity with
//! the LOCAL worktree by capturing the artifact triple on the host and
//! replaying it in the sandbox. The script text lives in
//! [`thegn_core::syncstate`] so the hibernator's reverse capture shares it.

use std::path::Path;

use anyhow::Context;
use sha2::{Digest, Sha256};

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeFingerprint {
    head: Option<String>,
    status_sha256: String,
}

#[derive(Debug)]
struct CaptureIncoherent {
    attempts: usize,
}

impl std::fmt::Display for CaptureIncoherent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "local parity capture failed: the worktree changed while it was captured ({} attempts); nothing was uploaded",
            self.attempts
        )
    }
}

impl std::error::Error for CaptureIncoherent {}

struct Capture {
    host_head: Option<String>,
    has_bundle: bool,
    patch: Option<Vec<u8>>,
    has_tar: bool,
    artifacts: Vec<(String, Vec<u8>)>,
}

#[expect(clippy::disallowed_methods, reason = "off-loop provisioning path")]
fn fingerprint(wt: &Path) -> anyhow::Result<WorktreeFingerprint> {
    use thegn_core::util::git_cmd;

    let head = git_cmd(wt)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|head| !head.is_empty());
    let status = git_cmd(wt)
        .args(["status", "--porcelain=v2", "-z", "--untracked-files=all"])
        .output()?;
    anyhow::ensure!(
        status.status.success(),
        "git status failed while fingerprinting worktree"
    );
    Ok(WorktreeFingerprint {
        head,
        status_sha256: lower_hex(&Sha256::digest(status.stdout)),
    })
}

fn capture_coherent<F, C, T>(mut probe: F, mut capture: C) -> anyhow::Result<T>
where
    F: FnMut() -> anyhow::Result<WorktreeFingerprint>,
    C: FnMut() -> anyhow::Result<T>,
{
    for attempt in 0..2 {
        let before = probe()?;
        let value = capture()?;
        let after = probe()?;
        if before == after {
            return Ok(value);
        }
        if attempt == 1 {
            return Err(anyhow::Error::new(CaptureIncoherent { attempts: 2 }));
        }
    }
    unreachable!("capture loop returns after at most two attempts")
}

#[expect(clippy::disallowed_methods, reason = "off-loop provisioning path")]
fn capture(wt: &Path, tmp: &Path) -> anyhow::Result<Capture> {
    use thegn_core::util::git_cmd;

    let host_head = git_cmd(wt)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|head| !head.is_empty());
    let bundle_file = tempfile::Builder::new()
        .prefix("bundle-")
        .suffix(".bundle")
        .tempfile_in(tmp)?;
    let tar_file = tempfile::Builder::new()
        .prefix("untracked-")
        .suffix(".tar")
        .tempfile_in(tmp)?;
    let list_file = tempfile::Builder::new()
        .prefix("untracked-")
        .suffix(".list")
        .tempfile_in(tmp)?;

    let has_bundle = git_cmd(wt)
        .args(["bundle", "create"])
        .arg(bundle_file.path())
        .args(["HEAD", "--not", "--remotes"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
        && nonempty(bundle_file.path());
    let patch = git_cmd(wt)
        .args(["diff", "HEAD", "--binary"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
        .filter(|patch| !patch.is_empty());
    let untracked = git_cmd(wt)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
        .filter(|list| !list.is_empty());
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
                .map(|status| status.success())
                .unwrap_or(false)
            && nonempty(tar_file.path())
    } else {
        false
    };
    let mut artifacts = Vec::new();
    if has_bundle {
        artifacts.push(("bundle".to_string(), std::fs::read(bundle_file.path())?));
    }
    if let Some(patch_bytes) = &patch {
        artifacts.push(("patch".to_string(), patch_bytes.clone()));
    }
    if has_tar {
        artifacts.push(("tar".to_string(), std::fs::read(tar_file.path())?));
    }
    Ok(Capture {
        host_head,
        has_bundle,
        patch,
        has_tar,
        artifacts,
    })
}

pub(crate) fn failure_note(error: &anyhow::Error) -> &'static str {
    match error
        .downcast_ref::<ReplayFailure>()
        .map(|failure| failure.stage)
    {
        Some(ReplayStage::Checkout | ReplayStage::ArtifactValidation | ReplayStage::Fetch)
        | None => "the sandbox keeps the origin checkout",
        Some(ReplayStage::Reset | ReplayStage::Patch | ReplayStage::Extraction) => {
            "the sandbox checkout may be partly overlaid; re-create the sandbox before trusting it"
        }
        Some(ReplayStage::Cleanup) => {
            "parity was applied but its staged artifacts were left in the sandbox"
        }
        Some(ReplayStage::Exec) => "the sandbox checkout state is unknown",
    }
}

/// Bring the sandbox clone to full parity with the LOCAL worktree at `wt_host`:
/// replay unpushed commits (a thin `git bundle … HEAD --not --remotes`), restore
/// uncommitted tracked changes (`git diff HEAD --binary`), and lay down untracked
/// non-ignored files (a tar). Host git reads use the GIT_*-scrubbed `git_cmd`
/// wrapper; the three artifacts are written into the sandbox `/tmp` and a single
/// replay script applies them in `workdir`. Replay failures are surfaced and
/// leave sandbox artifacts in place for diagnosis. Staged checkout promotion
/// remains deferred.
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
    let wt = Path::new(wt_host);
    if !wt.join(".git").exists() {
        // No git metadata (a bare directory) — nothing to mirror.
        return Ok(());
    }
    let tmp = tempfile::Builder::new().prefix("tg-parity-").tempdir()?;
    let captured = capture_coherent(|| fingerprint(wt), || capture(wt, tmp.path()))?;
    if !captured.has_bundle && captured.patch.is_none() && !captured.has_tar {
        // Clean working tree with nothing unpushed — the origin clone is parity.
        return Ok(());
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

    let manifest = manifest_for_artifacts(&captured.artifacts);
    for (name, bytes) in &captured.artifacts {
        let dst = staged_artifact_path(&dir, name);
        block_on_provider(|| async { provider.write(id, &dst, bytes).await })
            .with_context(|| format!("upload to staging directory {dir} failed"))?;
    }

    // Replay over the clone, in the workdir. Failed stages preserve remote artifacts.
    let script = replay_script_verified(
        workdir,
        &dir,
        &manifest,
        captured
            .host_head
            .as_deref()
            .filter(|_| captured.has_bundle),
        captured.patch.is_some(),
        captured.has_tar,
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
    use super::{
        CaptureIncoherent, ReplayFailure, ReplayStage, WorktreeFingerprint, capture_coherent,
        failure_note, fingerprint, manifest_for_artifacts, nonempty, random_staging_dir,
    };

    fn fingerprint_value(head: &str, digest: &str) -> WorktreeFingerprint {
        WorktreeFingerprint {
            head: Some(head.to_string()),
            status_sha256: digest.to_string(),
        }
    }

    #[test]
    fn capture_retries_once_then_succeeds_when_the_second_pass_is_stable() {
        let sequence = [
            fingerprint_value("a", "1"),
            fingerprint_value("b", "2"),
            fingerprint_value("c", "3"),
            fingerprint_value("c", "3"),
        ];
        let mut retry_probe_count = 0;
        let mut retry_capture_count = 0;
        let captured = capture_coherent(
            || {
                let value = sequence[retry_probe_count].clone();
                retry_probe_count += 1;
                Ok(value)
            },
            || {
                retry_capture_count += 1;
                Ok(retry_capture_count)
            },
        )
        .unwrap();
        assert_eq!(retry_probe_count, 4);
        assert_eq!(retry_capture_count, 2);
        assert_eq!(captured, 2);
    }

    #[test]
    fn capture_fails_typed_after_two_incoherent_passes() {
        let sequence = [
            fingerprint_value("a", "1"),
            fingerprint_value("b", "2"),
            fingerprint_value("c", "3"),
            fingerprint_value("d", "4"),
        ];
        let mut failure_probe_count = 0;
        let mut failure_capture_count = 0;
        let result = capture_coherent(
            || {
                let value = sequence[failure_probe_count].clone();
                failure_probe_count += 1;
                Ok(value)
            },
            || {
                failure_capture_count += 1;
                Ok(failure_capture_count)
            },
        );
        assert!(
            matches!(result, Err(error) if error.downcast_ref::<CaptureIncoherent>().is_some())
        );
        assert_eq!(failure_probe_count, 4);
        assert_eq!(failure_capture_count, 2);
    }

    #[test]
    fn stable_worktree_captures_once() {
        let stable = fingerprint_value("same-head", "same-status");
        let mut stable_probe_count = 0;
        let mut stable_capture_count = 0;
        let captured = capture_coherent(
            || {
                stable_probe_count += 1;
                Ok(stable.clone())
            },
            || {
                stable_capture_count += 1;
                Ok("captured")
            },
        )
        .unwrap();
        assert_eq!(stable_probe_count, 2);
        assert_eq!(stable_capture_count, 1);
        assert_eq!(captured, "captured");
    }

    #[test]
    fn failure_note_never_claims_origin_after_a_mutating_stage() {
        let expected = [
            (ReplayStage::Exec, "the sandbox checkout state is unknown"),
            (
                ReplayStage::Checkout,
                "the sandbox keeps the origin checkout",
            ),
            (
                ReplayStage::ArtifactValidation,
                "the sandbox keeps the origin checkout",
            ),
            (ReplayStage::Fetch, "the sandbox keeps the origin checkout"),
            (
                ReplayStage::Reset,
                "the sandbox checkout may be partly overlaid; re-create the sandbox before trusting it",
            ),
            (
                ReplayStage::Patch,
                "the sandbox checkout may be partly overlaid; re-create the sandbox before trusting it",
            ),
            (
                ReplayStage::Extraction,
                "the sandbox checkout may be partly overlaid; re-create the sandbox before trusting it",
            ),
            (
                ReplayStage::Cleanup,
                "parity was applied but its staged artifacts were left in the sandbox",
            ),
        ];
        for (stage, expected_note) in expected {
            let error = anyhow::Error::new(ReplayFailure {
                stage,
                detail: "fixture".to_string(),
            });
            assert_eq!(failure_note(&error), expected_note, "stage {stage:?}");
        }
        assert_eq!(
            failure_note(&anyhow::anyhow!("upload failed")),
            "the sandbox keeps the origin checkout"
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture invokes git to exercise worktree fingerprinting"
    )]
    fn fingerprint_changes_when_an_untracked_file_appears() {
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

        let before = fingerprint(repo.path()).unwrap();
        std::fs::write(repo.path().join("untracked.txt"), "new\n").unwrap();
        let after = fingerprint(repo.path()).unwrap();
        assert_ne!(before, after);
        assert_eq!(before.head, after.head);
    }

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
