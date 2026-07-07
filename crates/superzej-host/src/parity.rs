//! Local→remote worktree parity, extracted from `agent.rs` (pinned by the
//! file-size ratchet): bring a provider sandbox's clone to full parity with
//! the LOCAL worktree by capturing the artifact triple on the host and
//! replaying it in the sandbox. The script text lives in
//! [`superzej_core::syncstate`] so the hibernator's reverse capture shares it.

use std::path::Path;

use crate::agent::{block_on_provider, sanitize_tag};

/// Bring the sandbox clone to full parity with the LOCAL worktree at `wt_host`:
/// replay unpushed commits (a thin `git bundle … HEAD --not --remotes`), restore
/// uncommitted tracked changes (`git diff HEAD --binary`), and lay down untracked
/// non-ignored files (a tar). Host git reads use the GIT_*-scrubbed `git_cmd`
/// wrapper; the three artifacts are written into the sandbox `/tmp` and a single
/// replay script applies them in `workdir`. Best-effort throughout — any capture
/// or apply failure leaves the pristine origin checkout intact.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
pub(crate) fn apply_local_parity(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    wt_host: &str,
    workdir: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    use superzej_core::syncstate::{artifact_path, replay_script};
    use superzej_core::util::git_cmd;
    const STEM: &str = "sz-parity";
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

    let tmp = std::env::temp_dir();
    let tag = format!("{}-{}", sanitize_tag(id), std::process::id());

    // 1. Unpushed commits → a thin bundle (prerequisites = the remote-tracking
    //    tips the sandbox clone already has). An empty bundle (nothing unpushed)
    //    exits non-zero; treat that as "no commits to carry".
    let bundle_host = tmp.join(format!("sz-parity-{tag}.bundle"));
    let has_bundle = git_cmd(wt)
        .args(["bundle", "create"])
        .arg(&bundle_host)
        .args(["HEAD", "--not", "--remotes"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        && bundle_host.metadata().map(|m| m.len() > 0).unwrap_or(false);

    // 2. Uncommitted tracked changes (staged + unstaged vs HEAD), incl. deletions.
    let patch = git_cmd(wt)
        .args(["diff", "HEAD", "--binary"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .filter(|p| !p.is_empty());

    // 3. Untracked, non-ignored files → a tar (paths relative to the worktree).
    let tar_host = tmp.join(format!("sz-parity-{tag}.tar"));
    let list_host = tmp.join(format!("sz-parity-{tag}.list"));
    let untracked = git_cmd(wt)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .filter(|l| !l.is_empty());
    let has_tar = if let Some(list) = &untracked {
        std::fs::write(&list_host, list).is_ok()
            && std::process::Command::new("tar")
                .arg("-C")
                .arg(wt)
                .arg("--null")
                .arg("--files-from")
                .arg(&list_host)
                .arg("-cf")
                .arg(&tar_host)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            && tar_host.metadata().map(|m| m.len() > 0).unwrap_or(false)
    } else {
        false
    };
    let _ = std::fs::remove_file(&list_host);

    if !has_bundle && patch.is_none() && !has_tar {
        // Clean working tree with nothing unpushed — the origin clone is parity.
        let _ = std::fs::remove_file(&bundle_host);
        let _ = std::fs::remove_file(&tar_host);
        return Ok(());
    }

    // Upload the captured artifacts into the sandbox /tmp.
    if has_bundle {
        let bytes = std::fs::read(&bundle_host)?;
        let dst = artifact_path(STEM, "bundle");
        block_on_provider(|| async { provider.write(id, &dst, &bytes).await })?;
    }
    if let Some(p) = &patch {
        let dst = artifact_path(STEM, "patch");
        block_on_provider(|| async { provider.write(id, &dst, p).await })?;
    }
    if has_tar {
        let bytes = std::fs::read(&tar_host)?;
        let dst = artifact_path(STEM, "tar");
        block_on_provider(|| async { provider.write(id, &dst, &bytes).await })?;
    }
    let _ = std::fs::remove_file(&bundle_host);
    let _ = std::fs::remove_file(&tar_host);

    // Replay over the clone, in the workdir. Each stage is independently guarded
    // (`[ -s file ]`) and non-fatal so a partial capture still helps.
    let script = replay_script(
        workdir,
        STEM,
        host_head.as_deref().filter(|_| has_bundle),
        patch.is_some(),
        has_tar,
        "local parity applied",
    );
    let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
    block_on_provider(|| async { provider.run_exec(id, &argv, None, exec_env).await })
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("replay exec failed: {e}"))
}
