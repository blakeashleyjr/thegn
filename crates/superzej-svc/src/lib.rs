//! superzej-svc — the native service layer.
//!
//! Each external service (git, GitHub, ssh) is a trait with two impls: a
//! `Native` impl (gix / octocrab / russh — landed in Phase 4/5) and a `Cli`
//! fallback that wraps superzej-core's already-tested subprocess code, kept
//! permanently so a native gap degrades to "slower but works," never "broken."
//!
//! Phase 0 establishes the seams; impls are filled in their respective phases.

pub mod gh;
pub mod git;
pub mod issue;
pub mod ssh;
