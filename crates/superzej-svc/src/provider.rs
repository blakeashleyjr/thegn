//! Managed-sandbox **providers** (the `provider` placement) — the async seam for
//! platforms that spawn dev environments via an HTTP API or SDK (Daytona,
//! Codespaces, Modal, …), as opposed to a static `exec_command` CLI.
//!
//! The host's `[env.<name>.provider]` config can drive a sandbox two ways:
//!  1. **CLI** — `up_command`/`exec_command` run a vendor CLI (no code here;
//!     handled by `superzej_core::placement::Placement::ensure`).
//!  2. **API** — `api_base` + `api_key_env` point at a REST provider; this module
//!     creates/destroys/lists sandboxes over HTTP and resolves an exec handle.
//!
//! Request/response shaping is split into **pure** functions (`*_url`,
//! `create_body`, `parse_*`) so they're unit-testable without a live endpoint;
//! the async trait methods are thin reqwest wrappers around them. The reference
//! impl is [`DaytonaProvider`] (Daytona's documented `POST /api/sandbox` + Bearer
//! auth); field names are defensive and easy to retarget to another platform.

use anyhow::{Context, Result, anyhow};
use std::path::Path;
use superzej_core::remote::SshTarget;

/// How to exec into a provider sandbox once it exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecKind {
    /// The provider exposes ssh; attach via the normal ssh/mosh transport.
    Ssh(SshTarget),
    /// Exec through a vendor CLI prefix, e.g. `["daytona", "ssh", "<id>", "--"]`.
    Command(Vec<String>),
}

/// A live (or discovered) provider sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxHandle {
    pub id: String,
    pub exec: ExecKind,
}

/// A managed-sandbox provider. Object-safe-ish async seam (mirrors
/// [`crate::ssh::RemoteExec`]); concrete impls own their HTTP client.
#[allow(async_fn_in_trait)]
pub trait RemoteProvider: Send + Sync {
    /// Create (and start) a new sandbox, returning a handle to exec into.
    async fn create(&self) -> Result<SandboxHandle>;
    /// Destroy a sandbox by id (idempotent — a missing sandbox is success).
    async fn destroy(&self, id: &str) -> Result<()>;
    /// List existing sandbox ids (for discovery / reuse).
    async fn list(&self) -> Result<Vec<String>>;
}

/// Daytona (`github.com/daytonaio/daytona`) REST provider. Targets the
/// documented `POST {api_base}/sandbox` (Bearer auth); `snapshot` selects the
/// image/template to create from.
pub struct DaytonaProvider {
    api_base: String,
    token: String,
    snapshot: String,
    client: reqwest::Client,
}

impl DaytonaProvider {
    /// `api_base` like `https://app.daytona.io/api`; `token` is the API key
    /// (already resolved from `api_key_env`); `snapshot` is the image/template.
    pub fn new(api_base: &str, token: &str, snapshot: &str) -> Self {
        DaytonaProvider {
            api_base: api_base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            snapshot: snapshot.to_string(),
            client: reqwest::Client::new(),
        }
    }

    // --- pure request/response shaping (unit-tested) ------------------------

    fn sandbox_url(&self) -> String {
        format!("{}/sandbox", self.api_base)
    }

    fn sandbox_id_url(&self, id: &str) -> String {
        format!("{}/sandbox/{id}", self.api_base)
    }

    /// The create body. `snapshot` empty ⇒ the provider's default image.
    fn create_body(&self) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        if !self.snapshot.trim().is_empty() {
            body.insert("snapshot".into(), self.snapshot.clone().into());
        }
        serde_json::Value::Object(body)
    }

    /// Extract a sandbox id from a create/list element. Daytona returns the id
    /// at the top level; tolerate a `{ "sandbox": { "id" } }` envelope too.
    fn parse_id(v: &serde_json::Value) -> Option<String> {
        v.get("id")
            .or_else(|| v.get("sandbox").and_then(|s| s.get("id")))
            .and_then(|i| i.as_str())
            .map(str::to_string)
    }

    /// Parse a list response (an array, or `{ "sandboxes": [...] }`).
    fn parse_list(v: &serde_json::Value) -> Vec<String> {
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| v.get("sandboxes").and_then(|s| s.as_array()).cloned())
            .unwrap_or_default();
        arr.iter().filter_map(Self::parse_id).collect()
    }

    /// The exec handle for a created sandbox id: the `daytona` CLI ssh bridge
    /// (handles per-sandbox auth without exposing keys here).
    fn exec_for(id: &str) -> ExecKind {
        ExecKind::Command(vec![
            "daytona".into(),
            "ssh".into(),
            id.to_string(),
            "--".into(),
        ])
    }
}

impl RemoteProvider for DaytonaProvider {
    async fn create(&self) -> Result<SandboxHandle> {
        let resp = self
            .client
            .post(self.sandbox_url())
            .bearer_auth(&self.token)
            .json(&self.create_body())
            .send()
            .await
            .context("daytona: POST /sandbox")?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .context("daytona: decode create response")?;
        if !status.is_success() {
            return Err(anyhow!("daytona create failed ({status}): {body}"));
        }
        let id = Self::parse_id(&body)
            .ok_or_else(|| anyhow!("daytona create: no sandbox id in response: {body}"))?;
        let exec = Self::exec_for(&id);
        Ok(SandboxHandle { id, exec })
    }

    async fn destroy(&self, id: &str) -> Result<()> {
        let resp = self
            .client
            .delete(self.sandbox_id_url(id))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("daytona: DELETE /sandbox/{id}")?;
        // 404 = already gone — treat as success (idempotent teardown).
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(anyhow!("daytona destroy failed ({})", resp.status()))
        }
    }

    async fn list(&self) -> Result<Vec<String>> {
        let resp = self
            .client
            .get(self.sandbox_url())
            .bearer_auth(&self.token)
            .send()
            .await
            .context("daytona: GET /sandbox")?;
        let body: serde_json::Value = resp.json().await.context("daytona: decode list")?;
        Ok(Self::parse_list(&body))
    }
}

/// Sprites (`sprites.dev`, Fly.io) REST provider. Firecracker microVMs with a
/// persistent fs, ~300ms live checkpoints, and L3 egress policies. Sprites are
/// **named** (caller-chosen), not server-assigned ids: create posts
/// `POST {api_base}/sprites {"name": …}`; teardown is `DELETE …/sprites/{name}`.
/// Exec uses the `sprite` CLI bridge (`sprite exec -s <name> --tty --`), which
/// carries per-sprite auth without exposing the token here. `api_base` defaults
/// to `https://api.sprites.dev/v1`; the token comes from `SPRITES_TOKEN`.
pub struct SpritesProvider {
    api_base: String,
    token: String,
    /// The sprite name to create/attach (caller-chosen; sprites are persistent).
    name: String,
    client: reqwest::Client,
}

impl SpritesProvider {
    /// `api_base` like `https://api.sprites.dev/v1` (empty ⇒ that default);
    /// `token` is the resolved `SPRITES_TOKEN`; `name` is the sprite to manage.
    pub fn new(api_base: &str, token: &str, name: &str) -> Self {
        let base = api_base.trim().trim_end_matches('/');
        SpritesProvider {
            api_base: if base.is_empty() {
                "https://api.sprites.dev/v1".to_string()
            } else {
                base.to_string()
            },
            token: token.to_string(),
            name: name.trim().to_string(),
            client: reqwest::Client::new(),
        }
    }

    // --- pure request/response shaping (unit-tested) ------------------------

    fn sprites_url(&self) -> String {
        format!("{}/sprites", self.api_base)
    }

    fn sprite_name_url(&self, name: &str) -> String {
        format!("{}/sprites/{name}", self.api_base)
    }

    /// The create body — sprites are named by the caller.
    fn create_body(name: &str) -> serde_json::Value {
        serde_json::json!({ "name": name })
    }

    /// Pull a sprite name from a create/list element (`{ "name": … }`, tolerating
    /// a `{ "sprite": { "name" } }` envelope).
    fn parse_name(v: &serde_json::Value) -> Option<String> {
        v.get("name")
            .or_else(|| v.get("sprite").and_then(|s| s.get("name")))
            .and_then(|n| n.as_str())
            .map(str::to_string)
    }

