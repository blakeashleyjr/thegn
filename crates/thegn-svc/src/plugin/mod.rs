//! Running an external program as a **data source**.
//!
//! Deliberately not calendar-specific. thegn already launches arbitrary
//! commands in a dozen places, but none of them can read *structured* data back
//! — `agent_run` caps and discards stdout, and its exit code is advisory. This
//! is the missing primitive, so the next data-source tile (mail, RSS, tasks…)
//! gets it for free.
//!
//! The wire format is newline-delimited JSON framed by
//! [`thegn_core::plugin_api::RpcMessage`], chosen over "print one big JSON
//! array" because the same reader then serves both a one-shot poll and a
//! long-lived watcher, and over LSP-style `Content-Length` framing because that
//! makes shell-script plugins impossible to write.

pub mod loader;
pub mod proc;
pub mod session;

pub use loader::{
    LoadedPlugin, SpecProblem, check_spec, check_specs, discover, host_contract, negotiate,
};
pub use proc::{PluginError, PluginRun, spawn_ndjson};
pub use session::{ResidentSession, SessionEvent, SessionWriter};
