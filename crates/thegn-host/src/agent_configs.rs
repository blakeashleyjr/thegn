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
//! **Phase 2 — full config tree (archived, budget-capped):** Walk each agent's
//! config directories (skipping bulky state dirs), pack the remaining files
//! into gzip'd ustar archives (`pack_batches`, ≤64 MiB each) and extract each
//! in one exec. A content-hash marker (`AGENT_CONFIGS_MARKER`) skips an
//! unchanged set. Files the packer refuses, and members of an archive that
//! fails, fall back to per-file uploads with bounded concurrency
//! (`UPLOAD_CONCURRENCY`). This is best-effort:
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
    "node_modules",
    ".git",
    "__pycache__",
    ".venv",
    "venv",
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

/// One Phase 2 file: `(rel, data, exec)`, `rel` relative to the sandbox `$HOME`.
type PackEntry = (String, Vec<u8>, bool);

/// Compressed-size ceiling for one Phase 2 config archive (THE-2056). A typical
/// claude tree is a few MB, so this almost always yields a single archive; past
/// it, [`pack_batches`] starts a new one so no single upload is unbounded.
const AGENT_CONFIG_ARCHIVE_CAP: usize = 64 * 1024 * 1024;

/// Marker (relative to the sandbox `$HOME`) holding the hex content hash of the
/// last fully-synced Phase 2 set. A matching hash skips Phase 2 entirely.
const AGENT_CONFIGS_MARKER: &str = ".thegn/agent-configs.sha256";

/// ustar block size.
const TAR_BLOCK: usize = 512;

/// Is `rel` a plain relative path with no `..`, `.`, empty component or NUL —
/// i.e. one that cannot name anything outside the directory it is joined to?
fn rel_is_confined(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\0')
        && rel
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Write `value` as a NUL-terminated, zero-padded octal field filling `field`.
fn tar_octal(field: &mut [u8], value: u64) -> Result<(), String> {
    let digits = field.len() - 1;
    let s = format!("{value:0digits$o}");
    if s.len() > digits {
        return Err(format!(
            "value {value} does not fit a {digits}-digit tar field"
        ));
    }
    field[..digits].copy_from_slice(s.as_bytes());
    field[digits] = 0;
    Ok(())
}

/// Build the 512-byte ustar header for a regular file (type `'0'`, uid/gid 0,
/// mtime 0, mode 0755/0644). A path over 100 bytes is split across the
/// `prefix` (≤155) and `name` (≤100) fields at a `/`; anything that can't be
/// split that way (over 255 bytes, or no usable `/`) is an `Err`, as is a
/// path that isn't [`rel_is_confined`].
fn ustar_header(rel: &str, size: usize, exec: bool) -> Result<[u8; TAR_BLOCK], String> {
    if !rel_is_confined(rel) {
        return Err(format!("refusing unconfined archive path {rel:?}"));
    }
    let bytes = rel.as_bytes();
    let (prefix, name): (&[u8], &[u8]) = if bytes.len() <= 100 {
        (&[], bytes)
    } else {
        // Pick the leftmost `/` that still fits the name in 100 bytes (the
        // shortest prefix), then check the prefix fits its 155.
        let split = (0..bytes.len())
            .find(|&i| bytes[i] == b'/' && bytes.len() - i - 1 <= 100)
            .ok_or_else(|| format!("archive path too long for ustar: {} bytes", bytes.len()))?;
        if split > 155 {
            return Err(format!(
                "archive path too long for ustar: {} bytes",
                bytes.len()
            ));
        }
        (&bytes[..split], &bytes[split + 1..])
    };
    let mut h = [0u8; TAR_BLOCK];
    h[..name.len()].copy_from_slice(name);
    tar_octal(&mut h[100..108], if exec { 0o755 } else { 0o644 })?;
    tar_octal(&mut h[108..116], 0)?; // uid
    tar_octal(&mut h[116..124], 0)?; // gid
    tar_octal(&mut h[124..136], size as u64)?;
    tar_octal(&mut h[136..148], 0)?; // mtime
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    tar_octal(&mut h[329..337], 0)?; // devmajor
    tar_octal(&mut h[337..345], 0)?; // devminor
    h[345..345 + prefix.len()].copy_from_slice(prefix);
    // Checksum: the sum of all header bytes with the checksum field as spaces.
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    let ck = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(ck.as_bytes());
    Ok(h)
}

/// The tar size of one member: header plus data padded to a whole block.
fn tar_member_len(size: usize) -> usize {
    TAR_BLOCK + size.div_ceil(TAR_BLOCK) * TAR_BLOCK
}

/// A minimal ustar writer: regular files only, root-owned, mtime 0. Private to
/// the agent-login sync, which only ever packs small config files.
struct TarPack {
    buf: Vec<u8>,
}

impl TarPack {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn add(&mut self, rel: &str, data: &[u8], exec: bool) -> Result<(), String> {
        let header = ustar_header(rel, data.len(), exec)?;
        self.buf.extend_from_slice(&header);
        self.buf.extend_from_slice(data);
        let pad = tar_member_len(data.len()) - TAR_BLOCK - data.len();
        self.buf.resize(self.buf.len() + pad, 0);
        Ok(())
    }

    /// Close the archive with the two zero end-of-archive blocks.
    fn finish(mut self) -> Vec<u8> {
        self.buf.resize(self.buf.len() + 2 * TAR_BLOCK, 0);
        self.buf
    }
}

/// One gzip'd tar archive and the indices (into the packed entry slice) of the
/// files it carries.
struct PackedArchive {
    bytes: Vec<u8>,
    members: Vec<usize>,
}

/// Worst-case slack for the gzip header/footer plus the compressed tar trailer.
const ARCHIVE_TRAILER_SLACK: usize = 2 * 1024;

/// Upper bound on what `len` uncompressed bytes can add to a sync-flushed
/// deflate stream (stored-block overhead is 5 bytes per ≤64 KiB block).
fn deflate_bound(len: usize) -> usize {
    len + len / 1000 + 64
}

/// An archive under construction: the tar stream plus its incremental gzip,
/// sync-flushed after every member so the compressed length is exact.
struct ArchiveBuilder {
    tar: TarPack,
    gz: flate2::write::GzEncoder<Vec<u8>>,
    fed: usize,
    members: Vec<usize>,
}

impl ArchiveBuilder {
    fn new() -> Self {
        Self {
            tar: TarPack::new(),
            gz: flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()),
            fed: 0,
            members: Vec::new(),
        }
    }

    fn compressed_len(&self) -> usize {
        self.gz.get_ref().len()
    }

    fn push(&mut self, idx: usize, rel: &str, data: &[u8], exec: bool) -> Result<(), String> {
        use std::io::Write as _;
        self.tar.add(rel, data, exec)?;
        self.gz
            .write_all(&self.tar.buf[self.fed..])
            .and_then(|()| self.gz.flush())
            .map_err(|e| e.to_string())?;
        self.fed = self.tar.buf.len();
        self.members.push(idx);
        Ok(())
    }

    fn finish(self) -> std::io::Result<PackedArchive> {
        use std::io::Write as _;
        let Self {
            tar,
            mut gz,
            fed,
            members,
        } = self;
        let tar = tar.finish();
        gz.write_all(&tar[fed..])?;
        Ok(PackedArchive {
            bytes: gz.finish()?,
            members,
        })
    }
}

