//! Streaming file finder: ripgrep's `ignore` crate walks the worktree in
//! parallel (respecting `.gitignore`), pushing each file into a cloned nucleo
//! injector from worker threads. First results appear within a frame; huge
//! repos fill in progressively without ever blocking the UI.

use crate::palette::engine::Engine;
use crate::palette::item::{Action, Row, RowKind};
use crate::theme;
use ignore::{WalkBuilder, WalkState};
use nucleo::Injector;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Walk `root` on a background thread, streaming file rows into `inj`. Stops
/// promptly when `cancel` flips (e.g. the user leaves File mode).
pub fn spawn(root: PathBuf, inj: Injector<Row>, cancel: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build_parallel();

        walker.run(|| {
            let inj = inj.clone();
            let cancel = cancel.clone();
            let root = root.clone();
            Box::new(move |result| {
                if cancel.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                if let Ok(entry) = result {
                    if entry.file_type().is_some_and(|t| t.is_file()) {
                        let path = entry.into_path();
                        Engine::push(&inj, file_row(&root, path));
                    }
                }
                WalkState::Continue
            })
        });
    });
}

fn file_row(root: &Path, path: PathBuf) -> Row {
    let rel = path.strip_prefix(root).unwrap_or(&path);
    let rel_str = rel.to_string_lossy().into_owned();
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel_str.clone());
    let parent = rel
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    Row {
        glyph: "≡".into(),
        hue: theme::BLUE,
        label: name,
        detail: parent,
        haystack: rel_str,
        kind: RowKind::File,
        action: Action::OpenFile(path.clone()),
        frecency_key: None,
        preview_path: Some(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::engine::Engine;
    use crate::palette::testutil;

    fn settle(e: &mut Engine) {
        for _ in 0..200 {
            e.tick();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
    }

    #[test]
    fn walk_streams_files_into_the_engine() {
        let repo = testutil::temp_git_repo("file-walk");
        let mut e = Engine::new();
        spawn(repo, e.injector(), Arc::new(AtomicBool::new(false)));
        settle(&mut e);
        let labels: Vec<String> = e.rows(50).into_iter().map(|r| r.label).collect();
        assert!(labels.iter().any(|l| l == "README.md"), "got {labels:?}");
    }

    #[test]
    fn preset_cancel_stops_the_walk() {
        let repo = testutil::temp_git_repo("file-walk-cancel");
        let mut e = Engine::new();
        // Cancelled before it starts: the parallel walker quits on first visit.
        spawn(repo, e.injector(), Arc::new(AtomicBool::new(true)));
        settle(&mut e);
        assert!(e.total() < 100); // bounded; the point is it doesn't hang
    }
}
