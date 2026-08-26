//! The **repo map** renderer: a ranked, line-budgeted outline of a worktree's
//! indexed entities, grouped by file (the thing Aider / Goose inject into agent
//! context, done on thegn's own tree-sitter entity index).
//!
//! This module is the **pure** half — it takes owned [`MapEntity`] rows plus a
//! caller in-degree signal and produces ranked, budgeted map text and a
//! serializable row form. No I/O, no clock, no DB: the host reads
//! [`crate::store::SemanticStore::entities_under`] +
//! [`crate::store::SemanticStore::caller_degrees`] off the loop and hands the
//! owned rows here, so the whole thing is unit-tested under the 95% core gate.
//!
//! Ranking is caller in-degree descending (the moral equivalent of Aider's
//! graph-centrality) with a **deterministic structural fallback** — entity-kind
//! weight, then file path, then line — so an edge-less worktree (no LSP has ever
//! run) still maps stably, and the same input always renders identically.

use serde::Serialize;

use crate::semantic::EntityKind;

/// One entity in the repo map: what to show and how to rank it. `file` is a
/// display path (the host relativizes the absolute store path to the worktree
/// root); `degree` is the caller in-degree (0 when no edge reaches it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntity {
    pub kind: EntityKind,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub degree: u32,
}

/// A serializable map row (`--json` / MCP result). `kind` is the stable
/// [`EntityKind::label`] string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MapRow {
    pub kind: &'static str,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub degree: u32,
}

/// Structural rank weight when in-degree ties (or is absent everywhere): types
/// and traits before callables before consts, so an edge-less map still leads
/// with the shapes a reader orients on. Lower sorts first.
fn kind_weight(kind: EntityKind) -> u8 {
    match kind {
        // Types / traits / interfaces — the shapes.
        EntityKind::Trait | EntityKind::Interface => 0,
        EntityKind::Struct | EntityKind::Enum | EntityKind::Class => 1,
        EntityKind::TypeAlias => 2,
        EntityKind::Module => 3,
        EntityKind::Impl => 4,
        // Callables.
        EntityKind::Function | EntityKind::Method => 5,
        // Values last.
        EntityKind::Const => 6,
    }
}

/// A worktree repo map: the ranked entities plus whether the index they came
/// from is honestly partial (the crawl hit its file cap). Ranking happens once,
/// at construction, so every reader sees the same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMap {
    /// Globally ranked (in-degree desc, then structural fallback).
    entities: Vec<MapEntity>,
    /// The source index was capped — some files were never crawled.
    partial: bool,
}

