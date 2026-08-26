//! The single guarded write path for workspace search & replace (THE-5).
//!
//! Every replacement — from the overlay, the CLI, and the structural tier —
//! goes through [`apply`]. Per file it: rejects a path that would escape the
//! worktree root or touch `.git/`, re-reads the current bytes, runs the pure
//! drift-checked edit computation ([`apply_edits`] / [`apply_span_edits`], which
//! skip a match whose snapshot no longer holds), and — only if something
//! changed — writes atomically (temp-then-rename in the same directory,
//! permissions preserved). One file's failure (read-only worktree, permission
//! denied) is captured per-file; the batch never aborts.
//!
//! Blocking by contract — callers run it off the event loop (the overlay via
//! `spawn_blocking`, the CLI directly).

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use thegn_core::search_replace::{
    ApplyReport, Edit, FileApplyResult, SpanEdit, apply_edits, apply_span_edits,
};

/// One file's pending edits, in the shape its search tier produced. Both go
/// through the same guarded I/O; only the in-memory transform differs.
pub enum FileEdits {
    /// Line-relative edits (the textual literal/regex tier).
    Line(Vec<Edit>),
    /// File-absolute span edits (the structural / ast-grep tier).
    Span(Vec<SpanEdit>),
}

/// Apply every file's edits through the guarded path, returning a per-file
/// [`ApplyReport`]. Confines all writes to `root`.
pub fn apply(root: &Path, files: Vec<(String, FileEdits)>) -> ApplyReport {
    let mut report = ApplyReport::default();
    let canon_root = root.canonicalize().ok();
    for (rel, edits) in files {
        report.push(apply_one(root, canon_root.as_deref(), &rel, edits));
    }
    report
}

fn apply_one(
    root: &Path,
    canon_root: Option<&Path>,
    rel: &str,
    edits: FileEdits,
) -> FileApplyResult {
    let mut result = FileApplyResult {
        path: rel.to_string(),
        applied: 0,
        skipped_drift: 0,
        error: None,
    };

    // 1. Path confinement — reject escape and `.git/` before touching disk.
    if let Err(e) = reject_unsafe_rel(rel) {
        result.error = Some(e);
        return result;
    }
    let abs = root.join(rel);

    // 2. Re-read the current bytes (drift is decided against these).
    let content = match std::fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(e) => {
            result.error = Some(format!("read failed: {e}"));
            return result;
        }
    };

    // 3. Symlink-escape defense: the resolved real path must stay under root.
    if let (Some(canon_root), Ok(canon)) = (canon_root, abs.canonicalize())
        && !canon.starts_with(canon_root)
    {
        result.error = Some("path resolves outside the worktree root".to_string());
        return result;
    }

    // 4. Pure, drift-checked edit computation.
    let out = match &edits {
        FileEdits::Line(es) => apply_edits(&content, es),
        FileEdits::Span(es) => apply_span_edits(&content, es),
    };
    result.applied = out.applied;
    result.skipped_drift = out.skipped_drift;

    // 5. Write atomically only if something actually changed.
    if out.applied > 0
        && out.content != content
        && let Err(e) = atomic_write(&abs, out.content.as_bytes())
    {
        // The write failed (read-only worktree, permission denied): report it,
        // count nothing as applied, keep going.
        result.applied = 0;
        result.error = Some(format!("write failed: {e}"));
    }
    result
}

