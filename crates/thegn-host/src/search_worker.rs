//! The workspace search worker (THE-5): off the event loop, streamed in bounded
//! batches, and cancellable via a shared generation token.
//!
//! [`spawn_search`] runs the whole walk+scan inside `spawn_blocking`, sends
//! bounded [`SearchBatch`]es over an unbounded channel, and pulses the
//! `TerminalWaker` per batch — the 0%-idle streaming pattern. It observes
//! cancellation **between files** by comparing its own generation to the shared
//! `current` token, so an abandoned search (the query was edited, or the overlay
//! closed) stops consuming CPU at the next file boundary rather than running to
//! completion. The overlay additionally discards any stale-generation batch at
//! the drain.
//!
//! [`search_collect`] is the synchronous sibling the CLI uses (same walk+scan,
//! no channel).
//!
//! File selection is the `ignore` crate walker: `.git/` is always excluded, and
//! symlinks are never followed (no escape out of the worktree). Globs and the
//! gitignore/hidden toggles come from [`WalkFilter`].

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ignore::WalkBuilder;
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc::UnboundedSender;

use thegn_core::search_replace::{Match, Matcher, SearchSpec, WalkFilter, scan_content};

/// How many matches to accumulate before flushing a batch to the loop.
const BATCH: usize = 64;
/// Skip files larger than this — search & replace is textual; a huge blob is
/// almost always a binary/artifact and would balloon memory.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// A NUL in this prefix window ⇒ treat the file as binary and skip it.
const BINARY_SNIFF: usize = 8_192;

/// One streamed batch of results, tagged with the generation it belongs to.
pub struct SearchBatch {
    pub sg: u64,
    pub matches: Vec<Match>,
    /// This is the final batch for `sg`.
    pub done: bool,
    /// The result cap was hit — more matches exist than are shown.
    pub truncated: bool,
}

