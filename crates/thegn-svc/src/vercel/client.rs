//! The Vercel Sandbox HTTP client: the only holder of the API token. Reuses
//! `provider_http_client()`, `CONTROL_GATE`, `validate_api_base` and the
//! body-limited readers.

use anyhow::{Result, anyhow};
use thegn_core::vercel::api::{LogLine, SandboxView, Scope};

use super::VercelToken;

fn not_built() -> anyhow::Error {
    anyhow!("vercel: not implemented in this lane")
}

/// The outcome of a session-timeout extension.
#[allow(dead_code)] // spine stub, filled by lane 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtendOutcome {
    Extended,
    /// A 4xx on extend: the plan ceiling; stop extending and warn once.
    AtCeiling,
}

/// The `/interactive` WebSocket ticket. `token` is a secret.
#[allow(dead_code)] // spine stub, filled by lane 3
pub(crate) struct InteractiveTicket {
    pub(crate) url: url::Url,
    pub(crate) token: zeroize::Zeroizing<String>,
}

#[allow(dead_code)] // spine stub, filled by lane 3
pub(crate) struct VercelClient {
    base: String,
    token: VercelToken,
    scope: Scope,
}

#[allow(dead_code)] // spine stub, filled by lane 3
impl VercelClient {
    pub(crate) fn new(base: &str, token: VercelToken, scope: Scope) -> Result<Self> {
        Ok(VercelClient {
            base: base.to_string(),
            token,
            scope,
        })
    }

    pub(crate) async fn create(&self, _body: &serde_json::Value) -> Result<SandboxView> {
        Err(not_built())
    }

    pub(crate) async fn get(&self, _name: &str, _resume: bool) -> Result<Option<SandboxView>> {
        Err(not_built())
    }

    pub(crate) async fn list_page(
        &self,
        _cursor: Option<&str>,
        _name_prefix: &str,
    ) -> Result<(Vec<SandboxView>, Option<String>)> {
        Err(not_built())
    }

    /// 404 = Ok.
    pub(crate) async fn delete(&self, _name: &str) -> Result<()> {
        Err(not_built())
    }

    pub(crate) async fn stop(&self, _session: &str) -> Result<()> {
        Err(not_built())
    }

    pub(crate) async fn extend(&self, _session: &str, _ms: u64) -> Result<ExtendOutcome> {
        Err(not_built())
    }

    pub(crate) async fn set_network_policy(
        &self,
        _name: &str,
        _body: &serde_json::Value,
    ) -> Result<()> {
        Err(not_built())
    }

    pub(crate) async fn run_wait(
        &self,
        _session: &str,
        _body: &serde_json::Value,
        _out_cap: usize,
    ) -> Result<(i32, Vec<u8>)> {
        Err(not_built())
    }

    /// Returns the command id.
    pub(crate) async fn run_detached(
        &self,
        _session: &str,
        _body: &serde_json::Value,
    ) -> Result<String> {
        Err(not_built())
    }

    pub(crate) async fn logs(
        &self,
        _session: &str,
        _cmd: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<LogLine>> {
        Err(not_built())
    }

    pub(crate) async fn stdin(
        &self,
        _session: &str,
        _cmd: &str,
        _data: &[u8],
        _close: bool,
    ) -> Result<()> {
        Err(not_built())
    }

    pub(crate) async fn kill(&self, _session: &str, _cmd: &str, _signal: i32) -> Result<()> {
        Err(not_built())
    }

    pub(crate) async fn fs_write_targz(
        &self,
        _session: &str,
        _cwd: &str,
        _targz: Vec<u8>,
    ) -> Result<()> {
        Err(not_built())
    }

    pub(crate) async fn fs_read(
        &self,
        _session: &str,
        _path: &str,
        _cap: usize,
    ) -> Result<Option<Vec<u8>>> {
        Err(not_built())
    }

    pub(crate) async fn fs_mkdir(&self, _session: &str, _path: &str) -> Result<()> {
        Err(not_built())
    }

    pub(crate) async fn interactive(&self, _session: &str) -> Result<InteractiveTicket> {
        Err(not_built())
    }

    pub(crate) async fn snapshots_for(&self, _name: &str) -> Result<Vec<String>> {
        Err(not_built())
    }

    pub(crate) async fn delete_snapshot(&self, _id: &str) -> Result<()> {
        Err(not_built())
    }
}
