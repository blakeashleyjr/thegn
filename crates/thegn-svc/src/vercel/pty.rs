//! The `/interactive` WebSocket PTY for a Vercel sandbox (in-process callers:
//! the `vercel-exec` bridge). Native panes stay off: `exec_api` is false.

use anyhow::{Result, anyhow};

use super::VercelProvider;
use crate::provider::{ExecSession, ExecSpec};

impl VercelProvider {
    /// Open a PTY session over `/interactive`. Spine stub, filled by lane 6.
    pub async fn open_exec(&self, _name: &str, _spec: &ExecSpec) -> Result<ExecSession> {
        Err(anyhow!("vercel: not implemented in this lane"))
    }
}
