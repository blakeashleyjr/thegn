//! Worktree-scoped filesystem access for the agent's ACP fs tools
//! (`fs/read_text_file`, `write`, `edit`). Extracted from `run.rs` (file-size
//! ratchet). Every path is confined to the worktree so an agent cannot read or
//! write outside its sandbox mount; the containment check canonicalizes the
//! deepest existing ancestor so an in-worktree symlink can't be used to escape.

/// Read a file requested by an agent over ACP `fs/read_text_file`, scoping the
/// resolved path to the worktree so the agent cannot read outside its sandbox
/// mount. Relative paths resolve against the worktree root.
pub(crate) fn read_scoped_file(worktree: &str, path: &str) -> Result<String, String> {
    let base = std::path::Path::new(worktree);
    let req = std::path::Path::new(path);
    let full = if req.is_absolute() {
        req.to_path_buf()
    } else {
        base.join(req)
    };
    let canon = full.canonicalize().map_err(|e| format!("{path}: {e}"))?;
    let base_canon = base
        .canonicalize()
        .map_err(|e| format!("{worktree}: {e}"))?;
    if !canon.starts_with(&base_canon) {
        return Err(format!("path escapes worktree: {path}"));
    }
    std::fs::read_to_string(&canon).map_err(|e| format!("{path}: {e}"))
}

/// Resolve a (possibly not-yet-existing) write target against the worktree,
/// rejecting absolute escapes and any `..` traversal. Used by the agent's
/// `write`/`edit` tools so they cannot touch files outside their sandbox mount.
pub(crate) fn scoped_target(worktree: &str, path: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Component, Path};
    let base = Path::new(worktree)
        .canonicalize()
        .map_err(|e| format!("{worktree}: {e}"))?;
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    if joined
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!("path must not contain '..': {path}"));
    }
    // A lexical `starts_with` is symlink-traversable: a symlink *inside* the
    // worktree pointing outside (e.g. `link -> /etc`) makes `link/passwd` pass
    // the lexical check while resolving out of the sandbox mount. Canonicalize the
    // deepest existing ancestor (which resolves any symlink in the path prefix)
    // and require *that* to stay within the worktree; then reattach the
    // not-yet-created tail (already `..`-free) to the symlink-free base.
    let mut existing: &Path = joined.as_path();
    let canon_existing = loop {
        match existing.canonicalize() {
            Ok(c) => break c,
            Err(_) => match existing.parent() {
                Some(p) => existing = p,
                None => return Err(format!("path escapes worktree: {path}")),
            },
        }
    };
    if !canon_existing.starts_with(&base) {
        return Err(format!("path escapes worktree: {path}"));
    }
    let tail = joined
        .strip_prefix(existing)
        .map_err(|_| format!("path escapes worktree: {path}"))?;
    // When the target already exists, `tail` is empty; `join("")` would append a
    // trailing separator and turn a file path into a "directory" path.
    if tail.as_os_str().is_empty() {
        Ok(canon_existing)
    } else {
        Ok(canon_existing.join(tail))
    }
}

/// Write full file contents for an agent's `write` tool, scoped to the worktree.
pub(crate) fn write_scoped_file(worktree: &str, path: &str, content: &str) -> Result<(), String> {
    let target = scoped_target(worktree, path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{path}: {e}"))?;
    }
    std::fs::write(&target, content).map_err(|e| format!("{path}: {e}"))
}

/// Apply an agent `edit` tool's `[{oldText,newText}]` replacements to a file,
/// scoped to the worktree. Each `oldText` must occur (first match is replaced);
/// a missing match is an error so the agent can correct itself.
pub(crate) fn apply_scoped_edits(
    worktree: &str,
    path: &str,
    edits: &serde_json::Value,
) -> Result<(), String> {
    let target = scoped_target(worktree, path)?;
    let mut text = std::fs::read_to_string(&target).map_err(|e| format!("{path}: {e}"))?;
    let arr = edits.as_array().ok_or("edits must be an array")?;
    for edit in arr {
        let old = edit
            .get("oldText")
            .and_then(|v| v.as_str())
            .ok_or("edit missing oldText")?;
        let new = edit
            .get("newText")
            .and_then(|v| v.as_str())
            .ok_or("edit missing newText")?;
        match text.find(old) {
            Some(idx) => text.replace_range(idx..idx + old.len(), new),
            None => {
                let preview: String = old.chars().take(40).collect();
                return Err(format!("oldText not found in {path}: {preview:?}"));
            }
        }
    }
    std::fs::write(&target, text).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_scoped_file_reads_inside_and_rejects_escape() {
        let dir = std::env::temp_dir().join(format!("sz-acp-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "hi there").unwrap();
        let wt = dir.to_str().unwrap();

        // Relative path resolves against the worktree root.
        assert_eq!(read_scoped_file(wt, "hello.txt").unwrap(), "hi there");
        // Absolute path inside the worktree is allowed.
        let abs = dir.join("hello.txt");
        assert_eq!(
            read_scoped_file(wt, abs.to_str().unwrap()).unwrap(),
            "hi there"
        );
        // A path that escapes the worktree (via ..) is rejected, not read.
        assert!(
            read_scoped_file(wt, "../../../etc/passwd").is_err(),
            "path escape must be rejected"
        );
        // A missing file is an error, not a panic.
        assert!(read_scoped_file(wt, "nope.txt").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_write_and_edit_stay_inside_worktree() {
        let dir = std::env::temp_dir().join(format!("sz-acp-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wt = dir.to_str().unwrap();

        // Write creates the file (and parent dirs) inside the worktree.
        write_scoped_file(wt, "sub/new.txt", "hello").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(),
            "hello"
        );

        // Edit applies precise replacements; a missing match errors.
        let edits = serde_json::json!([{ "oldText": "hello", "newText": "goodbye" }]);
        apply_scoped_edits(wt, "sub/new.txt", &edits).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(),
            "goodbye"
        );
        let bad = serde_json::json!([{ "oldText": "absent", "newText": "x" }]);
        assert!(apply_scoped_edits(wt, "sub/new.txt", &bad).is_err());

        // Traversal escapes are rejected for both write and edit.
        assert!(write_scoped_file(wt, "../escape.txt", "x").is_err());
        assert!(apply_scoped_edits(wt, "../escape.txt", &edits).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn scoped_write_rejects_symlink_traversal_out_of_worktree() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("sz-acp-symlink-{}", std::process::id()));
        let wt = root.join("worktree");
        let outside = root.join("outside");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let wt_s = wt.to_str().unwrap();

        // A symlink inside the worktree pointing outside it: writing "escape/pwned"
        // must be rejected even though it contains no ".." and lexically starts with
        // the worktree root.
        symlink(&outside, wt.join("escape")).unwrap();
        assert!(
            write_scoped_file(wt_s, "escape/pwned", "x").is_err(),
            "symlink traversal out of the worktree must be rejected"
        );
        assert!(
            !outside.join("pwned").exists(),
            "file must not have been written outside the worktree"
        );

        // A symlink that stays inside the worktree is still fine.
        let inner = wt.join("real");
        std::fs::create_dir_all(&inner).unwrap();
        symlink(&inner, wt.join("innerlink")).unwrap();
        write_scoped_file(wt_s, "innerlink/ok.txt", "hi").unwrap();
        assert_eq!(std::fs::read_to_string(inner.join("ok.txt")).unwrap(), "hi");

        let _ = std::fs::remove_dir_all(&root);
    }
}
