//! Pure commit-message trailer handling for `thegn dispatch commit` (THE-1972).
//!
//! Trailers are `Key: value` lines in the last paragraph of a commit message,
//! git-trailer style. [`with_trailers`] appends caller-supplied trailers,
//! skipping any that the message already carries, and normalises line endings.

/// Check that `trailer` is a single-line `Key: value` trailer and return it
/// trimmed. The key is non-empty ASCII alphanumerics and `-`; the value is
/// non-empty after trimming.
pub fn validate_trailer(trailer: &str) -> Result<String, String> {
    if trailer.contains('\n') || trailer.contains('\r') {
        return Err(format!("trailer must be a single line: {trailer:?}"));
    }
    let trimmed = trailer.trim();
    if !is_trailer_line(trimmed) {
        return Err(format!("trailer must look like `Key: value`: {trimmed:?}"));
    }
    Ok(trimmed.to_string())
}

/// `Key: value` with a token key and a non-empty value.
fn is_trailer_line(line: &str) -> bool {
    let Some((key, value)) = line.split_once(':') else {
        return false;
    };
    !key.is_empty()
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !value.trim().is_empty()
}

/// Append `trailers` to `msg` as a git trailer block.
///
/// CRLF is normalised to LF and trailing blank lines are trimmed; the result
/// ends with exactly one newline. A trailer already present as a line of the
/// message (compared trimmed) is not added again, nor is one repeated within
/// `trailers`. New trailers join the message's existing trailer block when its
/// last paragraph is one, otherwise they start a new paragraph. Callers should
/// run [`validate_trailer`] first; shape is not re-checked here.
pub fn with_trailers(msg: &str, trailers: &[String]) -> String {
    let normalized = msg.replace("\r\n", "\n").replace('\r', "\n");
    let body = normalized.trim_end();
    let mut present: Vec<String> = body.lines().map(|l| l.trim().to_string()).collect();
    let mut fresh: Vec<String> = Vec::new();
    for trailer in trailers {
        let t = trailer.trim().to_string();
        if t.is_empty() || present.contains(&t) {
            continue;
        }
        present.push(t.clone());
        fresh.push(t);
    }
    let mut out = body.to_string();
    if !fresh.is_empty() {
        if last_paragraph_is_trailers(body) {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
        out.push_str(&fresh.join("\n"));
    }
    out.push('\n');
    out
}

/// True when `body` has a subject paragraph followed by a final paragraph
/// made only of trailer lines.
fn last_paragraph_is_trailers(body: &str) -> bool {
    let Some((head, last)) = body.rsplit_once("\n\n") else {
        return false;
    };
    !head.trim().is_empty() && last.lines().all(|l| is_trailer_line(l.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    #[test]
    fn empty_trailers_only_normalises_newlines() {
        assert_eq!(with_trailers("feat: x\n\n\n", &[]), "feat: x\n");
        assert_eq!(with_trailers("feat: x", &[]), "feat: x\n");
    }

    #[test]
    fn appends_new_paragraph() {
        assert_eq!(
            with_trailers("feat: x\n\nbody text\n", &t("A-B: 1")),
            "feat: x\n\nbody text\n\nA-B: 1\n"
        );
    }

    #[test]
    fn subject_only_gets_blank_line() {
        assert_eq!(with_trailers("feat: x", &t("A: 1")), "feat: x\n\nA: 1\n");
    }

    #[test]
    fn joins_existing_trailer_block() {
        assert_eq!(
            with_trailers("feat: x\n\nSigned-off-by: a\n", &t("A: 1")),
            "feat: x\n\nSigned-off-by: a\nA: 1\n"
        );
    }

    #[test]
    fn duplicate_trailer_is_skipped() {
        let msg = "feat: x\n\nA: 1\n";
        assert_eq!(with_trailers(msg, &t("A: 1")), msg);
        assert_eq!(with_trailers(msg, &t("  A: 1  ")), msg);
    }

    #[test]
    fn duplicates_within_input_collapse_and_blank_skipped() {
        let tr = vec!["A: 1".to_string(), "A: 1".to_string(), "  ".to_string()];
        assert_eq!(with_trailers("s", &tr), "s\n\nA: 1\n");
    }

    #[test]
    fn crlf_is_normalised() {
        assert_eq!(
            with_trailers("feat: x\r\n\r\nA: 1\r\n", &t("B: 2")),
            "feat: x\n\nA: 1\nB: 2\n"
        );
        assert_eq!(with_trailers("a\rb", &[]), "a\nb\n");
    }

    #[test]
    fn crlf_duplicate_detected() {
        let out = with_trailers("s\r\n\r\nA: 1\r\n", &t("A: 1"));
        assert_eq!(out, "s\n\nA: 1\n");
    }

    #[test]
    fn non_trailer_last_paragraph_starts_new_block() {
        assert_eq!(
            with_trailers("s\n\nfixes: nothing\nplain", &t("A: 1")),
            "s\n\nfixes: nothing\nplain\n\nA: 1\n"
        );
    }

    #[test]
    fn validate_accepts_and_trims() {
        assert_eq!(
            validate_trailer(" Claude-Session: https://x/y "),
            Ok("Claude-Session: https://x/y".to_string())
        );
    }

    #[test]
    fn validate_rejects_bad_shapes() {
        for bad in [
            "no colon",
            ": value",
            "Key:",
            "Key:   ",
            "Bad Key: v",
            "K\n: v",
            "K: v\r",
            "K: a\nL: b",
            "",
        ] {
            assert!(validate_trailer(bad).is_err(), "{bad:?}");
        }
    }
}
