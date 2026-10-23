//! Bounded, redacted CI-log data shared by every read path.
//!
//! Provider output is untrusted text.  This module is deliberately pure so a
//! provider, cache, prompt, terminal, CLI, or control surface can all apply
//! the same bounds and redaction rules before exposing it.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The marker used for every value removed from a CI log.
pub const REDACTION_MARKER: &str = "***redacted***";
/// An absolute storage ceiling. Configured limits may be lower, never higher.
pub const HARD_MAX_LOG_BYTES: usize = 4 * 1024 * 1024;
/// An absolute line ceiling used by the cache as a last-resort guard.
pub const HARD_MAX_LOG_LINES: usize = 100_000;
/// Version of the redaction rules applied to an entry's text. 0 = never
/// redacted (or unknown), 1 = every cached row written before versioning (the
/// integer `redacted` column held 1), 2 = the current `redact` rules. Bump it
/// whenever `redact`/`redact_stream_line` learn a new secret shape, so cached
/// rows are re-redacted on their next read.
pub const REDACTION_POLICY_VERSION: u32 = 2;

/// A redacted, bounded log entry suitable for persistence or transport.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiLogEntry {
    pub worktree: String,
    pub run_id: String,
    pub job_id: String,
    pub job_name: String,
    pub text: String,
    #[serde(default)]
    pub truncated: bool,
    /// True when a redaction pass was applied; this does not prove that every
    /// possible secret shape was recognized.
    #[serde(default)]
    pub redacted: bool,
    /// Redaction policy version applied to this entry; 0 for older producers.
    #[serde(default)]
    pub policy_version: u32,
    pub fetched_at: i64,
    #[serde(default)]
    pub head_sha: String,
}

/// The identity used to deduplicate an autofix handoff.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CiLogCandidate {
    pub worktree: String,
    pub run_id: String,
    pub job_id: String,
    pub head_sha: String,
}

impl CiLogEntry {
    /// Construct an entry while enforcing the public log contract.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        worktree: impl Into<String>,
        run_id: impl Into<String>,
        job_id: impl Into<String>,
        job_name: impl Into<String>,
        text: &str,
        max_lines: usize,
        max_bytes: usize,
        fetched_at: i64,
    ) -> Self {
        let redacted_text = redact(text);
        let (bounded, truncated) = bounded_window(&redacted_text, max_lines, max_bytes);
        Self {
            worktree: worktree.into(),
            run_id: run_id.into(),
            job_id: job_id.into(),
            job_name: job_name.into(),
            text: bounded,
            truncated,
            redacted: true,
            policy_version: REDACTION_POLICY_VERSION,
            fetched_at,
            head_sha: String::new(),
        }
    }

    /// Reapply current redaction and hard bounds to an entry from an older
    /// policy. Entries produced by a newer policy are preserved unchanged.
    pub fn revalidated(mut self) -> Self {
        if self.policy_version < REDACTION_POLICY_VERSION {
            let (text, truncated) =
                bounded_tail(&redact(&self.text), HARD_MAX_LOG_LINES, HARD_MAX_LOG_BYTES);
            self.text = text;
            self.truncated |= truncated;
            self.redacted = true;
            self.policy_version = REDACTION_POLICY_VERSION;
        }
        self
    }

    pub fn candidate(&self) -> CiLogCandidate {
        CiLogCandidate {
            worktree: self.worktree.clone(),
            run_id: self.run_id.clone(),
            job_id: self.job_id.clone(),
            head_sha: self.head_sha.clone(),
        }
    }
}

/// Keep the newest complete UTF-8 lines under both limits.
///
/// A zero limit means keep nothing; it never means unlimited. Lines are kept
/// as complete newline-delimited chunks, so a multi-byte character is never
/// split and the returned text is always valid UTF-8.
pub fn bounded_tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }
    if max_lines == 0 || max_bytes == 0 {
        return (String::new(), true);
    }

    let chunks: Vec<&str> = text.split_inclusive('\n').collect();
    let mut kept = Vec::new();
    let mut bytes = 0;
    for chunk in chunks.iter().rev().take(max_lines) {
        if bytes + chunk.len() > max_bytes {
            break;
        }
        bytes += chunk.len();
        kept.push(*chunk);
    }
    kept.reverse();
    let out = kept.concat();
    let truncated = out.len() != text.len() || kept.len() < chunks.len();
    (out, truncated)
}

static URL_USERINFO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(https?://)[^\s/@:]+(?::[^\s/@]*)?@").unwrap());
static AUTH_HEADER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\b(?:authorization|proxy-authorization)\s*:\s*)[^\s]+(?:\s+[^\s]+)?").unwrap()
});
static BEARER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(\bbearer\s+)[^\s]+").unwrap());
static DOUBLE_QUOTED_ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(\b(?:[A-Za-z_][A-Za-z0-9_-]*(?:token|password|passwd|secret|auth|credential)[A-Za-z0-9_-]*|[A-Za-z_][A-Za-z0-9-]*[_-]key(?:[_-]id)?|token|password|passwd|secret|key|api[_-]?key|auth|credential)\s*[=:]\s*)"[^"]*""#,
    )
    .unwrap()
});
static SINGLE_QUOTED_ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(\b(?:[A-Za-z_][A-Za-z0-9_-]*(?:token|password|passwd|secret|auth|credential)[A-Za-z0-9_-]*|[A-Za-z_][A-Za-z0-9-]*[_-]key(?:[_-]id)?|token|password|passwd|secret|key|api[_-]?key|auth|credential)\s*[=:]\s*)'[^']*'"#,
    )
    .unwrap()
});
static ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(\b(?:[A-Za-z_][A-Za-z0-9_-]*(?:token|password|passwd|secret|auth|credential)[A-Za-z0-9_-]*|[A-Za-z_][A-Za-z0-9-]*[_-]key(?:[_-]id)?|token|password|passwd|secret|key|api[_-]?key|auth|credential)\s*[=:]\s*)[^\s"'`,;]+"#,
    )
    .unwrap()
});
static JWT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{8,}\b").unwrap()
});
static AWS_KEY_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b").unwrap());
static PROVIDER_TOKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,})\b").unwrap()
});

