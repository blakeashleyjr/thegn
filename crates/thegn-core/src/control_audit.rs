//! The control-plane audit record — a pure, tested description of one mutating
//! control call (or auth rejection) for the `thegn::control::audit` tracing
//! target.
//!
//! The transport adapters (HTTP/gRPC) build a record from the caller's
//! [`AuthCtx`](../../thegn_svc/control/auth) + the verb and emit it via tracing,
//! which is free when no subscriber is installed and file-persisted when
//! `THEGN_LOG` is set. The record NEVER carries a token secret — only the public
//! pairing-id half is ever in scope at handler level. Keeping the field set here
//! (pure `thegn-core`) puts it under the coverage gate.

use crate::control::{Scope, Verb, required_scope};

/// The outcome of an audited control call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    /// The call was authorized and dispatched.
    Ok,
    /// The caller's token lacked the capability's required scope.
    NoScope,
    /// No credential, or an invalid/revoked/expired one.
    Unauthorized,
    /// The action itself failed after authorization.
    Error,
}

impl AuditOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditOutcome::Ok => "ok",
            AuditOutcome::NoScope => "no_scope",
            AuditOutcome::Unauthorized => "unauthorized",
            AuditOutcome::Error => "error",
        }
    }
}

/// Whether a verb's invocations are audited: every call whose required scope is
/// `write`, `git` or `admin` (read verbs are not audited — auth rejections are,
/// separately, at the adapter).
pub fn is_audited(verb: Verb) -> bool {
    required_scope(verb) != Scope::Read
}

/// One audit record. Borrows its strings — built at the handler and emitted
/// immediately, never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord<'a> {
    /// The caller's PUBLIC pairing id — safe to log by design; never a secret.
    pub pairing_id: &'a str,
    /// The caller's human label.
    pub label: &'a str,
    /// The capability id being invoked (`<domain>.<action>`).
    pub capability: &'static str,
    /// The target resource (session id / worktree path / pairing id); may be "".
    pub target: &'a str,
    /// The scope the capability requires — context for the outcome.
    pub scope: Scope,
    pub outcome: AuditOutcome,
}

impl<'a> AuditRecord<'a> {
    /// Build a record for `verb`, resolving the capability id + required scope
    /// from the catalog. `target` is the acted-on resource (or `""`).
    pub fn for_verb(
        pairing_id: &'a str,
        label: &'a str,
        verb: Verb,
        target: &'a str,
        outcome: AuditOutcome,
    ) -> Self {
        let capability = crate::capability::for_verb(verb)
            .map(|c| c.id.as_str())
            .unwrap_or("");
        Self {
            pairing_id,
            label,
            capability,
            target,
            scope: required_scope(verb),
            outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mutating_verbs_are_audited() {
        assert!(is_audited(Verb::GitCommit));
        assert!(is_audited(Verb::SendInput));
        assert!(is_audited(Verb::Shutdown));
        assert!(!is_audited(Verb::ListSessions));
        assert!(!is_audited(Verb::Me));
    }

    #[test]
    fn record_resolves_capability_and_scope_and_carries_no_secret() {
        let r = AuditRecord::for_verb(
            "pair_abc",
            "phone",
            Verb::GitCommit,
            "/w/feature",
            AuditOutcome::Ok,
        );
        assert_eq!(r.capability, "git.commit");
        assert_eq!(r.scope, Scope::Git);
        assert_eq!(r.pairing_id, "pair_abc");
        assert_eq!(r.target, "/w/feature");
        assert_eq!(r.outcome.as_str(), "ok");
        // The record only ever holds the public id half — there is no field a
        // token secret could be placed in.
    }

    #[test]
    fn outcome_strings_are_stable() {
        assert_eq!(AuditOutcome::Ok.as_str(), "ok");
        assert_eq!(AuditOutcome::NoScope.as_str(), "no_scope");
        assert_eq!(AuditOutcome::Unauthorized.as_str(), "unauthorized");
        assert_eq!(AuditOutcome::Error.as_str(), "error");
    }
}
