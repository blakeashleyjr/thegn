//! Jobs-section task-output scratch files (audit run.rs:14904).
//!
//! The Jobs `o` key dumps a task's captured output to a file and opens it in
//! `bat`. Task names come from repo files (package.json scripts, justfile
//! recipes) and are therefore attacker-controllable: a name with `/`, `..`,
//! spaces, or shell metacharacters could escape a predictable `/tmp` path,
//! follow a pre-planted symlink, or inject into the `bat` command line. This
//! module owns the two defenses — filename sanitization and a private per-user
//! scratch dir — extracted from the pinned `run.rs`.

/// Sanitize a repo-controlled task name into a single safe filename segment:
/// keep only `[A-Za-z0-9._-]`, collapse everything else to `_`, and never let
/// the result be empty or a `.`/`..` traversal.
pub(crate) fn safe_task_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        out = "task".to_string();
    }
    out
}

/// Per-user private scratch dir for task-output dumps, under XDG state (not the
/// world-writable `/tmp` symlink/clobber vector). Created 0700 so another local
/// user can't pre-plant a symlink at our target path.
pub(crate) fn task_output_dir() -> std::path::PathBuf {
    let dir = thegn_core::util::xdg_state_home().join("thegn/task-output");
    // best-effort: create the private dir; the caller's write surfaces failures.
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)); // best-effort: hardening: a failed chmod must never block the caller
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_task_filename_neutralizes_traversal_and_metachars() {
        // Path separators must NOT escape the scratch dir: a repo script named
        // "build/../../etc/passwd" collapses to a single flat filename segment,
        // so `Path::join` can never climb out of the private dir. (Embedded dots
        // are harmless without a surrounding separator; a *standalone* `..` is
        // rewritten to "task" — covered below.)
        let evil = safe_task_filename("build/../..//home/user/x");
        assert!(!evil.contains('/'), "no path separators survive: {evil}");
        assert!(
            !evil.contains(std::path::MAIN_SEPARATOR),
            "no OS path separators survive: {evil}"
        );
        // Shell metacharacters and spaces collapse to `_`.
        let meta = safe_task_filename("test; rm -rf ~ `boom`");
        assert!(
            meta.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "only safe chars remain: {meta}"
        );
        // A benign name is preserved verbatim.
        assert_eq!(safe_task_filename("build-app.v2"), "build-app.v2");
        // Degenerate names never yield an empty or dot-traversal filename.
        assert_eq!(safe_task_filename(""), "task");
        assert_eq!(safe_task_filename("."), "task");
        assert_eq!(safe_task_filename(".."), "task");
    }
}
