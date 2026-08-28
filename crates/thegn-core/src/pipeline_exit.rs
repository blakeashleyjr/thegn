//! Why a headless pipeline worker died, and what to do about it — the **pure**
//! half of the transport-error retry (THE-86): classify a dead session's final
//! screen, then decide whether the daemon may relaunch it.
//!
//! # No-I/O doctrine
//!
//! Everything here is a pure function over plain data (`&str`, integers) — no
//! clock, no filesystem, no subprocess, no tokio. The daemon observer
//! (`thegn-host/src/daemon/pipeline_retry.rs`) owns every impurity: it reads
//! the tombstone, resolves the roster row, sleeps the backoff, and performs the
//! relaunch. Keeping the decision here keeps it table-testable and inside the
//! 95%-coverage gate, exactly like `pipeline_run` / `pipeline_resume`.
//!
//! # Structure, not judgment
//!
//! The daemon stamper is the one deliberate exception to "nothing advances a
//! stage" — but only downward: every outcome below parks the row as
//! `waiting_human` (the daemon can never write `done` or `failed`, so it can
//! park a row but never finish one). A relaunch re-stamps the SAME roster row
//! (`stamp_dispatch_run`) rather than chaining rows — the retry is one row
//! cycling through attempts, not a fan-out; the human-driven resume mechanism
//! (`session open --resume-work`) is what chains rows.

/// What the final screen says about why the worker died.
///
/// Transport failures are the daemon's to retry (a network hiccup is nobody's
/// judgment call); usage-limit stops are the operator's (spending money is a
/// human decision — the row parks for a person, never relaunches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitClass {
    /// A connection/network/provider-outage failure — retryable.
    Transport { signature: String },
    /// A rate/usage/credit limit — park for a human, never auto-retry.
    Limit { signature: String },
}

/// The substring lists a screen is classified against. Case-insensitive;
/// transport is tested before limit; the first match wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitSignatures {
    /// Retryable transport-failure markers (a connection error, an HTTP 5xx).
    pub transport: Vec<String>,
    /// Usage/limit markers (a weekly cap, a billing wall).
    pub limit: Vec<String>,
}