/// One rendered line before formatting: a file header or an entity under it.
enum Line<'a> {
    Header(&'a str),
    Ent(&'a MapEntity),
}

impl RepoMap {
    /// Rank the entities and take ownership. Same input ⇒ same order (total,
    /// deterministic comparator).
    pub fn new(mut entities: Vec<MapEntity>, partial: bool) -> Self {
        entities.sort_by(|a, b| {
            // In-degree descending is the primary signal…
            b.degree
                .cmp(&a.degree)
                // …then the deterministic structural fallback.
                .then_with(|| kind_weight(a.kind).cmp(&kind_weight(b.kind)))
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.name.cmp(&b.name))
        });
        RepoMap { entities, partial }
    }

    /// Whether the map has any entity.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Total indexed entities (before any budget).
    pub fn total(&self) -> usize {
        self.entities.len()
    }

    /// Whether the underlying index is partial.
    pub fn partial(&self) -> bool {
        self.partial
    }

    /// Group the ranked entities by file, preserving global rank order: the file
    /// whose top entity ranks highest comes first, and within a file entities
    /// stay in rank order. One pass, so a large index stays cheap.
    fn groups(&self) -> Vec<(&str, Vec<&MapEntity>)> {
        let mut groups: Vec<(&str, Vec<&MapEntity>)> = Vec::new();
        let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for e in &self.entities {
            let i = *index.entry(e.file.as_str()).or_insert_with(|| {
                groups.push((e.file.as_str(), Vec::new()));
                groups.len() - 1
            });
            groups[i].1.push(e);
        }
        groups
    }

    /// The lines that fit within `budget` (headers included), most-important
    /// file first, plus the count of entities elided beyond them. A budget too
    /// small for even one file yields no lines and everything elided — the
    /// caller still prints the elision marker.
    fn shown(&self, budget: usize) -> (Vec<Line<'_>>, usize) {
        let budget = budget.max(1);
        let groups = self.groups();
        let mut plan: Vec<Line> = Vec::new();
        for (file, ents) in &groups {
            plan.push(Line::Header(file));
            for e in ents {
                plan.push(Line::Ent(e));
            }
        }
        if plan.len() <= budget {
            return (plan, 0);
        }
        // Truncate, reserving one line for the elision marker; never leave an
        // orphan trailing header (a file heading with none of its entities).
        let mut take = budget - 1;
        while take > 0 && matches!(plan[take - 1], Line::Header(_)) {
            take -= 1;
        }
        plan.truncate(take);
        let shown_ents = plan.iter().filter(|l| matches!(l, Line::Ent(_))).count();
        (plan, self.entities.len() - shown_ents)
    }

    /// Render the human-readable map under a line budget. The partial notice (a
    /// meta line) is never subject to the budget, so a capped index always says
    /// so no matter how tight the budget.
    pub fn render(&self, budget: usize) -> String {
        let mut out = String::new();
        if self.partial {
            out.push_str("(partial index — crawl hit its file cap; some files were not indexed)\n");
        }
        if self.entities.is_empty() {
            out.push_str("(no indexed entities)");
            return out;
        }
        let (lines, elided) = self.shown(budget);
        for line in &lines {
            match line {
                Line::Header(file) => out.push_str(&format!("{file}\n")),
                Line::Ent(e) => {
                    let deg = if e.degree > 0 {
                        format!("  ×{}", e.degree)
                    } else {
                        String::new()
                    };
                    out.push_str(&format!(
                        "  {} {}  L{}{}\n",
                        e.kind.label(),
                        e.name,
                        e.line,
                        deg
                    ));
                }
            }
        }
        if elided > 0 {
            out.push_str(&format!(
                "… {elided} more entr{} elided (budget {} lines)\n",
                if elided == 1 { "y" } else { "ies" },
                budget.max(1)
            ));
        }
        // Trim the trailing newline for a clean single/multi-line body.
        while out.ends_with('\n') {
            out.pop();
        }
        out
    }

    /// The serializable rows the map would show under `budget` (the same set the
    /// text render shows), in render order. `--json` / MCP consumers get exactly
    /// what a human would see, plus the totals in the wrapper.
    pub fn rows(&self, budget: usize) -> Vec<MapRow> {
        let (lines, _elided) = self.shown(budget);
        lines
            .iter()
            .filter_map(|l| match l {
                Line::Ent(e) => Some(MapRow {
                    kind: e.kind.label(),
                    name: e.name.clone(),
                    file: e.file.clone(),
                    line: e.line,
                    degree: e.degree,
                }),
                Line::Header(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(kind: EntityKind, name: &str, file: &str, line: u32, degree: u32) -> MapEntity {
        MapEntity {
            kind,
            name: name.to_string(),
            file: file.to_string(),
            line,
            degree,
        }
    }

    #[test]
    fn edges_rank_the_map() {
        // Higher in-degree ranks first, regardless of kind/file/line.
        let map = RepoMap::new(
            vec![
                ent(EntityKind::Function, "low", "a.rs", 1, 1),
                ent(EntityKind::Function, "high", "z.rs", 99, 14),
                ent(EntityKind::Function, "mid", "m.rs", 5, 6),
            ],
            false,
        );
        let rows = map.rows(100);
        assert_eq!(rows[0].name, "high");
        assert_eq!(rows[1].name, "mid");
        assert_eq!(rows[2].name, "low");
    }

    #[test]
    fn edgeless_index_ranks_structurally_and_deterministically() {
        // No degrees anywhere: fall back to kind weight, then path, then line.
        let build = || {
            RepoMap::new(
                vec![
                    ent(EntityKind::Const, "K", "a.rs", 1, 0),
                    ent(EntityKind::Function, "f", "a.rs", 30, 0),
                    ent(EntityKind::Struct, "S", "a.rs", 10, 0),
                    ent(EntityKind::Trait, "T", "b.rs", 3, 0),
                ],
                false,
            )
        };
        let a = build().render(100);
        let b = build().render(100);
        assert_eq!(a, b, "same input renders identically");
        let rows = build().rows(100);
        // Trait (weight 0) before Struct (1) before Function (5) before Const (6).
        let order: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, ["T", "S", "f", "K"], "{a}");
    }

    #[test]
    fn ties_break_by_file_then_line() {
        // Same degree + same kind: file path, then line.
        let map = RepoMap::new(
            vec![
                ent(EntityKind::Function, "b2", "b.rs", 2, 3),
                ent(EntityKind::Function, "a1", "a.rs", 9, 3),
                ent(EntityKind::Function, "a0", "a.rs", 1, 3),
            ],
            false,
        );
        let rows = map.rows(100);
        let order: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, ["a0", "a1", "b2"]);
    }

    #[test]
    fn budget_bounds_the_output_with_elision() {
        // Three files, six entities: a tight budget stops with an elision marker.
        let map = RepoMap::new(
            vec![
                ent(EntityKind::Function, "f1", "a.rs", 1, 9),
                ent(EntityKind::Function, "f2", "a.rs", 2, 8),
                ent(EntityKind::Function, "g1", "b.rs", 1, 7),
                ent(EntityKind::Function, "g2", "b.rs", 2, 6),
                ent(EntityKind::Function, "h1", "c.rs", 1, 5),
                ent(EntityKind::Function, "h2", "c.rs", 2, 4),
            ],
            false,
        );
        // Budget 3 lines: header(a.rs) + f1 + f2 would be 3, but then elision
        // needs a line, so it reserves one → header + f1 shown, rest elided.
        let text = map.render(3);
        assert!(text.contains("a.rs"), "{text}");
        assert!(text.contains("more entr"), "{text}");
        // The elided count is honest: total minus what was shown.
        let shown = map.rows(3).len();
        assert!((1..6).contains(&shown), "shown={shown}: {text}");
        assert!(
            text.contains(&format!("{} more", 6 - shown)),
            "elided count wrong: {text}"
        );
    }

    #[test]
    fn budget_smaller_than_one_file_still_marks_elision() {
        let map = RepoMap::new(
            vec![
                ent(EntityKind::Function, "a", "a.rs", 1, 2),
                ent(EntityKind::Function, "b", "a.rs", 2, 1),
            ],
            false,
        );
        // Budget 1: no room for header+entity+elision, so nothing is shown but
        // the elision marker still reports everything elided.
        let text = map.render(1);
        assert!(text.contains("2 more"), "{text}");
        assert!(map.rows(1).is_empty());
    }

    #[test]
    fn whole_map_fits_when_budget_is_generous() {
        let map = RepoMap::new(
            vec![
                ent(EntityKind::Struct, "S", "a.rs", 1, 0),
                ent(EntityKind::Function, "f", "a.rs", 5, 0),
            ],
            false,
        );
        let text = map.render(100);
        assert!(!text.contains("more entr"), "{text}");
        assert!(text.contains("a.rs"));
        assert!(text.contains("struct S  L1"), "{text}");
        assert!(text.contains("fn f  L5"), "{text}");
        assert_eq!(map.rows(100).len(), 2);
    }

    #[test]
    fn empty_index_renders_a_clear_message() {
        let map = RepoMap::new(Vec::new(), false);
        assert!(map.is_empty());
        assert_eq!(map.render(100), "(no indexed entities)");
        assert!(map.rows(100).is_empty());
        assert_eq!(map.total(), 0);
    }

    #[test]
    fn partial_notice_survives_a_tiny_budget() {
        let map = RepoMap::new(vec![ent(EntityKind::Function, "f", "a.rs", 1, 0)], true);
        // Even at budget 1 the partial notice is present (it is a meta line, not
        // budgeted) so a reader never mistakes a capped index for complete.
        let text = map.render(1);
        assert!(text.contains("partial index"), "{text}");
        assert!(map.partial());
    }

    #[test]
    fn degree_marker_only_when_nonzero() {
        let map = RepoMap::new(
            vec![
                ent(EntityKind::Function, "hot", "a.rs", 1, 4),
                ent(EntityKind::Function, "cold", "a.rs", 2, 0),
            ],
            false,
        );
        let text = map.render(100);
        assert!(text.contains("fn hot  L1  ×4"), "{text}");
        assert!(
            text.contains("fn cold  L2\n") || text.ends_with("fn cold  L2"),
            "{text}"
        );
    }

    #[test]
    fn json_rows_carry_the_rank_signal() {
        let map = RepoMap::new(vec![ent(EntityKind::Method, "m", "src/a.rs", 12, 3)], false);
        let rows = map.rows(100);
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(json["kind"], "method");
        assert_eq!(json["name"], "m");
        assert_eq!(json["file"], "src/a.rs");
        assert_eq!(json["line"], 12);
        assert_eq!(json["degree"], 3);
    }

    #[test]
    fn every_kind_has_a_weight() {
        // Exhaustive: no kind panics / is unranked (compiler-checked match).
        for k in [
            EntityKind::Function,
            EntityKind::Method,
            EntityKind::Struct,
            EntityKind::Enum,
            EntityKind::Trait,
            EntityKind::Impl,
            EntityKind::Class,
            EntityKind::Interface,
            EntityKind::TypeAlias,
            EntityKind::Const,
            EntityKind::Module,
        ] {
            let _ = kind_weight(k);
        }
    }
}
