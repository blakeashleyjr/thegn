//! Stream mode for a Vercel sandbox: a detached command, the NDJSON log
//! follow, and stdin over `POST …/stdin` (no resize, no raw TUI). The fallback
//! when `/interactive` is refused.

use anyhow::{Result, anyhow};

use super::VercelProvider;
use crate::provider::{ExecSession, ExecSpec};

impl VercelProvider {
    /// Open a stream-mode session. Spine stub, filled by lane 6.
    pub async fn open_stream(&self, _name: &str, _spec: &ExecSpec) -> Result<ExecSession> {
        Err(anyhow!("vercel: not implemented in this lane"))
    }
}
