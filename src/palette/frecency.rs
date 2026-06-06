//! Frecency: rank commands and nav targets by how recently and often they were
//! chosen, so an empty query surfaces what you actually use. Rows are sorted
//! before injection, so frecency decides empty-query order while nucleo's score
//! takes over the moment you type.

use super::item::Row;
use crate::db::Db;
use std::collections::HashMap;

/// Usage snapshot: frecency key -> (count, last_used epoch secs).
pub struct Scores(HashMap<String, (i64, i64)>);

/// Load the usage table once (cheap; a handful of rows).
pub fn load() -> Scores {
    let map = Db::open()
        .ok()
        .and_then(|db| db.palette_usage().ok())
        .map(|v| v.into_iter().map(|(k, c, l)| (k, (c, l))).collect())
        .unwrap_or_default();
    Scores(map)
}

impl Scores {
    /// Sort key: most-recent first, then most-frequent. Unknown rows rank last
    /// but keep their original relative order (stable sort).
    fn rank(&self, row: &Row) -> (i64, i64) {
        row.frecency_key
            .as_ref()
            .and_then(|k| self.0.get(k))
            .map(|&(count, last)| (last, count))
            .unwrap_or((0, 0))
    }

    pub fn sort(&self, rows: &mut [Row]) {
        rows.sort_by_key(|r| std::cmp::Reverse(self.rank(r)));
    }
}

/// Record that a row was chosen (no-op for rows without a frecency key).
pub fn bump(row: &Row) {
    if let Some(key) = &row.frecency_key {
        if let Ok(db) = Db::open() {
            let _ = db.bump_palette_usage(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::item::{Action, Row};
    use crate::palette::testutil;

    fn row(key: Option<&str>) -> Row {
        Row {
            glyph: "x".into(),
            hue: crate::theme::TEAL,
            label: key.unwrap_or("none").into(),
            detail: String::new(),
            haystack: String::new(),
            kind: crate::palette::item::RowKind::Command,
            action: Action::Dashboard,
            frecency_key: key.map(String::from),
            preview_path: None,
        }
    }

    #[test]
    fn bump_then_load_ranks_used_rows_first() {
        testutil::sandbox();
        // A fresh, isolated usage table for this test's keys.
        let used = row(Some("frec:used"));
        let other = row(Some("frec:other"));
        let unused = row(Some("frec:unused"));
        bump(&other);
        bump(&used);
        bump(&used); // used is most recent and most frequent

        let scores = load();
        let mut rows = vec![unused.clone(), used.clone(), other.clone()];
        scores.sort(&mut rows);
        assert_eq!(rows[0].frecency_key.as_deref(), Some("frec:used"));
        // The never-used row sinks to last.
        assert_eq!(rows[2].frecency_key.as_deref(), Some("frec:unused"));
    }

    #[test]
    fn rows_without_keys_keep_relative_order() {
        let scores = Scores(std::collections::HashMap::new());
        let mut rows = vec![row(None), row(Some("k")), row(None)];
        // Mark the middle one as used so it floats up; the keyless pair keeps order.
        rows[0].label = "first".into();
        rows[2].label = "second".into();
        scores.sort(&mut rows);
        let keyless: Vec<String> = rows
            .iter()
            .filter(|r| r.frecency_key.is_none())
            .map(|r| r.label.clone())
            .collect();
        assert_eq!(keyless, vec!["first", "second"]);
    }

    #[test]
    fn bump_ignores_rows_without_a_key() {
        testutil::sandbox();
        // Should not panic or error.
        bump(&row(None));
    }
}
