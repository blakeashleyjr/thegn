//! Compiling a caller-supplied pattern for `wait --until match:<regex>`.
//!
//! The pattern arrives over the control API from whoever is supervising a
//! session, so it is untrusted input compiled inside the daemon. Two things make
//! that safe:
//!
//! * **`regex` does not backtrack.** Matching is linear in the input by
//!   construction, so the classic "evil regex" denial of service — the one that
//!   makes `(a+)+$` hang a PCRE engine for minutes — simply does not exist here.
//! * **Compilation is bounded.** What *is* reachable is memory: `a{1000}{1000}`
//!   asks for a program with a billion states. `size_limit` and
//!   `dfa_size_limit` cap the compiled program and the lazy-DFA cache, turning
//!   that into a prompt error instead of a daemon that balloons.
//!
//! A length cap on top keeps the error message about the pattern rather than
//! about the engine, and stops a multi-megabyte body being parsed at all.
//!
//! Compile in the *service*, before the pattern reaches a session actor, so a
//! bad pattern is a 4xx to the caller rather than an internal error raised from
//! inside a task nobody is awaiting.

/// Longest pattern accepted. Generous for "the agent printed X", small enough
/// that a pasted file is rejected as such.
pub const MAX_PATTERN_BYTES: usize = 512;

/// Compiled-program ceiling. One MiB is far past any human-written pattern.
const SIZE_LIMIT: usize = 1 << 20;
/// Lazy-DFA cache ceiling, applied per match rather than per compile.
const DFA_SIZE_LIMIT: usize = 1 << 20;

/// Why a wait pattern was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitRegexError {
    /// Longer than [`MAX_PATTERN_BYTES`].
    TooLong(usize),
    /// Empty — almost certainly a mistake, and it would match instantly.
    Empty,
    /// Rejected by the engine: a syntax error, or past the resource limits.
    Invalid(String),
}

impl std::fmt::Display for WaitRegexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong(n) => write!(
                f,
                "wait pattern is {n} bytes; the maximum is {MAX_PATTERN_BYTES}"
            ),
            Self::Empty => write!(f, "wait pattern is empty"),
            Self::Invalid(e) => write!(f, "invalid wait pattern: {e}"),
        }
    }
}

impl std::error::Error for WaitRegexError {}

/// Compile `pat` under the bounds documented above.
pub fn compile_wait_regex(pat: &str) -> Result<regex::Regex, WaitRegexError> {
    if pat.is_empty() {
        return Err(WaitRegexError::Empty);
    }
    if pat.len() > MAX_PATTERN_BYTES {
        return Err(WaitRegexError::TooLong(pat.len()));
    }
    regex::RegexBuilder::new(pat)
        .size_limit(SIZE_LIMIT)
        .dfa_size_limit(DFA_SIZE_LIMIT)
        .build()
        // The engine's own message names the offending construct; keep it.
        .map_err(|e| WaitRegexError::Invalid(e.to_string()))
}

/// The index of the first line matching `re`, if any.
///
/// Indices are into the iterator, so a caller scanning a scrollback tail gets a
/// position it can report ("matched 12 lines back") rather than a bare boolean.
pub fn first_match_line<'a>(
    re: &regex::Regex,
    lines: impl IntoIterator<Item = &'a str>,
) -> Option<usize> {
    lines.into_iter().position(|line| re.is_match(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_pattern_compiles_and_matches() {
        let re = compile_wait_regex("Do you want to proceed").expect("should compile");
        assert!(re.is_match("  Do you want to proceed? (y/n)"));
        assert!(!re.is_match("all done"));
    }

    #[test]
    fn an_empty_pattern_is_refused() {
        assert_eq!(
            compile_wait_regex("").expect_err("empty is refused"),
            WaitRegexError::Empty
        );
    }

    #[test]
    fn an_over_long_pattern_is_refused_by_length() {
        let pat = "a".repeat(MAX_PATTERN_BYTES + 1);
        assert_eq!(
            compile_wait_regex(&pat).expect_err("over-long is refused"),
            WaitRegexError::TooLong(MAX_PATTERN_BYTES + 1)
        );
    }

    #[test]
    fn a_pattern_at_the_limit_is_accepted() {
        let pat = "a".repeat(MAX_PATTERN_BYTES);
        assert!(compile_wait_regex(&pat).is_ok());
    }

    #[test]
    fn a_syntax_error_is_reported_not_panicked() {
        let err = compile_wait_regex("(unclosed").expect_err("should refuse");
        assert!(matches!(err, WaitRegexError::Invalid(_)));
        assert!(err.to_string().contains("invalid wait pattern"));
    }

    /// The resource-exhaustion case: this is refused by the size limit, and —
    /// the part that matters — it is refused *promptly*. A test that merely
    /// asserted `is_err()` would still pass if compilation took a minute.
    #[test]
    fn a_state_explosion_is_refused_in_bounded_time() {
        let start = std::time::Instant::now();
        let err = compile_wait_regex("a{1000}{1000}").expect_err("should refuse");
        assert!(matches!(err, WaitRegexError::Invalid(_)));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "compilation must fail fast, took {:?}",
            start.elapsed()
        );
    }

    /// The pattern that hangs a backtracking engine is linear here, so it is
    /// accepted and matched rather than refused.
    #[test]
    fn a_classic_evil_regex_is_harmless() {
        let re = compile_wait_regex("(a+)+$").expect("should compile");
        let start = std::time::Instant::now();
        assert!(!re.is_match(&format!("{}b", "a".repeat(64))));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn first_match_line_reports_the_position() {
        let re = compile_wait_regex("proceed").expect("should compile");
        let lines = ["building", "linking", "proceed? (y/n)", "proceed again"];
        assert_eq!(first_match_line(&re, lines), Some(2));
    }

    #[test]
    fn first_match_line_is_none_when_nothing_matches() {
        let re = compile_wait_regex("proceed").expect("should compile");
        assert_eq!(first_match_line(&re, ["a", "b"]), None);
        assert_eq!(first_match_line(&re, std::iter::empty()), None);
    }
}
