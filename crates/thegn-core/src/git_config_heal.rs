//! No-follow, atomic, lock-serialised removal of a stray `core.worktree` from a
//! main checkout's `.git/config` (THE-371).
//!
//! git itself cannot repair this (it canonicalises the value on every config
//! read and aborts on a missing path), so the edit is textual — but it runs as
//! an automatic repair, so the write must be a proper transaction:
//!
//!  * `.git` must be a real directory (not a link) and `config` a real regular
//!    file (opened `O_NOFOLLOW`), both owned by the current user;
//!  * git's own lock protocol: `config.lock` is created exclusively, the new
//!    bytes go into it, and it is renamed over `config`. A concurrent
//!    `git config` therefore either finishes first (we re-read its result under
//!    the lock) or finds the lock held and fails cleanly — nothing is lost;
//!  * every byte outside the removed assignment is preserved exactly (CRLF,
//!    missing final newline, comments, non-UTF-8, continuations);
//!  * syntax the scanner cannot round-trip exactly is refused, never guessed.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Largest config we are willing to rewrite.
const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;

/// What the transaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripOutcome {
    /// No `[core] worktree` assignment (or no config): nothing written.
    Unchanged,
    /// The assignment was removed and the config atomically replaced.
    Stripped,
    /// The config was not touched and the repair is uncertain; callers must
    /// not proceed as if the repository identity were healthy.
    Refused(String),
}

fn refuse(msg: impl Into<String>) -> StripOutcome {
    StripOutcome::Refused(msg.into())
}

/// Removes the lock file on every exit path that did not publish it.
struct LockGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if self.armed {
            // best-effort: failure path cleanup; the original config is intact.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_config(path: &Path) -> Result<(Vec<u8>, std::fs::Metadata), String> {
    let mut file = crate::fsperm::open_regular_file_nofollow(path)
        .map_err(|e| format!("cannot open config without following links: {e}"))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !crate::fsperm::owned_by_current_user(&meta) {
        return Err("config is not owned by the current user".into());
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Err("config is too large to rewrite safely".into());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    Ok((bytes, meta))
}

fn gitdir_is_real_dir(gitdir: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(gitdir).map_err(|e| format!(".git: {e}"))?;
    if !meta.file_type().is_dir() {
        return Err(".git is not a real directory (link or file)".into());
    }
    if !crate::fsperm::owned_by_current_user(&meta) {
        return Err(".git is not owned by the current user".into());
    }
    match gitdir.canonicalize() {
        Ok(c) if c == gitdir => Ok(()),
        _ => Err(".git path resolves through a link".into()),
    }
}

/// Remove a stray `[core] worktree` from `<root>/.git/config` transactionally.
pub fn strip_stray_core_worktree(root: &Path) -> StripOutcome {
    let Ok(canon_root) = root.canonicalize() else {
        return StripOutcome::Unchanged;
    };
    let gitdir = canon_root.join(".git");
    let cfg = gitdir.join("config");
    if std::fs::symlink_metadata(&cfg).is_err() {
        return StripOutcome::Unchanged;
    }
    if let Err(e) = gitdir_is_real_dir(&gitdir) {
        return refuse(e);
    }
    // Unlocked pre-check: the overwhelmingly common case writes nothing and
    // takes no lock.
    let (bytes, _) = match read_config(&cfg) {
        Ok(v) => v,
        Err(e) => return refuse(e),
    };
    // Cheap gate: without the token there is nothing to repair, and unrelated
    // syntax elsewhere in the file must never turn into a refusal.
    if !contains_worktree_token(&bytes) {
        return StripOutcome::Unchanged;
    }
    match strip_core_worktree_bytes(&bytes) {
        Ok(None) => return StripOutcome::Unchanged,
        Ok(Some(_)) => {}
        Err(e) => return refuse(e),
    }

    let lock_path = gitdir.join("config.lock");
    let mut lock = match crate::fsperm::create_owner_only_exclusive(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            return refuse(format!(
                "config.lock unavailable (held by another writer?): {e}"
            ));
        }
    };
    let mut guard = LockGuard {
        path: lock_path.clone(),
        armed: true,
    };

