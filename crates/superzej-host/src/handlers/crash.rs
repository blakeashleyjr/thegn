//! Pane fast-crash messaging, split out of the ratchet-capped `run.rs`.
//!
//! When a sole shell keeps crashing on startup the loop stops respawning it. A
//! sandbox/exec failure (e.g. a broken `--userns keep-id` podman exec) writes its
//! real error to the pane before dying; [`keeps_crashing_status`] surfaces that
//! captured tail so the failure is legible instead of a pane that just vanished.

/// The last non-blank line of a crashed pane's output tail, trimmed and
/// length-capped — the concrete reason to show the user. `None` when the pane
/// produced no usable output (fall back to the generic hint). Input is already
/// ANSI-stripped by the pane history ring.
pub(crate) fn crash_reason(tail: &str) -> Option<String> {
    tail.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().chars().take(200).collect())
}

/// Status shown when a sole shell keeps crashing on startup: names the real error
/// when one was captured, else the generic backend/shell hint.
pub(crate) fn keeps_crashing_status(tail: &str) -> String {
    match crash_reason(tail) {
        Some(r) => {
            format!("Shell keeps crashing on startup — not respawning. Last error: {r}")
        }
        None => "Shell keeps crashing on startup — not respawning. \
                 Check your sandbox backend and shell config, \
                 then switch worktrees to retry."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_is_last_non_blank_line() {
        let tail = "starting\nError: crun: readlink: No such file or directory\n\n";
        assert_eq!(
            crash_reason(tail).as_deref(),
            Some("Error: crun: readlink: No such file or directory")
        );
    }

    #[test]
    fn reason_none_for_blank_tail() {
        assert_eq!(crash_reason("   \n\n"), None);
        assert_eq!(crash_reason(""), None);
    }

    #[test]
    fn reason_is_length_capped() {
        let long = "x".repeat(500);
        assert_eq!(crash_reason(&long).unwrap().chars().count(), 200);
    }

    #[test]
    fn status_names_error_when_present() {
        let s = keeps_crashing_status("boom: exec probe failed: crun error");
        assert!(
            s.contains("Last error: boom: exec probe failed: crun error"),
            "{s}"
        );
    }

    #[test]
    fn status_falls_back_to_generic_hint() {
        let s = keeps_crashing_status("");
        assert!(s.contains("Check your sandbox backend"), "{s}");
        assert!(!s.contains("Last error:"), "{s}");
    }
}
