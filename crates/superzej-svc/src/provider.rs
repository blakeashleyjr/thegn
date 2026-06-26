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

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> DaytonaProvider {
        DaytonaProvider::new("https://app.daytona.io/api/", "tok", "debian:stable")
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
