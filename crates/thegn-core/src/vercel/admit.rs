//! Config admission for `provider = "vercel"` envs. Errors carry a field label
//! and a fixed reason only, never a configured value.

use std::fmt;

/// A refused `[env.<name>.provider]` field: label + fixed reason, no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitError {
    pub field: &'static str,
    pub reason: &'static str,
}

impl fmt::Display for AdmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for AdmitError {}

/// Validate a vercel env's provider table (team/project ids, size, region,
/// unsupported keys). Spine stub, filled by lane 2.
pub fn validate_env(_pc: &crate::config::EnvProviderConfig) -> Result<(), AdmitError> {
    Ok(())
}

/// `^[a-z0-9][a-z0-9-]{0,62}$` (conservative; Vercel's charset is UNVERIFIED).
/// Spine stub, filled by lane 2: refuses everything until then.
pub fn valid_sandbox_name(_name: &str) -> bool {
    false
}

/// `team_[A-Za-z0-9]+`. Spine stub, filled by lane 2.
pub fn valid_team_id(_s: &str) -> bool {
    false
}

/// `prj_[A-Za-z0-9]+`. Spine stub, filled by lane 2.
pub fn valid_project_id(_s: &str) -> bool {
    false
}