/// Pack `entries` into gzip'd ustar archives of at most `cap` compressed bytes
/// each (a single member larger than `cap` still gets its own archive). Entries
/// the tar writer refuses (over-long or unconfined paths) are returned by index
/// as leftovers for the per-file path. Pure — unit-tested with a small `cap`.
fn pack_batches(entries: &[PackEntry], cap: usize) -> (Vec<PackedArchive>, Vec<usize>) {
    fn close(b: ArchiveBuilder, archives: &mut Vec<PackedArchive>, leftover: &mut Vec<usize>) {
        if b.members.is_empty() {
            return;
        }
        let members = b.members.clone();
        match b.finish() {
            Ok(a) => archives.push(a),
            Err(_) => leftover.extend(members),
        }
    }
    let mut archives = Vec::new();
    let mut leftover = Vec::new();
    let mut cur = ArchiveBuilder::new();
    for (i, (rel, data, exec)) in entries.iter().enumerate() {
        if ustar_header(rel, data.len(), *exec).is_err() {
            leftover.push(i);
            continue;
        }
        let bound = cur.compressed_len()
            + deflate_bound(tar_member_len(data.len()))
            + ARCHIVE_TRAILER_SLACK;
        if !cur.members.is_empty() && bound > cap {
            close(
                std::mem::replace(&mut cur, ArchiveBuilder::new()),
                &mut archives,
                &mut leftover,
            );
        }
        if cur.push(i, rel, data, *exec).is_err() {
            // A failed in-memory gzip write leaves the builder inconsistent:
            // route everything it held (and this entry) to the per-file path.
            let b = std::mem::replace(&mut cur, ArchiveBuilder::new());
            leftover.extend(b.members);
            leftover.push(i);
        }
    }
    close(cur, &mut archives, &mut leftover);
    leftover.sort_unstable();
    (archives, leftover)
}

/// Stable hex sha256 over the Phase 2 set: sorted `(rel, exec, bytes)`, each
/// field length-prefixed, under a format tag so a scheme change re-syncs.
fn content_hash(entries: &[PackEntry]) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut order: Vec<&PackEntry> = entries.iter().collect();
    order.sort();
    let mut h = Sha256::new();
    h.update(b"thegn-agent-configs-v1\0");
    for (rel, data, exec) in order {
        h.update((rel.len() as u64).to_le_bytes());
        h.update(rel.as_bytes());
        h.update([u8::from(*exec)]);
        h.update((data.len() as u64).to_le_bytes());
        h.update(data);
    }
    h.finalize()
        .iter()
        .flat_map(|b| {
            [
                HEX[usize::from(b >> 4)] as char,
                HEX[usize::from(b & 0x0f)] as char,
            ]
        })
        .collect()
}

/// The remote script that extracts `archive` into `base` and always removes
/// the `stage` dir, exiting with tar's status. Every path is `sh_quote`d.
fn archive_extract_script(base: &str, stage: &str, archive: &str) -> String {
    let q = thegn_core::util::sh_quote;
    format!(
        "mkdir -p -- {b} && tar -xzf {a} -C {b} --no-same-owner 2>&1; rc=$?; rm -rf -- {s}; exit $rc",
        b = q(base),
        a = q(archive),
        s = q(stage),
    )
}

/// Time left before `deadline`, or `None` once it has passed.
fn remaining(deadline: std::time::Instant) -> Option<std::time::Duration> {
    deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|d| !d.is_zero())
}

