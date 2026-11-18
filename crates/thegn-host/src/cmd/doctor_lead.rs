//! `thegn doctor`: Lead readiness (THE-2213). Reads config and the DB
//! locally; never talks to the daemon.
//!
//! Stub: filled by b161 lane 4. Until then it reports no checks.
#![allow(dead_code, reason = "filled by b161 lane 4")]

use thegn_core::config::Config;

/// Every Lead readiness check.
pub(crate) struct LeadReadiness {
    pub checks: Vec<Check>,
}

/// One readiness check.
pub(crate) struct Check {
    pub id: &'static str,
    pub level: Level,
    pub text: String,
    pub fix: Option<String>,
}

/// A check's severity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Ok,
    Warn,
    Fail,
    Info,
}

/// Inspect the Lead's readiness. Stub: no checks.
pub(crate) fn inspect(_cfg: &Config) -> LeadReadiness {
    LeadReadiness { checks: Vec::new() }
}

/// Print the text report. Stub: prints nothing.
pub(crate) fn print(_r: &LeadReadiness) {}

/// The JSON report.
pub(crate) fn json(_r: &LeadReadiness) -> serde_json::Value {
    serde_json::json!({"checks": []})
}
