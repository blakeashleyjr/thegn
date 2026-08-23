//! thegn-svc — the native service layer.
//!
//! Each external service is a provider seam (`thegn_core::seam`): a trait with
//! a `Native` impl where one exists (gix for git reads, octocrab for GitHub)
//! and a `Cli` fallback that wraps thegn-core's already-tested subprocess code,
//! kept permanently so a native gap degrades to "slower but works," never
//! "broken." ssh has no native impl — the `ssh` CLI is the transport.

pub mod bridge;
pub mod calendar;
pub mod ci;
pub mod control;
pub mod fly;
pub mod forward;
pub mod gh;
pub mod git;
pub mod host;
pub mod ipc;
pub mod iroh_reach;
pub mod issue;
pub mod log;
pub mod lsp;
pub mod machine0;
#[cfg(test)]
mod platform_ratchet_tests;
pub mod plugin;
pub mod projection;
pub mod provider;
pub mod prq;
pub mod revtunnel;
pub mod seam;
pub mod share;
pub mod snapshot;
pub mod ssh;
pub mod usage;
pub mod vpn;
pub mod vps;