    /// Parse a list response (an array, or `{ "sprites": [...] }`).
    fn parse_list(v: &serde_json::Value) -> Vec<String> {
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| v.get("sprites").and_then(|s| s.as_array()).cloned())
            .unwrap_or_default();
        arr.iter().filter_map(Self::parse_name).collect()
    }

    /// The exec handle for a sprite: the `sprite` CLI exec bridge (PTY-capable).
    fn exec_for(name: &str) -> ExecKind {
        ExecKind::Command(vec![
            "sprite".into(),
            "exec".into(),
            "-s".into(),
            name.to_string(),
            "--tty".into(),
            "--".into(),
        ])
    }
}

impl RemoteProvider for SpritesProvider {
    async fn create(&self) -> Result<SandboxHandle> {
        if self.name.is_empty() {
            return Err(anyhow!(
                "sprites: set `[env.<name>.provider] id = \"<sprite-name>\"` — sprites are named"
            ));
        }
        let resp = self
            .client
            .post(self.sprites_url())
            .bearer_auth(&self.token)
            .json(&Self::create_body(&self.name))
            .send()
            .await
            .context("sprites: POST /sprites")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            return Err(anyhow!("sprites create failed ({status}): {body}"));
        }
        // The server echoes the name; fall back to the requested one.
        let name = Self::parse_name(&body).unwrap_or_else(|| self.name.clone());
        let exec = Self::exec_for(&name);
        Ok(SandboxHandle { id: name, exec })
    }

    async fn destroy(&self, id: &str) -> Result<()> {
        let resp = self
            .client
            .delete(self.sprite_name_url(id))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("sprites: DELETE /sprites/{name}")?;
        // 404 = already gone — idempotent teardown.
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(anyhow!("sprites destroy failed ({})", resp.status()))
        }
    }

    async fn list(&self) -> Result<Vec<String>> {
        let resp = self
            .client
            .get(self.sprites_url())
            .bearer_auth(&self.token)
            .send()
            .await
            .context("sprites: GET /sprites")?;
        let body: serde_json::Value = resp.json().await.context("sprites: decode list")?;
        Ok(Self::parse_list(&body))
    }
}

// ===========================================================================
// Capability-segmented provider axes. A provider implements `RemoteProvider`
// (lifecycle, required) plus whichever optional sub-traits below it supports;
// the `Provider` enum is the generic dispatcher and declares support via
// `caps()`. Adding a provider = one enum variant + its sub-trait impls + a
// `caps()` arm. (Async-fn-in-trait isn't dyn-safe, so we dispatch by enum, the
// same pattern as `vpn::for_provider` / the host's old `ApiProvider`.)
// ===========================================================================

/// A network egress rule: a domain pattern (`github.com`, `*.npmjs.org`, `*`) and
/// an action. Mirrors the Sprites `policy/network` rule shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PolicyRule {
    pub domain: String,
    pub action: String, // "allow" | "deny"
}

/// Lower superzej's `network_allow`/`network_block` domain lists to a provider
/// network policy. **Pure** (unit-tested). Order encodes the precedence:
/// deny rules first (a block beats an allow), then allow rules, then a trailing
/// default-`deny *` **only when an allow-list is present** (an allow-list means
/// "deny everything else") — exactly the local DNS-filter semantics
/// ([`superzej_core::dns_filter`]). Empty + empty ⇒ no rules (allow all).
pub fn rules_from(allow: &[String], block: &[String]) -> Vec<PolicyRule> {
    let mut rules = Vec::new();
    for d in block {
        let d = d.trim();
        if !d.is_empty() {
            rules.push(PolicyRule {
                domain: d.to_string(),
                action: "deny".into(),
            });
        }
    }
    let mut any_allow = false;
    for d in allow {
        let d = d.trim();
        if !d.is_empty() {
            rules.push(PolicyRule {
                domain: d.to_string(),
                action: "allow".into(),
            });
            any_allow = true;
        }
    }
    if any_allow {
        rules.push(PolicyRule {
            domain: "*".into(),
            action: "deny".into(),
        });
    }
    rules
}

/// Egress-policy translation — realizes [`EgressKind::Translate`]
/// (`superzej_core::capabilities`): lower allow/block lists to the provider's own
/// network controls, since we can't run our DNS filter inside the provider's box.
#[allow(async_fn_in_trait)]
pub trait ProviderEgress: Send + Sync {
    async fn set_network_policy(&self, id: &str, allow: &[String], block: &[String]) -> Result<()>;
    async fn get_network_policy(&self, id: &str) -> Result<Vec<PolicyRule>>;
}

impl SpritesProvider {
    fn policy_network_url(&self, name: &str) -> String {
        format!("{}/sprites/{name}/policy/network", self.api_base)
    }
}

impl ProviderEgress for SpritesProvider {
    async fn set_network_policy(&self, id: &str, allow: &[String], block: &[String]) -> Result<()> {
        let body = serde_json::json!({ "rules": rules_from(allow, block) });
        let resp = self
            .client
            .post(self.policy_network_url(id))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("sprites: POST /policy/network")?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let st = resp.status();
            let b = resp.text().await.unwrap_or_default();
            Err(anyhow!("sprites set network policy failed ({st}): {b}"))
        }
    }

    async fn get_network_policy(&self, id: &str) -> Result<Vec<PolicyRule>> {
        let resp = self
            .client
            .get(self.policy_network_url(id))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("sprites: GET /policy/network")?;
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok(body
            .get("rules")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        Some(PolicyRule {
                            domain: v.get("domain")?.as_str()?.to_string(),
                            action: v.get("action")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// A checkpoint/snapshot of a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointInfo {
    pub id: String,
    pub label: Option<String>,
}

/// Snapshot / restore — realizes `Capabilities::can_snapshot`. A live, transactional
/// capture of the sandbox's fs+memory (Sprites' headline ~300ms checkpoints).
#[allow(async_fn_in_trait)]
pub trait ProviderCheckpoints: Send + Sync {
    /// Create a checkpoint, returning its id.
    async fn checkpoint(&self, id: &str, label: Option<&str>) -> Result<String>;
    async fn list_checkpoints(&self, id: &str) -> Result<Vec<CheckpointInfo>>;
    async fn restore(&self, id: &str, checkpoint: &str) -> Result<()>;
}

impl SpritesProvider {
    /// List endpoint (plural): `…/checkpoints`.
    fn checkpoints_url(&self, name: &str) -> String {
        format!("{}/sprites/{name}/checkpoints", self.api_base)
    }
    /// Create endpoint (SINGULAR): `…/checkpoint` — returns an NDJSON stream.
    fn checkpoint_create_url(&self, name: &str) -> String {
        format!("{}/sprites/{name}/checkpoint", self.api_base)
    }
    fn restore_url(&self, name: &str, cp: &str) -> String {
        format!("{}/sprites/{name}/checkpoints/{cp}/restore", self.api_base)
    }
    /// Parse a checkpoint list element (`{ "id", "comment"? }`).
    fn parse_checkpoint(v: &serde_json::Value) -> Option<CheckpointInfo> {
        let id = v.get("id").and_then(|i| i.as_str())?.to_string();
        let label = v
            .get("comment")
            .or_else(|| v.get("label"))
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Some(CheckpointInfo { id, label })
    }
    /// Extract the new checkpoint id from the create stream's NDJSON lines. The
    /// `complete` message reads `Checkpoint <id> created successfully`; an info
    /// line reads `  ID: <id>`. Try both.
    fn parse_checkpoint_stream(body: &str) -> Option<String> {
        let mut from_complete = None;
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let data = v.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if let Some(rest) = data.trim().strip_prefix("ID:") {
                return Some(rest.trim().to_string());
            }
            if v.get("type").and_then(|t| t.as_str()) == Some("complete")
                && let Some(rest) = data.strip_prefix("Checkpoint ")
            {
                from_complete = rest.split_whitespace().next().map(str::to_string);
            }
        }
        from_complete
    }
}

impl ProviderCheckpoints for SpritesProvider {
    async fn checkpoint(&self, id: &str, label: Option<&str>) -> Result<String> {
        let mut body = serde_json::Map::new();
        if let Some(l) = label.map(str::trim).filter(|l| !l.is_empty()) {
            body.insert("comment".into(), l.into());
        }
        let resp = self
            .client
            .post(self.checkpoint_create_url(id))
            .bearer_auth(&self.token)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .context("sprites: POST /checkpoint")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("sprites checkpoint failed ({status}): {text}"));
        }
        Self::parse_checkpoint_stream(&text)
            .ok_or_else(|| anyhow!("sprites checkpoint: no id in stream: {text}"))
    }

    async fn list_checkpoints(&self, id: &str) -> Result<Vec<CheckpointInfo>> {
        let resp = self
            .client
            .get(self.checkpoints_url(id))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("sprites: GET /checkpoints")?;
        let v: serde_json::Value = resp.json().await.context("sprites: decode checkpoints")?;
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| v.get("checkpoints").and_then(|c| c.as_array()).cloned())
            .unwrap_or_default();
        Ok(arr.iter().filter_map(Self::parse_checkpoint).collect())
    }

    async fn restore(&self, id: &str, checkpoint: &str) -> Result<()> {
        let resp = self
            .client
            .post(self.restore_url(id, checkpoint))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("sprites: POST /checkpoints/{id}/restore")?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("sprites restore failed ({})", resp.status()))
        }
    }
}

