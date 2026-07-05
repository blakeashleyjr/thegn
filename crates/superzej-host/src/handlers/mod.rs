//! Event-loop drain handlers extracted from `run.rs` (which is pinned by the
//! file-size ratchet): each submodule owns the loop-side handling of one
//! off-thread producer's channel, taking the loop locals it mutates as a
//! context struct. The loop calls one `drain_*` per wake; everything here runs
//! ON the loop and must stay I/O-free.

pub(crate) mod host;
pub(crate) mod overlay;
pub(crate) mod panel_changes;
pub(crate) mod provision;
pub(crate) mod repo_trust;
pub(crate) mod startup;
pub(crate) mod terminal;
pub(crate) mod wizard;