impl ExitSignatures {
    /// The shipped defaults — [`DEFAULT_TRANSPORT_SIGNATURES`] /
    /// [`DEFAULT_LIMIT_SIGNATURES`]. What a `[pipeline.transport_retry]`
    /// section with no lists configured uses.
    pub fn defaults() -> Self {
        Self {
            transport: DEFAULT_TRANSPORT_SIGNATURES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            limit: DEFAULT_LIMIT_SIGNATURES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl From<&crate::config_pipeline::TransportRetry> for ExitSignatures {
    fn from(cfg: &crate::config_pipeline::TransportRetry) -> Self {
        Self {
            transport: cfg.transport_signatures.clone(),
            limit: cfg.limit_signatures.clone(),
        }
    }
}

/// Why a nonzero exit of a headless worker counts as a *transport* failure.
/// Deliberately conservative substring phrases: a false "limit" parks work a
/// human has to re-drive, but a false "transport" relaunches work that will
/// fail the same way — so phrases name the connection/provider, never a bare
/// number (a screen full of token counts must not read as an outage). An
/// operator overrides the whole list per key; config replaces, never extends.
pub const DEFAULT_TRANSPORT_SIGNATURES: &[&str] = &[
    "connection error.",
    "connection reset",
    "connection refused",
    "connection timed out",
    "connection closed before",
    "network error",
    "network request failed",
    "socket hang up",
    "econnreset",
    "econnrefused",
    "etimedout",
    "timed out",
    "timeout",
    "overloaded_error",
    "overloaded",
    "bad gateway",
    "service unavailable",
    "internal server error",
    "http 429",
    "http 500",
    "http 502",
    "http 503",
    "http 529",
];

/// Why a nonzero exit counts as a *usage limit* — the park-for-a-human class.
pub const DEFAULT_LIMIT_SIGNATURES: &[&str] = &[
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
];

/// Backoff ceiling for [`decide`]: `backoff_ms * 2^(n-1)` never waits longer
/// than this, however many attempts a row has burned.
pub const MAX_BACKOFF_MS: u64 = 60_000;

/// The nudge a relaunched worker is seeded with when the harness can resume
/// its own session (`CONTINUE` cap): the roster row keeps its artifact and
/// worktree; the agent picks up where it left off.
pub const RETRY_NUDGE: &str =
    "You were interrupted by a transport error; continue where you left off.";

/// Classify a dead worker's final screen.
///
/// `failed` is the exit gate: `false` (exit 0) classifies as `None` whatever
/// the screen says — the artifact gate owns exit-0 verdicts, and a success
/// must never be re-read as a failure by substring matching. `screen` is the
/// flattened final screen (ANSI stripped); matching is substring,
/// case-insensitive, transport before limit, first match wins.
pub fn classify(failed: bool, screen: &str, sig: &ExitSignatures) -> Option<ExitClass> {
    if !failed {
        return None;
    }
    let hay = screen.to_lowercase();
    for s in &sig.transport {
        if hay.contains(&s.to_lowercase()) {
            return Some(ExitClass::Transport {
                signature: s.clone(),
            });
        }
    }
    for s in &sig.limit {
        if hay.contains(&s.to_lowercase()) {
            return Some(ExitClass::Limit {
                signature: s.clone(),
            });
        }
    }
    None
}

/// What the daemon does with a classified exit. Every outcome parks the row
/// (`waiting_human` + a `note`) — `Retry` just additionally relaunches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDecision {
    /// Relaunch after `delay_ms`; `attempt` is the attempt number that just
    /// failed (1-based). The row's note is [`retry_note`].
    Retry { attempt: u32, delay_ms: u64 },
    /// Park for a human — a usage limit never auto-retries.
    Park { note: String },
    /// Transport retries burned; park with the verdict.
    Exhausted { note: String },
}

/// The durable note a *retrying* row carries, written before the relaunch so a
/// daemon crash mid-backoff still leaves an honest ledger. Pinned by test so
/// `dispatch list` output is stable.
pub fn retry_note(signature: &str, attempt: u32, max_attempts: u32) -> String {
    format!("transport: {signature} (attempt {attempt}/{max_attempts})")
}

/// The relaunch decision table (design §2.3).
///
/// `attempt` is the 1-based count of transport failures observed for the row.
/// Transport under the cap retries with exponential backoff
/// (`base_backoff_ms * 2^(attempt-1)`, capped at [`MAX_BACKOFF_MS`]); over the
/// cap it is `Exhausted`. A limit parks immediately, at any attempt — spending
/// money is never the daemon's call.
pub fn decide(
    class: &ExitClass,
    attempt: u32,
    max_attempts: u32,
    base_backoff_ms: u64,
) -> RetryDecision {
    match class {
        ExitClass::Limit { signature } => RetryDecision::Park {
            note: format!("limit: {signature}"),
        },
        ExitClass::Transport { signature } if attempt <= max_attempts => {
            // `2^(attempt-1)` with the shift bounded so a corrupt attempt
            // counter saturates instead of panicking; the cap does the rest.
            let shift = attempt.saturating_sub(1).min(63);
            let delay = base_backoff_ms
                .saturating_mul(2u64.saturating_pow(shift))
                .min(MAX_BACKOFF_MS);
            RetryDecision::Retry {
                attempt,
                delay_ms: delay,
            }
        }
        ExitClass::Transport { signature } => RetryDecision::Exhausted {
            note: format!("transport retry exhausted after {attempt} attempts: {signature}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig() -> ExitSignatures {
        ExitSignatures {
            transport: vec!["connection error.".into(), "Overloaded_Error".into()],
            limit: vec!["weekly limit".into()],
        }
    }

    // --- classification table -----------------------------------------------

    #[test]
    fn transport_matches_case_insensitively_and_first_wins() {
        let s = sig();
        // Case-insensitive, substring.
        assert_eq!(
            classify(true, "… Connection Error. retrying aborted", &s),
            Some(ExitClass::Transport {
                signature: "connection error.".into()
            })
        );
        // The configured signature is echoed verbatim (the note is stable).
        assert_eq!(
            classify(true, "OVERLOADED_ERROR while streaming", &s),
            Some(ExitClass::Transport {
                signature: "Overloaded_Error".into()
            })
        );
    }

    #[test]
    fn transport_is_tested_before_limit() {
        // A screen mentioning both is a transport failure — the retryable
        // class wins, so a flaky connection carrying a limit warning in its
        // scrollback still retries.
        let s = ExitSignatures {
            transport: vec!["connection reset".into()],
            limit: vec!["usage limit".into()],
        };
        assert!(matches!(
            classify(true, "usage limit hit after connection reset", &s),
            Some(ExitClass::Transport { .. })
        ));
    }

    #[test]
    fn limit_matches_and_parks() {
        let s = sig();
        assert_eq!(
            classify(true, "You have hit your Weekly Limit for Pro usage", &s),
            Some(ExitClass::Limit {
                signature: "weekly limit".into()
            })
        );
    }

    #[test]
    fn a_clean_exit_and_an_unmatched_screen_classify_as_none() {
        let s = sig();
        // The exit-0 gate: a success is never re-read as a failure.
        assert_eq!(classify(false, "connection error.", &s), None);
        assert_eq!(classify(true, "compile error: mismatched types", &s), None);
        assert_eq!(classify(true, "", &s), None);
    }

    #[test]
    fn the_default_lists_classify_real_world_screens() {
        let d = ExitSignatures::defaults();
        let transport = [
            "Connection error. — SDK retry budget exhausted",
            "stream error: OVERLOADED_ERROR (provider is at capacity)",
            "502 Bad Gateway from api.internal",
            "request timed out after 30s (ETIMEDOUT)",
            "Service Unavailable — please retry",
        ];
        for screen in transport {
            assert!(
                matches!(
                    classify(true, screen, &d),
                    Some(ExitClass::Transport { .. })
                ),
                "{screen} must classify as transport"
            );
        }
        let limit = [
            "Weekly limit reached (~100% of your plan)",
            "You've hit your usage limit",
            "credit balance too low — add a payment method",
        ];
        for screen in limit {
            assert!(
                matches!(classify(true, screen, &d), Some(ExitClass::Limit { .. })),
                "{screen} must classify as limit"
            );
        }
    }

    // --- decision table ------------------------------------------------------

    #[test]
    fn backoff_doubles_and_is_capped() {
        let t = ExitClass::Transport {
            signature: "connection error.".into(),
        };
        let d = |attempt| decide(&t, attempt, 3, 2_000);
        assert_eq!(
            d(1),
            RetryDecision::Retry {
                attempt: 1,
                delay_ms: 2_000
            }
        );
        assert_eq!(
            d(2),
            RetryDecision::Retry {
                attempt: 2,
                delay_ms: 4_000
            }
        );
        assert_eq!(
            d(3),
            RetryDecision::Retry {
                attempt: 3,
                delay_ms: 8_000
            }
        );
        // Even attempt 20 never waits past the cap (and never overflows).
        assert_eq!(
            decide(&t, 20, 30, 2_000),
            RetryDecision::Retry {
                attempt: 20,
                delay_ms: MAX_BACKOFF_MS
            }
        );
    }

    #[test]
    fn transport_over_max_is_exhausted_with_the_pinned_note() {
        let t = ExitClass::Transport {
            signature: "connection error.".into(),
        };
        assert_eq!(
            decide(&t, 4, 3, 2_000),
            RetryDecision::Exhausted {
                note: "transport retry exhausted after 4 attempts: connection error.".into()
            }
        );
    }

    #[test]
    fn a_limit_parks_at_any_attempt_and_never_retries() {
        let l = ExitClass::Limit {
            signature: "weekly limit".into(),
        };
        assert_eq!(
            decide(&l, 1, 3, 2_000),
            RetryDecision::Park {
                note: "limit: weekly limit".into()
            }
        );
        assert_eq!(
            decide(&l, 9, 3, 2_000),
            RetryDecision::Park {
                note: "limit: weekly limit".into()
            }
        );
    }

    #[test]
    fn the_retry_note_format_is_pinned() {
        assert_eq!(
            retry_note("connection error.", 2, 3),
            "transport: connection error. (attempt 2/3)"
        );
    }
}