/// One entry in a sandbox directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Join a remote base dir and a relative path into a clean remote path.
fn join_remote(base: &str, rel: &str) -> String {
    let base = base.trim_end_matches('/');
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Collect files under `root` as `(absolute, relative)` pairs for upload. Pure +
/// iterative (no recursion); skips `.git` (never sync the repo metadata into a
/// sandbox). Returns an error only if `root` can't be read.
fn collect_files(root: &Path) -> Result<Vec<(std::path::PathBuf, String)>> {
    use ignore::WalkBuilder;
    // A missing source is an error (don't silently sync nothing).
    anyhow::ensure!(
        root.exists(),
        "source dir does not exist: {}",
        root.display()
    );
    // Gitignore-aware: upload the worktree's tracked + untracked WORK but SKIP the
    // build/junk the repo ignores (target/, result/, .direnv/, node_modules/…) —
    // otherwise a `data = "sync"` projection pushes gigabytes of artifacts over the
    // per-file fs API. `.git` is always skipped (the sandbox has its own from the
    // clone). Dotfiles are KEPT (`hidden(false)`) since `.envrc`/`.config` are real
    // state; `require_git(false)` applies `.gitignore` even though a worktree's
    // `.git` is a file pointing into the canonical repo.
    let mut out = Vec::new();
    let walk = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walk.flatten() {
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(root)
        {
            out.push((
                entry.path().to_path_buf(),
                rel.to_string_lossy().replace('\\', "/"),
            ));
        }
    }
    Ok(out)
}

/// Remote filesystem access — backs a provider `sync` projection (push the local
/// worktree into the sandbox fs, pull changes back). `read`/`write`/`list_dir`/
/// `delete` are provider-specific; the recursive `upload_dir`/`download_dir` are
/// generic defaults over them, so any provider gets directory sync for free.
#[allow(async_fn_in_trait)]
pub trait ProviderFiles: Send + Sync {
    async fn read(&self, id: &str, path: &str) -> Result<Vec<u8>>;
    async fn write(&self, id: &str, path: &str, data: &[u8]) -> Result<()>;
    async fn list_dir(&self, id: &str, path: &str) -> Result<Vec<FileEntry>>;
    async fn delete(&self, id: &str, path: &str) -> Result<()>;

    /// Write `data` as an **executable** (mode 0755 where the provider's file API
    /// supports a mode). Default delegates to [`write`](Self::write) — the bytes
    /// land, the exec bit is best-effort; providers with a mode override this.
    async fn write_exec(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.write(id, path, data).await
    }

    /// Recursively upload `local` into the sandbox at `remote` (skips `.git`).
    async fn upload_dir(&self, id: &str, local: &Path, remote: &str) -> Result<()> {
        for (abs, rel) in collect_files(local)? {
            let data = std::fs::read(&abs).with_context(|| format!("read {}", abs.display()))?;
            self.write(id, &join_remote(remote, &rel), &data).await?;
        }
        Ok(())
    }

    /// Recursively download the sandbox `remote` dir into `local`.
    async fn download_dir(&self, id: &str, remote: &str, local: &Path) -> Result<()> {
        let mut stack = vec![String::new()]; // rel dirs to visit
        while let Some(rel) = stack.pop() {
            let listing = self.list_dir(id, &join_remote(remote, &rel)).await?;
            for e in listing {
                if e.name == ".git" {
                    continue;
                }
                let child = if rel.is_empty() {
                    e.name.clone()
                } else {
                    format!("{rel}/{}", e.name)
                };
                if e.is_dir {
                    stack.push(child);
                } else {
                    let data = self.read(id, &join_remote(remote, &child)).await?;
                    let dest = local.join(&child);
                    if let Some(p) = dest.parent() {
                        std::fs::create_dir_all(p).ok();
                    }
                    std::fs::write(&dest, data)
                        .with_context(|| format!("write {}", dest.display()))?;
                }
            }
        }
        Ok(())
    }
}

impl SpritesProvider {
    /// The fs operation endpoint: `…/sprites/{name}/fs/{op}` (op = read|write|list
    /// |delete). The target path is a **query param** (`?path=…&workingDir=/`),
    /// not part of the URL — matching the live v1 API (and the `sprites-py` SDK).
    fn fs_op_url(&self, name: &str, op: &str) -> String {
        format!("{}/sprites/{name}/fs/{op}", self.api_base)
    }
    /// PUT `/fs/write` with an explicit unix `mode` (octal string). `write`
    /// uses `0644`; `write_exec` uses `0755` so a pushed binary is runnable.
    async fn write_with_mode(&self, id: &str, path: &str, data: &[u8], mode: &str) -> Result<()> {
        let resp = self
            .client
            .put(self.fs_op_url(id, "write"))
            .bearer_auth(&self.token)
            .query(&[
                ("path", path),
                ("workingDir", "/"),
                ("mode", mode),
                ("mkdirParents", "true"),
            ])
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await
            .context("sprites: PUT /fs/write")?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("sprites write {path} failed ({})", resp.status()))
        }
    }

    /// Parse a listing entry: `{name, size, isDir}` (the server uses camelCase
    /// `isDir`; tolerate `is_dir` too).
    fn parse_entry(v: &serde_json::Value) -> Option<FileEntry> {
        let name = v.get("name")?.as_str()?.to_string();
        let is_dir = v
            .get("isDir")
            .or_else(|| v.get("is_dir"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let size = v.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        Some(FileEntry { name, is_dir, size })
    }
}

impl ProviderFiles for SpritesProvider {
    async fn read(&self, id: &str, path: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(self.fs_op_url(id, "read"))
            .bearer_auth(&self.token)
            .query(&[("path", path), ("workingDir", "/")])
            .send()
            .await
            .context("sprites: GET /fs/read")?;
        if !resp.status().is_success() {
            return Err(anyhow!("sprites read {path} failed ({})", resp.status()));
        }
        Ok(resp.bytes().await.context("sprites: read body")?.to_vec())
    }

    async fn write(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.write_with_mode(id, path, data, "0644").await
    }

    async fn write_exec(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        self.write_with_mode(id, path, data, "0755").await
    }

    async fn list_dir(&self, id: &str, path: &str) -> Result<Vec<FileEntry>> {
        let resp = self
            .client
            .get(self.fs_op_url(id, "list"))
            .bearer_auth(&self.token)
            .query(&[("path", path), ("workingDir", "/")])
            .send()
            .await
            .context("sprites: GET /fs/list")?;
        if !resp.status().is_success() {
            return Err(anyhow!("sprites list {path} failed ({})", resp.status()));
        }
        let v: serde_json::Value = resp.json().await.context("sprites: decode listing")?;
        let arr = v
            .get("entries")
            .and_then(|e| e.as_array())
            .cloned()
            .or_else(|| v.as_array().cloned())
            .unwrap_or_default();
        Ok(arr.iter().filter_map(Self::parse_entry).collect())
    }

    async fn delete(&self, id: &str, path: &str) -> Result<()> {
        let resp = self
            .client
            .delete(self.fs_op_url(id, "delete"))
            .bearer_auth(&self.token)
            .query(&[("path", path), ("workingDir", "/"), ("recursive", "true")])
            .send()
            .await
            .context("sprites: DELETE /fs/delete")?;
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(anyhow!("sprites delete {path} failed ({})", resp.status()))
        }
    }
}

