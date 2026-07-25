//! Small, pure naming helpers shared by the loop and the CLI. Kept out of the
//! ratchet-pinned `run.rs` so the god-file stays lean.

/// Slugify an issue's `number` + `title` into a branch-name tail, or honour an
/// explicit `hint` when one is given. Lowercases, collapses runs of
/// non-alphanumerics to single dashes, trims leading/trailing dashes, and caps
/// the result at 48 chars.
pub(crate) fn issue_branch_tail(number: &str, title: &str, hint: Option<&str>) -> String {
    if let Some(h) = hint.filter(|h| !h.trim().is_empty()) {
        return h.trim().to_string();
    }
    let raw = format!("{number}-{title}");
    let mut out = String::new();
    let mut prev_dash = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').chars().take(48).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_takes_precedence_when_non_blank() {
        assert_eq!(
            issue_branch_tail("42", "ignored title", Some("custom-branch")),
            "custom-branch"
        );
        // A hint is trimmed before use.
        assert_eq!(issue_branch_tail("1", "x", Some("  spaced  ")), "spaced");
    }

    #[test]
    fn blank_hint_falls_through_to_slug() {
        assert_eq!(
            issue_branch_tail("7", "Fix the bug", Some("   ")),
            "7-fix-the-bug"
        );
        assert_eq!(issue_branch_tail("7", "Fix the bug", None), "7-fix-the-bug");
    }

    #[test]
    fn collapses_runs_and_trims_dashes() {
        // Punctuation runs collapse to single dashes; leading/trailing trimmed.
        assert_eq!(
            issue_branch_tail("ABC-9", "!!Hello,   World!!", None),
            "abc-9-hello-world"
        );
    }

    #[test]
    fn non_ascii_becomes_dashes() {
        // Non-ASCII-alphanumerics are treated as separators.
        assert_eq!(issue_branch_tail("1", "café über", None), "1-caf-ber");
    }

    #[test]
    fn caps_at_48_chars() {
        let long = "a".repeat(100);
        let tail = issue_branch_tail("1", &long, None);
        assert_eq!(tail.chars().count(), 48);
        assert!(tail.starts_with("1-aaaa"));
    }
}
