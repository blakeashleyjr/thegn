//! Agent-login sync — upload coding agents' host config/credential files into a
//! provider sandbox so the agent (claude code, codex, pi, custom) is logged-in
//! there. Split out of `agent.rs` (god-file ratchet) as the "Sync agent logins"
//! provisioning step. Every network write is bounded (per-file timeout + a
//! whole-step wall-clock budget) so a hung provider fs endpoint can never strand
//! the loading screen — the classic sprite-launch hang.

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

/// Whole-step wall-clock budget for the agent-login sync. Even with each file
/// bounded, a large login tree of individually-slow files could still add up to
/// minutes of spinner; once this is spent we stop uploading the remaining files
/// (best-effort — the agent is still usable, it just may re-auth in-sprite) so
/// the sprite finishes coming up. Logged so the drop is visible.
const AGENT_CONFIG_STEP_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

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
/// Per-agent paths come from `envplan::agent_config_paths`; missing host paths
/// are skipped. Files go via the fs `write`. A genuine upload error (nothing got
/// through at all) aborts the step (surfaced on the splash); partial success is a
/// √ with logged warnings.
pub(crate) fn upload_agent_configs(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    sandbox_home: &str,
    agents: &[String],
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = sandbox_home.trim_end_matches('/');
    // Per-file BEST-EFFORT: one unwritable file (a transient `sprites write`
    // 5xx, an oversized/odd path) must not `?`-abort the whole step and paint a
    // red × over "Sync agent logins" while every other login uploaded fine. Warn
    // + continue per file; only fail the step if NOTHING got through.
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    // Whole-step deadline: once spent, remaining files are dropped best-effort so
    // a slow-per-file login tree can't hold the loading screen open forever.
    let deadline = std::time::Instant::now() + AGENT_CONFIG_STEP_BUDGET;
    let mut upload = |dest: String, data: &[u8]| {
        if std::time::Instant::now() >= deadline {
            skipped += 1;
            return;
        }
        // Bound each write so one hung request can't strand the step. The timeout
        // runs inside `block_on_provider`'s own runtime, so it fires even when the
        // provider's HTTP call itself never returns.
        match block_on_provider(|| async {
            match tokio::time::timeout(AGENT_CONFIG_UPLOAD_TIMEOUT, provider.write(id, &dest, data))
                .await
            {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "upload timed out after {}s",
                    AGENT_CONFIG_UPLOAD_TIMEOUT.as_secs()
                )),
            }
        }) {
            Ok(()) => ok += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(
                    target: "szhost::startup",
                    dest = %dest,
                    error = %e,
                    "agent-config upload: skipping one file (best-effort)"
                );
            }
        }
    };
    for agent in agents {
        let (files, dirs) = superzej_core::envplan::agent_config_paths(agent);
        for f in files {
            let src = Path::new(&host_home).join(&f);
            let Ok(data) = std::fs::read(&src) else {
                continue;
            };
            upload(format!("{base}/{f}"), &data);
        }
        for d in dirs {
            let src = Path::new(&host_home).join(&d);
            if !src.is_dir() {
                continue;
            }
            // Upload only the auth/config files, NOT the agent's bulky session
            // state (transcripts/caches) — see `collect_agent_config_files`.
            for (abs, rel) in collect_agent_config_files(&src) {
                let Ok(data) = std::fs::read(&abs) else {
                    continue;
                };
                upload(format!("{base}/{d}/{rel}"), &data);
            }
        }
    }
    if failed > 0 || skipped > 0 {
        tracing::warn!(
            target: "szhost::startup",
            ok, failed, skipped,
            "agent-config upload finished with some files skipped"
        );
    }
    if skipped > 0 {
        superzej_core::msg::warn(&format!(
            "agent-login sync hit its {}s budget; {skipped} file(s) not uploaded \
             — the agent may need to re-authenticate in the sandbox.",
            AGENT_CONFIG_STEP_BUDGET.as_secs()
        ));
    }
    // Nothing uploaded but files failed ⇒ a real problem (provider down / auth) —
    // surface it. A partial success is a √ with logged warnings.
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
}