/// What a provider supports beyond lifecycle — drives CLI/UI gating so the rest
/// of the system degrades gracefully (no snapshot command for a provider that
/// can't checkpoint, etc.). Flags flip on as each axis is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderCaps {
    pub files: bool,
    pub checkpoints: bool,
    pub egress: bool,
    pub exec_api: bool,
}

/// Whether the named provider has a native exec API (PTY-over-WebSocket), so an
/// env can prefer it over the vendor CLI *without* first constructing a
/// (token-requiring) [`Provider`]. Mirrors `Provider::caps().exec_api` by name.
pub fn exec_api_by_name(provider: &str) -> bool {
    matches!(provider.trim(), "sprites")
}

/// A cheap, dependency-free content fingerprint (FNV-1a 64-bit, hex). Used by
/// [`Provider::ensure_executable`] to decide whether a pushed binary needs
/// re-uploading — collision-resistance isn't security-critical here, only
/// change-detection, so a fast non-crypto hash is the right tool.
pub fn fingerprint(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

// ===========================================================================
// Native exec (the `exec_api` capability) — a generic, provider-agnostic PTY
// stream so an interactive pane attaches over the provider's API instead of a
// local PTY child running a vendor CLI. The transport is decoupled via channels
// (async-fn-in-trait isn't object-safe, so there's no `dyn ExecStream`): a
// provider's `open_exec`/`attach_exec` spawns a driver task that bridges its
// socket to these channels, and the host wires them into a pane.
// ===========================================================================

/// What to run for a native exec session (provider-agnostic).
#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// The command + args to run in the sandbox (e.g. the login shell).
    pub argv: Vec<String>,
    /// Allocate a PTY (interactive panes always do).
    pub tty: bool,
    pub cols: u16,
    pub rows: u16,
    /// Extra environment as `KEY=VALUE` pairs.
    pub env: Vec<(String, String)>,
    /// Working directory inside the sandbox. Providers without a cwd param bake
    /// it into `argv` upstream; `None` ⇒ the sandbox default.
    pub cwd: Option<String>,
}

/// One server→client event from a native exec session. PTY mode merges stderr
/// into the pty stream, so only `Stdout`/`Exit` occur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecFrame {
    Stdout(Vec<u8>),
    Exit(i32),
}

/// One client→server control message for a native exec session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecControl {
    Stdin(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// A live native exec session: a provider-agnostic handle built from channels.
/// The provider spawns a driver task that bridges the underlying transport (e.g.
/// a WebSocket) to these channels until the socket closes or `Close` is sent.
pub struct ExecSession {
    /// Server→client output and the terminal exit.
    pub frames: tokio::sync::mpsc::Receiver<ExecFrame>,
    /// Client→server stdin/resize/close.
    pub control: tokio::sync::mpsc::Sender<ExecControl>,
    /// The provider session id once the server announces it (for reattach). It
    /// arrives asynchronously on connect, so it's a watch the caller reads when
    /// persisting — `None` until the session-info frame lands.
    pub session_id: tokio::sync::watch::Receiver<Option<String>>,
}

/// A raw bidirectional byte stream to an in-sandbox `host:port`, tunneled over the
/// provider's TCP-over-WebSocket proxy. Unlike [`ExecSession`] there is no framing
/// — bytes are forwarded verbatim — so it can carry an arbitrary protocol (e.g. an
/// SSH connection to an in-sandbox `sshd`). The driver task lives on the ambient
/// runtime until either side closes.
pub struct ProxyStream {
    /// Server→client bytes (data from the in-sandbox service).
    pub rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// Client→server bytes (data to the in-sandbox service). Dropping it (or
    /// sending nothing) and draining `rx` to `None` ends the stream.
    pub tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

/// Rewrite an `http(s)` API base to its `ws(s)` scheme for the WebSocket handshake.
fn ws_scheme(api_base: &str) -> String {
    if let Some(rest) = api_base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = api_base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        api_base.to_string()
    }
}

/// Build the Sprites open-exec WSS URL: `wss://…/sprites/{id}/exec?cmd=…&tty=…`.
fn exec_ws_url(api_base: &str, id: &str, spec: &ExecSpec) -> Result<String> {
    let base = ws_scheme(api_base);
    let mut url = reqwest::Url::parse(&format!("{base}/sprites/{id}/exec"))
        .context("sprites: bad exec url")?;
    {
        let mut q = url.query_pairs_mut();
        for a in &spec.argv {
            q.append_pair("cmd", a);
        }
        q.append_pair("tty", if spec.tty { "true" } else { "false" });
        q.append_pair("stdin", "true");
        q.append_pair("cols", &spec.cols.to_string());
        q.append_pair("rows", &spec.rows.to_string());
        for (k, v) in &spec.env {
            q.append_pair("env", &format!("{k}={v}"));
        }
    }
    Ok(url.to_string())
}

/// Build the Sprites reattach WSS URL: `wss://…/sprites/{id}/exec/{session}?cols=…&rows=…`.
fn attach_ws_url(api_base: &str, id: &str, session: &str, cols: u16, rows: u16) -> Result<String> {
    let base = ws_scheme(api_base);
    let mut url = reqwest::Url::parse(&format!("{base}/sprites/{id}/exec/{session}"))
        .context("sprites: bad attach url")?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("cols", &cols.to_string());
        q.append_pair("rows", &rows.to_string());
    }
    Ok(url.to_string())
}

/// Build the Sprites TCP-proxy WSS URL: `wss://…/sprites/{id}/proxy`.
fn proxy_ws_url(api_base: &str, id: &str) -> Result<String> {
    let base = ws_scheme(api_base);
    Ok(reqwest::Url::parse(&format!("{base}/sprites/{id}/proxy"))
        .context("sprites: bad proxy url")?
        .to_string())
}

/// The init frame the Sprites `/proxy` socket expects before it becomes a raw
/// relay: `{"host":"…","port":N}`.
fn proxy_init_json(host: &str, port: u16) -> String {
    format!(
        "{{\"host\":{},\"port\":{port}}}",
        serde_json::to_string(host).unwrap_or_else(|_| "\"127.0.0.1\"".into())
    )
}

/// The resize control frame the Sprites exec socket expects.
fn resize_json(cols: u16, rows: u16) -> String {
    format!("{{\"type\":\"resize\",\"cols\":{cols},\"rows\":{rows}}}")
}

// --- non-PTY (tty=false) binary stream framing -----------------------------
// In non-PTY mode each binary frame is prefixed with a 1-byte stream id:
//   0=stdin (client→server), 1=stdout, 2=stderr, 3=exit, 4=stdin-EOF.
// (PTY mode is raw, no prefix.) The bridge runs non-PTY so stdout stays a clean
// byte stream separate from the agent's stderr logs.

/// Prefix client stdin with the stdin stream id (0) for non-PTY mode.
fn encode_stream_stdin(data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(data.len() + 1);
    v.push(0u8);
    v.extend_from_slice(data);
    v
}

/// A decoded non-PTY server→client binary frame.
#[derive(Debug, PartialEq, Eq)]
enum StreamMsg {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    /// Empty/unknown stream id — ignored.
    Ignore,
}

/// Split a non-PTY server→client binary frame on its 1-byte stream id.
fn decode_stream_frame(b: &[u8]) -> StreamMsg {
    let Some((&id, rest)) = b.split_first() else {
        return StreamMsg::Ignore;
    };
    match id {
        1 => StreamMsg::Stdout(rest.to_vec()),
        2 => StreamMsg::Stderr(rest.to_vec()),
        3 => StreamMsg::Exit(String::from_utf8_lossy(rest).trim().parse().unwrap_or(0)),
        _ => StreamMsg::Ignore,
    }
}