fn last_pem_marker(line: &str) -> Option<bool> {
    let begin = line.rfind("-----BEGIN ");
    let end = line.rfind("-----END ");
    match (begin, end) {
        (None, None) => None,
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (Some(begin), Some(end)) => Some(begin > end),
    }
}

/// Redact credentials commonly emitted by CI tools while retaining ordinary
/// diagnostics and line structure.
pub fn redact(text: &str) -> String {
    // Replace PEM contents line-by-line so a stack of log lines remains a
    // stack of log lines and neither the header nor the key material survives.
    // A tail can begin after the BEGIN delimiter was evicted by an upstream
    // provider. If the first PEM framing marker is END, conservatively treat
    // the entire preceding prefix as key material.
    let mut seen_begin = false;
    let mut unmatched_end = false;
    for part in text.split_inclusive('\n') {
        let line = part.strip_suffix('\n').unwrap_or(part);
        match last_pem_marker(line) {
            Some(true) => seen_begin = true,
            Some(false) if !seen_begin => {
                unmatched_end = true;
                break;
            }
            _ => {}
        }
    }
    let mut in_pem = unmatched_end;
    let mut pem_safe = String::with_capacity(text.len());
    for part in text.split_inclusive('\n') {
        let line = part.strip_suffix('\n').unwrap_or(part);
        let marker = last_pem_marker(line);
        let has_marker = marker.is_some();
        if marker == Some(true) {
            in_pem = true;
        }
        if in_pem || has_marker {
            pem_safe.push_str(REDACTION_MARKER);
        } else {
            pem_safe.push_str(line);
        }
        if let Some(begin) = marker {
            in_pem = begin;
        }
        if part.ends_with('\n') {
            pem_safe.push('\n');
        }
    }

    let s = URL_USERINFO.replace_all(&pem_safe, format!("$1{REDACTION_MARKER}@"));
    let s = AUTH_HEADER.replace_all(&s, format!("$1{REDACTION_MARKER}"));
    let s = BEARER.replace_all(&s, format!("$1{REDACTION_MARKER}"));
    let s = DOUBLE_QUOTED_ASSIGNMENT.replace_all(&s, format!("$1\"{REDACTION_MARKER}\""));
    let s = SINGLE_QUOTED_ASSIGNMENT.replace_all(&s, format!("$1'{REDACTION_MARKER}'"));
    let s = ASSIGNMENT.replace_all(&s, format!("$1{REDACTION_MARKER}"));
    let s = JWT.replace_all(&s, REDACTION_MARKER);
    let s = AWS_KEY_ID.replace_all(&s, REDACTION_MARKER);
    PROVIDER_TOKEN
        .replace_all(&s, REDACTION_MARKER)
        .into_owned()
}

/// Redact one complete provider line while preserving PEM framing state across
/// lines. The caller must bound the line before calling this helper.
pub fn redact_stream_line(line: &[u8], in_pem: &mut bool) -> String {
    let Ok(line) = std::str::from_utf8(line) else {
        return format!(
            "{REDACTION_MARKER}{}",
            if line.ends_with(b"\n") { "\n" } else { "" }
        );
    };
    let (body, newline) = line
        .strip_suffix('\n')
        .map_or((line, ""), |body| (body, "\n"));
    let (body, cr) = body
        .strip_suffix('\r')
        .map_or((body, ""), |body| (body, "\r"));
    let marker = last_pem_marker(body);
    let has_marker = marker.is_some();
    if marker == Some(true) {
        *in_pem = true;
    }
    let result = if *in_pem || has_marker {
        REDACTION_MARKER.to_string()
    } else {
        redact(body)
    };
    if let Some(begin) = marker {
        *in_pem = begin;
    }
    format!("{result}{cr}{newline}")
}

/// Select the terminal run identities retained by a configured cache policy.
pub fn retained_run_ids(run_ids_newest_first: &[String], keep: usize) -> BTreeSet<String> {
    run_ids_newest_first.iter().take(keep).cloned().collect()
}

/// Compare the full candidate identity, including the PR head SHA.
pub fn same_candidate(a: &CiLogCandidate, b: &CiLogCandidate) -> bool {
    a == b
}

/// A cached log is evidence only for a known equal run head, and only when
/// the job did not finish after the log was captured.
pub fn cached_log_is_evidence_for(
    entry: &CiLogEntry,
    run_sha: &str,
    job_finished_at: Option<&str>,
) -> bool {
    if entry.head_sha.trim().is_empty() || run_sha.trim().is_empty() || entry.head_sha != run_sha {
        return false;
    }
    if let Some(finished_at) = job_finished_at.and_then(crate::ci::epoch_secs)
        && finished_at > entry.fetched_at
    {
        return false;
    }
    true
}

/// Unknown-head log entries may be shown from a provider fetch, but never
/// persisted as cache evidence.
pub fn cacheable_log_entry(entry: &CiLogEntry) -> bool {
    !entry.head_sha.trim().is_empty()
}

/// Number of complete log lines retained before the first failure marker.
pub const FAILURE_CONTEXT_LINES: usize = 20;

const EARLIER_OMITTED: &str = "... earlier log lines omitted ...\n";
const LATER_OMITTED: &str = "... later log lines omitted ...\n";

/// Whether a bounded log payload is a failure-anchored window rather than a
/// plain newest-line tail.
pub fn is_failure_window(text: &str) -> bool {
    text.starts_with(EARLIER_OMITTED)
        || text.ends_with(LATER_OMITTED)
        || (!text.ends_with('\n') && text.lines().next().is_some_and(crate::ci::line_is_failure))
}

