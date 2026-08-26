//! Host-side diagnostics wiring: the installation identity (version / channel /
//! build) the crash report and `doctor` record. The always-on ring, the panic
//! hook, and the crash-report writer all live in `thegn_core`; this module only
//! feeds them the host-only facts (the embedded git sha, the resolved channel).

use thegn_core::diagnostics::{self, Identity};

/// thegn's version, channel, and build metadata.
pub fn identity(channel: &str) -> Identity {
    Identity {
        version: env!("CARGO_PKG_VERSION").to_string(),
        channel: channel.to_string(),
        build: build_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

/// Register the identity once (idempotent — a `OnceLock` under the hood).
pub fn register_identity(channel: &str) {
    diagnostics::set_identity(identity(channel));
}

/// The build string: short git sha and/or build time, embedded by `build.rs`.
/// `None` when neither is available (a source-tarball build).
pub fn build_string() -> Option<String> {
    let sha = option_env!("THEGN_GIT_SHA").unwrap_or("");
    let bt = option_env!("THEGN_BUILD_TIME").unwrap_or("");
    match (sha.is_empty(), bt.is_empty()) {
        (true, true) => None,
        (false, true) => Some(sha.to_string()),
        (true, false) => Some(format!("build {bt}")),
        (false, false) => Some(format!("{sha} (build {bt})")),
    }
}
