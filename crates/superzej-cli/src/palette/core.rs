//! The palette's engine-facing brain: maps the parsed (mode, query) to result
//! rows, owning the nucleo `Engine` and the lifecycle of the streaming sources
//! (file walk, ripgrep). The iocraft component holds this behind an
//! `Arc<Mutex<_>>` and reads `rows()` each frame; background workers push into
//! the engine's injector independently of that lock.

use super::engine::Engine;
use super::frecency::{self, Scores};
use super::item::Row;
use super::mode::{Mode, Parsed};
use super::sources;
use crate::commands;
use crate::config::Config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Core {
    cfg: Config,
    worktree: PathBuf,
    engine: Engine,
    mode: Option<Mode>,
    last_query: String,
    scores: Scores,
    file_cancel: Arc<AtomicBool>,
    content_cancel: Arc<AtomicBool>,
}

impl Core {
    pub fn new(cfg: Config) -> Core {
        Core {
            worktree: commands::resolve_worktree(None),
            cfg,
            engine: Engine::new(),
            mode: None,
            last_query: String::new(),
            scores: frecency::load(),
            file_cancel: Arc::new(AtomicBool::new(false)),
            content_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Apply the latest input: switch source on mode change, re-search on query
    /// change. Cheap and idempotent — safe to call every frame.
    pub fn set_input(&mut self, parsed: &Parsed) {
        if self.mode != Some(parsed.mode) {
            self.mode = Some(parsed.mode);
            self.last_query = parsed.query.clone();
            self.enter_mode(parsed.mode, &parsed.query);
        } else if parsed.query != self.last_query {
            self.last_query = parsed.query.clone();
            match parsed.mode {
                Mode::Content => self.search_content(&parsed.query),
                _ => self.engine.set_query(&parsed.query),
            }
        }
    }

    /// Repopulate the engine for a freshly-entered mode.
    fn enter_mode(&mut self, mode: Mode, query: &str) {
        self.cancel_workers();
        if mode == Mode::Content {
            self.search_content(query);
            return;
        }
        self.engine.restart();
        let inj = self.engine.injector();
        match mode {
            Mode::Smart => {
                let mut rows = sources::command::rows(&self.cfg);
                rows.extend(sources::nav::rows());
                self.scores.sort(&mut rows);
                for r in rows {
                    Engine::push(&inj, r);
                }
            }
            Mode::Command => {
                let mut rows = sources::command::rows(&self.cfg);
                self.scores.sort(&mut rows);
                for r in rows {
                    Engine::push(&inj, r);
                }
            }
            Mode::Nav => {
                let mut rows = sources::nav::rows();
                self.scores.sort(&mut rows);
                for r in rows {
                    Engine::push(&inj, r);
                }
            }
            Mode::Git => {
                for r in sources::git::rows(&self.worktree) {
                    Engine::push(&inj, r);
                }
            }
            Mode::File => {
                let flag = Arc::new(AtomicBool::new(false));
                self.file_cancel = flag.clone();
                sources::file::spawn(self.worktree.clone(), inj, flag);
            }
            Mode::Content => unreachable!("handled above"),
        }
        self.engine.set_query(query);
    }

    /// (Re)start a ripgrep content search for `query`, cancelling any in-flight
    /// one. Empty pattern means nucleo passes every rg hit through in hit order.
    fn search_content(&mut self, query: &str) {
        self.content_cancel.store(true, Ordering::Relaxed);
        self.engine.restart();
        self.engine.set_query("");
        let q = query.trim();
        if q.len() >= 2 {
            let flag = Arc::new(AtomicBool::new(false));
            self.content_cancel = flag.clone();
            sources::content::spawn(
                self.worktree.clone(),
                q.to_string(),
                self.engine.injector(),
                flag,
            );
        }
    }

    fn cancel_workers(&self) {
        self.file_cancel.store(true, Ordering::Relaxed);
        self.content_cancel.store(true, Ordering::Relaxed);
    }

    /// Advance matching; true if the result snapshot changed (needs a redraw).
    pub fn tick(&mut self) -> bool {
        self.engine.tick()
    }

    /// Top `max` matched rows.
    pub fn rows(&self, max: usize) -> Vec<Row> {
        self.engine.rows(max)
    }

    pub fn total(&self) -> usize {
        self.engine.total()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::mode::parse;
    use crate::palette::testutil;

    /// Tick until the matcher settles with results (or a bound elapses).
    fn settle(c: &mut Core) {
        for _ in 0..400 {
            let changed = c.tick();
            if !changed && c.total() > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
    }

    #[test]
    fn command_mode_filters_the_catalog() {
        testutil::sandbox();
        let mut c = Core::new(Config::default());
        c.set_input(&parse(">tog"));
        settle(&mut c);
        let labels: Vec<String> = c.rows(50).into_iter().map(|r| r.label).collect();
        assert!(labels.iter().any(|l| l.contains("Toggle")));
        assert!(!labels.iter().any(|l| l.contains("lazygit")));
    }

    #[test]
    fn switching_modes_repopulates() {
        testutil::sandbox();
        let db = crate::db::Db::open().unwrap();
        db.put_worktree("r/feat-y", "/r", "/wt/feat-y", "feat/y", None)
            .unwrap();

        let mut c = Core::new(Config::default());
        c.set_input(&parse(">tog"));
        settle(&mut c);
        c.set_input(&parse("@")); // -> Nav
        settle(&mut c);
        let labels: Vec<String> = c.rows(200).into_iter().map(|r| r.label).collect();
        assert!(labels.iter().any(|l| l == "feat/y"));
    }

    #[test]
    fn content_search_ignores_too_short_queries() {
        testutil::sandbox();
        let mut c = Core::new(Config::default());
        c.set_input(&parse("/x")); // 1 char -> no search
        for _ in 0..30 {
            c.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(c.total(), 0);
    }

    #[test]
    fn content_search_finds_matches_in_the_repo() {
        testutil::sandbox();
        // Core resolves the worktree to the cwd, which is this crate's repo.
        let mut c = Core::new(Config::default());
        c.set_input(&parse("/RegexMatcherBuilder"));
        settle(&mut c);
        let hits = c.rows(50);
        assert!(!hits.is_empty(), "expected ripgrep hits");
        assert!(hits.iter().any(|r| r.detail.contains("content.rs")));
    }
}
