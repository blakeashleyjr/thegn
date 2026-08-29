//! Pure classification of harness-level errors in live agent output.
//!
//! Tool calls can fail transiently without meaning that the agent itself has
//! failed. This module only recognizes configured harness banners (limits,
//! connection failures, and authentication failures), leaving generic tool
//! noise to the agent's normal activity state.

/// A matched agent-level error banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentErrorKind {
    /// Matched a harness failure banner (usage limit, connection error, auth).
    HarnessBanner,
}

/// Config-listed substrings that classify as agent-level errors.
/// Case-insensitive matching.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentErrorSignatures {
    pub signatures: Vec<String>,
}

/// The shipped harness failure banners. Generic tool-call errors are
/// deliberately absent: they are transient noise unless the harness reports
/// a condition that can stop the agent from continuing.
pub const DEFAULT_AGENT_ERROR_SIGNATURES: &[&str] = &[
    "weekly limit",
    "rate limit",
    "usage limit",
    "limit reached",
    "quota exceeded",
    "out of credits",
    "insufficient credits",
    "credit balance",
    "billing",
    "payment required",
    "connection error.",
    "connection refused",
    "connection timed out",
    "network error",
    "network request failed",
];

impl AgentErrorSignatures {
    /// The shipped defaults — harness failure banners relevant to live agent
    /// output. Generic authentication/permission text is intentionally left
    /// configurable: tool-call results commonly contain those words without
    /// meaning that the agent harness itself has failed.
    pub fn defaults() -> Self {
        Self {
            signatures: DEFAULT_AGENT_ERROR_SIGNATURES
                .iter()
                .map(|signature| (*signature).to_string())
                .collect(),
        }
    }

    /// True when no signatures are configured — matching is a no-op.
    pub fn is_empty(&self) -> bool {
        self.signatures.is_empty()
    }
}

/// Classify one output line. `None` means that the line is not an agent-level
/// error. Matching is case-insensitive substring matching.
pub fn classify_error_line(line: &str, sig: &AgentErrorSignatures) -> Option<AgentErrorKind> {
    if sig.is_empty() {
        return None;
    }

    let line = line.to_lowercase();
    sig.signatures
        .iter()
        // Invalid empty entries are rejected by strict config validation. Do
        // not let one nevertheless classify every line when callers construct
        // the pure value directly or use a leniently loaded config.
        .find(|signature| !signature.trim().is_empty() && line.contains(&signature.to_lowercase()))
        .map(|_| AgentErrorKind::HarnessBanner)
}

/// Per-session error state cleared on next normal output.
#[derive(Debug, Clone, Default)]
pub struct AgentErrorState {
    pub error_active: bool,
    pub last_signature: Option<String>,
}

impl AgentErrorState {
    /// Record a match: set `error_active` and note the matching text.
    pub fn note_error(&mut self, sig: &str) {
        self.error_active = true;
        self.last_signature = Some(sig.to_string());
    }

    /// Clear state because the agent resumed normal output.
    pub fn clear_on_resume(&mut self) {
        self.error_active = false;
        self.last_signature = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_shipped_harness_signatures() {
        let signatures = AgentErrorSignatures::defaults();
        assert_eq!(
            signatures.signatures,
            DEFAULT_AGENT_ERROR_SIGNATURES
                .iter()
                .map(|signature| (*signature).to_string())
                .collect::<Vec<_>>()
        );
        assert!(!signatures.is_empty());
    }

    #[test]
    fn classify_known_banner_case_insensitively() {
        let signatures = AgentErrorSignatures::defaults();
        assert_eq!(
            classify_error_line("Weekly limit reached (~100% of your plan)", &signatures),
            Some(AgentErrorKind::HarnessBanner)
        );
        assert_eq!(
            classify_error_line("CONNECTION ERROR.", &signatures),
            Some(AgentErrorKind::HarnessBanner)
        );
    }

    #[test]
    fn empty_signatures_never_classify() {
        let signatures = AgentErrorSignatures::default();
        assert!(signatures.is_empty());
        assert_eq!(
            classify_error_line("Weekly limit reached", &signatures),
            None
        );
    }

    #[test]
    fn tool_call_noise_and_stack_traces_do_not_classify() {
        let signatures = AgentErrorSignatures::defaults();
        for line in [
            "Error: Command failed with no output",
            "● Fetch(https://example.test/api)",
            "Error: permission denied",
            "Error: authentication failed",
            "thread 'main' panicked at src/main.rs:12:4",
            "    at stack_frame (worker.rs:42:9)",
        ] {
            assert_eq!(classify_error_line(line, &signatures), None, "{line}");
        }

        // An invalid empty entry is harmless even when callers construct the
        // signatures directly instead of going through strict config validation.
        let empty_entry = AgentErrorSignatures {
            signatures: vec![String::new()],
        };
        assert_eq!(classify_error_line("ordinary output", &empty_entry), None);

        // Operators can still opt into an exact harness-specific phrase
        // without reintroducing the broad shipped defaults.
        let configured = AgentErrorSignatures {
            signatures: vec!["agent authentication failed".into()],
        };
        assert_eq!(
            classify_error_line("agent authentication failed", &configured),
            Some(AgentErrorKind::HarnessBanner)
        );
    }

    #[test]
    fn state_records_and_clears_the_last_error() {
        let mut state = AgentErrorState::default();
        assert!(!state.error_active);
        assert_eq!(state.last_signature, None);

        state.note_error("weekly limit");
        assert!(state.error_active);
        assert_eq!(state.last_signature.as_deref(), Some("weekly limit"));

        state.note_error("connection refused");
        assert_eq!(state.last_signature.as_deref(), Some("connection refused"));

        state.clear_on_resume();
        assert!(!state.error_active);
        assert_eq!(state.last_signature, None);
    }
}