/// Reject a relative path that would escape the root or touch the git dir.
fn reject_unsafe_rel(rel: &str) -> Result<(), String> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err("absolute path refused".to_string());
    }
    for comp in p.components() {
        match comp {
            Component::ParentDir => return Err("`..` path component refused".to_string()),
            Component::Normal(c) if c == std::ffi::OsStr::new(".git") => {
                return Err("`.git/` is excluded".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute path refused".to_string());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Atomic write: a temp file in the same directory, permissions copied from the
/// target when it exists, then `rename` over the target. `rename` on the same
/// filesystem is atomic, so a crash never leaves a torn file.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = temp_path(parent, path);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // Preserve the target's permissions on the replacement.
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn temp_path(parent: &Path, target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        ".{name}.thegn-sr.{}.{nanos}.tmp",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::search_replace::{Edit, SpanEdit, fnv1a_64};

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "thegn-sr-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn applies_line_edits_atomically() {
        let root = tmpdir();
        std::fs::write(root.join("a.txt"), "foo bar foo\n").unwrap();
        let h = fnv1a_64(b"foo bar foo");
        let edits = vec![
            Edit {
                line: 1,
                byte_start: 0,
                byte_end: 3,
                content_hash: h,
                replacement: "X".into(),
            },
            Edit {
                line: 1,
                byte_start: 8,
                byte_end: 11,
                content_hash: h,
                replacement: "Y".into(),
            },
        ];
        let report = apply(&root, vec![("a.txt".into(), FileEdits::Line(edits))]);
        assert_eq!(report.total_applied(), 2);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "X bar Y\n"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn drift_is_skipped_and_file_untouched() {
        let root = tmpdir();
        std::fs::write(root.join("a.txt"), "current\n").unwrap();
        let stale = fnv1a_64(b"original");
        let edits = vec![Edit {
            line: 1,
            byte_start: 0,
            byte_end: 7,
            content_hash: stale,
            replacement: "X".into(),
        }];
        let report = apply(&root, vec![("a.txt".into(), FileEdits::Line(edits))]);
        assert_eq!(report.total_applied(), 0);
        assert_eq!(report.total_skipped(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "current\n"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_only_file_reports_and_batch_continues() {
        let root = tmpdir();
        let ro = root.join("ro.txt");
        std::fs::write(&ro, "foo\n").unwrap();
        let ok = root.join("ok.txt");
        std::fs::write(&ok, "foo\n").unwrap();
        // Make ro.txt read-only.
        let mut perms = std::fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro, perms).unwrap();
        // Also make its parent dir writable but the file itself unwritable; on
        // Unix rename into a dir needs dir write, which we have — so force the
        // failure by making the dir read-only would block ok.txt too. Instead we
        // rely on create(tmp)+rename; a readonly *file* still gets replaced by
        // rename on Unix. So assert the batch processes both and reports honestly
        // rather than asserting a specific errno.
        let h = fnv1a_64(b"foo");
        let e = |_p: &str| Edit {
            line: 1,
            byte_start: 0,
            byte_end: 3,
            content_hash: h,
            replacement: "BAR".into(),
        };
        let report = apply(
            &root,
            vec![
                ("ro.txt".into(), FileEdits::Line(vec![e("ro.txt")])),
                ("ok.txt".into(), FileEdits::Line(vec![e("ok.txt")])),
            ],
        );
        // ok.txt always applies; the batch never aborts regardless of ro.txt.
        assert_eq!(report.files.len(), 2);
        assert!(
            report
                .files
                .iter()
                .any(|f| f.path == "ok.txt" && f.applied == 1)
        );
        assert_eq!(std::fs::read_to_string(&ok).unwrap(), "BAR\n");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn span_edits_apply() {
        let root = tmpdir();
        std::fs::write(root.join("s.rs"), "log(x)\n").unwrap();
        let h = fnv1a_64(b"log(x)");
        let span = SpanEdit {
            byte_start: 0,
            byte_end: 6,
            content_hash: h,
            replacement: "debug(x)".into(),
        };
        let report = apply(&root, vec![("s.rs".into(), FileEdits::Span(vec![span]))]);
        assert_eq!(report.total_applied(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("s.rs")).unwrap(),
            "debug(x)\n"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_path_escape_and_git() {
        assert!(reject_unsafe_rel("../etc/passwd").is_err());
        assert!(reject_unsafe_rel("/etc/passwd").is_err());
        assert!(reject_unsafe_rel(".git/config").is_err());
        assert!(reject_unsafe_rel("src/.git/x").is_err());
        assert!(reject_unsafe_rel("src/main.rs").is_ok());
    }

    #[test]
    fn escape_via_edits_is_reported_not_written() {
        let root = tmpdir();
        let report = apply(
            &root,
            vec![(
                "../escape.txt".into(),
                FileEdits::Line(vec![Edit {
                    line: 1,
                    byte_start: 0,
                    byte_end: 1,
                    content_hash: 0,
                    replacement: "X".into(),
                }]),
            )],
        );
        assert_eq!(report.files.len(), 1);
        assert!(report.files[0].error.is_some());
        assert_eq!(report.total_applied(), 0);
        std::fs::remove_dir_all(&root).ok();
    }
}
