//! Vercel Sandbox provider (`provider = "vercel"`): an HTTP control plane,
//! exec over the command API and the `/interactive` WebSocket through the
//! `thegn vercel-exec` self-bridge, and fly-style spend safety (an
//! intent-before-create ledger, an account fingerprint and a lifetime reaper).
//!
//! The thegn sandbox id is the Vercel sandbox **name**; the session id changes
//! on every resume and is resolved per call (cached in-process only).

pub mod client;
pub mod files;
pub mod pty;
pub mod stream;

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use futures_util::future::BoxFuture;
use thegn_core::vercel::api::{SandboxView, Scope};

use crate::provider::{InventoryReport, PolicyRule, ProviderEgress, RemoteProvider, SandboxHandle};

/// Whether a call may resume a stopped sandbox (which buys compute).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// Resume a stopped sandbox if needed (explicit open, interactive panes).
    Allow,
    /// Never resume: a stopped sandbox is an error (chrome reads, reaper).
    Never,
}

/// The Vercel API token. Redacted `Debug`; no `Display`/`Serialize`. Sent only
/// as the `Authorization` header to the validated `api_base` authority.
pub struct VercelToken(zeroize::Zeroizing<String>);

impl VercelToken {
    pub fn new(token: String) -> Self {
        VercelToken(zeroize::Zeroizing::new(token))
    }

    /// The raw token, for the `Authorization` header only.
    #[allow(dead_code)] // spine stub, filled by lane 3
    pub(crate) fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for VercelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VercelToken(<redacted>)")
    }
}

/// Everything needed to drive one Vercel env.
#[derive(Debug)]
pub struct VercelSpec {
    /// Control-plane base (empty ⇒ [`thegn_core::vercel::api::DEFAULT_API_BASE`]).
    pub api_base: String,
    pub token: VercelToken,
    /// The configured secret ref name (value-free account identity).
    pub credential_ref: String,
    pub scope: Scope,
    /// The sandbox name this provider creates (empty for inventory-only use).
    pub name: String,
    pub vcpus: u32,
    pub image: Option<String>,
    pub region: Option<String>,
    pub max_instances: u32,
    pub max_lifetime_secs: u64,
}

fn not_built() -> anyhow::Error {
    anyhow!("vercel: not implemented in this lane")
}

/// The Vercel provider. The token lives only in `client`; `spec.token` is
/// emptied at construction.
#[allow(dead_code)] // spine stub, filled by lane 4
pub struct VercelProvider {
    spec: VercelSpec,
    client: client::VercelClient,
    owner: Option<String>,
    /// name -> current session id (in-process cache only).
    sessions: Mutex<HashMap<String, String>>,
}

impl VercelProvider {
    pub fn new(mut spec: VercelSpec) -> Result<Self> {
        let token = std::mem::replace(&mut spec.token, VercelToken::new(String::new()));
        let base = if spec.api_base.trim().is_empty() {
            thegn_core::vercel::api::DEFAULT_API_BASE.to_string()
        } else {
            spec.api_base.trim().to_string()
        };
        let client = client::VercelClient::new(&base, token, spec.scope.clone())?;
        Ok(VercelProvider {
            spec,
            client,
            owner: None,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn with_owner(mut self, owner: String) -> Self {
        self.owner = Some(owner);
        self
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Value-free account fingerprint for the ledger. Spine stub, filled by
    /// lane 4.
    pub fn account(&self) -> Result<String> {
        Err(not_built())
    }

    /// `thegn fleet` inventory. Never creates or resumes. Spine stub, filled
    /// by lane 4.
    pub async fn sandbox_inventory(&self) -> Result<InventoryReport> {
        Err(not_built())
    }

    /// The current session id for `name`, resuming only under
    /// [`Wake::Allow`]. Spine stub, filled by lane 4.
    pub async fn session_for(&self, _name: &str, _wake: Wake) -> Result<String> {
        Err(not_built())
    }

    /// Resume a stopped sandbox. Spine stub, filled by lane 4.
    pub async fn resume(&self, _name: &str) -> Result<()> {
        Err(not_built())
    }

    /// Stop the current session (compute to $0; the filesystem persists).
    /// Spine stub, filled by lane 4.
    pub async fn stop_session(&self, _name: &str) -> Result<()> {
        Err(not_built())
    }

    /// Extend the session timeout when due (bridge-only). Spine stub, filled
    /// by lane 4.
    pub async fn extend_if_due(&self, _name: &str) -> Result<Option<u64>> {
        Err(not_built())
    }

    /// One-shot command: `(exit_code, output)`. Spine stub, filled by lane 4.
    pub async fn run_exec(
        &self,
        _name: &str,
        _argv: &[String],
        _cwd: Option<&str>,
        _env: &[(String, String)],
        _wake: Wake,
    ) -> Result<(i32, String)> {
        Err(not_built())
    }

    /// Delete the sandbox and its snapshots. Spine stub, filled by lane 4.
    pub async fn delete_with_snapshots(&self, _name: &str) -> Result<()> {
        Err(not_built())
    }

    /// The reaper's read of one sandbox (never resumes). Spine stub, filled by
    /// lane 4.
    pub async fn reaper_view(&self, _name: &str) -> Result<Option<SandboxView>> {
        Err(not_built())
    }
}

impl RemoteProvider for VercelProvider {
    fn create<'a>(&'a self) -> BoxFuture<'a, Result<SandboxHandle>> {
        Box::pin(async move { Err(not_built()) })
    }

    fn destroy<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Err(not_built()) })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move { Err(not_built()) })
    }
}

impl ProviderEgress for VercelProvider {
    fn set_network_policy<'a>(
        &'a self,
        _id: &'a str,
        _allow: &'a [String],
        _block: &'a [String],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Err(not_built()) })
    }

    fn get_network_policy<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<Vec<PolicyRule>>> {
        Box::pin(async move { Err(not_built()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_debug_is_redacted() {
        let token = VercelToken::new("vcp_secret_value".into());
        let shown = format!("{token:?}");
        assert!(!shown.contains("vcp_secret_value"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn spec_token_is_emptied_into_the_client() {
        let provider = VercelProvider::new(VercelSpec {
            api_base: String::new(),
            token: VercelToken::new("vcp_secret_value".into()),
            credential_ref: "VERCEL_TOKEN".into(),
            scope: Scope {
                team_id: "team_a".into(),
                project_id: "prj_a".into(),
            },
            name: "tg-a".into(),
            vcpus: 2,
            image: None,
            region: None,
            max_instances: 2,
            max_lifetime_secs: 3600,
        })
        .expect("spine stub constructs");
        assert_eq!(provider.spec.token.expose(), "");
        assert!(!format!("{:?}", provider.spec).contains("vcp_secret_value"));
        let owned = provider.with_owner("wt-1".into());
        assert_eq!(owned.owner(), Some("wt-1"));
    }
}