/// Walk `root` honoring `filter`, invoking `on_file(rel, content)` for each
/// selected, readable, text file. `on_file` returns `false` to stop the walk;
/// `cancelled` is polled between files so a superseded search stops early.
fn walk<F, C>(root: &Path, filter: &WalkFilter, mut on_file: F, cancelled: C)
where
    F: FnMut(String, String) -> bool,
    C: Fn() -> bool,
{
    let walker = WalkBuilder::new(root)
        // `hidden(true)` means *skip* hidden entries.
        .hidden(!filter.include_hidden)
        .git_ignore(filter.respect_gitignore)
        .git_global(filter.respect_gitignore)
        .git_exclude(filter.respect_gitignore)
        .ignore(filter.respect_gitignore)
        .parents(filter.respect_gitignore)
        // Never follow symlinks — a link out of the worktree is not descended.
        .follow_links(false)
        // `.git/` is always excluded, even with include_hidden.
        .filter_entry(|e| e.file_name() != std::ffi::OsStr::new(".git"))
        .build();

    for entry in walker {
        if cancelled() {
            return;
        }
        let Ok(entry) = entry else { continue };
        // Only regular files: with follow_links(false), a symlink's file_type is
        // the symlink itself, so `is_file()` is false — symlinks are skipped.
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let Ok(relp) = path.strip_prefix(root) else {
            continue;
        };
        let rel = relp.to_string_lossy().replace('\\', "/");
        if !filter.path_selected(&rel) {
            continue;
        }
        // Size cap.
        if let Ok(meta) = entry.metadata()
            && meta.len() > MAX_FILE_BYTES
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // Binary sniff.
        if bytes.iter().take(BINARY_SNIFF).any(|&b| b == 0) {
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        if !on_file(rel, content) {
            return;
        }
    }
}

/// Synchronous walk+scan for the CLI. Returns `(matches, truncated)`; an invalid
/// regex is a caller-visible error. Blocking.
pub fn search_collect(
    root: &Path,
    spec: &SearchSpec,
    filter: &WalkFilter,
    max_results: usize,
) -> Result<(Vec<Match>, bool), String> {
    let matcher = Matcher::build(spec)?;
    let mut all: Vec<Match> = Vec::new();
    let mut truncated = false;
    walk(
        root,
        filter,
        |rel, content| {
            let remaining = if max_results == 0 {
                0
            } else {
                max_results.saturating_sub(all.len())
            };
            if max_results != 0 && remaining == 0 {
                truncated = true;
                return false;
            }
            let hits = scan_content(&rel, &content, &matcher, remaining);
            all.extend(hits);
            if max_results != 0 && all.len() >= max_results {
                truncated = true;
                return false;
            }
            true
        },
        || false,
    );
    if max_results != 0 && all.len() > max_results {
        all.truncate(max_results);
        truncated = true;
    }
    Ok((all, truncated))
}

/// Spawn the streamed, cancellable search worker (the overlay path). `sg` is
/// this search's generation; `current` is the shared token the overlay bumps on
/// every query/option edit — the worker stops as soon as they differ.
#[allow(clippy::too_many_arguments)]
pub fn spawn_search(
    root: std::path::PathBuf,
    spec: SearchSpec,
    filter: WalkFilter,
    sg: u64,
    current: Arc<AtomicU64>,
    max_results: usize,
    tx: UnboundedSender<SearchBatch>,
    waker: TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        let matcher = match Matcher::build(&spec) {
            Ok(m) => m,
            Err(_) => {
                // An invalid spec shouldn't reach here (the overlay validates),
                // but never leave the spinner hanging: signal an empty done.
                let _ = tx.send(SearchBatch {
                    sg,
                    matches: Vec::new(),
                    done: true,
                    truncated: false,
                });
                let _ = waker.wake();
                return;
            }
        };
        let is_stale = || current.load(Ordering::Acquire) != sg;
        let mut batch: Vec<Match> = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;

        walk(
            &root,
            &filter,
            |rel, content| {
                if is_stale() {
                    return false;
                }
                let remaining = if max_results == 0 {
                    0
                } else {
                    max_results.saturating_sub(total)
                };
                if max_results != 0 && remaining == 0 {
                    truncated = true;
                    return false;
                }
                let hits = scan_content(&rel, &content, &matcher, remaining);
                total += hits.len();
                batch.extend(hits);
                if batch.len() >= BATCH {
                    let b = std::mem::take(&mut batch);
                    if tx
                        .send(SearchBatch {
                            sg,
                            matches: b,
                            done: false,
                            truncated: false,
                        })
                        .is_err()
                    {
                        return false; // receiver gone
                    }
                    let _ = waker.wake();
                }
                if max_results != 0 && total >= max_results {
                    truncated = true;
                    return false;
                }
                true
            },
            is_stale,
        );

        // Don't emit a terminal `done` for a superseded search — the newer
        // generation owns the surface now.
        if is_stale() {
            return;
        }
        let _ = tx.send(SearchBatch {
            sg,
            matches: std::mem::take(&mut batch),
            done: true,
            truncated,
        });
        let _ = waker.wake();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::search_replace::SearchMode;

    fn spec(q: &str) -> SearchSpec {
        SearchSpec {
            query: q.into(),
            mode: SearchMode::Literal,
            case_sensitive: false,
            whole_word: false,
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "thegn-sw-{}-{}",
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
    fn collect_finds_matches_and_excludes_git() {
        let root = tmpdir();
        std::fs::write(root.join("a.txt"), "hello foo\nfoo again\n").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("config"), "foo secret\n").unwrap();
        let (matches, truncated) =
            search_collect(&root, &spec("foo"), &WalkFilter::default(), 0).unwrap();
        // Two hits in a.txt; nothing from `.git/`.
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|m| m.path == "a.txt"));
        assert!(!truncated);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn collect_truncates_at_max() {
        let root = tmpdir();
        std::fs::write(root.join("a.txt"), "x x x x x\n").unwrap();
        let (matches, truncated) =
            search_collect(&root, &spec("x"), &WalkFilter::default(), 2).unwrap();
        assert_eq!(matches.len(), 2);
        assert!(truncated);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn collect_skips_binary() {
        let root = tmpdir();
        std::fs::write(root.join("bin"), b"foo\x00\x01bar").unwrap();
        std::fs::write(root.join("txt"), "foo\n").unwrap();
        let (matches, _) = search_collect(&root, &spec("foo"), &WalkFilter::default(), 0).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "txt");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn collect_glob_include_exclude() {
        let root = tmpdir();
        std::fs::write(root.join("a.rs"), "foo\n").unwrap();
        std::fs::write(root.join("a.md"), "foo\n").unwrap();
        let filter = WalkFilter {
            include_globs: vec!["*.rs".into()],
            ..WalkFilter::default()
        };
        let (matches, _) = search_collect(&root, &spec("foo"), &filter, 0).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "a.rs");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn collect_invalid_regex_errors() {
        let root = tmpdir();
        let bad = SearchSpec {
            query: "(".into(),
            mode: SearchMode::Regex,
            case_sensitive: true,
            whole_word: false,
        };
        assert!(search_collect(&root, &bad, &WalkFilter::default(), 0).is_err());
        std::fs::remove_dir_all(&root).ok();
    }
}
