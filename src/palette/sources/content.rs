//! Embedded content search — ripgrep's own engine as a library. `grep-regex`
//! builds a smart-case matcher; `grep-searcher` scans each file the `ignore`
//! walker yields, on a background thread, streaming each hit into the injector.
//! A cancel flag + a total cap keep it bounded and instantly replaceable when
//! the query changes.

use crate::palette::engine::Engine;
use crate::palette::item::{Action, Row, RowKind};
use crate::theme;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use ignore::WalkBuilder;
use nucleo::Injector;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Cap on total hits streamed for a single query (keeps the UI bounded). Hitting
/// it is logged by the caller's footer via `total()` vs this constant.
pub const MAX_HITS: usize = 2000;

/// Run a content search on a background thread, streaming match rows into `inj`.
/// `cancel` flips when the query changes or the user leaves Content mode.
pub fn spawn(root: PathBuf, query: String, inj: Injector<Row>, cancel: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // Smart-case regex; if the query isn't a valid regex, treat it literally.
        let matcher = match RegexMatcherBuilder::new()
            .case_smart(true)
            .build(&query)
            .or_else(|_| {
                RegexMatcherBuilder::new()
                    .case_smart(true)
                    .build(&regex_escape(&query))
            }) {
            Ok(m) => m,
            Err(_) => return,
        };

        let count = Arc::new(AtomicUsize::new(0));
        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(true)
            .build();

        for result in walker {
            if cancel.load(Ordering::Relaxed) || count.load(Ordering::Relaxed) >= MAX_HITS {
                break;
            }
            let Ok(entry) = result else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.into_path();
            let mut searcher = SearcherBuilder::new().line_number(true).build();
            let (inj, cancel, count, root) =
                (inj.clone(), cancel.clone(), count.clone(), root.clone());
            let path_for_sink = path.clone();
            let _ = searcher.search_path(
                &matcher,
                &path,
                UTF8(move |lnum, line| {
                    if cancel.load(Ordering::Relaxed) || count.load(Ordering::Relaxed) >= MAX_HITS {
                        return Ok(false);
                    }
                    count.fetch_add(1, Ordering::Relaxed);
                    Engine::push(
                        &inj,
                        content_row(&root, &path_for_sink, lnum as usize, line.trim_end()),
                    );
                    Ok(true)
                }),
            );
        }
    });
}

fn content_row(root: &Path, path: &Path, lnum: usize, line: &str) -> Row {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel_str = rel.to_string_lossy().into_owned();
    Row {
        glyph: "⌕".into(),
        hue: theme::AMBER,
        label: line.trim_start().to_string(),
        detail: format!("{rel_str}:{lnum}"),
        haystack: format!("{line} {rel_str}"),
        kind: RowKind::Content,
        action: Action::OpenFileAt(path.to_path_buf(), lnum),
        frecency_key: None,
        preview_path: Some(path.to_path_buf()),
    }
}

/// Escape regex metacharacters so a non-regex query searches literally.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::testutil;

    fn settle(e: &mut Engine) {
        for _ in 0..300 {
            e.tick();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
    }

    #[test]
    fn escape_quotes_regex_metachars() {
        assert_eq!(regex_escape("a(b)"), "a\\(b\\)");
        assert_eq!(regex_escape("plain"), "plain");
    }

    #[test]
    fn invalid_regex_falls_back_to_literal_search() {
        testutil::sandbox();
        // Build a file containing a literal "(" so the escaped-literal fallback
        // (an unbalanced "(" is invalid regex) finds a hit.
        let dir = testutil::sandbox().join("content-src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "needle(here\n").unwrap();

        let mut e = Engine::new();
        let cancel = Arc::new(AtomicBool::new(false));
        spawn(dir.clone(), "needle(".into(), e.injector(), cancel);
        settle(&mut e);
        assert!(e.total() >= 1, "literal fallback should find the '(' line");
    }

    #[test]
    fn preset_cancel_stops_immediately() {
        testutil::sandbox();
        let dir = testutil::sandbox().join("content-src2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "alpha\nbeta\n").unwrap();

        let mut e = Engine::new();
        let cancel = Arc::new(AtomicBool::new(true)); // already cancelled
        spawn(dir.clone(), "alpha".into(), e.injector(), cancel);
        settle(&mut e);
        assert_eq!(e.total(), 0, "a pre-cancelled search emits nothing");
    }
}
