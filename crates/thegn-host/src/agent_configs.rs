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

use crate::agent::{block_on_provider, provision_step_timeout};
use std::path::Path;

/// Whether a resolved file is executable for anyone (unix mode `& 0o111`). The
/// agent-login sync must preserve this: `~/.claude/hooks/*.sh` are executable
/// scripts, and the sprites fs API defaults a plain `write` to mode `0644`, so
/// a hook uploaded via `write` lands non-executable and Claude Code fails with
/// `…/agentmemory-*.sh: Permission denied`. Non-unix hosts have no exec bit.
#[cfg(unix)]
fn is_executable(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_executable(_md: &std::fs::Metadata) -> bool {
    false
}

/// Upload `data` to `dest` in the sandbox, preserving the executable bit: an
/// executable source (a hook script) goes via `write_exec` (mode 0755) so it's
/// runnable in-sandbox; everything else via `write` (0644).
async fn write_preserving_mode(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    dest: &str,
    data: &[u8],
    exec: bool,
) -> anyhow::Result<()> {
    if exec {
        provider.write_exec(id, dest, data).await
    } else {
        provider.write(id, dest, data).await
    }
}

/// A Claude Code settings file that can carry a `hooks` block.
fn is_claude_settings(rel: &str) -> bool {
    rel.ends_with(".claude/settings.json") || rel.ends_with(".claude/settings.local.json")
}

/// Strip the host's `hooks` block from a Claude `settings.json`/`settings.local.json`
/// before it lands in a sandbox. Host hooks shell out to host-only services (the
/// agentmemory backend, the desktop `notify.sh`) whose scripts/paths don't function
/// in the VM, so leaving them in only fires `…: not found` / hook errors on every
/// Claude event inside the sprite (the reported SessionStart/PostToolUse/Stop
/// failures). No-op for non-settings files, non-JSON-object content, or settings
/// with no `hooks`. Pure — unit-tested.
fn strip_sandbox_hooks(rel: &str, data: Vec<u8>) -> Vec<u8> {
    if !is_claude_settings(rel) {
        return data;
    }
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_slice(&data) else {
        return data;
    };
    if obj.remove("hooks").is_none() {
        return data;
    }
    match serde_json::to_vec_pretty(&serde_json::Value::Object(obj)) {
        Ok(mut b) => {
            b.push(b'\n');
            b
        }
        Err(_) => data,
    }
}

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

/// Per-write ceiling for the small non-`Exec` provisioning transfers (dotfiles,
/// atuin creds): a KB-sized write that hasn't returned this quickly is a wedged
/// endpoint, not slow progress. Mirrors `AGENT_CONFIG_UPLOAD_TIMEOUT`.
const PROVISION_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Run a provider await under a hard `to` ceiling (`label` names it in the error):
/// a stalled/half-open fs or exec endpoint can leave a non-`Exec` provisioning
/// step (dotfiles/atuin/checkpoint/devshell push) blocked forever — those don't
/// get the per-step `tokio::time::timeout` the caller wraps around `Exec` steps,
/// so wrap them here so the loading screen can't freeze. `Err` on expiry.
pub(crate) fn with_provision_timeout<T, Fut>(
    label: &str,
    to: std::time::Duration,
    f: impl FnOnce() -> Fut + Send,
) -> anyhow::Result<T>
where
    T: Send,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    block_on_provider(|| async move {
        match tokio::time::timeout(to, f()).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("{label} timed out after {}s", to.as_secs())),
        }
    })
}

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
pub(crate) fn collect_agent_config_files(dir: &Path) -> Vec<(std::path::PathBuf, String, bool)> {
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
                    out.push((
                        path.clone(),
                        rel.to_string_lossy().replace('\\', "/"),
                        is_executable(&md),
                    ));
                }
            }
        }
    }
    out
}

/// Upload just the auth-critical files (Phase 1) for `agents` into the sandbox
/// `$HOME` at `base`, preserving the executable bit. Returns `(ok, failed)`.
/// Small (~5 tiny files per agent) + best-effort — the shared core of the
/// initial login sync and the connect-time [`resync_agent_auth`] refresh.
/// SOURCE path for an agent-config path `rel` (relative to `$HOME`): re-rooted
/// at the provider's relocated credential home (`CLAUDE_CONFIG_DIR` /
/// `CODEX_HOME`) when set. The host's LIVE login may be a relocated home (e.g.
/// a claude-profiles dir) while the default `~/.claude` fossilizes — the login
/// sync uploading the fossil gave every sprite a stale identity. The sandbox
/// TARGET keeps the default rel path (in-sandbox agents run without the
/// relocation env).
fn agent_source_path(host_home: &str, rel: &str) -> std::path::PathBuf {
    relocated_source(rel, &|k| std::env::var(k).ok())
        .unwrap_or_else(|| Path::new(host_home).join(rel))
}

/// The relocation core, pure over an env lookup (unit-tested without touching
/// process env). `None` ⇒ no provider relocates `rel` (use `$HOME/<rel>`).
fn relocated_source(rel: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<std::path::PathBuf> {
    for p in thegn_core::account::providers() {
        let Some(root) = env(p.home_env)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|v| std::path::PathBuf::from(thegn_core::util::expand_tilde(&v)))
        else {
            continue;
        };
        if rel == p.default_dir {
            return Some(root);
        }
        if let Some(rest) = rel.strip_prefix(p.default_dir)
            && let Some(rest) = rest.strip_prefix('/')
        {
            return Some(root.join(rest));
        }
        // claude's top-level `.claude.json` rides beside the config dir: with
        // `CLAUDE_CONFIG_DIR` set claude keeps it INSIDE that dir; a
        // claude-profiles home keeps it at the profile root (the dir's
        // parent). First existing wins; neither ⇒ fall back to `$HOME`.
        if p.id == "claude" && rel == ".claude.json" {
            let inside = root.join(".claude.json");
            if inside.is_file() {
                return Some(inside);
            }
            if let Some(beside) = root.parent().map(|d| d.join(".claude.json"))
                && beside.is_file()
            {
                return Some(beside);
            }
        }
    }
    None
}