    // Under the lock no cooperating writer can change the config; re-read the
    // bytes we will actually replace.
    let (source, meta) = match read_config(&cfg) {
        Ok(v) => v,
        Err(e) => return refuse(e),
    };
    let cleaned = match strip_core_worktree_bytes(&source) {
        Ok(Some(c)) => c,
        Ok(None) => return StripOutcome::Unchanged,
        Err(e) => return refuse(e),
    };
    if let Err(e) = lock
        .write_all(&cleaned)
        .and_then(|()| lock.sync_all())
        .and_then(|()| std::fs::set_permissions(&lock_path, meta.permissions()))
    {
        return refuse(format!("writing config.lock failed: {e}"));
    }
    drop(lock);
    // Revalidate identity and content immediately before publishing; a
    // non-cooperating writer (no lock) that raced us aborts the repair.
    if let Err(e) = gitdir_is_real_dir(&gitdir) {
        return refuse(e);
    }
    match read_config(&cfg) {
        Ok((again, _)) if again == source => {}
        Ok(_) => return refuse("config changed during repair; aborted without overwriting"),
        Err(e) => return refuse(e),
    }
    if let Err(e) = crate::fsperm::rename_replace(&lock_path, &cfg) {
        return refuse(format!("replacing config failed: {e}"));
    }
    guard.armed = false;
    // best-effort: durability of the rename; the replace itself is atomic.
    if let Ok(dir) = std::fs::File::open(&gitdir) {
        let _ = dir.sync_all(); // best-effort: durability of the rename only
    }
    StripOutcome::Stripped
}

fn contains_worktree_token(bytes: &[u8]) -> bool {
    bytes
        .windows(8)
        .any(|w| w.eq_ignore_ascii_case(b"worktree"))
}

