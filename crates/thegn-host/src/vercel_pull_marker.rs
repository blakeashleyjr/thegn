//! The "results not pulled" marker: one file per Vercel sandbox name under
//! `$XDG_STATE_HOME/thegn/vercel/unpulled/<name>`, written when a TTY session
//! ends and cleared by a successful `thegn env pull`.
//!
//! Spine stub (b156 lane 1): signatures are fixed; lane 6 fills the bodies.

/// Record that `sandbox` may hold commits not yet pulled back.
#[allow(dead_code)] // spine stub, filled by lane 6
pub(crate) fn mark(_sandbox: &str) {}

/// Drop `sandbox`'s marker.
#[allow(dead_code)] // spine stub, filled by lane 6
pub(crate) fn clear(_sandbox: &str) {}

/// Whether `sandbox` carries the marker.
#[allow(dead_code)] // spine stub, filled by lane 6
pub(crate) fn is_marked(_sandbox: &str) -> bool {
    false
}

/// Clear the marker of the sandbox bound to `worktree` (after a pull).
pub(crate) fn clear_for_worktree(_cfg: &thegn_core::config::Config, _worktree: &str) {}