/// Ship one archive: `mktemp -d` a private stage, write the archive there,
/// extract it into `base` (the script removes the stage). Bounded by
/// `deadline`. `Err` ⇒ the caller falls back to per-file uploads.
async fn ship_archive(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    base: &str,
    bytes: &[u8],
    deadline: std::time::Instant,
) -> anyhow::Result<()> {
    let budget =
        || remaining(deadline).ok_or_else(|| anyhow::anyhow!("agent-config budget exhausted"));
    let mk = [
        "/bin/sh".to_string(),
        "-c".to_string(),
        "umask 077; mktemp -d \"${TMPDIR:-/tmp}/tg-agent-configs.XXXXXX\"".to_string(),
    ];
    let (code, out) = tokio::time::timeout(budget()?, provider.run_exec(id, &mk, None, &[]))
        .await
        .map_err(|_| anyhow::anyhow!("mktemp timed out"))??;
    let stage = out.trim().to_string();
    anyhow::ensure!(
        code == 0 && stage.starts_with('/') && !stage.contains(['\n', '\0']),
        "mktemp failed (exit {code})"
    );
    let archive = format!("{stage}/agent-configs.tgz");
    let wrote = match remaining(deadline) {
        Some(to) => tokio::time::timeout(to, provider.write(id, &archive, bytes))
            .await
            .map_err(|_| anyhow::anyhow!("archive upload timed out"))
            .and_then(|r| r),
        None => Err(anyhow::anyhow!("agent-config budget exhausted")),
    };
    let script = if wrote.is_ok() {
        archive_extract_script(base, &stage, &archive)
    } else {
        format!("rm -rf -- {}", thegn_core::util::sh_quote(&stage))
    };
    let argv = ["/bin/sh".to_string(), "-c".to_string(), script];
    // best-effort: when the write failed this run is stage cleanup only; its
    // result is checked below only on the extract path. The cleanup-only
    // script still gets a short slot past an exhausted budget.
    let to = remaining(deadline).unwrap_or(std::time::Duration::from_secs(5));
    let ran = tokio::time::timeout(to, provider.run_exec(id, &argv, None, &[]))
        .await
        .map_err(|_| anyhow::anyhow!("archive extract timed out"))
        .and_then(|r| r);
    wrote?;
    let (code, out) = ran?;
    if code != 0 {
        let tail: String = out
            .chars()
            .rev()
            .take(512)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        anyhow::bail!("archive extract exited {code}: {}", tail.trim());
    }
    Ok(())
}

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
fn collect_agent_config_files_for(
    dir: &Path,
    allow_plugins: bool,
) -> Vec<(std::path::PathBuf, String, bool)> {
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
                if AGENT_STATE_SKIP_DIRS.iter().any(|s| *s == name)
                    && !(allow_plugins && name == "plugins")
                {
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

pub(crate) fn collect_agent_config_files(dir: &Path) -> Vec<(std::path::PathBuf, String, bool)> {
    collect_agent_config_files_for(dir, false)
}

/// [`collect_agent_config_files`] filtered through [`phase2_allowed`]; returns
/// `(allowed, total_walked)`.
fn collect_allowed_agent_files(
    agent: &str,
    dir: &Path,
) -> (Vec<(std::path::PathBuf, String, bool)>, usize) {
    let all = collect_agent_config_files_for(dir, agent.trim() == "hermes");
    let total = all.len();
    let kept = all
        .into_iter()
        .filter(|(_, rel, _)| phase2_allowed(agent, rel))
        .collect();
    (kept, total)
}

/// Whether [`phase2_allowed`] has an explicit arm for this agent.
pub(crate) fn phase2_is_known(agent: &str) -> bool {
    matches!(
        agent.trim(),
        "claude" | "claude-code" | "codex" | "pi" | "hermes"
    )
}

/// Phase 2 allowlist: is `rel` (relative to the agent's config dir) a file the
/// login sync should carry? Known agents get a positive allowlist of top-level
/// entries (settings, instructions, skills, hooks…) so telemetry, transcripts
/// and runtime state never ship; unknown agents keep the blocklist-only
/// behaviour ([`AGENT_STATE_SKIP_DIRS`] still applies on top in the walk).
fn phase2_allowed(agent: &str, rel: &str) -> bool {
    let rel = rel.trim_start_matches("./");
    if agent.trim() == "hermes"
        && (rel.ends_with(".db") || rel.split('/').any(|part| part == "cache"))
    {
        return false;
    }
    let mut parts = rel.split('/');
    let top = parts.next().unwrap_or("");
    let nested = rel.contains('/');
    match agent.trim() {
        "claude" | "claude-code" => match top {
            "settings.json" | "settings.local.json" | "CLAUDE.md" | "keybindings.json" => !nested,
            "skills" | "agents" | "commands" | "hooks" | "output-styles" => nested,
            "plugins" => nested && !parts.any(|p| p == "cache" || p == "repos"),
            t => !nested && t.starts_with("statusline"),
        },
        "codex" => match top {
            "auth.json" | "config.toml" | "config.json" | "AGENTS.md" => !nested,
            "prompts" => nested,
            _ => false,
        },
        "pi" => match top {
            "settings.json" => !nested,
            "agent" => {
                let mut rest = rel.strip_prefix("agent/").unwrap_or("").split('/');
                let Some(second) = rest.next() else {
                    return false;
                };
                if matches!(
                    second,
                    "extensions"
                        | "skills"
                        | "agents"
                        | "prompts"
                        | "themes"
                        | "compound-engineering"
                ) {
                    return true;
                }
                matches!(
                    second,
                    "auth.json" | "models.json" | "AGENTS.md" | "trust.json"
                ) && rest.next().is_none()
            }
            _ => false,
        },
        "hermes" => match top {
            "config.yaml"
            | "auth.json"
            | ".env"
            | "SOUL.md"
            | "AGENTS.md"
            | "delegation-override.yaml" => !nested,
            "skills" | "hooks" | "prompts" | "memories" | "scripts" => nested,
            "plugins" => nested && !parts.any(|part| part == "cache"),
            _ => false,
        },
        _ => true,
    }
}

fn cap_unknown_agent_files(
    agent: &str,
    files: &mut Vec<(std::path::PathBuf, String, bool)>,
    warn: &mut dyn FnMut(&str),
) {
    if phase2_is_known(agent) || files.len() <= 2_000 {
        return;
    }
    let count = files.len();
    files.retain(|(_, rel, _)| !rel.contains('/'));
    let agent = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    warn(&format!(
        "agent-login sync: {agent}'s config tree has {count} files; only its top-level files were synced"
    ));
}

/// Where an agent's login files are read from on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoginSource {
    /// No `login_homes` entry: the live host login (`$HOME`, re-rooted at the
    /// relocated home when `CLAUDE_CONFIG_DIR` / `CODEX_HOME` is set).
    Live,
    /// `[env.<n>.provider] login_homes.<key>`: a dedicated account's home (the
    /// claude config dir / the `CODEX_HOME` root). Only files under it are read.
    Dedicated {
        key: &'static str,
        root: std::path::PathBuf,
    },
}

/// The `login_homes` key an agent kind reads (`None` ⇒ no dedicated home).
fn login_home_key(agent: &str) -> Option<&'static str> {
    match agent.trim() {
        "claude" | "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

/// Resolve each agent's [`LoginSource`]. A `login_homes` dir that does not
/// exist drops that agent with one `warn` line: the live login is never read
/// in its place, so a dedicated-account intent is not silently replaced.
fn resolve_login_sources(
    agents: &[String],
    login_homes: &std::collections::BTreeMap<String, String>,
    host_home: &Path,
    warn: &mut dyn FnMut(&str),
) -> Vec<(String, LoginSource)> {
    let mut out = Vec::with_capacity(agents.len());
    for agent in agents {
        let dedicated = login_home_key(agent).and_then(|key| {
            login_homes
                .get(key)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .map(|v| {
                    (
                        key,
                        std::path::PathBuf::from(thegn_core::util::expand_tilde_in(v, host_home)),
                    )
                })
        });
        match dedicated {
            None => out.push((agent.clone(), LoginSource::Live)),
            Some((key, root)) if root.is_dir() => {
                out.push((agent.clone(), LoginSource::Dedicated { key, root }));
            }
            Some((key, root)) => warn(&format!(
                "login_homes.{key} = {} is not a directory; {} login not synced \
                 (the live host login is not used in its place)",
                root.display(),
                agent.trim()
            )),
        }
    }
    out
}

/// SOURCE path for an agent-config path `rel` (relative to `$HOME`), or `None`
/// when `source` has no such file to offer.
///
/// [`LoginSource::Live`]: re-rooted at the provider's relocated credential home
/// (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`) when set. The host's LIVE login may be a
/// relocated home (e.g. a claude-profiles dir) while the default `~/.claude`
/// fossilizes — the login sync uploading the fossil gave every sprite a stale
/// identity. [`LoginSource::Dedicated`]: re-rooted at the `login_homes` dir;
/// a path outside it is never read from `$HOME`. The sandbox TARGET keeps the
/// default rel path (in-sandbox agents run without the relocation env).
fn agent_source_path(
    host_home: &str,
    rel: &str,
    source: &LoginSource,
) -> Option<std::path::PathBuf> {
    match source {
        LoginSource::Live => Some(
            relocated_source(rel, &|k| std::env::var(k).ok())
                .unwrap_or_else(|| Path::new(host_home).join(rel)),
        ),
        LoginSource::Dedicated { key, root } => dedicated_source(key, root, rel),
    }
}

/// `rel` re-rooted at a dedicated `login_homes` dir for provider `key`, with
/// claude's `.claude.json` inside-then-beside rule. `None` ⇒ not under it.
fn dedicated_source(key: &str, root: &Path, rel: &str) -> Option<std::path::PathBuf> {
    let p = thegn_core::account::provider(key)?;
    if rel == p.default_dir {
        return Some(root.to_path_buf());
    }
    if let Some(rest) = rel.strip_prefix(p.default_dir)
        && let Some(rest) = rest.strip_prefix('/')
    {
        return Some(root.join(rest));
    }
    if key == "claude" && rel == ".claude.json" {
        return claude_json_near(root);
    }
    None
}

/// claude's top-level `.claude.json` rides beside the config dir: with
/// `CLAUDE_CONFIG_DIR` set claude keeps it INSIDE that dir; a claude-profiles
/// home keeps it at the profile root (the dir's parent). First existing wins.
fn claude_json_near(root: &Path) -> Option<std::path::PathBuf> {
    let inside = root.join(".claude.json");
    if inside.is_file() {
        return Some(inside);
    }
    root.parent()
        .map(|d| d.join(".claude.json"))
        .filter(|beside| beside.is_file())
}

/// A host file's bytes plus its executable bit.
type HostFile = (Vec<u8>, bool);

/// An auth-critical file: relative path, bytes, executable bit.
type AuthFile = (String, Vec<u8>, bool);

/// Read a host file and whether it is executable. `None` ⇒ absent/unreadable.
fn read_host_file(path: &Path) -> Option<HostFile> {
    let data = std::fs::read(path).ok()?;
    let exec = std::fs::metadata(path)
        .map(|md| is_executable(&md))
        .unwrap_or(false);
    Some((data, exec))
}

/// The Phase 1 (auth-critical) files for `sources` as `(rel, data, exec)`,
/// pure over a `read` seam. Absent files are skipped silently.
fn collect_auth_critical(
    sources: &[(String, LoginSource)],
    host_home: &str,
    read: &mut dyn FnMut(&Path) -> Option<HostFile>,
) -> Vec<AuthFile> {
    let mut out = Vec::new();
    for (agent, source) in sources {
        for f in thegn_core::envplan::agent_auth_critical_files(agent) {
            let Some(src) = agent_source_path(host_home, &f, source) else {
                continue;
            };
            let Some((data, exec)) = read(&src) else {
                continue; // file absent on this host — skip silently
            };
            let data = strip_sandbox_hooks(&f, data);
            out.push((f, data, exec));
        }
    }
    out
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
        // Neither `.claude.json` beside nor inside ⇒ fall back to `$HOME`.
        if p.id == "claude"
            && rel == ".claude.json"
            && let Some(found) = claude_json_near(&root)
        {
            return Some(found);
        }
    }
    None
}

/// Push the host's git identity (`user.name`/`user.email`, effective repo→global
/// values) into a provider sandbox's provisioning exec env as
/// `THEGN_GIT_NAME`/`THEGN_GIT_EMAIL`, so `git_auth_script` can persist them to
/// the sandbox `~/.gitconfig` — otherwise in-sandbox commits are authored as the
/// provider default (`Sprite <noreply@sprites.dev>`). No-op for an unset key;
/// values with control characters or over the length cap are dropped
/// ([`thegn_core::git_identity::resolve`]).
// off-loop by contract: only called from the blocking provisioning path.
#[expect(clippy::disallowed_methods)]
pub(crate) fn push_host_git_identity(repo_root: &Path, exec_env: &mut Vec<(String, String)>) {
    let read = |args: &[&str]| -> Option<String> {
        let o = thegn_core::util::git_cmd(repo_root)
            .args(args)
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let id = thegn_core::git_identity::resolve(&read);
    for (var, value) in [("THEGN_GIT_NAME", id.name), ("THEGN_GIT_EMAIL", id.email)] {
        if let Some(v) = value {
            exec_env.push((var.to_string(), v));
        }
    }
}

/// Upload just the auth-critical files (Phase 1) for `sources` into the sandbox
/// `$HOME` at `base`: owner-only (`write_private`, mode 0600 where the provider
/// supports it), or `write_exec` for an executable source. Returns
/// `(ok, failed)`. Small (~5 tiny files per agent) + best-effort — the shared
/// core of the initial login sync and the connect-time [`resync_agent_auth`]
/// refresh. Errors and logs name the path only, never file content.
fn sync_agent_auth_critical(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    base: &str,
    sources: &[(String, LoginSource)],
) -> (usize, usize) {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let mut ok = 0usize;
    let mut failed = 0usize;
    let critical = collect_auth_critical(sources, &host_home, &mut read_host_file);
    let critical_dests = critical
        .iter()
        .map(|(f, _, _)| format!("{base}/{f}"))
        .collect::<Vec<_>>();
    let results = block_on_provider(move || async move {
        use futures::future::join_all;

        let uploads = critical
            .into_iter()
            .map(|(f, data, exec)| {
                let dest = format!("{base}/{f}");
                async move {
                    let write = async {
                        if exec {
                            provider.write_exec(id, &dest, &data).await
                        } else {
                            provider.write_private(id, &dest, &data).await
                        }
                    };
                    let result =
                        match tokio::time::timeout(AGENT_CONFIG_UPLOAD_TIMEOUT, write).await {
                            Ok(r) => r,
                            Err(_) => Err(anyhow::anyhow!(
                                "upload timed out after {}s",
                                AGENT_CONFIG_UPLOAD_TIMEOUT.as_secs()
                            )),
                        };
                    (dest, result)
                }
            })
            .collect::<Vec<_>>();
        Ok(join_all(uploads).await)
    });
    match results {
        Ok(results) => {
            for (dest, result) in results {
                match result {
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
        Err(e) => {
            // Keep the aggregate counts/log shape if the provider runtime itself
            // cannot be created: every collected upload failed to run.
            failed = critical_dests.len();
            for dest in critical_dests {
                tracing::warn!(
                    target: "thegn::startup",
                    dest = %dest,
                    error = %e,
                    "agent-config auth-critical upload failed (best-effort)"
                );
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
/// cheap (~5 tiny files) + best-effort. `loc` is the location the caller already
/// read checked. Resolves the sandbox `$HOME` and the agent kinds itself so the
/// caller stays a one-liner.
pub(crate) fn resync_agent_auth(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    cfg: &thegn_core::config::Config,
    worktree: &str,
    loc: &thegn_core::remote::GitLoc,
    env_name: &str,
) {
    use thegn_core::store::WorkspaceStore;
    // Agent kinds: mirror `run_provisioning` — an explicit `[sandbox.home]
    // agents` list wins, else the `[[agents]]` picker, else host detection.
    let repo_root = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| std::path::PathBuf::from(worktree));
    let home = cfg
        .resolve_env(
            &repo_root,
            loc,
            Path::new(worktree),
            &thegn_core::sandbox_pin::EnvSelection::named(env_name),
        )
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
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let no_homes = std::collections::BTreeMap::new();
    let login_homes = cfg
        .env
        .get(env_name)
        .map_or(&no_homes, |e| &e.provider.login_homes);
    let sources = resolve_login_sources(
        &agents,
        login_homes,
        Path::new(&host_home),
        &mut thegn_core::msg::warn,
    );
    if sources.is_empty() {
        return;
    }
    // Resolve the sandbox `$HOME` (where the auth files must land) the same way
    // `run_provisioning` does — a tiny non-tty exec.
    let sprite_home = match crate::provider_workdir::cached_home(id) {
        Some(home) => home,
        None => block_on_provider(|| async {
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
        .unwrap_or_else(|| "/root".to_string()),
    };
    let (ok, failed) =
        sync_agent_auth_critical(provider, id, sprite_home.trim_end_matches('/'), &sources);
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
    login_homes: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = sandbox_home.trim_end_matches('/');
    // `login_homes` (a dedicated account) replaces the live login per agent; a
    // missing dir drops that agent with one warning.
    let sources = resolve_login_sources(
        agents,
        login_homes,
        Path::new(&host_home),
        &mut thegn_core::msg::warn,
    );

    // -----------------------------------------------------------------------
    // Phase 1: auth-critical files — explicit allowlist, always uploaded,
    // no deadline. Even a slow provider finishes these in a few seconds.
    // The agent is guaranteed to be logged-in after this phase completes.
    // -----------------------------------------------------------------------
    let (auth_ok, auth_failed) = sync_agent_auth_critical(provider, id, base, &sources);

    // -----------------------------------------------------------------------
    // Phase 2: full config tree — walk directories, skip bulky-state dirs,
    // upload remaining files in parallel batches, stop when budget expires.
    // Auth-critical files already uploaded above are deduplicated.
    // -----------------------------------------------------------------------

    // Build the full upload list synchronously before entering async.
    // Dedup against the auth-critical set so we don't double-upload.
    let mut already_uploaded: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (agent, _) in &sources {
        for f in thegn_core::envplan::agent_auth_critical_files(agent) {
            already_uploaded.insert(f);
        }
    }

    // `rel` is relative to the sandbox `$HOME` (`base`): packing and the
    // per-file fallback both join it to `base` themselves.
    let mut all_uploads: Vec<PackEntry> = Vec::new();
    let mut unknown_cap_warnings = std::collections::HashSet::new();
    for (agent, source) in &sources {
        let (files, dirs) = thegn_core::envplan::agent_config_paths(agent);
        let mut agent_uploads = Vec::new();
        for f in files {
            if already_uploaded.contains(&f) || !phase2_allowed(agent, &f) {
                continue;
            }
            let Some(src) = agent_source_path(&host_home, &f, source) else {
                continue;
            };
            let Some((data, exec)) = read_host_file(&src) else {
                continue;
            };
            let data = strip_sandbox_hooks(&f, data);
            agent_uploads.push((f, data, exec));
        }
        for d in dirs {
            let Some(src) = agent_source_path(&host_home, &d, source) else {
                continue;
            };
            if !src.is_dir() {
                continue;
            }
            let (collected, walked) = collect_allowed_agent_files(agent, &src);
            tracing::info!(
                target: "thegn::startup",
                agent = %agent,
                dir = %d,
                allowlisted = collected.len(),
                total = walked,
                "agent-config phase 2: allowlisted vs total files"
            );
            for (abs, rel, exec) in collected {
                let host_rel = format!("{d}/{rel}");
                if already_uploaded.contains(&host_rel) {
                    continue;
                }
                let Ok(data) = std::fs::read(&abs) else {
                    continue;
                };
                let data = strip_sandbox_hooks(&host_rel, data);
                agent_uploads.push((host_rel, data, exec));
            }
        }
        let warning_key = agent.trim().to_owned();
        cap_unknown_agent_files(agent, &mut agent_uploads, &mut |message| {
            if unknown_cap_warnings.insert(warning_key.clone()) {
                thegn_core::msg::warn(message);
            }
        });
        all_uploads.extend(agent_uploads);
    }

    let total_phase2 = all_uploads.len();
    let started = std::time::Instant::now();
    let deadline = started + AGENT_CONFIG_STEP_BUDGET;
    let hash = content_hash(&all_uploads);
    let marker = format!("{base}/{AGENT_CONFIGS_MARKER}");

    // One tokio runtime for the whole phase: the marker probe, the archive
    // shipments and the per-file fallback all share it.
    let (p2_ok, p2_failed, p2_skipped) = block_on_provider(|| async {
        use futures::future::join_all;
        if total_phase2 == 0 {
            return Ok((0, 0, 0));
        }
        // Unchanged since the last complete sync ⇒ skip Phase 2 entirely.
        let prev =
            tokio::time::timeout(AGENT_CONFIG_UPLOAD_TIMEOUT, provider.read(id, &marker)).await;
        if let Ok(Ok(prev)) = prev
            && String::from_utf8_lossy(&prev).trim() == hash
        {
            tracing::info!(
                target: "thegn::startup",
                files = total_phase2,
                "agent-config phase 2 unchanged; skipping"
            );
            return Ok((0, 0, 0));
        }

        // Pack everything into as few gzip'd tar archives as the cap allows
        // and extract each in one exec (THE-2056: ~800 per-file round-trips
        // took minutes). Entries the packer refuses, plus every member of an
        // archive that fails to ship, go through the per-file fallback.
        let (archives, mut per_file) = pack_batches(&all_uploads, AGENT_CONFIG_ARCHIVE_CAP);
        let packed_bytes: usize = archives.iter().map(|a| a.bytes.len()).sum();
        let mut ok = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        for a in &archives {
            match ship_archive(provider, id, base, &a.bytes, deadline).await {
                Ok(()) => ok += a.members.len(),
                Err(e) => {
                    tracing::warn!(
                        target: "thegn::startup",
                        files = a.members.len(),
                        bytes = a.bytes.len(),
                        error = %e,
                        "agent-config archive failed; falling back to per-file uploads"
                    );
                    per_file.extend_from_slice(&a.members);
                }
            }
        }
        per_file.sort_unstable();

        // Per-file fallback: bounded-concurrency chunks under the same deadline.
        let per_file_total = per_file.len();
        let mut attempted = 0usize;
        for chunk in per_file.chunks(UPLOAD_CONCURRENCY) {
            if std::time::Instant::now() >= deadline {
                // Budget exhausted: count all remaining (this chunk + the rest).
                skipped += per_file_total - attempted;
                break;
            }
            attempted += chunk.len();

            let mut futs = Vec::with_capacity(chunk.len());
            for &idx in chunk {
                let (rel, data, exec) = &all_uploads[idx];
                if !rel_is_confined(rel) {
                    // Never write outside `base`, on either path.
                    failed += 1;
                    tracing::warn!(
                        target: "thegn::startup",
                        rel = %rel,
                        "agent-config upload: refusing an unconfined path"
                    );
                    continue;
                }
                let dest = format!("{base}/{rel}");
                futs.push(async move {
                    let r = tokio::time::timeout(
                        AGENT_CONFIG_UPLOAD_TIMEOUT,
                        write_preserving_mode(provider, id, &dest, data, *exec),
                    )
                    .await;
                    (dest, r)
                });
            }

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

        if failed == 0 && skipped == 0 {
            // best-effort: the marker only lets the next `env up` skip an
            // identical sync; a missing marker just re-syncs.
            let _ = tokio::time::timeout(
                AGENT_CONFIG_UPLOAD_TIMEOUT,
                provider.write(id, &marker, hash.as_bytes()),
            )
            .await;
        }
        tracing::info!(
            target: "thegn::startup",
            archives = archives.len(),
            files = total_phase2,
            bytes = packed_bytes,
            per_file = per_file_total,
            ok,
            failed,
            skipped,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "agent-config phase 2 synced"
        );
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

fn account_failure_line(provider_id: &str, identity: &str, reason: &str) -> String {
    format!("{provider_id}/{identity}: {reason}")
}

fn account_upload_provider_failure_lines(
    uploads: &[AccountUpload],
    written_by_upload: &[usize],
    error: &str,
) -> Vec<String> {
    uploads
        .iter()
        .zip(written_by_upload)
        .filter(|(_, written)| **written == 0)
        .map(|(upload, _)| {
            let identity = upload.dest_root.rsplit('/').next().unwrap_or("unknown");
            account_failure_line(&upload.provider_id, identity, error)
        })
        .collect()
}

/// The Sync agent logins step result: the config upload's own result, plus a failure naming every account home that
/// did not reach the sandbox (the step is best-effort, so the caller marks it Failed and continues).
pub(crate) fn agent_configs_step_result(
    configs: anyhow::Result<()>,
    account_failures: &[String],
) -> anyhow::Result<()> {
    if account_failures.is_empty() {
        return configs;
    }
    let account_ids = account_failures
        .iter()
        .map(|failure| {
            failure
                .split_once(": ")
                .map_or(failure.as_str(), |(id, _)| id)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let accounts = format!(
        "account homes not published ({account_ids}): {}",
        account_failures.join("; ")
    );
    match configs {
        Ok(()) => Err(anyhow::anyhow!("{accounts}")),
        Err(error) => Err(anyhow::anyhow!(
            "{accounts}; config upload failed: {error:#}"
        )),
    }
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
) -> Vec<String> {
    let db = match thegn_core::db::Db::open() {
        Ok(db) => db,
        Err(error) => {
            return vec![format!(
                "coding-agent accounts were not uploaded: the state database could not be opened: {error:#}"
            )];
        }
    };
    let base = sprite_home.trim_end_matches('/');
    let mut uploads: Vec<AccountUpload> = Vec::new();
    let mut failures = Vec::new();
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
                failures.push(account_failure_line(p.id, &identity, &error.to_string()));
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
        return failures;
    }
    let deadline = std::time::Instant::now() + AGENT_CONFIG_STEP_BUDGET;
    let mut written_by_upload = vec![0; uploads.len()];
    let result = block_on_provider(|| async {
        let mut publish_failures = Vec::new();
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
                publish_failures.push(account_failure_line(&upload.provider_id, identity, &reason));
                continue;
            };
            if written == 0 {
                tracing::warn!(target: "thegn::startup", provider = %upload.provider_id, account = %identity, reason = %reason, "account home publish failed or outcome unknown");
                publish_failures.push(account_failure_line(&upload.provider_id, identity, &reason));
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
            publish_failures.push(account_failure_line(&upload.provider_id, identity, &reason));
        }
        Ok::<Vec<String>, anyhow::Error>(publish_failures)
    });
    match result {
        Ok(publish_failures) => {
            failures.extend(publish_failures);
        }
        Err(error) => {
            failures.extend(account_upload_provider_failure_lines(
                &uploads,
                &written_by_upload,
                &error.to_string(),
            ));
            for (upload_index, upload) in uploads.iter().enumerate() {
                if written_by_upload[upload_index] == 0 {
                    continue;
                }
                let identity = upload.dest_root.rsplit('/').next().unwrap_or("unknown");
                let Some(cleanup_timeout) = account_cleanup_timeout(deadline) else {
                    let reason = error.to_string();
                    tracing::warn!(target: "thegn::startup", provider = %upload.provider_id, account = %identity, reason = %reason, "account home publish failed or outcome unknown");
                    failures.push(account_failure_line(&upload.provider_id, identity, &reason));
                    continue;
                };
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
                failures.push(account_failure_line(&upload.provider_id, identity, &reason));
            }
        }
    }
    failures
}

/// The `export <HOME_ENV>="$HOME/.thegn/accounts/<provider>/<slug>"; …` prefix to
/// splice ahead of a PROVIDER pane's inner command, pointing each agent CLI at the
/// ACTIVE account deployed by [`upload_agent_accounts`]. `$HOME` is expanded
/// in-sprite (the sandbox login `$HOME` isn't known host-side, so we can't bake an
/// absolute path). Empty when no account is active for any provider — the pane
/// then uses the agent's default `~/.<agent>` home (backwards-compatible). A
/// database or selection read failure is returned so the provider pane can
/// refuse instead of using that default home.
pub(crate) fn account_pane_env_exports(
    cfg: &thegn_core::config::Config,
    worktree: &str,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let db = thegn_core::db::Db::open()
        .context("open the state database to read the active coding-agent account")?;
    account_exports_for(cfg, &db, worktree)
}

fn account_exports_for(
    cfg: &thegn_core::config::Config,
    db: &thegn_core::db::Db,
    worktree: &str,
) -> anyhow::Result<String> {
    use thegn_core::store::WorkspaceStore;

    let repo_root = db
        .repo_root_for(worktree)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| Path::new(worktree).to_path_buf());
    let slug = crate::agent::repo_slug_checked(db, &repo_root)?;
    let mut out = String::new();
    for p in thegn_core::account::providers() {
        let Some(name) =
            thegn_core::account::active_name_checked(cfg, db, worktree, slug.as_deref(), p.id)?
        else {
            continue;
        };
        out.push_str(&account_export(p.home_env, p.id, &name));
    }
    Ok(out)
}

/// Preserve the selection failure for the pane spawner to report through the
/// running UI loop instead of logging a warning that may have no sink.
pub(crate) fn account_exports_or_refuse(read: anyhow::Result<String>) -> Result<String, String> {
    read.map_err(|error| format!("{error:#}"))
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

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    fn allowed_rels(agent: &str, root: &Path) -> Vec<String> {
        let mut v: Vec<String> = collect_allowed_agent_files(agent, root)
            .0
            .into_iter()
            .map(|(_, r, _)| r)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn phase2_claude_allowlist_collects_only_config() {
        let _state = crate::testenv::IsolatedState::with_env(&[]);
        let root = std::env::temp_dir().join(format!("tg-allow-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root); // best-effort: scratch dir may not exist
        for f in [
            "telemetry/e.json",
            "cache/x.bin",
            "mcp-needs-auth-cache.json",
            "projects/p/a.jsonl",
            "state/mcp-discover-verdicts.json",
            "skills/x/SKILL.md",
            "hooks/a.sh",
            "settings.json",
            "CLAUDE.md",
            "plugins/cache/z.json",
            "statusline-command.sh",
        ] {
            touch(&root, f);
        }
        let got = allowed_rels("claude", &root);
        // `plugins/` stays excluded by AGENT_STATE_SKIP_DIRS (second filter).
        assert_eq!(
            got,
            vec![
                "CLAUDE.md",
                "hooks/a.sh",
                "settings.json",
                "skills/x/SKILL.md",
                "statusline-command.sh"
            ]
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup
    }

    #[test]
    fn phase2_codex_pi_and_unknown_tables() {
        let cases = [
            ("codex", "config.toml", true),
            ("codex", "prompts/a.md", true),
            ("codex", "AGENTS.md", true),
            ("codex", "auth.json", true),
            ("pi", "agent/npm", false),
            ("pi", "agent/git", false),
            ("pi", "agent/sessions", false),
            ("pi", "agent/auth.json", true),
            ("pi", "agent/skills/a/SKILL.md", true),
            ("pi", "settings.json", true),
            ("pi", "settings.json/nested", false),
            ("pi", "history/x", false),
            ("pi", "telemetry.json", false),
            ("hermes", "profiles/x", false),
            ("hermes", "config.yaml", true),
            ("hermes", "skills/x", true),
            ("hermes", "skills/x.db", false),
            ("hermes", "cache/x", false),
            ("hermes", "plugins/cache/x", false),
            ("claude", "plugins/a/b.json", true),
            ("claude", "plugins/cache/b.json", false),
            ("claude", "plugins/a/repos/b.json", false),
            ("claude", "telemetry/a", false),
        ];
        for (agent, rel, expected) in cases {
            assert_eq!(phase2_allowed(agent, rel), expected, "{agent}/{rel}");
        }
    }

    #[test]
    fn phase2_hermes_allowlist_collects_only_config() {
        let root = std::env::temp_dir().join(format!("tg-allow-unk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root); // best-effort: scratch dir may not exist
        for f in [
            "config.yaml",
            "auth.json",
            ".env",
            "SOUL.md",
            "AGENTS.md",
            "delegation-override.yaml",
            "profiles/x",
            "skills/x",
            "plugins/x",
            "cache/x",
        ] {
            touch(&root, f);
        }
        let allowed = allowed_rels("hermes", &root);
        assert_eq!(
            allowed,
            vec![
                ".env",
                "AGENTS.md",
                "SOUL.md",
                "auth.json",
                "config.yaml",
                "delegation-override.yaml",
                "plugins/x",
                "skills/x",
            ]
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup
    }

    #[test]
    fn phase2_skip_dirs_drop_dependency_and_repository_state() {
        let root = tempfile::tempdir().unwrap();
        for f in [
            "node_modules/pkg/index.js",
            ".git/config",
            "__pycache__/x.pyc",
        ] {
            touch(root.path(), f);
        }
        touch(root.path(), "config.json");
        assert_eq!(allowed_rels("unknown", root.path()), vec!["config.json"]);
    }

    #[test]
    fn phase2_unknown_agent_cap_keeps_top_level_and_warns_once() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..2_001 {
            touch(root.path(), &format!("nested/{n}.json"));
        }
        touch(root.path(), "auth.json");
        touch(root.path(), "config.json");
        let (mut files, walked) = collect_allowed_agent_files("custom", root.path());
        assert_eq!(walked, 2_003);
        let mut warnings = Vec::new();
        cap_unknown_agent_files("custom", &mut files, &mut |message| {
            warnings.push(message.to_string())
        });
        assert_eq!(files.len(), 2);
        assert_eq!(
            warnings,
            [
                "agent-login sync: custom's config tree has 2003 files; only its top-level files were synced"
            ]
        );
    }

    #[test]
    fn account_exports_use_distinct_lossless_home_components() {
        let first = account_export("CODEX_HOME", "codex", "work/a");
        let second = account_export("CODEX_HOME", "codex", "work-a");
        assert_ne!(first, second);
    }

    #[test]
    fn account_exports_for_exports_the_active_account() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        thegn_core::account::set_active(
            &db,
            thegn_core::account::Bind::Global,
            "/wt",
            None,
            "claude",
            "work",
        )
        .unwrap();

        let out = account_exports_for(&thegn_core::config::Config::default(), &db, "/wt").unwrap();
        assert!(out.contains("$HOME/.thegn/accounts/claude/work"));
    }

    #[test]
    fn account_exports_for_is_empty_without_a_selection() {
        let db = thegn_core::db::Db::open_memory().unwrap();

        assert_eq!(
            account_exports_for(&thegn_core::config::Config::default(), &db, "/wt").unwrap(),
            String::new()
        );
    }

    #[test]
    fn account_exports_or_refuse_refuses_an_unreadable_selection() {
        assert_eq!(
            account_exports_or_refuse(Err(anyhow::anyhow!("database is locked"))),
            Err("database is locked".into())
        );
    }

    #[test]
    fn account_exports_or_refuse_passes_exports_through() {
        assert_eq!(
            account_exports_or_refuse(Ok("export X=1; ".into())),
            Ok("export X=1; ".into())
        );
        assert_eq!(
            account_exports_or_refuse(Ok(String::new())),
            Ok(String::new())
        );
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

    fn homes(pairs: &[(&str, &Path)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.to_string_lossy().into_owned()))
            .collect()
    }

    fn put(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn login_homes_reroot_claude_and_codex_and_never_read_the_live_login() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("home");
        // The live login: must never be read once `login_homes` is set.
        put(&live.join(".claude/.credentials.json"), b"LIVE");
        put(&live.join(".claude.json"), b"LIVE");
        put(&live.join(".codex/auth.json"), b"LIVE");
        put(&live.join(".config/codex/config.json"), b"LIVE");
        // The dedicated accounts: a claude profile (`.claude.json` beside the
        // config dir) and a codex home.
        let claude = tmp.path().join("acct/claude/.claude");
        put(&claude.join(".credentials.json"), b"DEDICATED-CLAUDE");
        put(
            &tmp.path().join("acct/claude/.claude.json"),
            b"DEDICATED-JSON",
        );
        let codex = tmp.path().join("acct/codex");
        put(&codex.join("auth.json"), b"DEDICATED-CODEX");
        put(&codex.join("config.toml"), b"model = \"x\"");

        let agents = ["claude".to_string(), "codex".to_string()];
        let mut warned = Vec::new();
        let sources = resolve_login_sources(
            &agents,
            &homes(&[("claude", claude.as_path()), ("codex", codex.as_path())]),
            &live,
            &mut |m: &str| warned.push(m.to_string()),
        );
        assert!(warned.is_empty(), "{warned:?}");
        assert_eq!(
            sources[1].1,
            LoginSource::Dedicated {
                key: "codex",
                root: codex.clone()
            }
        );

        let home_s = live.to_string_lossy().into_owned();
        let mut reads = Vec::new();
        let got = collect_auth_critical(&sources, &home_s, &mut |p: &Path| {
            reads.push(p.to_path_buf());
            read_host_file(p)
        });
        assert!(
            reads.iter().all(|p| !p.starts_with(&live)),
            "the live home was read: {reads:?}"
        );
        let by_rel: std::collections::BTreeMap<&str, &[u8]> = got
            .iter()
            .map(|(rel, data, _)| (rel.as_str(), data.as_slice()))
            .collect();
        assert_eq!(by_rel[".claude/.credentials.json"], b"DEDICATED-CLAUDE");
        assert_eq!(by_rel[".claude.json"], b"DEDICATED-JSON");
        assert_eq!(by_rel[".codex/auth.json"], b"DEDICATED-CODEX");
        assert_eq!(by_rel[".codex/config.toml"], b"model = \"x\"");
        // Outside the codex home ⇒ never read from `$HOME`.
        assert!(
            !by_rel.contains_key(".config/codex/config.json"),
            "{by_rel:?}"
        );
        // The pure path rules.
        let src = &sources[0].1;
        assert_eq!(
            agent_source_path(&home_s, ".claude", src),
            Some(claude.clone())
        );
        assert_eq!(agent_source_path(&home_s, ".claudeX/y", src), None);
    }

    #[test]
    fn a_missing_login_home_skips_the_agent_with_one_warning_and_no_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("home");
        put(&live.join(".claude/.credentials.json"), b"LIVE");
        put(&live.join(".claude.json"), b"LIVE");
        let gone = tmp.path().join("no-such-account");
        let agents = ["claude".to_string()];
        let mut warned = Vec::new();
        let sources = resolve_login_sources(
            &agents,
            &homes(&[("claude", gone.as_path())]),
            &live,
            &mut |m: &str| warned.push(m.to_string()),
        );
        assert!(sources.is_empty(), "{sources:?}");
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("login_homes.claude"), "{}", warned[0]);
        assert!(!warned[0].contains("LIVE"), "{}", warned[0]);
        let mut read_count = 0usize;
        let got = collect_auth_critical(&sources, &live.to_string_lossy(), &mut |p: &Path| {
            read_count += 1;
            read_host_file(p)
        });
        assert!(got.is_empty());
        assert_eq!(
            read_count, 0,
            "no live-login read in place of a missing home"
        );
    }

    #[test]
    fn login_homes_leave_other_agents_on_the_live_login() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("home");
        let claude = tmp.path().join("acct");
        std::fs::create_dir_all(&claude).unwrap();
        let agents = ["codex".to_string(), "pi".to_string()];
        let mut warned = Vec::new();
        let sources = resolve_login_sources(
            &agents,
            &homes(&[("claude", claude.as_path())]),
            &live,
            &mut |m: &str| warned.push(m.to_string()),
        );
        assert!(warned.is_empty());
        assert!(
            sources.iter().all(|(_, s)| *s == LoginSource::Live),
            "{sources:?}"
        );
        // A `~` login home expands against the host home.
        let tilde = [("codex".to_string(), "~/acct".to_string())]
            .into_iter()
            .collect();
        let sources = resolve_login_sources(
            &["codex".to_string()],
            &tilde,
            tmp.path(),
            &mut |m: &str| warned.push(m.to_string()),
        );
        assert_eq!(
            sources[0].1,
            LoginSource::Dedicated {
                key: "codex",
                root: claude
            }
        );
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
        // THE-2051: the host's MCP needs-auth cache must never ride along —
        // it pins the sandbox's MCP servers to the host's auth state.
        assert!(
            !claude_crit
                .iter()
                .any(|f| f.contains("mcp-needs-auth-cache")),
            "auth-critical must not carry mcp-needs-auth-cache: {claude_crit:?}"
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
    fn agent_configs_step_passes_configs_result_through_without_account_failures() {
        assert!(agent_configs_step_result(Ok(()), &[]).is_ok());
        let error = agent_configs_step_result(Err(anyhow::anyhow!("x")), &[]).unwrap_err();
        assert_eq!(error.to_string(), "x");
    }

    #[test]
    fn agent_configs_step_fails_naming_each_unpublished_account() {
        let failures = [
            format!("claude/acct1: publish exited 1: boom {}", "x".repeat(512)),
            "codex/work: write auth.json timed out".to_string(),
        ];
        let error = agent_configs_step_result(Ok(()), &failures).unwrap_err();
        let detail = error.to_string();
        assert!(detail.contains("account homes not published"));
        assert!(
            detail.starts_with("account homes not published (claude/acct1, codex/work):"),
            "account ids must precede long publish reasons so splash truncation retains them: {detail}"
        );
        assert!(detail.contains("claude/acct1"));
        assert!(detail.contains("publish exited 1: boom"));
        assert!(detail.contains("codex/work"));
    }

    #[test]
    fn agent_configs_step_keeps_the_configs_error_and_the_account_failures() {
        let failures = ["claude/acct1: publish failed: x".to_string()];
        let error =
            agent_configs_step_result(Err(anyhow::anyhow!("upload .claude failed")), &failures)
                .unwrap_err();
        let detail = error.to_string();
        assert!(detail.contains("upload .claude failed"));
        assert!(detail.contains("claude/acct1: publish failed: x"));
    }

    #[test]
    fn account_failure_line_names_provider_and_account() {
        assert_eq!(
            account_failure_line("claude", "acct1", "publish failed: x"),
            "claude/acct1: publish failed: x"
        );
    }

    #[test]
    fn provider_error_names_every_account_without_written_files() {
        let uploads = [
            AccountUpload {
                provider_id: "claude".to_string(),
                dest_root: "/home/.thegn/accounts/claude/acct1".to_string(),
                stage_root: "/stage/acct1".to_string(),
                files: Vec::new(),
                read_failure: None,
            },
            AccountUpload {
                provider_id: "codex".to_string(),
                dest_root: "/home/.thegn/accounts/codex/work".to_string(),
                stage_root: "/stage/work".to_string(),
                files: Vec::new(),
                read_failure: None,
            },
            AccountUpload {
                provider_id: "pi".to_string(),
                dest_root: "/home/.thegn/accounts/pi/partial".to_string(),
                stage_root: "/stage/partial".to_string(),
                files: Vec::new(),
                read_failure: None,
            },
        ];
        let failures =
            account_upload_provider_failure_lines(&uploads, &[0, 0, 1], "runtime failed");
        assert_eq!(
            failures,
            vec![
                "claude/acct1: runtime failed".to_string(),
                "codex/work: runtime failed".to_string()
            ]
        );
        let step_error = agent_configs_step_result(Ok(()), &failures).unwrap_err();
        assert!(step_error.to_string().contains("claude/acct1"));
        assert!(step_error.to_string().contains("codex/work"));
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

    // --- THE-2056: archived Phase 2 -------------------------------------

    /// A parsed ustar member: `(path, mode, data)`.
    type TarMember = (String, u32, Vec<u8>);

    fn octal_field(f: &[u8]) -> u64 {
        let s = std::str::from_utf8(f).unwrap();
        u64::from_str_radix(s.trim_matches(|c| c == '\0' || c == ' '), 8).unwrap()
    }

    fn cstr(f: &[u8]) -> String {
        let end = f.iter().position(|&b| b == 0).unwrap_or(f.len());
        String::from_utf8(f[..end].to_vec()).unwrap()
    }

    /// Minimal ustar reader: checks magic, checksum, type, zero padding and
    /// the two-zero-block trailer.
    fn parse_tar(bytes: &[u8]) -> Vec<TarMember> {
        assert!(
            bytes.len().is_multiple_of(TAR_BLOCK),
            "tar not block-aligned"
        );
        let mut out = Vec::new();
        let mut off = 0;
        loop {
            let h = &bytes[off..off + TAR_BLOCK];
            if h.iter().all(|&b| b == 0) {
                assert!(
                    bytes[off..].iter().all(|&b| b == 0) && bytes.len() - off >= 2 * TAR_BLOCK,
                    "missing two-zero-block trailer"
                );
                return out;
            }
            assert_eq!(&h[257..263], b"ustar\0");
            assert_eq!(h[156], b'0', "regular file only");
            let mut ck = h.to_vec();
            ck[148..156].copy_from_slice(b"        ");
            let sum: u64 = ck.iter().map(|&b| u64::from(b)).sum();
            assert_eq!(octal_field(&h[148..156]), sum, "header checksum");
            assert_eq!(octal_field(&h[108..116]), 0, "uid");
            assert_eq!(octal_field(&h[116..124]), 0, "gid");
            assert_eq!(octal_field(&h[136..148]), 0, "mtime");
            let name = cstr(&h[..100]);
            let prefix = cstr(&h[345..500]);
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let mode = u32::try_from(octal_field(&h[100..108])).unwrap();
            let size = usize::try_from(octal_field(&h[124..136])).unwrap();
            off += TAR_BLOCK;
            let data = bytes[off..off + size].to_vec();
            let padded = size.div_ceil(TAR_BLOCK) * TAR_BLOCK;
            assert!(
                bytes[off + size..off + padded].iter().all(|&b| b == 0),
                "padding must be zero"
            );
            off += padded;
            out.push((path, mode, data));
        }
    }

    fn gunzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Read as _;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    /// Deterministic incompressible bytes (xorshift), so gzip can't shrink them.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[0]
            })
            .collect()
    }

    #[test]
    fn tar_pack_round_trips_names_modes_and_data() {
        let mut tar = TarPack::new();
        tar.add(".claude/settings.json", b"{}\n", false).unwrap();
        tar.add(".claude/hooks/a.sh", &noise(700, 3), true).unwrap();
        tar.add(".claude/empty", b"", false).unwrap();
        let got = parse_tar(&tar.finish());
        assert_eq!(
            got,
            vec![
                (".claude/settings.json".to_string(), 0o644, b"{}\n".to_vec()),
                (".claude/hooks/a.sh".to_string(), 0o755, noise(700, 3)),
                (".claude/empty".to_string(), 0o644, Vec::new()),
            ]
        );
    }

    #[test]
    fn tar_pack_splits_long_paths_across_prefix_and_name() {
        let long = format!(".claude/skills/{}/{}", "d".repeat(120), "f".repeat(90));
        assert!(long.len() > 100 && long.len() <= 255);
        let mut tar = TarPack::new();
        tar.add(&long, b"x", false).unwrap();
        let bytes = tar.finish();
        assert!(!cstr(&bytes[345..500]).is_empty(), "prefix field used");
        assert_eq!(parse_tar(&bytes), vec![(long, 0o644, b"x".to_vec())]);

        let mut tar = TarPack::new();
        // Over 255 bytes, or a >100-byte final component: no ustar split.
        assert!(
            tar.add(&format!("a/{}", "b".repeat(300)), b"", false)
                .is_err()
        );
        assert!(tar.add(&"c".repeat(101), b"", false).is_err());
        assert!(
            tar.add(
                &format!("{}/{}", "p".repeat(200), "n".repeat(50)),
                b"",
                false
            )
            .is_err()
        );
        assert_eq!(
            tar.finish().len(),
            2 * TAR_BLOCK,
            "refused entries add nothing"
        );
    }

    #[test]
    fn tar_pack_rejects_unconfined_paths() {
        let mut tar = TarPack::new();
        for bad in [
            "/etc/passwd",
            "../escape",
            ".claude/../../escape",
            "a/./b",
            "a//b",
            "",
            "nul\0byte",
        ] {
            assert!(tar.add(bad, b"x", false).is_err(), "accepted {bad:?}");
            assert!(!rel_is_confined(bad), "{bad:?} confined");
        }
        assert!(rel_is_confined(".claude/..hidden/x"));
        assert_eq!(tar.finish().len(), 2 * TAR_BLOCK);
    }

    #[test]
    fn pack_batches_gzips_and_splits_at_the_cap() {
        let mut entries: Vec<PackEntry> = (0..6u64)
            .map(|i| (format!(".claude/f{i}"), noise(3000, i + 1), i % 2 == 0))
            .collect();
        entries.push(("../outside".to_string(), b"x".to_vec(), false));
        let cap = 10_000;
        let (archives, leftover) = pack_batches(&entries, cap);
        assert_eq!(leftover, vec![6], "the unconfined entry is left over");
        assert!(archives.len() >= 2, "split expected: {}", archives.len());
        let mut seen = Vec::new();
        for a in &archives {
            assert_eq!(&a.bytes[..2], &[0x1f, 0x8b], "gzip magic");
            if a.members.len() > 1 {
                assert!(a.bytes.len() <= cap, "archive {} > cap", a.bytes.len());
            }
            let members = parse_tar(&gunzip(&a.bytes));
            assert_eq!(members.len(), a.members.len());
            for (m, &idx) in members.iter().zip(&a.members) {
                let (rel, data, exec) = &entries[idx];
                assert_eq!(&m.0, rel);
                assert_eq!(&m.2, data);
                assert_eq!(m.1, if *exec { 0o755 } else { 0o644 });
            }
            seen.extend_from_slice(&a.members);
        }
        assert_eq!(
            seen,
            (0..6).collect::<Vec<_>>(),
            "every packable entry once, in order"
        );

        // The production cap packs the same set into one archive.
        let (one, _) = pack_batches(&entries, AGENT_CONFIG_ARCHIVE_CAP);
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn content_hash_is_stable_under_input_order() {
        let a: Vec<PackEntry> = vec![
            (".claude/a".into(), b"1".to_vec(), false),
            (".claude/b.sh".into(), b"2".to_vec(), true),
            (".codex/c".into(), b"3".to_vec(), false),
        ];
        let mut b = a.clone();
        b.reverse();
        let h = content_hash(&a);
        assert_eq!(h, content_hash(&b));
        assert_eq!(h.len(), 64);
        assert!(
            h.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        let mut c = a.clone();
        c[1].2 = false;
        assert_ne!(h, content_hash(&c), "exec bit is hashed");
        let mut d = a.clone();
        d[0].1 = b"changed".to_vec();
        assert_ne!(h, content_hash(&d), "content is hashed");
        let mut e = a;
        e[2].0 = ".codex/renamed".into();
        assert_ne!(h, content_hash(&e), "paths are hashed");
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test drives the archive extract script in a real shell"
    )]
    fn archive_extract_script_extracts_and_removes_the_stage() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let have_tar = std::process::Command::new("/bin/sh")
            .args(["-c", "command -v tar >/dev/null"])
            .status()
            .is_ok_and(|s| s.success());
        if !have_tar {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("home dir");
        let stage = root.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        let entries: Vec<PackEntry> = vec![
            (".claude/settings.json".into(), b"{}".to_vec(), false),
            (".claude/hooks/a.sh".into(), b"#!/bin/sh\n".to_vec(), true),
        ];
        let (archives, leftover) = pack_batches(&entries, AGENT_CONFIG_ARCHIVE_CAP);
        assert!(leftover.is_empty());
        let archive = stage.join("agent-configs.tgz");
        std::fs::write(&archive, &archives[0].bytes).unwrap();
        let script = archive_extract_script(
            &base.to_string_lossy(),
            &stage.to_string_lossy(),
            &archive.to_string_lossy(),
        );
        assert!(script.contains("--no-same-owner"));
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read(base.join(".claude/settings.json")).unwrap(),
            b"{}"
        );
        let md = std::fs::metadata(base.join(".claude/hooks/a.sh")).unwrap();
        assert!(is_executable(&md), "exec bit survives extraction");
        assert!(!stage.exists(), "stage removed");

        // A failing extract still removes the stage and reports non-zero.
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(&archive, b"not a gzip").unwrap();
        let script = archive_extract_script(
            &base.to_string_lossy(),
            &stage.to_string_lossy(),
            &archive.to_string_lossy(),
        );
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .status()
            .unwrap();
        assert!(!status.success());
        assert!(!stage.exists(), "stage removed on failure too");
    }
}