fn is_ws(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

fn trim_start(l: &[u8]) -> &[u8] {
    let n = l.iter().take_while(|b| is_ws(**b)).count();
    &l[n..]
}

fn ends_with_odd_backslash(content: &[u8]) -> bool {
    let n = content.iter().rev().take_while(|b| **b == b'\\').count();
    n % 2 == 1
}

/// `line` without its `\n` / `\r\n` terminator.
fn content_of(line: &[u8]) -> &[u8] {
    let l = line.strip_suffix(b"\n").unwrap_or(line);
    l.strip_suffix(b"\r").unwrap_or(l)
}

/// Drop every `[core]`-section `worktree` assignment (with its continuation
/// lines) from raw config bytes. `Ok(None)`: nothing to remove. `Err`: syntax
/// the scanner will not guess at, so the caller must leave the file alone.
pub(crate) fn strip_core_worktree_bytes(src: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut out = Vec::with_capacity(src.len());
    let mut in_core = false;
    let mut removed = false;
    let mut dropping_continuation = false;
    let mut in_continuation = false;
    let mut first = true;
    let mut ambiguous = false;
    for raw in src.split_inclusive(|b| *b == b'\n') {
        let content = content_of(raw);
        let scan = if first {
            content.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(content)
        } else {
            content
        };
        first = false;
        let t = trim_start(scan);
        let is_comment = matches!(t.first(), Some(b'#' | b';'));
        let has_comment_char = t.iter().any(|b| matches!(b, b'#' | b';'));
        let continues = !is_comment && ends_with_odd_backslash(content);
        if continues && has_comment_char {
            // Only fatal if we end up editing this file (see below).
            ambiguous = true;
        }
        if in_continuation {
            // Body of a multi-line value: never a header or key.
            in_continuation = continues;
            if dropping_continuation {
                dropping_continuation = continues;
                continue;
            }
            out.extend_from_slice(raw);
            continue;
        }
        if t.first() == Some(&b'[') {
            let close = t.iter().position(|b| *b == b']');
            let Some(close) = close else {
                ambiguous = true;
                in_core = false;
                in_continuation = continues;
                out.extend_from_slice(raw);
                continue;
            };
            let head = t[1..close].trim_ascii();
            let rest = trim_start(&t[close + 1..]);
            in_core = head.eq_ignore_ascii_case(b"core");
            if in_core && !rest.is_empty() && !matches!(rest.first(), Some(b'#' | b';')) {
                // `[core] worktree = x` on one line.
                if rest
                    .to_ascii_lowercase()
                    .windows(8)
                    .any(|w| w == b"worktree")
                {
                    return Err("core.worktree shares a line with its section header".into());
                }
            }
            in_continuation =
                continues && !rest.is_empty() && !matches!(rest.first(), Some(b'#' | b';'));
            out.extend_from_slice(raw);
            continue;
        }
        if in_core && !is_comment {
            let key_len = t
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || **b == b'-')
                .count();
            let key = &t[..key_len];
            let after = trim_start(&t[key_len..]);
            let is_assign = after.is_empty() || matches!(after.first(), Some(b'=' | b'#' | b';'));
            if key.eq_ignore_ascii_case(b"worktree") && is_assign {
                removed = true;
                dropping_continuation = continues;
                in_continuation = continues;
                continue;
            }
        }
        in_continuation = continues;
        dropping_continuation = false;
        out.extend_from_slice(raw);
    }
    if removed && ambiguous {
        return Err("ambiguous syntax alongside core.worktree".into());
    }
    Ok(removed.then_some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &[u8]) -> Option<Vec<u8>> {
        strip_core_worktree_bytes(s).unwrap()
    }

    #[test]
    fn preserves_crlf_and_missing_final_newline() {
        let src = b"[core]\r\n\tbare = false\r\n\tworktree = /x\r\n[a]\r\n\tk = v";
        assert_eq!(
            strip(src).unwrap(),
            b"[core]\r\n\tbare = false\r\n[a]\r\n\tk = v".to_vec()
        );
    }

    #[test]
    fn preserves_non_utf8_comments_and_subsections() {
        let src =
            b"# \xff\xfe comment\n[core]\n\tworktree = /x\n[core \"s\"]\n\tworktree = keep\n; end";
        assert_eq!(
            strip(src).unwrap(),
            b"# \xff\xfe comment\n[core]\n[core \"s\"]\n\tworktree = keep\n; end".to_vec()
        );
    }

    #[test]
    fn duplicate_core_sections_and_keys_all_stripped() {
        let src = b"[core]\nworktree=/a\n[x]\n[CORE]\n WorkTree = /b\nbare = no\n";
        assert_eq!(
            strip(src).unwrap(),
            b"[core]\n[x]\n[CORE]\nbare = no\n".to_vec()
        );
    }

    #[test]
    fn continuation_lines_follow_the_dropped_assignment() {
        let src = b"[core]\nworktree = /a\\\n[not-a-header]\nbare = true\n";
        assert_eq!(strip(src).unwrap(), b"[core]\nbare = true\n".to_vec());
    }

    #[test]
    fn continuation_of_other_value_is_not_parsed() {
        // The `worktree` text sits inside another key's continuation.
        let src = b"[core]\nexcludesfile = a\\\nworktree = b\n";
        assert!(strip(src).is_none());
    }

    #[test]
    fn similar_keys_and_comments_untouched() {
        let src = b"[core]\n# worktree = /x\nworktreefoo = 1\n";
        assert!(strip(src).is_none());
    }

    #[test]
    fn same_line_header_assignment_is_refused() {
        assert!(strip_core_worktree_bytes(b"[core] worktree = /x\n").is_err());
        // Ambiguity is only fatal when a core.worktree edit is involved.
        assert!(strip_core_worktree_bytes(b"[core\n").unwrap().is_none());
        assert!(
            strip_core_worktree_bytes(b"[core]\nk = v # c \\\n")
                .unwrap()
                .is_none()
        );
        assert!(strip_core_worktree_bytes(b"[core]\nk = v # c \\\n\nworktree = /x\n").is_err());
        assert_eq!(
            strip(b"[ core ]\nworktree = /x\n").unwrap(),
            b"[ core ]\n".to_vec()
        );
    }

    #[test]
    fn bom_header_is_recognised() {
        let src = b"\xEF\xBB\xBF[core]\nworktree = /x\n";
        assert_eq!(strip(src).unwrap(), b"\xEF\xBB\xBF[core]\n".to_vec());
    }

    #[cfg(unix)]
    mod fs {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        fn repo(cfg: &[u8]) -> tempfile::TempDir {
            let d = tempfile::tempdir().unwrap();
            std::fs::create_dir(d.path().join(".git")).unwrap();
            std::fs::write(d.path().join(".git/config"), cfg).unwrap();
            d
        }

        #[test]
        fn repairs_atomically_preserving_mode() {
            let d = repo(b"[core]\r\n\tworktree = /no/such\r\n\tbare = false");
            let cfg = d.path().join(".git/config");
            std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o640)).unwrap();
            assert_eq!(strip_stray_core_worktree(d.path()), StripOutcome::Stripped);
            assert_eq!(std::fs::read(&cfg).unwrap(), b"[core]\r\n\tbare = false");
            assert_eq!(
                std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert!(!d.path().join(".git/config.lock").exists());
            assert_eq!(strip_stray_core_worktree(d.path()), StripOutcome::Unchanged);
        }

        #[test]
        fn config_symlink_is_refused_and_target_untouched() {
            let d = repo(b"");
            let ext = d.path().join("external");
            let body = b"[core]\nworktree = /x\n";
            std::fs::write(&ext, body).unwrap();
            std::fs::remove_file(d.path().join(".git/config")).unwrap();
            symlink(&ext, d.path().join(".git/config")).unwrap();
            assert!(matches!(
                strip_stray_core_worktree(d.path()),
                StripOutcome::Refused(_)
            ));
            assert_eq!(std::fs::read(&ext).unwrap(), body);
        }

        #[test]
        fn dot_git_symlink_is_refused() {
            let d = tempfile::tempdir().unwrap();
            let real = tempfile::tempdir().unwrap();
            let body = b"[core]\nworktree = /x\n";
            std::fs::write(real.path().join("config"), body).unwrap();
            symlink(real.path(), d.path().join(".git")).unwrap();
            assert!(matches!(
                strip_stray_core_worktree(d.path()),
                StripOutcome::Refused(_)
            ));
            assert_eq!(std::fs::read(real.path().join("config")).unwrap(), body);
        }

        #[test]
        fn config_directory_is_refused() {
            let d = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(d.path().join(".git/config")).unwrap();
            assert!(matches!(
                strip_stray_core_worktree(d.path()),
                StripOutcome::Refused(_)
            ));
        }

        #[test]
        fn held_lock_refuses_and_keeps_config() {
            let body = b"[core]\nworktree = /x\n";
            let d = repo(body);
            std::fs::write(d.path().join(".git/config.lock"), b"held").unwrap();
            assert!(matches!(
                strip_stray_core_worktree(d.path()),
                StripOutcome::Refused(_)
            ));
            assert_eq!(std::fs::read(d.path().join(".git/config")).unwrap(), body);
            assert_eq!(
                std::fs::read(d.path().join(".git/config.lock")).unwrap(),
                b"held"
            );
        }

        #[test]
        fn unparseable_config_is_untouched() {
            let body = b"[core] worktree = /x\n";
            let d = repo(body);
            assert!(matches!(
                strip_stray_core_worktree(d.path()),
                StripOutcome::Refused(_)
            ));
            assert_eq!(std::fs::read(d.path().join(".git/config")).unwrap(), body);
        }

        #[test]
        fn unrelated_ambiguous_syntax_is_unchanged_not_refused() {
            let body = b"[alias]\n\tx = !a ; b \\\n\t c\n[core]\n\tbare = false\n";
            let d = repo(body);
            assert_eq!(strip_stray_core_worktree(d.path()), StripOutcome::Unchanged);
            assert_eq!(std::fs::read(d.path().join(".git/config")).unwrap(), body);
        }

        #[test]
        fn missing_config_is_unchanged() {
            let d = tempfile::tempdir().unwrap();
            std::fs::create_dir(d.path().join(".git")).unwrap();
            assert_eq!(strip_stray_core_worktree(d.path()), StripOutcome::Unchanged);
        }
    }
}