/// Keep a bounded window around the first failure line for stored and served
/// logs. If the newest-line tail already contains that failure (or there is no
/// failure), output is identical to [`bounded_tail`]. [`failure_excerpt`]
/// remains the byte-only window used for prompts.
pub fn bounded_window(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let (tail, truncated) = bounded_tail(text, max_lines, max_bytes);
    if !truncated || max_lines == 0 || max_bytes == 0 {
        return (tail, truncated);
    }

    let chunks: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(anchor) = chunks
        .iter()
        .position(|chunk| crate::ci::line_is_failure(chunk.trim_end_matches(['\r', '\n'])))
    else {
        return (tail, truncated);
    };

    let tail_start = text.len() - tail.len();
    let anchor_start: usize = chunks[..anchor].iter().map(|chunk| chunk.len()).sum();
    if anchor_start >= tail_start {
        return (tail, truncated);
    }

    (
        build_bounded_window(&chunks, anchor, max_lines, max_bytes),
        true,
    )
}

/// Redact and window a log for a reader. A caller's `requested_lines` can only
/// narrow the configured policy; both limits stay under the hard ceilings.
pub fn served_window(
    text: &str,
    requested_lines: Option<usize>,
    policy_lines: usize,
    policy_bytes: usize,
) -> (String, bool) {
    let lines = requested_lines
        .map_or(policy_lines, |requested| requested.min(policy_lines))
        .min(HARD_MAX_LOG_LINES);
    let bytes = policy_bytes.min(HARD_MAX_LOG_BYTES);
    bounded_window(&redact(text), lines, bytes)
}

fn build_bounded_window(
    chunks: &[&str],
    anchor: usize,
    max_lines: usize,
    max_bytes: usize,
) -> String {
    let context_lines = FAILURE_CONTEXT_LINES.min(max_lines.saturating_sub(1) / 4);
    let context_start = anchor.saturating_sub(context_lines);
    let has_earlier = anchor > 0;
    let has_later = anchor + 1 < chunks.len();
    let anchor_chunk = chunks[anchor];

    let mut include_earlier = has_earlier
        && max_lines > 1
        && anchor_chunk.len() <= max_bytes.saturating_sub(EARLIER_OMITTED.len());
    let earlier_bytes = if include_earlier {
        EARLIER_OMITTED.len()
    } else {
        0
    };
    let anchor_budget = max_bytes - earlier_bytes;
    let anchor_bytes = if anchor_chunk.len() <= anchor_budget {
        anchor_chunk.len()
    } else {
        floor_char_boundary(anchor_chunk, anchor_budget)
    };
    let anchor_text = &anchor_chunk[..anchor_bytes];

    if anchor_bytes < anchor_chunk.len() {
        return anchor_text.to_owned();
    }

    let reserve_later = has_later
        && max_lines > 1 + usize::from(include_earlier)
        && anchor_bytes
            <= max_bytes
                .saturating_sub(earlier_bytes)
                .saturating_sub(LATER_OMITTED.len());
    let later_bytes = if reserve_later {
        LATER_OMITTED.len()
    } else {
        0
    };
    let content_budget = max_bytes - earlier_bytes - later_bytes - anchor_bytes;
    let line_budget = max_lines - 1 - usize::from(include_earlier) - usize::from(reserve_later);

    let mut context = Vec::new();
    let mut used_context_bytes = 0;
    for chunk in chunks[context_start..anchor].iter().rev().take(line_budget) {
        if used_context_bytes + chunk.len() > content_budget {
            break;
        }
        used_context_bytes += chunk.len();
        context.push(*chunk);
    }
    context.reverse();

    if include_earlier && anchor.saturating_sub(context.len()) == 0 {
        include_earlier = false;
    }

    let include_later = has_later
        && max_lines > context.len() + 1 + usize::from(include_earlier)
        && used_context_bytes + anchor_bytes
            <= max_bytes
                .saturating_sub(if include_earlier {
                    EARLIER_OMITTED.len()
                } else {
                    0
                })
                .saturating_sub(LATER_OMITTED.len());
    let later_bytes = if include_later {
        LATER_OMITTED.len()
    } else {
        0
    };

    let mut out = String::new();
    if include_earlier {
        out.push_str(EARLIER_OMITTED);
    }
    for chunk in &context {
        out.push_str(chunk);
    }
    out.push_str(anchor_text);

    let earlier_bytes = if include_earlier {
        EARLIER_OMITTED.len()
    } else {
        0
    };
    let mut used_bytes = earlier_bytes + used_context_bytes + anchor_bytes;
    let used_lines = usize::from(include_earlier) + context.len() + 1;
    let forward_lines = max_lines - used_lines - usize::from(include_later);
    let mut next = anchor + 1;
    for chunk in chunks.iter().skip(next).take(forward_lines) {
        if used_bytes + chunk.len() > max_bytes - later_bytes {
            break;
        }
        out.push_str(chunk);
        used_bytes += chunk.len();
        next += 1;
    }
    if include_later && next < chunks.len() {
        out.push_str(LATER_OMITTED);
    }
    out
}