/// A parsed Sprites exec text (control) frame.
#[derive(Debug, PartialEq, Eq)]
enum SpriteCtrl {
    /// The session-info frame announced its `session_id` (for reattach).
    Session(String),
    /// The command exited with this code.
    Exit(i32),
    /// A frame we don't act on (port notifications, unknown shapes).
    Ignore,
}

/// Classify a Sprites exec text frame: `{"type":"exit","exit_code":N}` ⇒ exit,
/// a frame carrying `session_id` ⇒ the session announcement, else ignore.
fn parse_sprite_ctrl(text: &str) -> SpriteCtrl {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return SpriteCtrl::Ignore;
    };
    if v.get("type").and_then(|t| t.as_str()) == Some("exit") {
        let code = v.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(0);
        return SpriteCtrl::Exit(code as i32);
    }
    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
        return SpriteCtrl::Session(sid.to_string());
    }
    SpriteCtrl::Ignore
}

impl SpritesProvider {
    /// Open a native PTY exec session over the Sprites WSS exec API — no `sprite`
    /// CLI. The returned [`ExecSession`]'s driver task lives on the ambient tokio
    /// runtime until the socket closes or `ExecControl::Close` is sent.
    pub async fn open_exec(&self, id: &str, spec: &ExecSpec) -> Result<ExecSession> {
        let url = exec_ws_url(&self.api_base, id, spec)?;
        self.start_session(url, spec.tty).await
    }

    /// Reattach to a persisted exec session (the server replays its scrollback).
    /// Reattach is always interactive (PTY), so the stream is raw.
    pub async fn attach_exec(
        &self,
        id: &str,
        session: &str,
        cols: u16,
        rows: u16,
    ) -> Result<ExecSession> {
        let url = attach_ws_url(&self.api_base, id, session, cols, rows)?;
        self.start_session(url, true).await
    }

    /// Run a one-shot command in the sprite over the WSS exec API (NON-tty) and
    /// collect its combined output + exit code. This is the CLI-free / bridge-free
    /// channel the env provisioner uses to set the sandbox up (the non-tty framing
    /// surfaces stdout + the exit; redirect stderr with `2>&1` in the script to
    /// capture it). Returns `(exit_code, output)`.
    pub async fn run_exec(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        let spec = ExecSpec {
            argv: argv.to_vec(),
            tty: false,
            cols: 0,
            rows: 0,
            env: env.to_vec(),
            cwd: cwd.map(str::to_string),
        };
        let mut sess = self.open_exec(id, &spec).await?;
        let mut out: Vec<u8> = Vec::new();
        let mut code = -1;
        while let Some(frame) = sess.frames.recv().await {
            match frame {
                ExecFrame::Stdout(b) => out.extend_from_slice(&b),
                ExecFrame::Exit(c) => {
                    code = c;
                    break;
                }
            }
        }
        let _ = sess.control.send(ExecControl::Close).await;
        Ok((code, String::from_utf8_lossy(&out).into_owned()))
    }

    /// Run the WSS handshake (bearer auth) and spawn the bridge task. `tty`
    /// selects the wire framing: raw (PTY) vs 1-byte stream-id prefixes (non-PTY).
    async fn start_session(&self, url: String, tty: bool) -> Result<ExecSession> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let auth: tokio_tungstenite::tungstenite::http::HeaderValue =
            format!("Bearer {}", self.token)
                .parse()
                .context("sprites: exec auth header")?;
        // A freshly-created sprite's exec endpoint isn't up during its first cold
        // boot, so a single `connect_async` hangs on a dead endpoint (~OS timeout)
        // and then fatally aborts the provision's `workspace` step. Retry with a
        // short per-attempt timeout + backoff so the connect lands the moment the
        // sprite warms, within a bounded budget (a genuinely unreachable sprite
        // still errors, just after the budget rather than on a lone long hang).
        const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
        const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(90);
        const BACKOFF: std::time::Duration = std::time::Duration::from_secs(3);
        let start = std::time::Instant::now();
        let ws = loop {
            let mut req = url
                .clone()
                .into_client_request()
                .context("sprites: build exec ws request")?;
            req.headers_mut().insert("Authorization", auth.clone());
            match tokio::time::timeout(ATTEMPT_TIMEOUT, tokio_tungstenite::connect_async(req)).await
            {
                Ok(Ok((ws, _resp))) => break ws,
                res => {
                    if start.elapsed() >= CONNECT_BUDGET {
                        return match res {
                            Ok(Err(e)) => Err(e).context("sprites: exec ws connect"),
                            _ => Err(anyhow!(
                                "sprites: exec ws connect timed out after {}s (sandbox never became ready)",
                                CONNECT_BUDGET.as_secs()
                            )),
                        };
                    }
                    tracing::debug!(
                        target: "szhost::sandbox",
                        elapsed_s = start.elapsed().as_secs(),
                        "sprite exec endpoint not ready (cold boot?); retrying connect"
                    );
                    tokio::time::sleep(BACKOFF).await;
                }
            }
        };

        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<ExecFrame>(256);
        let (control_tx, control_rx) = tokio::sync::mpsc::channel::<ExecControl>(256);
        let (sid_tx, sid_rx) = tokio::sync::watch::channel::<Option<String>>(None);

        tokio::spawn(drive_exec(ws, tty, frames_tx, control_rx, sid_tx));

        Ok(ExecSession {
            frames: frames_rx,
            control: control_tx,
            session_id: sid_rx,
        })
    }

    /// Open a raw TCP relay to an in-sandbox `host:port` over the Sprites `/proxy`
    /// WSS endpoint (bearer auth + a `{host,port}` init frame, then a transparent
    /// byte relay). Used to tunnel an `ssh` connection to an in-sandbox `sshd`.
    pub async fn open_proxy(&self, id: &str, host: &str, port: u16) -> Result<ProxyStream> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let url = proxy_ws_url(&self.api_base, id)?;
        let mut req = url
            .into_client_request()
            .context("sprites: build proxy ws request")?;
        let auth = format!("Bearer {}", self.token)
            .parse()
            .context("sprites: proxy auth header")?;
        req.headers_mut().insert("Authorization", auth);
        let (mut ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .context("sprites: proxy ws connect")?;
        // The init frame selects the target; after it, the socket is a raw relay.
        ws.send(Message::Text(proxy_init_json(host, port)))
            .await
            .context("sprites: proxy init")?;

        let (to_srv_tx, to_srv_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let (from_srv_tx, from_srv_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        tokio::spawn(drive_proxy(ws, to_srv_rx, from_srv_tx));
        Ok(ProxyStream {
            rx: from_srv_rx,
            tx: to_srv_tx,
        })
    }
}

/// The proxy relay task: pump raw bytes WebSocket ⇄ the [`ProxyStream`] channels.
/// No framing — binary frames are forwarded verbatim in both directions.
async fn drive_proxy<S>(
    ws: S,
    mut to_srv: tokio::sync::mpsc::Receiver<Vec<u8>>,
    from_srv: tokio::sync::mpsc::Sender<Vec<u8>>,
) where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message>
        + futures_util::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let (mut write, mut read) = ws.split();
    loop {
        tokio::select! {
            out = to_srv.recv() => match out {
                Some(b) => {
                    if write.send(Message::Binary(b)).await.is_err() {
                        break;
                    }
                }
                None => break, // client closed
            },
            msg = read.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if from_srv.send(b).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // ignore text/ping/pong
                Some(Err(_)) => break,
            },
        }
    }
    let _ = write.send(Message::Close(None)).await;
}