/// Push the host's git identity (`user.name`/`user.email`, effective repo→global
/// values) into a provider sandbox's provisioning exec env as
/// `THEGN_GIT_NAME`/`THEGN_GIT_EMAIL`, so `git_auth_script` can persist them to
/// the sandbox `~/.gitconfig` — otherwise in-sandbox commits are authored as the
/// provider default (`Sprite <noreply@sprites.dev>`). No-op for an unset key.
// off-loop by contract: only called from the blocking provisioning path.
#[expect(clippy::disallowed_methods)]
pub(crate) fn push_host_git_identity(repo_root: &Path, exec_env: &mut Vec<(String, String)>) {
    for (var, key) in [
        ("THEGN_GIT_NAME", "user.name"),
        ("THEGN_GIT_EMAIL", "user.email"),
    ] {
        let out = thegn_core::util::git_cmd(repo_root)
            .args(["config", "--get", key])
            .output();
        if let Ok(o) = out
            && o.status.success()
        {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !v.is_empty() {
                exec_env.push((var.to_string(), v));
            }
        }
    }
}

fn sync_agent_auth_critical(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    base: &str,
    agents: &[String],
) -> (usize, usize) {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let mut ok = 0usize;
    let mut failed = 0usize;
    for agent in agents {
        for f in thegn_core::envplan::agent_auth_critical_files(agent) {
            let src = agent_source_path(&host_home, &f);
            let Ok(data) = std::fs::read(&src) else {
                continue; // file absent on this host — skip silently
            };
            let data = strip_sandbox_hooks(&f, data);
            let exec = std::fs::metadata(&src)
                .map(|md| is_executable(&md))
                .unwrap_or(false);
            let dest = format!("{base}/{f}");
            match block_on_provider(|| async {
                match tokio::time::timeout(
                    AGENT_CONFIG_UPLOAD_TIMEOUT,
                    write_preserving_mode(provider, id, &dest, &data, exec),
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
                Ok(()) => ok += 1,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(
                        target: "thegn::startup",
                        dest = %dest,
                        error = %e,
                        "agent-config auth-critical upload failed (best-effort)"
                    );
                }
            }
        }
    }
    (ok, failed)
}

/// Re-sync ONLY the auth-critical files into an already-provisioned sandbox, so
/// a rotated host OAuth token is refreshed. Claude Code stores its subscription
/// token in `~/.claude/.credentials.json` (accessToken + rotating refreshToken +
/// expiresAt); the host rewrites it on refresh, so the sandbox's provision-time
/// snapshot goes stale and the in-sandbox agent 401s ("Invalid authentication
/// credentials") despite reporting "logged in". Runs on every provider bring-up
/// (even the cached path where full provisioning short-circuits on its marker) —
/// cheap (~5 tiny files) + best-effort. Resolves the sandbox `$HOME` and the
/// agent kinds itself so the caller stays a one-liner.
pub(crate) fn resync_agent_auth(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    cfg: &thegn_core::config::Config,
    worktree: &str,
    env_name: &str,
) {
    use thegn_core::store::WorkspaceStore;
    // Agent kinds: mirror `run_provisioning` — an explicit `[sandbox.home]
    // agents` list wins, else the `[[agents]]` picker, else host detection.
    let loc = thegn_core::remote::GitLoc::for_worktree(Path::new(worktree));
    let repo_root = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let home = cfg
        .resolve_env(&repo_root, &loc, Path::new(worktree), Some(env_name))
        .sandbox
        .home;
    let agents = if !home.agents.is_empty() {
        home.agents
    } else {
        let picker = crate::agent::provisioned_agent_kinds(cfg);
        if picker.is_empty() {
            crate::agent::detect_host_agents()
        } else {
            picker
        }
    };
    if agents.is_empty() {
        return;
    }
    // Resolve the sandbox `$HOME` (where the auth files must land) the same way
    // `run_provisioning` does — a tiny non-tty exec.
    let sprite_home = block_on_provider(|| async {
        provider
            .run_exec(
                id,
                &[
                    "/bin/sh".to_string(),
                    "-lc".to_string(),
                    "printf %s \"$HOME\"".to_string(),
                ],
                None,
                &[],
            )
            .await
    })
    .ok()
    .filter(|(code, out)| *code == 0 && !out.trim().is_empty())
    .map(|(_, out)| out.trim().to_string())
    .unwrap_or_else(|| "/root".to_string());
    let (ok, failed) =
        sync_agent_auth_critical(provider, id, sprite_home.trim_end_matches('/'), &agents);
    tracing::debug!(
        target: "thegn::startup",
        id, ok, failed,
        "connect-time agent-auth re-sync (refresh rotated OAuth token)"
    );
}

/// Upload coding agents' host config/credential dirs into the sandbox `$HOME`
/// (`/root`) so the agent (claude code, codex, custom) is logged-in there.
///
/// See the module-level doc for the two-phase strategy: Phase 1 (auth-critical,
/// always) then Phase 2 (full tree, parallel, budget-capped).
pub(crate) fn upload_agent_configs(
    provider: &thegn_svc::provider::Provider,
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
    let (auth_ok, auth_failed) = sync_agent_auth_critical(provider, id, base, agents);

    // -----------------------------------------------------------------------
    // Phase 2: full config tree — walk directories, skip bulky-state dirs,
    // upload remaining files in parallel batches, stop when budget expires.
    // Auth-critical files already uploaded above are deduplicated.
    // -----------------------------------------------------------------------

    // Build the full upload list synchronously before entering async.
    // Dedup against the auth-critical set so we don't double-upload.
    let mut already_uploaded: std::collections::HashSet<String> = std::collections::HashSet::new();
    for agent in agents {
        for f in thegn_core::envplan::agent_auth_critical_files(agent) {
            already_uploaded.insert(f);
        }
    }

    let mut all_uploads: Vec<(String, Vec<u8>, bool)> = Vec::new();
    for agent in agents {
        let (files, dirs) = thegn_core::envplan::agent_config_paths(agent);
        for f in files {
            if already_uploaded.contains(&f) {
                continue;
            }
            let src = agent_source_path(&host_home, &f);
            let Ok(data) = std::fs::read(&src) else {
                continue;
            };
            let data = strip_sandbox_hooks(&f, data);
            let exec = std::fs::metadata(&src)
                .map(|md| is_executable(&md))
                .unwrap_or(false);
            all_uploads.push((format!("{base}/{f}"), data, exec));
        }
        for d in dirs {
            let src = agent_source_path(&host_home, &d);
            if !src.is_dir() {
                continue;
            }
            for (abs, rel, exec) in collect_agent_config_files(&src) {
                let host_rel = format!("{d}/{rel}");
                if already_uploaded.contains(&host_rel) {
                    continue;
                }
                let Ok(data) = std::fs::read(&abs) else {
                    continue;
                };
                let data = strip_sandbox_hooks(&host_rel, data);
                all_uploads.push((format!("{base}/{host_rel}"), data, exec));
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
                .map(|(dest, data, exec)| {
                    let dest = dest.as_str();
                    async move {
                        let r = tokio::time::timeout(
                            AGENT_CONFIG_UPLOAD_TIMEOUT,
                            write_preserving_mode(provider, id, dest, data, *exec),
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
                            target: "thegn::startup",
                            dest = %dest,
                            error = %e,
                            "agent-config upload: skipping one file (best-effort)"
                        );
                    }
                    Err(_) => {
                        failed += 1;
                        tracing::warn!(
                            target: "thegn::startup",
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
            target: "thegn::startup",
            ok,
            failed,
            skipped,
            "agent-config upload finished with some files skipped"
        );
    }
    if skipped > 0 {
        // Auth-critical files (Phase 1) already uploaded — only non-critical
        // extras were skipped, so the agent is still usable.
        thegn_core::msg::warn(&format!(
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

struct AccountUpload {
    provider_id: String,
    dest_root: String,
    stage_root: String,
    files: Vec<(String, Vec<u8>, bool)>,
    read_failure: Option<String>,
}

fn account_stage_root(parent: &str, identity: &str, nonce: &str) -> String {
    format!("{parent}/.{identity}.thegn-stage-{nonce}")
}

fn account_upload_ready(
    expected: usize,
    written: usize,
    failed: bool,
    within_budget: bool,
) -> bool {
    !failed && expected == written && within_budget
}

fn account_shell_quote(path: &str) -> String {
    let quoted = thegn_core::util::sh_quote(path);
    if quoted == path {
        format!("'{quoted}'")
    } else {
        quoted
    }
}

fn account_publish_script(dest_root: &str, stage_root: &str) -> String {
    let state_dirs = AGENT_STATE_SKIP_DIRS
        .iter()
        .map(|name| account_shell_quote(name))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "set -u; exec 2>&1; d={}; s={}; o=\"$d.thegn-old-$$\"; \
         restore() {{ for n in {state_dirs}; do \
             if [ -e \"$s/$n\" ] || [ -L \"$s/$n\" ]; then mv -- \"$s/$n\" \"$o/$n\" || :; fi; \
         done; if [ -e \"$o\" ] || [ -L \"$o\" ]; then mv -- \"$o\" \"$d\" || :; fi; }}; \
         [ -d \"$s\" ] && [ ! -L \"$s\" ] || exit 41; \
         if [ -e \"$d\" ] || [ -L \"$d\" ]; then mv -- \"$d\" \"$o\" || exit 42; fi; \
         for n in {state_dirs}; do \
             if [ -d \"$o/$n\" ] && [ ! -L \"$o/$n\" ] && [ ! -e \"$s/$n\" ]; then \
                 mv -- \"$o/$n\" \"$s/$n\" || {{ restore; exit 45; }}; \
             fi; \
         done; \
         if ! mv -- \"$s\" \"$d\"; then restore; exit 43; fi; \
         rm -rf -- \"$o\" 2>&1 || exit 44",
        account_shell_quote(dest_root),
        account_shell_quote(stage_root),
    )
}

fn account_discard_script(stage_root: &str) -> String {
    format!(
        "exec 2>&1; rm -rf -- {}",
        thegn_core::util::sh_quote(stage_root)
    )
}

fn account_cleanup_timeout(deadline: std::time::Instant) -> Option<std::time::Duration> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    (remaining > std::time::Duration::ZERO).then_some(AGENT_CONFIG_UPLOAD_TIMEOUT.min(remaining))
}

fn account_publish_timeout(deadline: std::time::Instant) -> std::time::Duration {
    AGENT_CONFIG_UPLOAD_TIMEOUT.min(
        deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(std::time::Duration::from_secs(5)),
    )
}

fn account_file_vanished(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

/// Deploy each declared coding-agent ACCOUNT's credential home into the sandbox
/// under a stable per-account path (`$HOME/.thegn/accounts/<provider>/<slug>/`),
/// so a provider pane can point the agent's home env var (`CLAUDE_CONFIG_DIR` /
/// `CODEX_HOME`) at the selected one (see [`account_pane_env_exports`]). This is
/// how client-side account switching (`[[accounts]]`) reaches a REMOTE provider:
/// path-preserving bind-mounting the host dir (what the local path does) can't
/// cross to a sprite, so we upload the dir's contents instead. Each account is
/// staged separately and its remote home is published all-or-nothing only after
/// every file succeeds within the step budget. Only *authed* accounts are
/// shipped. Best-effort + budget-bounded; a missing/unreadable dir is skipped.
/// No-op when no `[[accounts]]`/managed accounts exist — the default
/// `~/.claude` upload in [`upload_agent_configs`] still covers the single-home case.
pub(crate) fn upload_agent_accounts(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    sprite_home: &str,
    cfg: &thegn_core::config::Config,
) {
    let Ok(db) = thegn_core::db::Db::open() else {
        return;
    };
    let base = sprite_home.trim_end_matches('/');
    let mut uploads: Vec<AccountUpload> = Vec::new();
    for p in thegn_core::account::providers() {
        for acct in thegn_core::account::list(cfg, &db, p.id) {
            // Only ship accounts that are actually logged in (auth marker present)
            // and whose dir exists — an un-authed managed dir would just 401 too.
            if !acct.authed || !acct.dir.is_dir() {
                continue;
            }
            let identity = thegn_core::account::home_component(&acct.name);
            let dest_root = format!("{base}/.thegn/accounts/{}/{}", p.id, identity);
            let parent = format!("{base}/.thegn/accounts/{}", p.id);
            let mut nonce = [0_u8; 8];
            if let Err(error) = getrandom::fill(&mut nonce) {
                tracing::warn!(target: "thegn::startup", provider = p.id, account = %identity, reason = %error, "account home not published; previous generation kept");
                continue;
            }
            let nonce = nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let stage_root = account_stage_root(&parent, &identity, &nonce);
            let is_claude = p.id == "claude";
            let mut files = Vec::new();
            let mut read_failure = None;
            for (abs, rel, exec) in collect_agent_config_files(&acct.dir) {
                match std::fs::read(&abs) {
                    Ok(data) => {
                        // Account settings have a bare rel; normalize before stripping
                        // host-only hooks, as in the prior account upload path.
                        let data = if is_claude {
                            strip_sandbox_hooks(&format!(".claude/{rel}"), data)
                        } else {
                            data
                        };
                        files.push((rel, data, exec));
                    }
                    Err(error) if account_file_vanished(&error) => {}
                    Err(error) => {
                        read_failure = Some(format!("read {}: {error}", abs.display()));
                        break;
                    }
                }
            }
            if !files.is_empty() || read_failure.is_some() {
                uploads.push(AccountUpload {
                    provider_id: p.id.to_string(),
                    dest_root,
                    stage_root,
                    files,
                    read_failure,
                });
            }
        }
    }
    if uploads.is_empty() {
        return;
    }
    let deadline = std::time::Instant::now() + AGENT_CONFIG_STEP_BUDGET;
    let mut written_by_upload = vec![0; uploads.len()];
    let result = block_on_provider(|| async {
        for (upload_index, upload) in uploads.iter().enumerate() {
            let identity = upload.dest_root.rsplit('/').next().unwrap_or("unknown");
            let mut failure = upload.read_failure.clone();
            let mut written = 0;
            if failure.is_none() {
                for (rel, data, exec) in &upload.files {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        failure = Some("account upload step budget exhausted".to_string());
                        break;
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let write_timeout = AGENT_CONFIG_UPLOAD_TIMEOUT.min(remaining);
                    match tokio::time::timeout(
                        write_timeout,
                        write_preserving_mode(
                            provider,
                            id,
                            &format!("{}/{rel}", upload.stage_root),
                            data,
                            *exec,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(())) => {
                            written += 1;
                            written_by_upload[upload_index] = written;
                        }
                        Ok(Err(error)) => {
                            failure = Some(format!("write {rel}: {error}"));
                            break;
                        }
                        Err(_) => {
                            failure = Some(format!("write {rel} timed out"));
                            break;
                        }
                    }
                }
            }
            if account_upload_ready(
                upload.files.len(),
                written,
                failure.is_some(),
                std::time::Instant::now() < deadline,
            ) {
                let script = account_publish_script(&upload.dest_root, &upload.stage_root);
                let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script];
                let publish_timeout = account_publish_timeout(deadline);
                match tokio::time::timeout(publish_timeout, provider.run_exec(id, &argv, None, &[]))
                    .await
                {
                    Ok(Ok((0, _))) => continue,
                    Ok(Ok((code, output))) => {
                        let tail = output
                            .chars()
                            .rev()
                            .take(512)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect::<String>();
                        failure = Some(format!("publish exited {code}: {tail}"));
                    }
                    Ok(Err(error)) => failure = Some(format!("publish failed: {error}")),
                    Err(_) => failure = Some("publish outcome unknown: timed out".to_string()),
                }
            } else if failure.is_none() {
                failure = Some("account upload incomplete or step budget exhausted".to_string());
            }
            let reason = failure.unwrap_or_else(|| "account upload failed".to_string());
            let Some(cleanup_timeout) = account_cleanup_timeout(deadline) else {
                tracing::warn!(target: "thegn::startup", provider = %upload.provider_id, account = %identity, reason = %reason, "account home publish failed or outcome unknown");
                continue;
            };
            if written == 0 {
                tracing::warn!(target: "thegn::startup", provider = %upload.provider_id, account = %identity, reason = %reason, "account home publish failed or outcome unknown");
                continue;
            }
            let discard_argv = vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                account_discard_script(&upload.stage_root),
            ];
            let reason = match tokio::time::timeout(
                cleanup_timeout,
                provider.run_exec(id, &discard_argv, None, &[]),
            )
            .await
            {
                Ok(Ok((0, _))) => reason,
                Ok(Ok((code, output))) => {
                    format!("{reason}; stage cleanup exited {code}: {output}")
                }
                Ok(Err(error)) => format!("{reason}; stage cleanup failed: {error}"),
                Err(_) => format!("{reason}; stage cleanup timed out"),
            };
            tracing::warn!(target: "thegn::startup", provider = %upload.provider_id, account = %identity, reason = %reason, "account home publish failed or outcome unknown");
        }
        Ok::<(), anyhow::Error>(())
    });
    if let Err(error) = result {
        for (upload_index, upload) in uploads.iter().enumerate() {
            if written_by_upload[upload_index] == 0 {
                continue;
            }
            let Some(cleanup_timeout) = account_cleanup_timeout(deadline) else {
                tracing::warn!(target: "thegn::startup", provider = %upload.provider_id, account = %upload.dest_root.rsplit('/').next().unwrap_or("unknown"), reason = %error, "account home publish failed or outcome unknown");
                continue;
            };
            let identity = upload.dest_root.rsplit('/').next().unwrap_or("unknown");
            let discard_argv = vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                account_discard_script(&upload.stage_root),
            ];
            let reason = match block_on_provider(|| async {
                match tokio::time::timeout(
                    cleanup_timeout,
                    provider.run_exec(id, &discard_argv, None, &[]),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!("stage cleanup timed out")),
                }
            }) {
                Ok((0, _)) => error.to_string(),
                Ok((code, output)) => format!("{error}; stage cleanup exited {code}: {output}"),
                Err(cleanup_error) => format!("{error}; stage cleanup failed: {cleanup_error}"),
            };
            tracing::warn!(target: "thegn::startup", provider = %upload.provider_id, account = %identity, reason = %reason, "account home publish failed or outcome unknown");
        }
    }
}

/// The `export <HOME_ENV>="$HOME/.thegn/accounts/<provider>/<slug>"; …` prefix to
/// splice ahead of a PROVIDER pane's inner command, pointing each agent CLI at the
/// ACTIVE account deployed by [`upload_agent_accounts`]. `$HOME` is expanded
/// in-sprite (the sandbox login `$HOME` isn't known host-side, so we can't bake an
/// absolute path). Empty when no account is active for any provider — the pane
/// then uses the agent's default `~/.<agent>` home (backwards-compatible).
pub(crate) fn account_pane_env_exports(cfg: &thegn_core::config::Config, worktree: &str) -> String {
    use thegn_core::store::WorkspaceStore;
    let Ok(db) = thegn_core::db::Db::open() else {
        return String::new();
    };
    let repo_root = db
        .repo_root_for(worktree)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| Path::new(worktree).to_path_buf());
    let slug = crate::agent::repo_slug(&db, &repo_root);
    let mut out = String::new();
    for p in thegn_core::account::providers() {
        let Some(name) =
            thegn_core::account::active_name(cfg, &db, worktree, slug.as_deref(), p.id)
        else {
            continue;
        };
        out.push_str(&account_export(p.home_env, p.id, &name));
    }
    out
}

fn account_export(home_env: &str, provider_id: &str, name: &str) -> String {
    format!(
        "export {home_env}=\"$HOME/.thegn/accounts/{provider_id}/{}\"; ",
        thegn_core::account::home_component(name),
    )
}

/// Upload the present host dotfiles/dotdirs into the sandbox's `$HOME` (`/root`).
/// A basename that's a FILE is uploaded via the fs `write`; a DIRECTORY (e.g.
/// `.config/gcloud`, `.aws`) is uploaded recursively via `upload_dir` — so cloud
/// creds and multi-file config carry over too. Missing host paths are skipped; a
/// genuine upload failure aborts the step. Each transfer is bounded: a stalled/
/// half-open fs endpoint would otherwise hang the loading screen forever (only
/// `Exec` steps get a per-step ceiling from the caller).
pub(crate) fn upload_dotfiles(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    sandbox_home: &str,
    files: &[String],
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = sandbox_home.trim_end_matches('/');
    for name in files {
        let src = Path::new(&host_home).join(name);
        let dest = format!("{base}/{name}");
        if src.is_dir() {
            with_provision_timeout("dotfiles upload", PROVISION_WRITE_TIMEOUT, || {
                provider.upload_dir(id, &src, &dest)
            })?;
        } else if let Ok(data) = std::fs::read(&src) {
            with_provision_timeout("dotfile write", PROVISION_WRITE_TIMEOUT, || {
                provider.write(id, &dest, &data)
            })?;
        }
    }
    Ok(())
}

/// Carry the host's atuin credentials + config into the sandbox so its shell
/// history joins atuin's own sync (host ↔ sprites). Opt-in (`[sandbox.home]
/// atuin = true`). Uploads the dereferenced `~/.config/atuin/config.toml` (the
/// home-manager `/nix/store` symlink is read THROUGH, so the real bytes land, not
/// a dangling link) + the auth/encryption files `~/.local/share/atuin/{key,
/// session}`. The history DBs are deliberately NOT copied — atuin's sync server
/// reconciles those. Best-effort: a missing source is skipped (only `key` and no
/// `session` is a normal state); a genuine upload error aborts (surfaced as a
/// best-effort step failure). Warns when there's nothing to carry.
pub(crate) fn upload_atuin_creds(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    sandbox_home: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = sandbox_home.trim_end_matches('/');
    // Config first, then the auth/encryption state. `provider.write` creates parent
    // dirs (mkdirParents), so the nested `.config/atuin` / `.local/share/atuin`
    // paths land without an explicit mkdir.
    //
    // `meta.db` is the SERVER-AUTH carrier: atuin >=18 keeps the sync session token
    // (the `hub_session` bearer for the sync server) in `meta.db`, NOT in a flat
    // `session` file (which modern atuin no longer writes). Without it the sandbox
    // has the encryption `key` but is logged OUT, so `auto_sync` can't authenticate
    // and Ctrl-R stays empty. We still carry `session` too (older atuin / other
    // hosts may have it) and deliberately skip the heavy history/records DBs — the
    // server reconciles those once authenticated. `meta.db` is small (~28K).
    let rels = [
        ".config/atuin/config.toml",
        ".local/share/atuin/key",
        ".local/share/atuin/session",
        ".local/share/atuin/meta.db",
    ];
    let mut carried = 0usize;
    let mut token_carried = false;
    for rel in rels {
        let src = Path::new(&host_home).join(rel);
        // `read` dereferences the symlink → the real bytes (the HM config.toml is a
        // `/nix/store` symlink that would dangle in the sandbox).
        if let Ok(data) = std::fs::read(&src) {
            let dest = format!("{base}/{rel}");
            with_provision_timeout("atuin cred write", PROVISION_WRITE_TIMEOUT, || {
                provider.write(id, &dest, &data)
            })?;
            carried += 1;
            if rel.ends_with("meta.db") || rel.ends_with("session") {
                token_carried = true;
            }
        }
    }
    if carried == 0 {
        thegn_core::msg::warn(
            "atuin sync: no host atuin config/credentials found (~/.config/atuin, \
             ~/.local/share/atuin) — nothing to carry.",
        );
        return Ok(());
    }
    // Prime history at provision time so it's baked into the checkpoint and Ctrl-R
    // is populated the instant the pane opens (instead of waiting for the first
    // `auto_sync` tick). `sync -f` forces a full reconcile regardless of the carried
    // last-sync throttle, pulling the server's records into the sandbox's empty
    // store. Best-effort: a sync failure (offline, server hiccup) just means history
    // fills in on the next auto_sync. Skipped when no auth token was carried. The
    // exec is bounded so a lost exit frame can't wedge the loading screen.
    if token_carried {
        let argv = vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "export PATH=\"$HOME/.local/bin:$HOME/.nix-profile/bin:$PATH\"; \
             command -v atuin >/dev/null 2>&1 && atuin sync -f 2>&1 || true"
                .to_string(),
        ];
        if let Err(e) =
            with_provision_timeout("atuin sync", provision_step_timeout("atuin"), || {
                provider.run_exec(id, &argv, None, exec_env)
            })
        {
            thegn_core::msg::warn(&format!(
                "atuin sync: priming history failed ({e}); it will fill in on auto_sync."
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_exports_use_distinct_lossless_home_components() {
        let first = account_export("CODEX_HOME", "codex", "work/a");
        let second = account_export("CODEX_HOME", "codex", "work-a");
        assert_ne!(first, second);
    }

    #[test]
    fn strip_sandbox_hooks_drops_hooks_from_claude_settings() {
        let settings = br#"{"model":"opus","hooks":{"Stop":[{"hooks":[]}]}}"#.to_vec();
        // Claude settings.json ⇒ hooks removed, other keys kept.
        let out = strip_sandbox_hooks(".claude/settings.json", settings.clone());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("hooks").is_none(), "hooks must be stripped");
        assert_eq!(v.get("model").and_then(|m| m.as_str()), Some("opus"));
        // settings.local.json too.
        let out = strip_sandbox_hooks(".claude/settings.local.json", settings.clone());
        assert!(!String::from_utf8_lossy(&out).contains("hooks"));
        // Non-settings files pass through untouched (byte-identical).
        assert_eq!(
            strip_sandbox_hooks(".claude/.credentials.json", settings.clone()),
            settings
        );
        // Settings with no hooks pass through unchanged.
        let no_hooks = br#"{"model":"opus"}"#.to_vec();
        assert_eq!(
            strip_sandbox_hooks(".claude/settings.json", no_hooks.clone()),
            no_hooks
        );
        // Non-JSON content is left as-is (never corrupts an odd file).
        let junk = b"not json".to_vec();
        assert_eq!(
            strip_sandbox_hooks(".claude/settings.json", junk.clone()),
            junk
        );
    }

    #[test]
    fn account_dir_settings_normalizes_before_strip() {
        // An account dir IS the claude config dir, so `collect_agent_config_files`
        // yields a BARE `settings.json` rel — which the raw matcher does NOT
        // recognize, so an unnormalized strip would ship the host `hooks` intact
        // (the account-switch regression). `upload_agent_accounts` prefixes
        // `.claude/` for claude accounts; assert both halves of that contract.
        let settings = br#"{"model":"opus","hooks":{"Stop":[{"hooks":[]}]}}"#.to_vec();
        // Bare rel is not matched — proves the normalization is load-bearing.
        assert!(!is_claude_settings("settings.json"));
        assert_eq!(
            strip_sandbox_hooks("settings.json", settings.clone()),
            settings
        );
        // Normalized rel (what upload_agent_accounts passes) DOES strip.
        let out = strip_sandbox_hooks(&format!(".claude/{}", "settings.json"), settings.clone());
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("hooks").is_none(), "hooks must be stripped");
        assert_eq!(v.get("model").and_then(|m| m.as_str()), Some("opus"));
        // A NESTED account file (`hooks/foo.sh` → `.claude/hooks/foo.sh`) is not a
        // settings file, so it rides through untouched — only top-level settings
        // carry the hooks block.
        assert!(!is_claude_settings(".claude/hooks/foo.sh"));
    }

    #[test]
    fn with_provision_timeout_bounds_a_hung_await() {
        use std::time::Duration;
        // A provider await that never completes (the wedged fs/exec endpoint) must
        // be abandoned at the ceiling with a "timed out" error, not hang forever —
        // the whole point of wrapping the non-Exec provisioning steps.
        let r: anyhow::Result<()> =
            with_provision_timeout("stall", Duration::from_millis(50), || async {
                futures::future::pending::<anyhow::Result<()>>().await
            });
        let e = r.expect_err("a pending future must time out");
        assert!(e.to_string().contains("timed out"), "got: {e}");
        assert!(e.to_string().contains("stall"), "label surfaced: {e}");
        // A prompt await passes its result through unchanged.
        let ok: anyhow::Result<u32> =
            with_provision_timeout("quick", Duration::from_secs(5), || async { Ok(7) });
        assert_eq!(ok.unwrap(), 7);
    }

    #[test]
    fn agent_source_relocates_via_home_env() {
        // test code: fixture setup, never on the event loop.
        let tmp = std::env::temp_dir().join(format!("tg-reloc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let dir = tmp.join("profile/.claude");
        std::fs::create_dir_all(&dir).unwrap();
        // Profile-home shape: `.claude.json` beside the config dir.
        std::fs::write(tmp.join("profile/.claude.json"), b"{}").unwrap();
        let dir_s = dir.to_string_lossy().into_owned();
        let env = move |k: &str| (k == "CLAUDE_CONFIG_DIR").then(|| dir_s.clone());
        // The dir + its children re-root at the relocated home.
        assert_eq!(relocated_source(".claude", &env), Some(dir.clone()));
        assert_eq!(
            relocated_source(".claude/.credentials.json", &env),
            Some(dir.join(".credentials.json"))
        );
        // Top-level `.claude.json` found beside the relocated dir.
        assert_eq!(
            relocated_source(".claude.json", &env),
            Some(tmp.join("profile/.claude.json"))
        );
        // Unrelated paths / unset providers stay on `$HOME`.
        assert_eq!(relocated_source(".config/claude/x", &env), None);
        assert_eq!(relocated_source(".codex/auth.json", &env), None);
        assert_eq!(relocated_source(".claude", &|_| None), None);
        // Prefix must be a path component: `.claudeX` is not `.claude`.
        assert_eq!(relocated_source(".claudeX/y", &env), None);
        let _ = std::fs::remove_dir_all(&tmp); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn agent_config_upload_skips_transcripts_and_bulk() {
        let root = std::env::temp_dir().join(format!("tg-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
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
            .map(|(_, rel, _)| rel)
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
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// Home-manager/NixOS hosts symlink the whole `~/.claude` tree into the nix
    /// store. The sync must follow those symlinks — otherwise `settings.json`
    /// (a regular file) uploads while the hook scripts it references (symlinks)
    /// are skipped, and the in-sandbox agent errors with `…-hook.sh: not found`.
    #[cfg(unix)] // exercises unix symlinks + exec bits
    #[test]
    fn agent_config_follows_symlinked_config_files() {
        let root = std::env::temp_dir().join(format!("tg-claude-sym-{}", std::process::id()));
        let store = std::env::temp_dir().join(format!("tg-claude-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&store); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::create_dir_all(&store).unwrap();

        // Real files "in the store", symlinked into the config tree like home-manager.
        let hook_target = store.join("agentmemory-session-start.sh");
        std::fs::write(&hook_target, b"#!/bin/sh\necho hi\n").unwrap();
        // Hook scripts are executable on the host; the sync must preserve that
        // through the symlink (else the sandbox hook errors `Permission denied`).
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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

        let collected = collect_agent_config_files(&root);
        let got: Vec<String> = collected.iter().map(|(_, rel, _)| rel.clone()).collect();
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
        // The hook's executable bit is detected through the symlink so the sync
        // uploads it via `write_exec` (0755), not `write` (0644) — otherwise the
        // in-sandbox hook fails with `Permission denied`. A plain config file
        // (settings.json) stays non-executable.
        let exec_of = |rel: &str| {
            collected
                .iter()
                .find(|(_, r, _)| r == rel)
                .map(|(_, _, x)| *x)
        };
        assert_eq!(
            exec_of("hooks/agentmemory-session-start.sh"),
            Some(true),
            "executable hook preserves its +x bit"
        );
        assert_eq!(
            exec_of("settings.json"),
            Some(false),
            "plain config file is not marked executable"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&store); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn auth_critical_files_are_a_small_known_set() {
        // Claude's auth-critical set must be small (≤10 files) and include
        // the oauth credential file — the most important one.
        let claude_crit = thegn_core::envplan::agent_auth_critical_files("claude");
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
        let pi_crit = thegn_core::envplan::agent_auth_critical_files("pi");
        assert!(
            pi_crit.len() <= 5,
            "pi auth-critical should be small: {pi_crit:?}"
        );
    }

    #[test]
    fn account_publish_script_moves_aside_before_promoting() {
        let dest = "/root/.thegn/accounts/claude/work a";
        let stage = "/root/.thegn/accounts/claude/.work-a.thegn-stage-0123456789abcdef";
        let script = account_publish_script(dest, stage);
        assert!(script.contains(&format!("d={};", account_shell_quote(dest))));
        assert!(script.contains(&format!("s={};", account_shell_quote(stage))));
        let aside = script.find("mv -- \"$d\" \"$o\"").unwrap();
        let promote = script.find("mv -- \"$s\" \"$d\"").unwrap();
        let remove_old = script.find("rm -rf -- \"$o\"").unwrap();
        assert!(
            aside < promote && promote < remove_old,
            "publish script order: {script}"
        );
        assert!(
            script.contains("mv -- \"$o\" \"$d\""),
            "failed promotion restores the old home"
        );
        assert!(
            !script.contains(&format!("d={dest};")),
            "path with a space must be shell quoted"
        );
        assert!(
            !script.contains(&format!("s={stage};")),
            "stage path must be shell quoted"
        );
    }

    #[test]
    fn account_upload_ready_rejects_failed_or_skipped_files() {
        assert!(account_upload_ready(2, 2, false, true));
        assert!(!account_upload_ready(2, 1, false, true));
        assert!(!account_upload_ready(2, 2, true, true));
        assert!(!account_upload_ready(2, 2, false, false));
    }

    #[test]
    fn account_file_vanished_only_ignores_not_found() {
        assert!(account_file_vanished(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(!account_file_vanished(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn account_cleanup_timeout_stays_within_remaining_budget() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let timeout = account_cleanup_timeout(deadline).unwrap();
        assert!(timeout <= std::time::Duration::from_secs(2));
        assert!(account_cleanup_timeout(std::time::Instant::now()).is_none());
        assert!(account_publish_timeout(std::time::Instant::now()) <= AGENT_CONFIG_UPLOAD_TIMEOUT);
        assert!(
            account_publish_timeout(std::time::Instant::now()) >= std::time::Duration::from_secs(5)
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test drives the publish script in a real shell"
    )]
    fn account_publish_script_runs_in_a_real_shell() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let stage = root.path().join(".home.thegn-stage-test");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(home.join("old.txt"), b"old").unwrap();
        std::fs::create_dir(home.join("projects")).unwrap();
        std::fs::write(home.join("projects/t.jsonl"), b"transcript").unwrap();
        std::fs::write(stage.join("new.txt"), b"new").unwrap();

        let home = home.to_string_lossy().into_owned();
        let stage = stage.to_string_lossy().into_owned();
        let script = account_publish_script(&home, &stage);
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(std::path::Path::new(&home).join("new.txt").exists());
        assert_eq!(
            std::fs::read(std::path::Path::new(&home).join("projects/t.jsonl")).unwrap(),
            b"transcript"
        );
        assert!(!std::path::Path::new(&home).join("old.txt").exists());
        assert!(!std::path::Path::new(&stage).exists());
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("home.thegn-old-")
        }));

        let second_stage = root.path().join(".home.thegn-stage-second");
        std::fs::create_dir(&second_stage).unwrap();
        std::fs::write(second_stage.join("second.txt"), b"second").unwrap();
        let second_stage = second_stage.to_string_lossy().into_owned();
        let second_script = account_publish_script(&home, &second_stage);
        let second_status = std::process::Command::new("/bin/sh")
            .args(["-c", &second_script])
            .status()
            .unwrap();
        assert!(second_status.success());
        assert!(std::path::Path::new(&home).join("second.txt").exists());
        assert_eq!(
            std::fs::read(std::path::Path::new(&home).join("projects/t.jsonl")).unwrap(),
            b"transcript"
        );

        let failed_stage = root.path().join(".home.thegn-stage-failed");
        std::fs::create_dir(&failed_stage).unwrap();
        std::fs::write(failed_stage.join("failed.txt"), b"failed").unwrap();
        let failed_stage = failed_stage.to_string_lossy().into_owned();
        let failed_script = account_publish_script(&home, &failed_stage);
        let fail_promotion = format!(
            "mv() {{ if [ \"$2\" = \"$s\" ]; then return 1; fi; command mv \"$@\"; }}; {failed_script}"
        );
        let failed_status = std::process::Command::new("/bin/sh")
            .args(["-c", &fail_promotion])
            .status()
            .unwrap();
        assert!(!failed_status.success());
        assert!(std::path::Path::new(&home).join("second.txt").exists());
        assert_eq!(
            std::fs::read(std::path::Path::new(&home).join("projects/t.jsonl")).unwrap(),
            b"transcript"
        );

        let missing_stage = root
            .path()
            .join(".missing-stage")
            .to_string_lossy()
            .into_owned();
        std::fs::write(std::path::Path::new(&home).join("old.txt"), b"old").unwrap();
        let missing_script = account_publish_script(&home, &missing_stage);
        let missing_status = std::process::Command::new("/bin/sh")
            .args(["-c", &missing_script])
            .status()
            .unwrap();
        assert!(!missing_status.success());
        assert!(std::path::Path::new(&home).join("old.txt").exists());
    }

    #[test]
    fn account_stage_root_is_a_hidden_sibling() {
        let parent = "/root/.thegn/accounts/claude";
        let dest = format!("{parent}/work-a");
        let stage = account_stage_root(parent, "work-a", "0123456789abcdef");
        assert_eq!(
            std::path::Path::new(&stage).parent(),
            std::path::Path::new(&dest).parent()
        );
        assert!(
            std::path::Path::new(&stage)
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.starts_with(".work-a.thegn-stage-"))
        );
    }
}
