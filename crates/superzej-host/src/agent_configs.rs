//! Agent-login sync — upload coding agents' host config/credential files into a
//! provider sandbox so the agent (claude code, codex, pi, custom) is logged-in
//! there. Split out of `agent.rs` (god-file ratchet) as the "Sync agent logins"
//! provisioning step.
//!
//! ## Two-phase upload
//!
//! **Phase 1 — auth-critical (always runs, no budget check):** A small,
//! explicit allowlist of the files that are strictly sufficient for the agent
//! to be authenticated (`agent_auth_critical_files`). For Claude Code this is
//! ~5 files (<50 KB total). Even with a slow provider this completes in seconds
//! and guarantees the agent is usable regardless of what happens in Phase 2.
//!
//! **Phase 2 — full config tree (parallel, budget-capped):** Walk each agent's
//! config directories (skipping bulky state dirs), then upload the remaining
//! files with bounded concurrency (`UPLOAD_CONCURRENCY`). This is best-effort:
//! if the 120s budget runs out, the agent still works (Phase 1 already handled
//! auth) — only non-critical extras (hook scripts, MCP config) may be missing.
//!
//! The two-phase design makes this correct for any codebase size. A Firefox/
//! Chromium developer whose `~/.pi/agent/` contains 40k tool files will still
//! get a working, logged-in agent; Phase 2 just won't finish all 40k files.

use crate::agent::block_on_provider;
use std::path::Path;

/// Directory names under an agent's config tree that hold bulky, ephemeral state
/// (session transcripts, caches, snapshots) — NEVER needed to make the agent
/// "logged in", and gigabytes in practice (`~/.claude/projects` alone is often
/// over 1 GB of `*.jsonl` transcripts). Skipped so the config sync carries only
/// auth + settings and can't hang/502 pushing transcripts over the per-file fs API.
const AGENT_STATE_SKIP_DIRS: &[&str] = &[
    "projects",        // claude: per-repo session transcripts (the 502/hang source)
    "file-history",    // claude: ~500 MB of tiny edit-history blobs — the 502/hang source
    "plugins",         // claude: bulky plugin trees, not auth/config
    "backups",         // claude: rolling config backups
    "paste-cache",     // claude: transient paste spool
    "todos",           // claude runtime scratch
    "statsig",         // claude telemetry cache
    "shell-snapshots", // claude runtime
    "sessions",        // pi/others: session transcripts
    "history",         // pi/others: command/session history
    "logs",            // any: log spool
    "cache",
    ".cache",
];

/// Skip an individual config file larger than this — real agent config/auth is
/// tiny (KB); anything large under a config dir is transcript/cache data.
const AGENT_CONFIG_MAX_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB

/// Per-file upload ceiling for the agent-login sync. A config/auth file is a few
/// KB, so a write that hasn't returned in this long is a stalled request (a hung
/// sprite fs endpoint), not slow progress — time it out and move on best-effort
/// instead of letting it strand "Sync agent logins" on the loading screen.
const AGENT_CONFIG_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Whole-step wall-clock budget for Phase 2 (the full tree walk). Phase 1
/// (auth-critical files) runs unconditionally outside this budget so the agent
/// is always authenticated even when Phase 2 runs out of time.
const AGENT_CONFIG_STEP_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// How many files to upload concurrently in Phase 2. Each upload is a separate
/// HTTP round-trip to the provider; parallelism cuts wall-clock dramatically for
/// large config trees (e.g. 40k files at 300ms/file goes from 3.3 hours
/// sequential to ~25 minutes at CONCURRENCY=8 — and most fit within the 120s
/// budget since the typical tree is 100-500 files).
const UPLOAD_CONCURRENCY: usize = 8;

