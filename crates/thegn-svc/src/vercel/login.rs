//! `model_credentials = "upload"`: copy the host's claude/codex login files
//! into the sandbox's tmpfs (after narrowing egress), and remove them again.
//!
//! Spine stub (b156 lane 1): signatures are fixed; lane 4 fills the bodies.
//! Until then `ensure_login` refuses, so nothing is uploaded.

use std::fmt;

use anyhow::{Result, bail};

use super::VercelProvider;

/// One agent's login files, held in zeroizing buffers. Redacted `Debug`.
pub struct LoginPayload {
    pub(crate) agent: String,
    pub(crate) files: Vec<(String, zeroize::Zeroizing<Vec<u8>>)>,
}

impl LoginPayload {
    /// Wrap `files` (`(vm_name, bytes)`) for `agent`.
    pub fn new(agent: &str, files: Vec<(String, Vec<u8>)>) -> Result<Self> {
        Ok(LoginPayload {
            agent: agent.to_string(),
            files: files
                .into_iter()
                .map(|(name, bytes)| (name, zeroize::Zeroizing::new(bytes)))
                .collect(),
        })
    }
}

impl fmt::Debug for LoginPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginPayload")
            .field("agent", &self.agent)
            .field("files", &self.files.len())
            .finish_non_exhaustive()
    }
}

/// Where an upload landed and the env pairs (only `LOGIN_ENV_KEYS`) that
/// point the agents at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginHandle {
    pub dir: String,
    pub env: Vec<(String, String)>,
}

impl VercelProvider {
    /// Narrow egress, then upload `payloads` into a fresh tmpfs login dir.
    pub async fn ensure_login(
        &self,
        _name: &str,
        _payloads: &[LoginPayload],
        _egress_user_domains: &[String],
        _install_agents: bool,
    ) -> Result<LoginHandle> {
        bail!("vercel: model_credentials = upload is not available in this build")
    }

    /// Remove a login dir this process uploaded (best-effort by the caller).
    pub async fn remove_login(&self, _name: &str, _handle: &LoginHandle) -> Result<()> {
        Ok(())
    }
}
