//! A thin wrapper over nucleo: a single-column fuzzy matcher over `Row`s.
//!
//! Items are injected (possibly from background worker threads via a cloned
//! `Injector`), the pattern is reset on each keystroke, and matching runs on
//! nucleo's own threadpool — so neither injection nor matching ever blocks the
//! render thread. The UI reads `rows()` from the latest snapshot each frame.

use super::item::Row;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Injector, Nucleo};
use std::sync::Arc;

pub struct Engine {
    nucleo: Nucleo<Row>,
    last_query: String,
    /// Set after a `restart()` so the next `set_query` always rescans (the new,
    /// empty item set has no relation to the previous pattern state).
    force_reparse: bool,
}

impl Engine {
    pub fn new() -> Engine {
        // One column (the row haystack). No notify wakeup needed: the App polls
        // `tick()` on a timer, which is simpler and keeps the worker decoupled.
        let nucleo = Nucleo::new(Config::DEFAULT, Arc::new(|| {}), None, 1);
        Engine {
            nucleo,
            last_query: String::new(),
            force_reparse: true,
        }
    }

    /// A cloneable, `Send` handle for pushing rows — hand one to a worker thread.
    pub fn injector(&self) -> Injector<Row> {
        self.nucleo.injector()
    }

    /// Push a row into the matcher; its `haystack` becomes the match column.
    pub fn push(inj: &Injector<Row>, row: Row) {
        inj.push(row, |r, cols| {
            cols[0] = r.haystack.clone().into();
        });
    }

    /// Clear all items and detach existing injectors (the next source repopulates).
    pub fn restart(&mut self) {
        self.nucleo.restart(true);
        self.last_query.clear();
        self.force_reparse = true;
    }

    /// Set the fuzzy pattern. Uses nucleo's append fast-path when the new query
    /// extends the previous one (a normal keystroke).
    pub fn set_query(&mut self, query: &str) {
        if !self.force_reparse && query == self.last_query {
            return;
        }
        let append = !self.force_reparse
            && !self.last_query.is_empty()
            && query.starts_with(&self.last_query);
        self.nucleo
            .pattern
            .reparse(0, query, CaseMatching::Smart, Normalization::Smart, append);
        self.last_query = query.to_string();
        self.force_reparse = false;
    }

    /// Advance the worker; returns whether the snapshot changed (needs a redraw).
    pub fn tick(&mut self) -> bool {
        self.nucleo.tick(10).changed
    }

    /// The top `max` matched rows, best-scored first.
    pub fn rows(&self, max: usize) -> Vec<Row> {
        let snap = self.nucleo.snapshot();
        let n = (snap.matched_item_count() as usize).min(max) as u32;
        snap.matched_items(0..n).map(|it| it.data.clone()).collect()
    }

    /// Total matched rows (may exceed what `rows()` returns).
    pub fn total(&self) -> usize {
        self.nucleo.snapshot().matched_item_count() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::item::{Action, Row};

    fn row(label: &str) -> Row {
        Row::command("x", crate::theme::TEAL, label, "", Action::Dashboard, label)
    }

    /// Tick until the background matcher settles (or a generous bound elapses).
    fn settle(e: &mut Engine) {
        for _ in 0..200 {
            let running = {
                // tick() returns `changed`; also peek `running` via another tick.
                e.tick();
                e.nucleo.tick(10).running
            };
            if !running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn empty_query_matches_all_in_insertion_order() {
        let mut e = Engine::new();
        let inj = e.injector();
        for l in ["alpha", "beta", "gamma"] {
            Engine::push(&inj, row(l));
        }
        e.set_query("");
        settle(&mut e);
        assert_eq!(e.total(), 3);
        let labels: Vec<String> = e.rows(10).into_iter().map(|r| r.label).collect();
        assert_eq!(labels, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn fuzzy_query_filters_and_ranks() {
        let mut e = Engine::new();
        let inj = e.injector();
        for l in ["alpha", "beta", "alabaster"] {
            Engine::push(&inj, row(l));
        }
        e.set_query("alp");
        settle(&mut e);
        let labels: Vec<String> = e.rows(10).into_iter().map(|r| r.label).collect();
        assert!(labels.contains(&"alpha".to_string()));
        assert!(!labels.contains(&"beta".to_string()));
    }

    #[test]
    fn restart_clears_items() {
        let mut e = Engine::new();
        let inj = e.injector();
        Engine::push(&inj, row("alpha"));
        e.set_query("");
        settle(&mut e);
        assert_eq!(e.total(), 1);

        e.restart();
        e.set_query("");
        settle(&mut e);
        assert_eq!(e.total(), 0);
    }

    #[test]
    fn rows_is_bounded_by_max() {
        let mut e = Engine::new();
        let inj = e.injector();
        for i in 0..50 {
            Engine::push(&inj, row(&format!("item{i}")));
        }
        e.set_query("");
        settle(&mut e);
        assert_eq!(e.rows(5).len(), 5);
        assert_eq!(e.total(), 50);
    }
}