/// The exec bridge task: pump WebSocket frames ⇄ the [`ExecSession`] channels.
/// Generic over the socket so the concrete (TLS) stream type need not be named.
/// `tty` selects the wire framing: raw bytes (PTY) vs 1-byte stream-id prefixes
/// (non-PTY — stdin gets a `0` prefix; only stdout/exit are surfaced, stderr is
/// dropped so a non-PTY consumer like the bridge sees a clean byte stream).
async fn drive_exec<S>(
    ws: S,
    tty: bool,
    frames_tx: tokio::sync::mpsc::Sender<ExecFrame>,
    mut control_rx: tokio::sync::mpsc::Receiver<ExecControl>,
    sid_tx: tokio::sync::watch::Sender<Option<String>>,
) where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message>
        + futures_util::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let (mut write, mut read) = ws.split();
    loop {
        tokio::select! {
            ctrl = control_rx.recv() => match ctrl {
                Some(ExecControl::Stdin(b)) => {
                    let frame = if tty { b } else { encode_stream_stdin(&b) };
                    if write.send(Message::Binary(frame)).await.is_err() {
                        break;
                    }
                }
                Some(ExecControl::Resize { cols, rows }) => {
                    let _ = write.send(Message::Text(resize_json(cols, rows))).await;
                }
                Some(ExecControl::Close) | None => {
                    let _ = write.send(Message::Close(None)).await;
                    break;
                }
            },
            msg = read.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if tty {
                        if frames_tx.send(ExecFrame::Stdout(b)).await.is_err() {
                            break;
                        }
                    } else {
                        match decode_stream_frame(&b) {
                            StreamMsg::Stdout(d) => {
                                if frames_tx.send(ExecFrame::Stdout(d)).await.is_err() {
                                    break;
                                }
                            }
                            StreamMsg::Exit(code) => {
                                let _ = frames_tx.send(ExecFrame::Exit(code)).await;
                                break;
                            }
                            // stderr (agent logs) and unknown ids: not part of the
                            // consumer's byte stream.
                            StreamMsg::Stderr(_) | StreamMsg::Ignore => {}
                        }
                    }
                }
                Some(Ok(Message::Text(t))) => match parse_sprite_ctrl(t.as_str()) {
                    SpriteCtrl::Session(s) => {
                        let _ = sid_tx.send(Some(s));
                    }
                    SpriteCtrl::Exit(code) => {
                        let _ = frames_tx.send(ExecFrame::Exit(code)).await;
                        break;
                    }
                    SpriteCtrl::Ignore => {}
                },
                Some(Ok(Message::Ping(p))) => {
                    let _ = write.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    let _ = frames_tx.send(ExecFrame::Exit(0)).await;
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => {
                    let _ = frames_tx.send(ExecFrame::Exit(-1)).await;
                    break;
                }
            },
        }
    }
}

/// The generic managed-sandbox provider dispatcher. Lifecycle methods delegate to
/// the variant's [`RemoteProvider`]; axis methods delegate to the variant's
/// sub-trait impl or return a clear "unsupported" error (gate on [`caps`]).
pub enum Provider {
    Daytona(DaytonaProvider),
    Sprites(SpritesProvider),
}

impl Provider {
    /// What this provider supports beyond lifecycle.
    pub fn caps(&self) -> ProviderCaps {
        match self {
            Provider::Daytona(_) => ProviderCaps::default(),
            // Flags flip on as each Sprites axis lands.
            Provider::Sprites(_) => ProviderCaps {
                egress: true,
                checkpoints: true,
                files: true,
                exec_api: true,
            },
        }
    }

    pub async fn create(&self) -> Result<SandboxHandle> {
        match self {
            Provider::Daytona(p) => p.create().await,
            Provider::Sprites(p) => p.create().await,
        }
    }

    pub async fn destroy(&self, id: &str) -> Result<()> {
        match self {
            Provider::Daytona(p) => p.destroy(id).await,
            Provider::Sprites(p) => p.destroy(id).await,
        }
    }

    pub async fn list(&self) -> Result<Vec<String>> {
        match self {
            Provider::Daytona(p) => p.list().await,
            Provider::Sprites(p) => p.list().await,
        }
    }

    /// Idempotently ensure a sandbox named `name` exists — the warm-on-open
    /// lifecycle. Returns `true` if it created one, `false` if it already existed.
    /// (List-then-create; a provider with a truly idempotent create can override.)
    pub async fn ensure_exists(&self, name: &str) -> Result<bool> {
        if self.list().await?.iter().any(|n| n == name) {
            return Ok(false);
        }
        self.create().await?;
        Ok(true)
    }

    /// Lower allow/block lists to the provider's network policy (egress translate).
    pub async fn set_network_policy(
        &self,
        id: &str,
        allow: &[String],
        block: &[String],
    ) -> Result<()> {
        match self {
            Provider::Sprites(p) => p.set_network_policy(id, allow, block).await,
            Provider::Daytona(_) => Err(anyhow!(
                "provider 'daytona' does not support egress translation"
            )),
        }
    }

    /// Create a checkpoint of the sandbox, returning its id.
    pub async fn checkpoint(&self, id: &str, label: Option<&str>) -> Result<String> {
        match self {
            Provider::Sprites(p) => p.checkpoint(id, label).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' does not support checkpoints")),
        }
    }

    pub async fn list_checkpoints(&self, id: &str) -> Result<Vec<CheckpointInfo>> {
        match self {
            Provider::Sprites(p) => p.list_checkpoints(id).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' does not support checkpoints")),
        }
    }

    pub async fn restore(&self, id: &str, checkpoint: &str) -> Result<()> {
        match self {
            Provider::Sprites(p) => p.restore(id, checkpoint).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' does not support checkpoints")),
        }
    }

    /// Read a file from the sandbox fs.
    pub async fn read(&self, id: &str, path: &str) -> Result<Vec<u8>> {
        match self {
            Provider::Sprites(p) => p.read(id, path).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' does not support file sync")),
        }
    }

    /// Write a file into the sandbox fs.
    pub async fn write(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        match self {
            Provider::Sprites(p) => p.write(id, path, data).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' does not support file sync")),
        }
    }

    /// Write an executable into the sandbox fs (mode 0755 where supported).
    pub async fn write_exec(&self, id: &str, path: &str, data: &[u8]) -> Result<()> {
        match self {
            Provider::Sprites(p) => p.write_exec(id, path, data).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' does not support file sync")),
        }
    }

    /// Idempotently install the executable `data` at `path` in the sandbox: a
    /// content-addressed handshake. Compares a fingerprint stored at `<path>.fp`
    /// and (re)uploads only on mismatch, returning whether it pushed (`true`) or
    /// the env was already current (`false`). This is the resident-bridge binary
    /// push — content-addressed so a rebuilt binary re-pushes, an unchanged one
    /// does not. Requires the provider's file API (`caps().files`).
    pub async fn ensure_executable(&self, id: &str, path: &str, data: &[u8]) -> Result<bool> {
        if !self.caps().files {
            return Err(anyhow!("provider does not support a file push"));
        }
        let want = fingerprint(data);
        let fp_path = format!("{path}.fp");
        let current = self
            .read(id, &fp_path)
            .await
            .ok()
            .and_then(|b| String::from_utf8(b).ok());
        if current.as_deref() == Some(want.as_str()) {
            return Ok(false);
        }
        self.write_exec(id, path, data).await?;
        self.write(id, &fp_path, want.as_bytes()).await?;
        Ok(true)
    }

    /// Push a local directory into the sandbox fs (provider `sync` projection).
    pub async fn upload_dir(&self, id: &str, local: &Path, remote: &str) -> Result<()> {
        match self {
            Provider::Sprites(p) => p.upload_dir(id, local, remote).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' does not support file sync")),
        }
    }

    /// Pull the sandbox fs back into a local directory.
    pub async fn download_dir(&self, id: &str, remote: &str, local: &Path) -> Result<()> {
        match self {
            Provider::Sprites(p) => p.download_dir(id, remote, local).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' does not support file sync")),
        }
    }

    /// Open a native PTY exec session (the `exec_api` capability), so an
    /// interactive pane attaches over the provider API with no vendor CLI. Gate
    /// on [`caps`]`().exec_api`.
    pub async fn open_exec(&self, id: &str, spec: &ExecSpec) -> Result<ExecSession> {
        match self {
            Provider::Sprites(p) => p.open_exec(id, spec).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' has no native exec API")),
        }
    }