/// Keep a bounded excerpt around the first recognizable failure line.
///
/// Without a failure marker, this keeps the newest complete lines. Omission
/// markers are included when they fit, and the result always respects the byte
/// budget without splitting a UTF-8 character.
pub fn failure_excerpt(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    if max_bytes == 0 {
        return String::new();
    }

    let chunks: Vec<&str> = text.split_inclusive('\n').collect();
    let anchor = chunks
        .iter()
        .position(|chunk| crate::ci::line_is_failure(chunk.trim_end_matches(['\r', '\n'])));

    let Some(anchor) = anchor else {
        let marker = if EARLIER_OMITTED.len() <= max_bytes {
            EARLIER_OMITTED
        } else {
            ""
        };
        let content_budget = max_bytes - marker.len();
        let (tail, _) = bounded_tail(text, usize::MAX, content_budget);
        if !tail.is_empty() {
            let mut excerpt = String::with_capacity(marker.len() + tail.len());
            excerpt.push_str(marker);
            excerpt.push_str(&tail);
            return excerpt;
        }

        let last = chunks.last().copied().unwrap_or("");
        let cut = floor_char_boundary(last, content_budget);
        let mut excerpt = String::with_capacity(marker.len() + cut);
        excerpt.push_str(marker);
        excerpt.push_str(&last[..cut]);
        return excerpt;
    };

    let mut start = anchor.saturating_sub(FAILURE_CONTEXT_LINES);
    let has_later = anchor + 1 < chunks.len();
    let mut context_bytes: usize = chunks[start..anchor].iter().map(|line| line.len()).sum();
    let projected_markers = if EARLIER_OMITTED.len() <= max_bytes {
        EARLIER_OMITTED.len()
            + if has_later && LATER_OMITTED.len() <= max_bytes - EARLIER_OMITTED.len() {
                LATER_OMITTED.len()
            } else {
                0
            }
    } else {
        0
    };
    let needs_earlier =
        start > 0 || (projected_markers > 0 && context_bytes > (max_bytes - projected_markers) / 4);
    let mut keep_earlier = needs_earlier && EARLIER_OMITTED.len() <= max_bytes;
    let mut marker_bytes = 0;
    if keep_earlier {
        marker_bytes += EARLIER_OMITTED.len();
    }
    let keep_later = has_later && LATER_OMITTED.len() <= max_bytes - marker_bytes;
    if keep_later {
        marker_bytes += LATER_OMITTED.len();
    }
    let content_budget = max_bytes - marker_bytes;

    // Context gets at most one quarter of the content budget. If necessary,
    // discard its oldest lines first and mark the resulting gap when possible.
    let context_budget = content_budget / 4;
    while context_bytes > context_budget && start < anchor {
        context_bytes -= chunks[start].len();
        start += 1;
        keep_earlier = true;
    }

    let mut content = String::with_capacity(content_budget);
    for line in &chunks[start..anchor] {
        content.push_str(line);
    }
    let mut used = content.len();
    let mut next = anchor;
    while next < chunks.len() {
        let line = chunks[next];
        if line.len() <= content_budget - used {
            content.push_str(line);
            used += line.len();
            next += 1;
        } else if next == anchor {
            let available = content_budget - used;
            let cut = floor_char_boundary(line, available);
            content.push_str(&line[..cut]);
            next += 1;
            break;
        } else {
            break;
        }
    }
    let omitted_later = next < chunks.len();

    // A marker can only be emitted if its full bytes fit. Content was already
    // budgeted with markers when possible; for a newly discovered context gap,
    // emit it only if doing so still respects the cap.
    let mut excerpt = String::with_capacity(max_bytes);
    if keep_earlier
        && excerpt.len()
            + EARLIER_OMITTED.len()
            + content.len()
            + usize::from(omitted_later && keep_later) * LATER_OMITTED.len()
            <= max_bytes
    {
        excerpt.push_str(EARLIER_OMITTED);
    }
    excerpt.push_str(&content);
    if omitted_later && keep_later && excerpt.len() + LATER_OMITTED.len() <= max_bytes {
        excerpt.push_str(LATER_OMITTED);
    }
    excerpt
}

/// Bounded, redacted CI-log evidence for a PR repair prompt.
///
/// Only cached entries captured for `head_sha` are included. When no cached
/// entry was captured for this head, callers keep their URL fallback.
pub fn pr_repair_excerpt(
    entries: &[CiLogEntry],
    head_sha: &str,
    max_bytes: usize,
) -> Option<String> {
    if head_sha.trim().is_empty() {
        return None;
    }

    let mut matching: Vec<&CiLogEntry> = entries
        .iter()
        .filter(|entry| entry.head_sha == head_sha && !entry.text.trim().is_empty())
        .collect();
    matching.sort_by(|left, right| {
        right
            .fetched_at
            .cmp(&left.fetched_at)
            .then_with(|| left.run_id.cmp(&right.run_id))
            .then_with(|| left.job_id.cmp(&right.job_id))
    });

    let mut out = String::new();
    let mut remaining = max_bytes;
    for entry in matching {
        let name: String = entry
            .job_name
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        let heading = format!("--- job {name} (run {}) ---\n", entry.run_id);
        if heading.len() >= remaining {
            break;
        }

        out.push_str(&heading);
        remaining -= heading.len();
        let body = failure_excerpt(&redact(&entry.text), remaining);
        out.push_str(&body);
        remaining -= body.len();
        if !body.ends_with('\n') && remaining > 0 {
            out.push('\n');
            remaining -= 1;
        }
    }

    (!out.is_empty()).then_some(out)
}

