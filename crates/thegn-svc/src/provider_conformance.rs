//! Provider conformance suite (first slice of WS08-P1-08): one `suite` entry
//! point run over today's `RemoteProvider` and its optional traits, against the
//! sprites mock and the Vercel fake. WS08 extends this file rather than
//! replacing it.
//!
//! Spine stub (b160 lane 1): the signatures are final; lane 8 fills the cases.

/// What one provider run of the suite works against.
pub struct Fixture {
    /// A sandbox the fixture created (and the provider owns).
    pub created: String,
    /// A name no sandbox has.
    pub missing: String,
    /// A sandbox the provider can see but did not create, when the fixture has one.
    pub foreign: Option<String>,
    /// Whether the provider can run a command in a sandbox.
    pub supports_run: bool,
}

/// The outcome of one suite run, case by case.
pub struct Report {
    pub passed: Vec<&'static str>,
    pub failed: Vec<(&'static str, String)>,
}

/// Run every conformance case against `p`.
///
/// Spine stub: runs no cases.
pub async fn suite(_p: &crate::provider::Provider, fx: &Fixture) -> Report {
    tracing::debug!(
        target: "thegn::conformance",
        created = fx.created,
        "provider conformance: no cases yet"
    );
    Report {
        passed: Vec::new(),
        failed: Vec::new(),
    }
}