    /// Open a raw TCP relay to an in-sandbox `host:port` over the provider's
    /// TCP-over-WebSocket proxy. Used to tunnel `ssh` to an in-sandbox `sshd`
    /// (`[env.<name>.provider] connect = "ssh"`).
    pub async fn open_proxy(&self, id: &str, host: &str, port: u16) -> Result<ProxyStream> {
        match self {
            Provider::Sprites(p) => p.open_proxy(id, host, port).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' has no TCP proxy API")),
        }
    }

    /// Run a one-shot command (non-tty) and collect `(exit_code, output)`. Used by
    /// the env provisioner to set up the sandbox without a CLI/bridge.
    pub async fn run_exec(
        &self,
        id: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i32, String)> {
        match self {
            Provider::Sprites(p) => p.run_exec(id, argv, cwd, env).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' has no native exec API")),
        }
    }

    /// Reattach to a persisted exec session (replays scrollback). `exec_api` only.
    pub async fn attach_exec(
        &self,
        id: &str,
        session: &str,
        cols: u16,
        rows: u16,
    ) -> Result<ExecSession> {
        match self {
            Provider::Sprites(p) => p.attach_exec(id, session, cols, rows).await,
            Provider::Daytona(_) => Err(anyhow!("provider 'daytona' has no native exec API")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> DaytonaProvider {
        DaytonaProvider::new("https://app.daytona.io/api/", "tok", "debian:stable")
    }

    fn sprites() -> SpritesProvider {
        SpritesProvider::new("https://api.sprites.dev/v1/", "tok", "my-sprite")
    }

    #[test]
    fn sprites_default_base_and_urls() {
        let p = sprites();
        assert_eq!(p.sprites_url(), "https://api.sprites.dev/v1/sprites");
        assert_eq!(
            p.sprite_name_url("my-sprite"),
            "https://api.sprites.dev/v1/sprites/my-sprite"
        );
        // Empty api_base falls back to the documented default.
        let d = SpritesProvider::new("", "t", "s");
        assert_eq!(d.sprites_url(), "https://api.sprites.dev/v1/sprites");
    }

    #[test]
    fn fingerprint_is_stable_and_change_sensitive() {
        let a = fingerprint(b"szhost-binary-bytes");
        assert_eq!(a, fingerprint(b"szhost-binary-bytes"), "stable");
        assert_eq!(a.len(), 16, "16 hex chars (64-bit)");
        assert_ne!(
            a,
            fingerprint(b"szhost-binary-bytez"),
            "1-byte change differs"
        );
        assert_ne!(a, fingerprint(b""), "empty differs");
    }

    #[test]
    fn sprites_create_body_names_the_sprite() {
        assert_eq!(
            SpritesProvider::create_body("dev"),
            serde_json::json!({"name": "dev"})
        );
    }

    #[test]
    fn sprites_parse_name_flat_and_enveloped() {
        assert_eq!(
            SpritesProvider::parse_name(&serde_json::json!({"name": "a"})).as_deref(),
            Some("a")
        );
        assert_eq!(
            SpritesProvider::parse_name(&serde_json::json!({"sprite": {"name": "b"}})).as_deref(),
            Some("b")
        );
        assert_eq!(
            SpritesProvider::parse_name(&serde_json::json!({"id": 1})),
            None
        );
    }

    #[test]
    fn sprites_parse_list_array_and_envelope() {
        let arr = serde_json::json!([{"name": "a"}, {"name": "b"}, {"id": "no-name"}]);
        assert_eq!(SpritesProvider::parse_list(&arr), vec!["a", "b"]);
        let env = serde_json::json!({"sprites": [{"name": "c"}]});
        assert_eq!(SpritesProvider::parse_list(&env), vec!["c"]);
        assert!(SpritesProvider::parse_list(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn sprites_exec_builds_cli_pty_bridge() {
        assert_eq!(
            SpritesProvider::exec_for("dev"),
            ExecKind::Command(vec![
                "sprite".into(),
                "exec".into(),
                "-s".into(),
                "dev".into(),
                "--tty".into(),
                "--".into(),
            ])
        );
    }

    #[test]
    fn sprites_has_native_exec_capability() {
        assert!(Provider::Sprites(sprites()).caps().exec_api);
        assert!(!Provider::Daytona(provider()).caps().exec_api);
    }

    #[test]
    fn ws_scheme_rewrites_http_family_only() {
        assert_eq!(
            ws_scheme("https://api.sprites.dev/v1"),
            "wss://api.sprites.dev/v1"
        );
        assert_eq!(ws_scheme("http://localhost:8080"), "ws://localhost:8080");
        // Already a ws(s) base is left alone.
        assert_eq!(ws_scheme("wss://x/y"), "wss://x/y");
    }

    #[test]
    fn exec_ws_url_carries_cmd_tty_dims_and_env() {
        let spec = ExecSpec {
            argv: vec!["/bin/sh".into(), "-lc".into(), "exec bash -l".into()],
            tty: true,
            cols: 120,
            rows: 40,
            env: vec![("FOO".into(), "bar baz".into())],
            cwd: None,
        };
        let url = exec_ws_url("https://api.sprites.dev/v1", "dev", &spec).unwrap();
        assert!(url.starts_with("wss://api.sprites.dev/v1/sprites/dev/exec?"));
        // Each arg is a repeated, percent-encoded `cmd` pair (insertion order).
        assert!(url.contains("cmd=%2Fbin%2Fsh"));
        assert!(url.contains("cmd=-lc"));
        assert!(url.contains("cmd=exec+bash+-l"));
        assert!(url.contains("tty=true"));
        assert!(url.contains("stdin=true"));
        assert!(url.contains("cols=120"));
        assert!(url.contains("rows=40"));
        assert!(url.contains("env=FOO%3Dbar+baz"));
    }

    #[test]
    fn attach_ws_url_targets_session_with_dims() {
        let url = attach_ws_url("https://api.sprites.dev/v1", "dev", "sess-123", 80, 24).unwrap();
        assert!(url.starts_with("wss://api.sprites.dev/v1/sprites/dev/exec/sess-123?"));
        assert!(url.contains("cols=80"));
        assert!(url.contains("rows=24"));
    }

    #[test]
    fn resize_json_is_the_documented_frame() {
        assert_eq!(
            resize_json(120, 40),
            r#"{"type":"resize","cols":120,"rows":40}"#
        );
    }

    #[test]
    fn non_pty_stream_framing_roundtrips() {
        // stdin is prefixed with the stdin stream id (0).
        assert_eq!(encode_stream_stdin(b"hi"), vec![0, b'h', b'i']);
        // server frames split on the 1-byte stream id.
        assert_eq!(
            decode_stream_frame(&[1, b'o', b'k']),
            StreamMsg::Stdout(b"ok".to_vec())
        );
        assert_eq!(
            decode_stream_frame(&[2, b'e', b'r', b'r']),
            StreamMsg::Stderr(b"err".to_vec())
        );
        assert_eq!(
            decode_stream_frame(&[3, b'1', b'3', b'7']),
            StreamMsg::Exit(137)
        );
        assert_eq!(decode_stream_frame(&[]), StreamMsg::Ignore);
        assert_eq!(decode_stream_frame(&[9, 1, 2]), StreamMsg::Ignore);
    }

    #[test]
    fn parse_sprite_ctrl_classifies_frames() {
        assert_eq!(
            parse_sprite_ctrl(r#"{"type":"exit","exit_code":0}"#),
            SpriteCtrl::Exit(0)
        );
        assert_eq!(
            parse_sprite_ctrl(r#"{"type":"exit","exit_code":137}"#),
            SpriteCtrl::Exit(137)
        );
        assert_eq!(
            parse_sprite_ctrl(r#"{"session_id":"abc","tty":true,"cols":80}"#),
            SpriteCtrl::Session("abc".into())
        );
        // Port notifications and garbage are ignored, not misclassified.
        assert_eq!(
            parse_sprite_ctrl(r#"{"type":"port_opened","port":8080}"#),
            SpriteCtrl::Ignore
        );
        assert_eq!(parse_sprite_ctrl("not json"), SpriteCtrl::Ignore);
    }

    #[test]
    fn rules_from_empty_is_allow_all() {
        assert!(rules_from(&[], &[]).is_empty());
    }

    #[test]
    fn rules_from_block_only_denies_listed_no_default() {
        let r = rules_from(&[], &["evil.com".into()]);
        assert_eq!(
            r,
            vec![PolicyRule {
                domain: "evil.com".into(),
                action: "deny".into()
            }]
        );
    }

    #[test]
    fn rules_from_allow_list_adds_default_deny_and_block_wins_first() {
        let r = rules_from(
            &["github.com".into(), " *.npmjs.org ".into(), "  ".into()],
            &["bad.com".into()],
        );
        // deny(block) first, then allow rules, then trailing default-deny.
        assert_eq!(
            r,
            vec![
                PolicyRule {
                    domain: "bad.com".into(),
                    action: "deny".into()
                },
                PolicyRule {
                    domain: "github.com".into(),
                    action: "allow".into()
                },
                PolicyRule {
                    domain: "*.npmjs.org".into(),
                    action: "allow".into()
                },
                PolicyRule {
                    domain: "*".into(),
                    action: "deny".into()
                },
            ]
        );
    }

    #[test]
    fn sprites_policy_url_and_caps() {
        assert_eq!(
            sprites().policy_network_url("dev"),
            "https://api.sprites.dev/v1/sprites/dev/policy/network"
        );
        assert!(Provider::Sprites(sprites()).caps().egress);
        assert!(!Provider::Daytona(provider()).caps().egress);
    }

    #[test]
    fn sprites_checkpoint_urls_and_parse() {
        let p = sprites();
        // List is plural; create is SINGULAR; restore is plural/{id}/restore.
        assert_eq!(
            p.checkpoints_url("dev"),
            "https://api.sprites.dev/v1/sprites/dev/checkpoints"
        );
        assert_eq!(
            p.checkpoint_create_url("dev"),
            "https://api.sprites.dev/v1/sprites/dev/checkpoint"
        );
        assert_eq!(
            p.restore_url("dev", "v1"),
            "https://api.sprites.dev/v1/sprites/dev/checkpoints/v1/restore"
        );
        // Real list element shape: flat {id, create_time, is_auto[, comment]}.
        assert_eq!(
            SpritesProvider::parse_checkpoint(
                &serde_json::json!({"id":"v0","create_time":"2026-06-27T05:08:24Z","is_auto":false})
            ),
            Some(CheckpointInfo {
                id: "v0".into(),
                label: None
            })
        );
        assert_eq!(
            SpritesProvider::parse_checkpoint(&serde_json::json!({"id":"c1","comment":"before"})),
            Some(CheckpointInfo {
                id: "c1".into(),
                label: Some("before".into())
            })
        );
        assert!(SpritesProvider::parse_checkpoint(&serde_json::json!({"x":1})).is_none());
        assert!(Provider::Sprites(sprites()).caps().checkpoints);
    }

    #[test]
    fn checkpoint_stream_id_from_real_ndjson() {
        // The exact NDJSON the live API streams from POST /checkpoint.
        let body = "\
{\"type\":\"info\",\"data\":\"Creating checkpoint...\"}\n\
{\"type\":\"info\",\"data\":\"  ID: v1\"}\n\
{\"type\":\"complete\",\"data\":\"Checkpoint v1 created successfully\"}\n";
        assert_eq!(
            SpritesProvider::parse_checkpoint_stream(body).as_deref(),
            Some("v1")
        );
        // Falls back to the `complete` message when there's no explicit ID line.
        let body2 = "{\"type\":\"complete\",\"data\":\"Checkpoint v7 created successfully\"}";
        assert_eq!(
            SpritesProvider::parse_checkpoint_stream(body2).as_deref(),
            Some("v7")
        );
        assert!(SpritesProvider::parse_checkpoint_stream("garbage").is_none());
    }

    #[test]
    fn join_remote_cleans_slashes() {
        assert_eq!(
            join_remote("/workspace/", "src/main.rs"),
            "/workspace/src/main.rs"
        );
        assert_eq!(join_remote("/workspace", "/a/b"), "/workspace/a/b");
        assert_eq!(join_remote("/workspace", ""), "/workspace");
    }

    #[test]
    fn sprites_fs_url_and_entry_parse() {
        let p = sprites();
        assert_eq!(
            p.fs_op_url("dev", "read"),
            "https://api.sprites.dev/v1/sprites/dev/fs/read"
        );
        // The live API uses camelCase `isDir` and numeric `size`.
        assert_eq!(
            SpritesProvider::parse_entry(&serde_json::json!({"name":"src","isDir":true})),
            Some(FileEntry {
                name: "src".into(),
                is_dir: true,
                size: 0
            })
        );
        assert_eq!(
            SpritesProvider::parse_entry(
                &serde_json::json!({"name":"a.rs","isDir":false,"size":12})
            ),
            Some(FileEntry {
                name: "a.rs".into(),
                is_dir: false,
                size: 12
            })
        );
        assert!(Provider::Sprites(sprites()).caps().files);
    }

    #[test]
    fn collect_files_skips_git_and_relativizes() {
        let dir = std::env::temp_dir().join(format!("sz-prov-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("src/main.rs"), b"fn main(){}").unwrap();
        std::fs::write(dir.join("README.md"), b"hi").unwrap();
        std::fs::write(dir.join(".git/config"), b"x").unwrap();
        let mut rels: Vec<String> = collect_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        rels.sort();
        assert_eq!(
            rels,
            vec!["README.md".to_string(), "src/main.rs".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daytona_rejects_egress_translation() {
        let p = Provider::Daytona(provider());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(p.set_network_policy("x", &[], &[]))
            .unwrap_err();
        assert!(err.to_string().contains("does not support egress"));
    }

    #[test]
    fn urls_trim_trailing_slash() {
        let p = provider();
        assert_eq!(p.sandbox_url(), "https://app.daytona.io/api/sandbox");
        assert_eq!(
            p.sandbox_id_url("sb-1"),
            "https://app.daytona.io/api/sandbox/sb-1"
        );
    }

    #[test]
    fn create_body_carries_snapshot_when_set() {
        let p = provider();
        assert_eq!(
            p.create_body(),
            serde_json::json!({"snapshot": "debian:stable"})
        );
        let bare = DaytonaProvider::new("x", "t", "  ");
        assert_eq!(bare.create_body(), serde_json::json!({}));
    }

    #[test]
    fn proxy_url_and_init_frame() {
        assert_eq!(
            proxy_ws_url("https://api.sprites.dev/v1", "s1").unwrap(),
            "wss://api.sprites.dev/v1/sprites/s1/proxy"
        );
        // init frame selects the in-sandbox target; host is JSON-escaped.
        assert_eq!(
            proxy_init_json("127.0.0.1", 22),
            r#"{"host":"127.0.0.1","port":22}"#
        );
    }

    #[test]
    fn parse_id_handles_flat_and_enveloped() {
        assert_eq!(
            DaytonaProvider::parse_id(&serde_json::json!({"id": "abc"})).as_deref(),
            Some("abc")
        );
        assert_eq!(
            DaytonaProvider::parse_id(&serde_json::json!({"sandbox": {"id": "xyz"}})).as_deref(),
            Some("xyz")
        );
        assert_eq!(
            DaytonaProvider::parse_id(&serde_json::json!({"x": 1})),
            None
        );
    }

    #[test]
    fn parse_list_handles_array_and_envelope() {
        let arr = serde_json::json!([{"id": "a"}, {"id": "b"}, {"name": "no-id"}]);
        assert_eq!(DaytonaProvider::parse_list(&arr), vec!["a", "b"]);
        let env = serde_json::json!({"sandboxes": [{"id": "c"}]});
        assert_eq!(DaytonaProvider::parse_list(&env), vec!["c"]);
        assert!(DaytonaProvider::parse_list(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn exec_for_builds_daytona_ssh_bridge() {
        assert_eq!(
            DaytonaProvider::exec_for("sb-9"),
            ExecKind::Command(vec![
                "daytona".into(),
                "ssh".into(),
                "sb-9".into(),
                "--".into()
            ])
        );
    }
}