fn floor_char_boundary(text: &str, mut byte: usize) -> usize {
    byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn served_window_never_widens_past_the_configured_policy() {
        let text = (0..50).map(|n| format!("line {n}\n")).collect::<String>();

        let (served, truncated) = served_window(&text, Some(40), 10, 1 << 20);

        assert_eq!(served.lines().count(), 10);
        assert!(truncated);
    }

    #[test]
    fn served_window_narrows_on_request() {
        let text = (0..50).map(|n| format!("line {n}\n")).collect::<String>();

        let (narrowed, narrowed_truncated) = served_window(&text, Some(3), 10, 1 << 20);
        let (policy, policy_truncated) = served_window(&text, None, 10, 1 << 20);

        assert_eq!(narrowed.lines().count(), 3);
        assert!(narrowed_truncated);
        assert_eq!(policy.lines().count(), 10);
        assert!(policy_truncated);
    }

    #[test]
    fn served_window_redacts_cached_text() {
        let (served, _) = served_window("token=plain-secret\nok\n", None, 100, 4096);

        assert!(!served.contains("plain-secret"));
        assert!(served.contains(REDACTION_MARKER));
    }

    #[test]
    fn served_window_clamps_to_hard_caps() {
        assert_eq!(
            served_window("a\n", None, usize::MAX, usize::MAX),
            ("a\n".to_string(), false)
        );
    }

    #[test]
    fn tail_keeps_newest_complete_lines_and_marks_truncation() {
        let (out, truncated) = bounded_tail("old\nnew-é\n", 1, 32);
        assert_eq!(out, "new-é\n");
        assert!(truncated);
        let (out, truncated) = bounded_tail("éé\nlast\n", 10, 6);
        assert_eq!(out, "last\n");
        assert!(truncated);
    }

    #[test]
    fn bounded_window_keeps_a_failure_older_than_the_tail() {
        let mut text = (0..5_000)
            .map(|line| format!("ok line {line}\n"))
            .collect::<String>();
        text = text.replacen("ok line 100\n", "error: boom\n", 1);

        let (window, truncated) = bounded_window(&text, 2_000, 1 << 20);

        assert!(window.contains("error: boom"));
        assert!(window.starts_with(EARLIER_OMITTED));
        assert!(window.ends_with(LATER_OMITTED));
        assert!(window.lines().count() <= 2_000);
        assert!(window.len() <= 1 << 20);
        assert!(truncated);
    }

    #[test]
    fn entry_new_stores_the_first_failure_of_a_long_log() {
        let mut text = (0..5_000)
            .map(|line| format!("ok line {line}\n"))
            .collect::<String>();
        text = text.replacen("ok line 100\n", "error: boom\n", 1);

        let entry = CiLogEntry::new("/wt", "run", "job", "tests", &text, 2_000, 1 << 20, 1);

        assert!(entry.text.contains("error: boom"));
        assert!(entry.truncated);
    }

    #[test]
    fn bounded_window_at_start_omits_the_earlier_marker() {
        let text = format!("error: first\n{}", "later line\n".repeat(100));

        let (window, truncated) = bounded_window(&text, 10, 512);

        assert!(window.starts_with("error: first\n"));
        assert!(!window.starts_with(EARLIER_OMITTED));
        assert!(window.ends_with(LATER_OMITTED));
        assert!(truncated);
    }

    #[test]
    fn bounded_window_marks_context_dropped_near_the_log_start() {
        let text = format!(
            "old line\n{}\nerror: failure\n{}",
            "x".repeat(80),
            "later\n".repeat(20)
        );

        let (window, truncated) = bounded_window(&text, 10, 100);

        assert!(window.starts_with(EARLIER_OMITTED));
        assert!(window.contains("error: failure\n"));
        assert!(!window.contains(&"x".repeat(80)));
        assert!(truncated);
    }

    #[test]
    fn bounded_window_emits_only_a_cut_anchor_prefix() {
        let anchor = "error: é€💥\n";
        let text = format!("\n{anchor}\nlater\n");

        let (window, truncated) = bounded_window(&text, 10, 9);

        assert_eq!(window, &anchor[..9]);
        assert!(truncated);
    }

    #[test]
    fn failure_window_helper_distinguishes_windows_from_plain_tails() {
        assert!(!is_failure_window("ordinary tail\n"));
        assert!(is_failure_window(
            "... earlier log lines omitted ...\nerror: old\n"
        ));
        assert!(is_failure_window(
            "error: first\n... later log lines omitted ...\n"
        ));
    }

    #[test]
    fn bounded_window_respects_tiny_line_and_byte_limits() {
        let anchor = "error: é€💥 failed\n";
        let text = format!(
            "{}context before\n{anchor}{}",
            "old\n".repeat(30),
            "later\n".repeat(30)
        );
        let limits = [(1, 1 << 20), (2, 1 << 20), (5, 64), (3, 16), (50, 40)];
        let results = limits
            .into_iter()
            .map(|(max_lines, max_bytes)| {
                let (window, _) = bounded_window(&text, max_lines, max_bytes);
                let line_count = window.lines().count();
                let contains_anchor = window.contains(anchor);
                (
                    max_lines,
                    max_bytes,
                    line_count,
                    window.len(),
                    contains_anchor,
                )
            })
            .collect::<Vec<_>>();

        for ((max_lines, max_bytes), (_, _, line_count, byte_count, contains_anchor)) in
            limits.into_iter().zip(results.iter())
        {
            assert!(
                *line_count <= max_lines && *byte_count <= max_bytes,
                "all bounded-window results: {results:?}"
            );
            if anchor.len() <= max_bytes {
                assert!(*contains_anchor, "all bounded-window results: {results:?}");
            }
        }
    }

    // Positive controls: these preserve the old payload byte-for-byte.
    #[test]
    fn bounded_window_equals_bounded_tail_without_a_failure() {
        let text = (0..5_000)
            .map(|line| format!("ordinary line {line}\n"))
            .collect::<String>();
        assert_eq!(
            bounded_window(&text, 2_000, 1 << 20),
            bounded_tail(&text, 2_000, 1 << 20)
        );
    }

    #[test]
    fn bounded_window_equals_bounded_tail_when_the_tail_has_the_failure() {
        let text = format!("{}error: recent\n", "ordinary\n".repeat(100));
        assert_eq!(
            bounded_window(&text, 10, 1 << 20),
            bounded_tail(&text, 10, 1 << 20)
        );
    }

    #[test]
    fn bounded_window_zero_limits_match_bounded_tail() {
        for limits in [(0, 100), (100, 0)] {
            assert_eq!(
                bounded_window("error: failure\n", limits.0, limits.1),
                (String::new(), true)
            );
        }
    }

    #[test]
    fn policy_version_new_entry_carries_the_current_version() {
        let entry = CiLogEntry::new("/wt", "run", "job", "tests", "ordinary\n", 10, 100, 1);
        assert_eq!(entry.policy_version, REDACTION_POLICY_VERSION);
    }

    #[test]
    fn policy_version_revalidated_re_redacts_older_entries() {
        let entry = CiLogEntry {
            text: "token=SYNTHETIC_TOKEN\nordinary\n".into(),
            policy_version: 1,
            ..Default::default()
        }
        .revalidated();

        assert!(!entry.text.contains("SYNTHETIC_TOKEN"));
        assert!(entry.text.contains("ordinary"));
        assert_eq!(entry.policy_version, REDACTION_POLICY_VERSION);
        assert!(entry.redacted);
    }

    #[test]
    fn policy_version_revalidated_enforces_hard_bounds() {
        let entry = CiLogEntry {
            text: "x".repeat(HARD_MAX_LOG_BYTES + 1),
            policy_version: 1,
            ..Default::default()
        }
        .revalidated();

        assert!(entry.text.len() <= HARD_MAX_LOG_BYTES);
        assert!(entry.truncated);
    }

    #[test]
    fn policy_version_revalidated_keeps_current_and_newer_entries() {
        for policy_version in [REDACTION_POLICY_VERSION, u32::MAX] {
            let entry = CiLogEntry {
                text: "token=SYNTHETIC_TOKEN\n".into(),
                policy_version,
                ..Default::default()
            };
            assert_eq!(entry.clone().revalidated(), entry);
        }
    }

    #[test]
    fn policy_version_defaults_to_zero_for_json_without_it() {
        let entry: CiLogEntry = serde_json::from_str(
            r#"{"worktree":"/wt","run_id":"run","job_id":"job","job_name":"tests","text":"ordinary","truncated":false,"redacted":true,"fetched_at":1,"head_sha":""}"#,
        )
        .unwrap();

        assert_eq!(entry.policy_version, 0);
    }

    #[test]
    fn zero_limits_are_not_unlimited() {
        assert_eq!(bounded_tail("one\n", 0, 100), (String::new(), true));
        assert_eq!(bounded_tail("one\n", 100, 0), (String::new(), true));
    }

    #[test]
    fn failure_excerpt_windows_a_mid_log_failure() {
        let mut lines = (0..3_000)
            .map(|line| format!("ok step {line}\n"))
            .collect::<String>();
        lines.push_str("error: SYNTHETIC_MID failure\n");
        lines.push_str(
            &(3_000..6_000)
                .map(|line| format!("ok step {line}\n"))
                .collect::<String>(),
        );

        let excerpt = failure_excerpt(&lines, 16 * 1024);

        assert!(excerpt.contains("SYNTHETIC_MID"));
        assert!(excerpt.contains("ok step 2980\n"));
        assert!(excerpt.contains("ok step 2999\n"));
        assert!(excerpt.starts_with(EARLIER_OMITTED));
        assert!(excerpt.ends_with(LATER_OMITTED));
        assert!(excerpt.len() <= 16 * 1024);
    }

    #[test]
    fn failure_excerpt_marker_at_start_end_and_absent() {
        let at_start_log = format!("error: first\n{}", "ok after\n".repeat(100));
        let at_start = failure_excerpt(&at_start_log, 128);
        assert!(!at_start.starts_with(EARLIER_OMITTED));
        assert!(at_start.starts_with("error: first\n"));
        assert!(at_start.ends_with(LATER_OMITTED));

        let at_end_log = format!("{}error: last\n", "old line\n".repeat(100));
        let at_end = failure_excerpt(&at_end_log, 64);
        assert!(at_end.starts_with(EARLIER_OMITTED));
        assert!(at_end.contains("error: last\n"));
        assert!(!at_end.ends_with(LATER_OMITTED));

        let absent_log = format!("{}newest line\n", "old line\n".repeat(100));
        let absent = failure_excerpt(&absent_log, 64);
        assert!(absent.starts_with(EARLIER_OMITTED));
        assert!(absent.ends_with("newest line\n"));

        let tiny = failure_excerpt(&"ordinary line\n".repeat(20), 8);
        assert!(tiny.len() <= 8);
        assert!(!tiny.starts_with(EARLIER_OMITTED));

        assert_eq!(failure_excerpt("short log\n", 64), "short log\n");
        assert_eq!(failure_excerpt("anything\n", 0), "");

        let mut early_anchor_log = (0..4)
            .map(|line| format!("context {line:02} {}\n", "x".repeat(16)))
            .collect::<String>();
        early_anchor_log.push_str("error: early failure\n");
        early_anchor_log.push_str(
            &(5..20)
                .map(|line| format!("later line {line}\n"))
                .collect::<String>(),
        );
        let early_anchor = failure_excerpt(&early_anchor_log, 128);
        assert!(early_anchor.starts_with(EARLIER_OMITTED));
        assert!(early_anchor.contains("error: early failure\n"));
        assert!(early_anchor.len() <= 128);
    }

    #[test]
    fn failure_excerpt_never_splits_utf8_or_exceeds_the_budget() {
        let text = format!("error: {}", "é".repeat(20_000));
        for budget in (1..=64).chain([16 * 1024]) {
            let excerpt = failure_excerpt(&text, budget);
            assert!(
                excerpt.len() <= budget,
                "budget {budget}, got {}",
                excerpt.len()
            );
            assert!(excerpt.is_char_boundary(excerpt.len()));
            assert_eq!(
                String::from_utf8(excerpt.clone().into_bytes()).unwrap(),
                excerpt
            );
        }
    }

    #[test]
    fn redacts_supported_secret_shapes_and_keeps_diagnostics() {
        let text = concat!(
            "Authorization: Bearer top-secret\n",
            "url=https://alice:password@example.test/x\n",
            "token=plain-secret password: other-secret\n",
            "KEY=plain-key PRIVATE_KEY=private-key ACCESS_KEY=access-key\n",
            "monkey=ordinary-diagnostic\n",
            "jwt eyJabcdefghijk.abcde.lmnopqrstuv\n",
            "AWS AKIA1234567890ABCDEF\n",
            "ghp_123456789012345678901234567890123456\n",
            "glpat-123456789012345678901234567890123456\n",
            "-----BEGIN PRIVATE KEY-----\nsecret-pem\n-----END PRIVATE KEY-----\n",
            "cargo: warning: ordinary diagnostic\n",
        );
        let out = redact(text);
        for secret in [
            "top-secret",
            "password@example",
            "plain-secret",
            "other-secret",
            "plain-key",
            "private-key",
            "access-key",
            "eyJabcdefghijk.abcde.lmnopqrstuv",
            "AKIA1234567890ABCDEF",
            "ghp_123456789012345678901234567890123456",
            "glpat-123456789012345678901234567890123456",
            "secret-pem",
        ] {
            assert!(!out.contains(secret), "secret survived: {secret} in {out}");
        }
        assert!(out.contains(REDACTION_MARKER));
        assert!(out.contains("ordinary diagnostic"));
        assert!(out.contains("ordinary-diagnostic"));
        assert_eq!(out.lines().count(), text.lines().count());
    }

    #[test]
    fn streaming_pem_state_redacts_body_before_tail_eviction() {
        let mut in_pem = false;
        let lines = [
            b"old diagnostic\n".as_slice(),
            b"-----BEGIN PRIVATE KEY-----\n".as_slice(),
            b"SYNTHETIC_PRIVATE_KEY_BODY\n".as_slice(),
            b"-----END PRIVATE KEY-----\n".as_slice(),
            b"ordinary failure\n".as_slice(),
        ];
        let redacted = lines
            .iter()
            .map(|line| redact_stream_line(line, &mut in_pem))
            .collect::<String>();
        let (tail, _) = bounded_tail(&redacted, 2, 256);
        assert!(!tail.contains("SYNTHETIC_PRIVATE_KEY_BODY"));
        assert!(tail.contains("ordinary failure"));
        assert!(tail.contains(REDACTION_MARKER));
    }

    #[test]
    fn provider_prefixed_pem_is_redacted_in_batch_and_streaming_paths() {
        let text = concat!(
            "build\tstep\t2024-01-01T00:00:00.0000000Z -----BEGIN OPENSSH PRIVATE KEY-----\n",
            "SYNTHETICKEYBODY\n",
            "build\tstep\t2024-01-01T00:00:01.0000000Z -----END OPENSSH PRIVATE KEY-----\n",
            "ordinary failure diagnostic\n",
        );
        let batch = redact(text);
        assert!(!batch.contains("SYNTHETICKEYBODY"));
        assert!(batch.contains("ordinary failure diagnostic"));

        let mut in_pem = false;
        let streamed = text
            .split_inclusive('\n')
            .map(|line| redact_stream_line(line.as_bytes(), &mut in_pem))
            .collect::<String>();
        assert!(!streamed.contains("SYNTHETICKEYBODY"));
        assert!(streamed.contains("ordinary failure diagnostic"));
        assert!(!in_pem);
    }

    #[test]
    fn end_before_begin_on_one_line_keeps_later_pem_open() {
        let text = concat!(
            "-----END A----- -----BEGIN B-----\n",
            "SYNTHETIC_KEY_MATERIAL\n",
            "-----END B-----\n",
            "ordinary diagnostic\n",
        );
        let batch = redact(text);
        assert!(!batch.contains("SYNTHETIC_KEY_MATERIAL"));
        assert!(batch.contains("ordinary diagnostic"));

        let mut in_pem = false;
        let streamed = text
            .split_inclusive('\n')
            .map(|line| redact_stream_line(line.as_bytes(), &mut in_pem))
            .collect::<String>();
        assert!(!streamed.contains("SYNTHETIC_KEY_MATERIAL"));
        assert!(streamed.contains("ordinary diagnostic"));
        assert!(!in_pem);
    }

    #[test]
    fn unmatched_pem_end_redacts_unknown_prefix_conservatively() {
        let text =
            "SYNTHETIC_UNKNOWN_PREFIX_SECRET\n-----END PRIVATE KEY-----\nuseful diagnostic\n";
        let redacted = redact(text);
        assert!(!redacted.contains("SYNTHETIC_UNKNOWN_PREFIX_SECRET"));
        assert!(redacted.contains("useful diagnostic"));
    }

    #[test]
    fn streaming_redaction_handles_invalid_utf8_and_crlf_pem() {
        let mut in_pem = false;
        assert_eq!(
            redact_stream_line(b"\xff\xfe\n", &mut in_pem),
            format!("{REDACTION_MARKER}\n")
        );
        for line in ["-----BEGIN X-----\r\n", "body\r\n", "-----END X-----\r\n"] {
            assert_eq!(
                redact_stream_line(line.as_bytes(), &mut in_pem),
                format!("{REDACTION_MARKER}\r\n")
            );
        }
        assert!(!in_pem);
        assert_eq!(
            redact_stream_line(b"ordinary\r\n", &mut in_pem),
            "ordinary\r\n"
        );
    }

    #[test]
    fn candidate_identity_includes_head_sha() {
        let a = CiLogCandidate {
            worktree: "w".into(),
            run_id: "r".into(),
            job_id: "j".into(),
            head_sha: "a".into(),
        };
        let mut b = a.clone();
        b.head_sha = "b".into();
        assert!(!same_candidate(&a, &b));
    }

    #[test]
    fn cached_log_evidence_requires_a_known_equal_head() {
        let entry = CiLogEntry {
            head_sha: "sha".into(),
            ..Default::default()
        };
        assert!(cached_log_is_evidence_for(&entry, "sha", None));
        assert!(!cached_log_is_evidence_for(&entry, "", None));
        assert!(!cached_log_is_evidence_for(
            &CiLogEntry::default(),
            "sha",
            None
        ));
        assert!(!cached_log_is_evidence_for(
            &CiLogEntry::default(),
            "",
            None
        ));
        assert!(!cached_log_is_evidence_for(&entry, "other", None));
    }

    #[test]
    fn whitespace_only_head_is_not_cached_log_evidence() {
        let entry = CiLogEntry {
            head_sha: "  \t".into(),
            ..Default::default()
        };
        assert!(!cached_log_is_evidence_for(&entry, "  \t", None));
    }

    #[test]
    fn job_finished_after_capture_is_not_evidence() {
        let entry = CiLogEntry {
            fetched_at: 100,
            head_sha: "sha".into(),
            ..Default::default()
        };
        assert!(cached_log_is_evidence_for(
            &entry,
            "sha",
            Some("1970-01-01T00:01:00Z")
        ));
        assert!(!cached_log_is_evidence_for(
            &entry,
            "sha",
            Some("1970-01-01T00:03:00Z")
        ));
        assert!(cached_log_is_evidence_for(&entry, "sha", Some("junk")));
        assert!(cached_log_is_evidence_for(&entry, "sha", None));
    }

    #[test]
    fn unknown_head_entries_are_not_cacheable() {
        assert!(!cacheable_log_entry(&CiLogEntry::default()));
        assert!(!cacheable_log_entry(&CiLogEntry {
            head_sha: "  \t".into(),
            ..Default::default()
        }));
        assert!(cacheable_log_entry(&CiLogEntry {
            head_sha: "sha".into(),
            ..Default::default()
        }));
    }

    #[test]
    fn constructor_bounds_hold_when_redaction_expands_input() {
        let input = (0..HARD_MAX_LOG_LINES)
            .map(|line| {
                if line + 1 == HARD_MAX_LOG_LINES {
                    "token=a padding_padding_padding_padding latest\n".to_string()
                } else {
                    "token=a padding_padding_padding_padding\n".to_string()
                }
            })
            .collect::<String>();
        let entry = CiLogEntry::new(
            "/wt",
            "run",
            "job",
            "test",
            &input,
            HARD_MAX_LOG_LINES,
            HARD_MAX_LOG_BYTES,
            1,
        );

        assert!(entry.truncated);
        assert!(entry.redacted);
        assert!(entry.text.len() <= HARD_MAX_LOG_BYTES);
        assert!(entry.text.lines().count() <= HARD_MAX_LOG_LINES);
        assert!(!entry.text.contains("token=a"));
        assert!(entry.text.ends_with("latest\n"));
    }

    fn pr_repair_entry(
        head_sha: &str,
        run_id: &str,
        job_id: &str,
        job_name: &str,
        text: &str,
        fetched_at: i64,
    ) -> CiLogEntry {
        CiLogEntry {
            worktree: "/w/feat".into(),
            run_id: run_id.into(),
            job_id: job_id.into(),
            job_name: job_name.into(),
            text: text.into(),
            head_sha: head_sha.into(),
            fetched_at,
            ..Default::default()
        }
    }

    #[test]
    fn pr_repair_excerpt_keeps_only_entries_for_the_head() {
        let entries = [
            pr_repair_entry("old", "1", "1", "old", "old log", 2),
            pr_repair_entry("abc", "2", "2", "current", "current log", 1),
        ];
        let excerpt = pr_repair_excerpt(&entries, "abc", 1024).unwrap();
        assert!(excerpt.contains("current log"));
        assert!(!excerpt.contains("old log"));
    }

    #[test]
    fn pr_repair_excerpt_is_none_without_head_evidence() {
        let entry = pr_repair_entry("abc", "1", "1", "test", "log", 1);
        assert_eq!(
            pr_repair_excerpt(std::slice::from_ref(&entry), "", 1024),
            None
        );
        assert_eq!(
            pr_repair_excerpt(std::slice::from_ref(&entry), "  \n", 1024),
            None
        );
        assert_eq!(pr_repair_excerpt(&[entry], "other", 1024), None);
        let blank = pr_repair_entry("abc", "1", "1", "test", " \n", 1);
        assert_eq!(pr_repair_excerpt(&[blank], "abc", 1024), None);
    }

    #[test]
    fn pr_repair_excerpt_re_redacts_cached_text() {
        let token = format!("ghp_{}", "a".repeat(36));
        let text = format!("token {token}\n");
        let entry = pr_repair_entry("abc", "1", "1", "test", &text, 1);
        let excerpt = pr_repair_excerpt(&[entry], "abc", 1024).unwrap();
        assert!(excerpt.contains(REDACTION_MARKER));
        assert!(!excerpt.contains(&token));
    }

    #[test]
    fn pr_repair_excerpt_windows_each_job_within_the_budget() {
        let text = (0..500)
            .map(|line| {
                if line == 250 {
                    "error: boom\n".to_owned()
                } else {
                    format!("line {line}\n")
                }
            })
            .collect::<String>();
        let entry = pr_repair_entry("abc", "1", "1", "tést", &text, 1);
        let excerpt = pr_repair_excerpt(&[entry], "abc", 2048).unwrap();
        assert!(excerpt.len() <= 2048);
        assert!(excerpt.contains("error: boom"));
        assert!(excerpt.contains(EARLIER_OMITTED));

        let unicode = pr_repair_entry("abc", "2", "2", "界", "ééééé", 2);
        let clipped = pr_repair_excerpt(&[unicode], "abc", 33).unwrap();
        assert!(
            clipped.len() <= 33,
            "multi-byte text must fit the byte budget"
        );
    }

    #[test]
    fn pr_repair_excerpt_orders_newest_first_then_run_and_job() {
        let entries = [
            pr_repair_entry("abc", "b", "z", "tie-later-job", "z", 10),
            pr_repair_entry("abc", "a", "b", "tie-earlier-run", "a", 10),
            pr_repair_entry("abc", "a", "a", "tie-earlier-job", "a", 10),
            pr_repair_entry("abc", "new", "1", "newest", "new", 20),
        ];
        let excerpt = pr_repair_excerpt(&entries, "abc", 2048).unwrap();
        let newest = excerpt.find("job newest").unwrap();
        let run_a_job_a = excerpt.find("job tie-earlier-job").unwrap();
        let run_a_job_b = excerpt.find("job tie-earlier-run").unwrap();
        let run_b_job_z = excerpt.find("job tie-later-job").unwrap();
        assert!(newest < run_a_job_a && run_a_job_a < run_a_job_b && run_a_job_b < run_b_job_z);
    }

    #[test]
    fn pr_repair_excerpt_stops_when_the_next_heading_does_not_fit() {
        let first = pr_repair_entry("abc", "1", "1", "first", "body\n", 2);
        let first_chunk = pr_repair_excerpt(std::slice::from_ref(&first), "abc", 1024).unwrap();
        let second = pr_repair_entry("abc", "2", "2", "second", "body\n", 1);
        let excerpt = pr_repair_excerpt(&[first, second], "abc", first_chunk.len() + 3).unwrap();
        assert!(excerpt.contains("job first"));
        assert!(!excerpt.contains("job second"));
    }
}