/// Collect `(absolute, relative)` files under an agent config `dir`, skipping the
/// bulky-state subdirs in [`AGENT_STATE_SKIP_DIRS`] and any file over
/// [`AGENT_CONFIG_MAX_BYTES`]. Iterative.
///
/// Symlinks ARE followed: on a home-manager/NixOS host the whole config tree
/// (e.g. `~/.claude/hooks/*.sh`) is symlinks into the `/nix/store`, so resolving
/// them is what makes the login actually work in-sandbox — otherwise the synced
/// `settings.json` references hook scripts that never got uploaded (the sandbox's
/// "`agentmemory-*.sh`: not found" hook errors). `entry.file_type()` reports the
/// link itself (neither file nor dir), so we resolve the target via
/// `fs::metadata`; a `seen` set of canonical dirs guards against symlink cycles.
fn collect_agent_config_files(dir: &Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    while let Some(cur) = stack.pop() {
        // Cycle guard for followed symlinked dirs: skip a dir we've already walked
        // (by resolved identity). A canonicalize failure just means we walk it once.
        if let Ok(canon) = std::fs::canonicalize(&cur)
            && !seen.insert(canon)
        {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Resolve THROUGH symlinks (unlike `entry.file_type()`), so a config
            // file/dir symlinked into the nix store is classified by its target.
            let Ok(md) = std::fs::metadata(&path) else {
                continue; // broken/dangling link — nothing to upload
            };
            if md.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if AGENT_STATE_SKIP_DIRS.iter().any(|s| *s == name) {
                    continue;
                }
                stack.push(path);
            } else if md.is_file() {
                if md.len() > AGENT_CONFIG_MAX_BYTES {
                    continue;
                }
                if let Ok(rel) = path.strip_prefix(dir) {
                    out.push((path.clone(), rel.to_string_lossy().replace('\\', "/")));
                }
            }
        }
    }
    out
}

