//! superzej-svc — the native service layer.
//!
//! Each external service (git, GitHub, ssh) is a trait with two impls: a
//! `Native` impl (gix / octocrab / russh — landed in Phase 4/5) and a `Cli`
//! fallback that wraps superzej-core's already-tested subprocess code, kept
//! permanently so a native gap degrades to "slower but works," never "broken."
//!
//! Phase 0 establishes the seams; impls are filled in their respective phases.

pub mod acp;
pub mod bridge;
pub mod ci;
pub mod control;
pub mod fly;
pub mod forward;
pub mod gh;
pub mod git;
pub mod host;
pub mod iroh_reach;
pub mod issue;
pub mod log;
pub mod lsp;
pub mod mcp_git;
pub mod projection;
pub mod provider;
pub mod revtunnel;
pub mod share;
pub mod snapshot;
pub mod ssh;
pub mod vpn;
pub mod vps;