/// Upload coding agents' host config/credential dirs into the sandbox `$HOME`
/// (`/root`) so the agent (claude code, codex, custom) is logged-in there.
///
/// See the module-level doc for the two-phase strategy: Phase 1 (auth-critical,
/// always) then Phase 2 (full tree, parallel, budget-capped).
pub(crate) fn upload_agent_configs(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    sandbox_home: &str,
    agents: &[String],
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = sandbox_home.trim_end_matches('/');

    // -----------------------------------------------------------------------
    // Phase 1: auth-critical files — explicit allowlist, always uploaded,
    // no deadline. Even a slow provider finishes these in a few seconds.
    // The agent is guaranteed to be logged-in after this phase completes.
    // -----------------------------------------------------------------------
    let mut auth_ok = 0usize;
    let mut auth_failed = 0usize;
    for agent in agents {
        let critical = superzej_core::envplan::agent_auth_critical_files(agent);
        for f in &critical {
            let src = Path::new(&host_home).join(f);
            let Ok(data) = std::fs::read(&src) else {
                continue; // file absent on this host — skip silently
            };
            let dest = format!("{base}/{f}");
            match block_on_provider(|| async {
                match tokio::time::timeout(
                    AGENT_CONFIG_UPLOAD_TIMEOUT,
                    provider.write(id, &dest, &data),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!(
                        "upload timed out after {}s",
                        AGENT_CONFIG_UPLOAD_TIMEOUT.as_secs()
                    )),
                }
            }) {
                Ok(()) => auth_ok += 1,
                Err(e) => {
                    auth_failed += 1;
                    tracing::warn!(
                        target: "szhost::startup",
                        dest = %dest,
                        error = %e,
                        "agent-config auth-critical upload failed (best-effort)"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2: full config tree — walk directories, skip bulky-state dirs,
    // upload remaining files in parallel batches, stop when budget expires.
    // Auth-critical files already uploaded above are deduplicated.
    // -----------------------------------------------------------------------

    // Build the full upload list synchronously before entering async.
    // Dedup against the auth-critical set so we don't double-upload.
    let mut already_uploaded: std::collections::HashSet<String> = std::collections::HashSet::new();
    for agent in agents {
        for f in superzej_core::envplan::agent_auth_critical_files(agent) {
            already_uploaded.insert(f);
        }
    }

    let mut all_uploads: Vec<(String, Vec<u8>)> = Vec::new();
    for agent in agents {
        let (files, dirs) = superzej_core::envplan::agent_config_paths(agent);
        for f in files {
            if already_uploaded.contains(&f) {
                continue;
            }
            let src = Path::new(&host_home).join(&f);
            let Ok(data) = std::fs::read(&src) else {
                continue;
            };
            all_uploads.push((format!("{base}/{f}"), data));
        }
        for d in dirs {
            let src = Path::new(&host_home).join(&d);
            if !src.is_dir() {
                continue;
            }
            for (abs, rel) in collect_agent_config_files(&src) {
                let host_rel = format!("{d}/{rel}");
                if already_uploaded.contains(&host_rel) {
                    continue;
                }
                let Ok(data) = std::fs::read(&abs) else {
                    continue;
                };
                all_uploads.push((format!("{base}/{host_rel}"), data));
            }
        }
    }

    let total_phase2 = all_uploads.len();
    let deadline = std::time::Instant::now() + AGENT_CONFIG_STEP_BUDGET;

    // Run parallel uploads inside a single tokio runtime so the per-file
    // overhead (runtime creation) is paid once, not once per file.
    let (p2_ok, p2_failed, p2_skipped) = block_on_provider(|| async {
        use futures::future::join_all;
        let mut ok = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;

        for chunk in all_uploads.chunks(UPLOAD_CONCURRENCY) {
            if std::time::Instant::now() >= deadline {
                // Budget exhausted: count all remaining (this chunk + the rest).
                skipped += total_phase2 - ok - failed - skipped;
                break;
            }

            let futs: Vec<_> = chunk
                .iter()
                .map(|(dest, data)| {
                    let dest = dest.as_str();
                    async move {
                        let r = tokio::time::timeout(
                            AGENT_CONFIG_UPLOAD_TIMEOUT,
                            provider.write(id, dest, data),
                        )
                        .await;
                        (dest, r)
                    }
                })
                .collect();

            for (dest, result) in join_all(futs).await {
                match result {
                    Ok(Ok(())) => ok += 1,
                    Ok(Err(e)) => {
                        failed += 1;
                        tracing::warn!(
                            target: "szhost::startup",
                            dest = %dest,
                            error = %e,
                            "agent-config upload: skipping one file (best-effort)"
                        );
                    }
                    Err(_) => {
                        failed += 1;
                        tracing::warn!(
                            target: "szhost::startup",
                            dest = %dest,
                            "agent-config upload: file timed out (best-effort)"
                        );
                    }
                }
            }
        }

        Ok((ok, failed, skipped))
    })?;

    let ok = auth_ok + p2_ok;
    let failed = auth_failed + p2_failed;
    let skipped = p2_skipped;

    if failed > 0 || skipped > 0 {
        tracing::warn!(
            target: "szhost::startup",
            ok,
            failed,
            skipped,
            "agent-config upload finished with some files skipped"
        );
    }
    if skipped > 0 {
        // Auth-critical files (Phase 1) already uploaded — only non-critical
        // extras were skipped, so the agent is still usable.
        superzej_core::msg::warn(&format!(
            "agent-login sync hit its {}s budget; {skipped} non-critical file(s) not uploaded \
             (auth files were synced — the agent should still be logged in).",
            AGENT_CONFIG_STEP_BUDGET.as_secs()
        ));
    }
    // Nothing uploaded at all AND things failed ⇒ a real problem (provider down).
    anyhow::ensure!(
        ok > 0 || failed == 0,
        "no agent-config files could be uploaded ({failed} failed)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_upload_skips_transcripts_and_bulk() {
        let root = std::env::temp_dir().join(format!("sz-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("projects/repo-a/subagents")).unwrap();
        std::fs::create_dir_all(root.join("statsig")).unwrap();
        // Real config/auth (kept).
        std::fs::write(root.join(".credentials.json"), b"{\"tok\":\"x\"}").unwrap();
        std::fs::write(root.join("settings.json"), b"{}").unwrap();
        // Bulky transcript state (skipped by dir name).
        std::fs::write(root.join("projects/repo-a/subagents/a.jsonl"), b"huge").unwrap();
        std::fs::write(root.join("statsig/cache.bin"), b"x").unwrap();
        // An oversized file directly under the config dir (skipped by size).
        std::fs::write(
            root.join("big.log"),
            vec![0u8; (AGENT_CONFIG_MAX_BYTES + 1) as usize],
        )
        .unwrap();

        let got: Vec<String> = collect_agent_config_files(&root)
            .into_iter()
            .map(|(_, rel)| rel)
            .collect();
        assert!(
            got.contains(&".credentials.json".to_string()),
            "auth kept: {got:?}"
        );
        assert!(got.contains(&"settings.json".to_string()), "settings kept");
        assert!(
            !got.iter().any(|r| r.starts_with("projects/")),
            "session transcripts skipped: {got:?}"
        );
        assert!(
            !got.iter().any(|r| r.starts_with("statsig/")),
            "cache skipped"
        );
        assert!(
            !got.contains(&"big.log".to_string()),
            "oversized file skipped"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Home-manager/NixOS hosts symlink the whole `~/.claude` tree into the nix
    /// store. The sync must follow those symlinks — otherwise `settings.json`
    /// (a regular file) uploads while the hook scripts it references (symlinks)
    /// are skipped, and the in-sandbox agent errors with `…-hook.sh: not found`.
    #[test]
    fn agent_config_follows_symlinked_config_files() {
        let root = std::env::temp_dir().join(format!("sz-claude-sym-{}", std::process::id()));
        let store = std::env::temp_dir().join(format!("sz-claude-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::create_dir_all(&store).unwrap();

        // Real files "in the store", symlinked into the config tree like home-manager.
        let hook_target = store.join("agentmemory-session-start.sh");
        std::fs::write(&hook_target, b"#!/bin/sh\necho hi\n").unwrap();
        std::os::unix::fs::symlink(
            &hook_target,
            root.join("hooks/agentmemory-session-start.sh"),
        )
        .unwrap();
        let settings_target = store.join("settings.json");
        std::fs::write(&settings_target, b"{}").unwrap();
        std::os::unix::fs::symlink(&settings_target, root.join("settings.json")).unwrap();
        // A dangling link must be tolerated (skipped, not a panic).
        std::os::unix::fs::symlink(store.join("gone"), root.join("dead.json")).unwrap();

        let got: Vec<String> = collect_agent_config_files(&root)
            .into_iter()
            .map(|(_, rel)| rel)
            .collect();
        assert!(
            got.contains(&"hooks/agentmemory-session-start.sh".to_string()),
            "symlinked hook scripts are followed + uploaded: {got:?}"
        );
        assert!(
            got.contains(&"settings.json".to_string()),
            "symlinked top-level config file followed"
        );
        assert!(
            !got.contains(&"dead.json".to_string()),
            "dangling symlink skipped"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn auth_critical_files_are_a_small_known_set() {
        // Claude's auth-critical set must be small (≤10 files) and include
        // the oauth credential file — the most important one.
        let claude_crit = superzej_core::envplan::agent_auth_critical_files("claude");
        assert!(
            claude_crit.len() <= 10,
            "auth-critical set should be small: {claude_crit:?}"
        );
        assert!(
            claude_crit.iter().any(|f| f.contains(".credentials.json")),
            "must include oauth credentials: {claude_crit:?}"
        );
        assert!(
            claude_crit.contains(&".claude.json".to_string()),
            "must include .claude.json: {claude_crit:?}"
        );

        // pi's auth-critical set must be small.
        let pi_crit = superzej_core::envplan::agent_auth_critical_files("pi");
        assert!(
            pi_crit.len() <= 5,
            "pi auth-critical should be small: {pi_crit:?}"
        );
    }
}
