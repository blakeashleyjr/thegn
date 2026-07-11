//! Semantic blast-radius (items 313 / 316): the *inter-entity* impact graph.
//!
//! Where [`crate::semantic`] answers "what did this diff change?" (the entities
//! *within* the patch), this module answers the question that makes a review
//! actionable: "**who depends on what I changed, and is any of it untested?**"
//!
//! Edges are `caller → callee` relationships sourced from the language server's
//! `textDocument/references` (never hand-rolled name resolution) — but that I/O
//! lives in the host. This module is the **pure** half: a stable entity join
//! key, the location→entity mapper, a path/name test-coverage classifier, a
//! total risk score, and the [`BlastRadius`] summary the footer and MCP tool
//! render. All of it takes owned data and returns owned data — no LSP, no DB, no
//! clock — so it is unit-tested under the 95% core coverage gate.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::semantic::{Entity, EntityKind, Touch};

/// A stable, content-free join key for an entity across parses. Two parses of an
/// unchanged entity yield the same id; changing the repo, file, name, or kind
/// yields a different id. Fields are separated so `("ab","c")` cannot collide
/// with `("a","bc")`.
pub fn entity_id(repo: &str, file: &str, qualified_name: &str, kind: EntityKind) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for part in [repo, file, qualified_name, kind.as_db_str()] {
        part.hash(&mut h);
        0u8.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// A caller reference location: a 1-based line in a file. LSP 0-based positions
/// are normalized to this shape at the host I/O edge before reaching the mapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLoc {
    pub file: String,
    /// 1-based line number.
    pub line: u32,
}

/// A reference location resolved to the entity that encloses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRef {
    pub file: String,
    pub entity: Entity,
}

/// Resolve each caller location to the entity that encloses it, using
/// [`Entity::contains`]. When spans nest, the innermost (last-by-start, mirroring
/// [`crate::semantic::entities_for_diff`]'s `owner`) wins. Locations that fall in
/// no entity (module-level, comments) or in an unknown file are dropped.
pub fn map_reference_to_entity(
    refs: &[RefLoc],
    entities_by_file: &BTreeMap<String, Vec<Entity>>,
) -> Vec<ResolvedRef> {
    let mut out = Vec::new();
    for r in refs {
        let Some(entities) = entities_by_file.get(&r.file) else {
            continue;
        };
        if let Some(e) = entities.iter().rev().find(|e| e.contains(r.line)) {
            out.push(ResolvedRef {
                file: r.file.clone(),
                entity: e.clone(),
            });
        }
    }
    out
}

/// Whether an entity is a test, from a pure path/name heuristic (no grammar
/// changes; refinable later). A test *caller* marks its callee covered.
pub fn is_test_entity(file: &str, name: &str) -> bool {
    let f = file.replace('\\', "/");
    let base = f.rsplit('/').next().unwrap_or(f.as_str());
    let path_test = f.contains("/tests/")
        || f.split('/').next() == Some("tests")
        || base.ends_with("_test.rs")
        || base.ends_with("_test.go")
        || base.ends_with("_test.py")
        || (base.starts_with("test_") && base.ends_with(".py"))
        || base.ends_with(".test.ts")
        || base.ends_with(".test.tsx")
        || base.ends_with(".test.js")
        || base.ends_with(".test.jsx")
        || base.ends_with(".spec.ts")
        || base.ends_with(".spec.tsx")
        || base.ends_with(".spec.js")
        || base.ends_with(".spec.jsx");
    let name_test = name.starts_with("test_")
        || matches!(name, "it" | "describe" | "test")
        || is_go_test_name(name);
    path_test || name_test
}

/// Go test/bench/example convention: `TestXxx`/`BenchmarkXxx`/`ExampleXxx` where
/// the suffix begins with an uppercase letter or digit (so `Testable` is not a
/// test, but `TestLogin` is).
fn is_go_test_name(name: &str) -> bool {
    for prefix in ["Test", "Benchmark", "Example"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            match rest.chars().next() {
                Some(c) if c.is_uppercase() || c.is_ascii_digit() => return true,
                _ => {}
            }
        }
    }
    false
}

/// Only callable entities are eligible to be flagged "untested" — flagging a
/// changed `struct`/`const`/`mod` with no test caller would be noise.
fn kind_is_testable(kind: EntityKind) -> bool {
    matches!(kind, EntityKind::Function | EntityKind::Method)
}

/// A changed entity (a callee) from the current diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedEntity {
    pub id: String,
    pub name: String,
    pub kind: EntityKind,
    pub touch: Touch,
}

/// A caller of a changed entity, resolved back to its enclosing entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerRef {
    pub id: String,
    pub file: String,
    pub name: String,
    pub kind: EntityKind,
}

/// Coverage classification: which changed entities are covered by a test caller
/// and which callable ones are untested.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Coverage {
    pub covered: Vec<String>,
    pub untested: Vec<String>,
}

/// Classify each changed entity: a caller for which [`is_test_entity`] holds
/// marks its callee *covered*; a changed **callable** entity with zero test
/// callers is *untested*. Non-callable kinds are neither.
pub fn classify_coverage(
    changed: &[ChangedEntity],
    callers_by_changed: &BTreeMap<String, Vec<CallerRef>>,
) -> Coverage {
    let mut cov = Coverage::default();
    for c in changed {
        let has_test = callers_by_changed
            .get(&c.id)
            .into_iter()
            .flatten()
            .any(|cr| is_test_entity(&cr.file, &cr.name));
        if has_test {
            cov.covered.push(c.id.clone());
        } else if kind_is_testable(c.kind) {
            cov.untested.push(c.id.clone());
        }
    }
    cov
}

/// A risk band. Total and deterministic — the same inputs always yield the same
/// band, so tests can pin the thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

/// Fold fan-out breadth, untested count, removed-entity severity, and change
/// breadth into a `low|medium|high` band. Monotonic in every input (holding the
/// others fixed) and total, so the threshold bands are pinnable by tests.
pub fn risk_score(
    changed: usize,
    callers: usize,
    files: usize,
    untested: usize,
    has_removed: bool,
) -> Risk {
    let mut score = 0u32;
    // Fan-out breadth.
    if callers >= 10 || files >= 5 {
        score += 2;
    } else if callers >= 3 || files >= 2 {
        score += 1;
    }
    // Untested surface.
    if untested >= 3 {
        score += 2;
    } else if untested >= 1 {
        score += 1;
    }
    // A removed entity is potentially breaking for every caller.
    if has_removed {
        score += 1;
    }
    // Sheer breadth of the change.
    if changed >= 5 {
        score += 1;
    }
    if score >= 4 {
        Risk::High
    } else if score >= 2 {
        Risk::Medium
    } else {
        Risk::Low
    }
}

/// The blast-radius summary the footer and MCP tool render: how many entities
/// changed, how many distinct callers reach them across how many files, how many
/// are untested, and the overall risk band.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastRadius {
    pub changed: usize,
    pub callers: usize,
    pub files: usize,
    pub untested: usize,
    pub risk: Risk,
}

impl BlastRadius {
    /// The one-line footer/tool render, e.g.
    /// "3 changed · 14 callers/6 files · 2 untested · risk:high".
    pub fn render(&self) -> String {
        format!(
            "{} changed · {} callers/{} files · {} untested · risk:{}",
            self.changed,
            self.callers,
            self.files,
            self.untested,
            self.risk.as_str()
        )
    }
}

/// Compose the pure classification + risk into a [`BlastRadius`] from the changed
/// (callee) set and each changed entity's resolved callers. Distinct callers and
/// files are counted across the whole change.
pub fn compute_blast_radius(
    changed: &[ChangedEntity],
    callers_by_changed: &BTreeMap<String, Vec<CallerRef>>,
) -> BlastRadius {
    let cov = classify_coverage(changed, callers_by_changed);
    let has_removed = changed.iter().any(|c| c.touch == Touch::Removed);
    let mut caller_ids: BTreeSet<&str> = BTreeSet::new();
    let mut files: BTreeSet<&str> = BTreeSet::new();
    for callers in callers_by_changed.values() {
        for cr in callers {
            caller_ids.insert(cr.id.as_str());
            files.insert(cr.file.as_str());
        }
    }
    let risk = risk_score(
        changed.len(),
        caller_ids.len(),
        files.len(),
        cov.untested.len(),
        has_removed,
    );
    BlastRadius {
        changed: changed.len(),
        callers: caller_ids.len(),
        files: files.len(),
        untested: cov.untested.len(),
        risk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(kind: EntityKind, name: &str, start: u32, end: u32) -> Entity {
        Entity {
            kind,
            name: name.to_string(),
            start_line: start,
            end_line: end,
        }
    }

    // ─── entity_id ──────────────────────────────────────────────────────────

    #[test]
    fn entity_id_is_stable_and_discriminating() {
        let a = entity_id("repo", "src/a.rs", "foo", EntityKind::Function);
        assert_eq!(
            a,
            entity_id("repo", "src/a.rs", "foo", EntityKind::Function)
        );
        // Any field change → different id.
        assert_ne!(
            a,
            entity_id("repo2", "src/a.rs", "foo", EntityKind::Function)
        );
        assert_ne!(
            a,
            entity_id("repo", "src/b.rs", "foo", EntityKind::Function)
        );
        assert_ne!(
            a,
            entity_id("repo", "src/a.rs", "bar", EntityKind::Function)
        );
        assert_ne!(a, entity_id("repo", "src/a.rs", "foo", EntityKind::Method));
    }

    #[test]
    fn entity_id_field_boundaries_do_not_collide() {
        // "ab"+"c" must not hash the same as "a"+"bc".
        assert_ne!(
            entity_id("ab", "c", "n", EntityKind::Const),
            entity_id("a", "bc", "n", EntityKind::Const)
        );
    }

    // ─── map_reference_to_entity ──────────────────────────────────────────────

    #[test]
    fn reference_inside_entity_resolves() {
        let mut by_file = BTreeMap::new();
        by_file.insert(
            "src/a.rs".to_string(),
            vec![ent(EntityKind::Function, "outer", 1, 20)],
        );
        let refs = vec![RefLoc {
            file: "src/a.rs".to_string(),
            line: 5,
        }];
        let got = map_reference_to_entity(&refs, &by_file);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].entity.name, "outer");
    }

    #[test]
    fn module_level_and_unknown_file_refs_are_dropped() {
        let mut by_file = BTreeMap::new();
        by_file.insert(
            "src/a.rs".to_string(),
            vec![ent(EntityKind::Function, "f", 10, 20)],
        );
        // line 2 is outside the entity; other.rs is unknown.
        let refs = vec![
            RefLoc {
                file: "src/a.rs".to_string(),
                line: 2,
            },
            RefLoc {
                file: "src/other.rs".to_string(),
                line: 15,
            },
        ];
        assert!(map_reference_to_entity(&refs, &by_file).is_empty());
    }

    #[test]
    fn nested_spans_pick_innermost() {
        let mut by_file = BTreeMap::new();
        by_file.insert(
            "src/a.rs".to_string(),
            // sorted by (start, end) as parse_entities yields; inner method nested.
            vec![
                ent(EntityKind::Impl, "impl Foo", 1, 30),
                ent(EntityKind::Method, "bar", 5, 10),
            ],
        );
        let refs = vec![RefLoc {
            file: "src/a.rs".to_string(),
            line: 7,
        }];
        let got = map_reference_to_entity(&refs, &by_file);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].entity.name, "bar");
    }

    // ─── is_test_entity ───────────────────────────────────────────────────────

    #[test]
    fn test_detection_by_path() {
        assert!(is_test_entity("crates/foo/tests/it.rs", "run"));
        assert!(is_test_entity("tests/e2e.rs", "run"));
        assert!(is_test_entity("pkg/foo_test.go", "Helper"));
        assert!(is_test_entity("app/login.test.ts", "helper"));
        assert!(is_test_entity("app/login.spec.tsx", "helper"));
        assert!(is_test_entity("suite/test_login.py", "helper"));
        assert!(is_test_entity("suite/login_test.py", "helper"));
        assert!(!is_test_entity("src/login.rs", "login"));
    }

    #[test]
    fn test_detection_by_name() {
        assert!(is_test_entity("src/a.rs", "test_login"));
        assert!(is_test_entity("src/a_test.go", "TestLogin"));
        assert!(is_test_entity("app/x.ts", "it"));
        assert!(is_test_entity("app/x.ts", "describe"));
        // Go convention needs an uppercase suffix: "Testable" is not a test.
        assert!(!is_test_entity("src/a.rs", "Testable"));
        assert!(!is_test_entity("src/a.rs", "Test"));
    }

    // ─── classify_coverage ────────────────────────────────────────────────────

    fn changed(id: &str, kind: EntityKind) -> ChangedEntity {
        ChangedEntity {
            id: id.to_string(),
            name: id.to_string(),
            kind,
            touch: Touch::Modified,
        }
    }
    fn caller(id: &str, file: &str, name: &str) -> CallerRef {
        CallerRef {
            id: id.to_string(),
            file: file.to_string(),
            name: name.to_string(),
            kind: EntityKind::Function,
        }
    }

    #[test]
    fn untested_when_no_test_caller() {
        let ch = vec![changed("c1", EntityKind::Function)];
        let mut map = BTreeMap::new();
        map.insert(
            "c1".to_string(),
            vec![caller("p1", "src/prod.rs", "use_it")],
        );
        let cov = classify_coverage(&ch, &map);
        assert_eq!(cov.untested, vec!["c1".to_string()]);
        assert!(cov.covered.is_empty());
    }

    #[test]
    fn covered_when_test_caller_present() {
        let ch = vec![changed("c1", EntityKind::Function)];
        let mut map = BTreeMap::new();
        map.insert(
            "c1".to_string(),
            vec![
                caller("p1", "src/prod.rs", "use_it"),
                caller("t1", "tests/it.rs", "test_use"),
            ],
        );
        let cov = classify_coverage(&ch, &map);
        assert_eq!(cov.covered, vec!["c1".to_string()]);
        assert!(cov.untested.is_empty());
    }

    #[test]
    fn non_callable_kinds_are_not_flagged_untested() {
        let ch = vec![changed("s1", EntityKind::Struct)];
        let cov = classify_coverage(&ch, &BTreeMap::new());
        assert!(cov.untested.is_empty());
        assert!(cov.covered.is_empty());
    }

    // ─── risk_score ─────────────────────────────────────────────────────────

    #[test]
    fn risk_bands_are_pinned() {
        // Nothing → low.
        assert_eq!(risk_score(1, 0, 0, 0, false), Risk::Low);
        // A little fan-out + one untested → medium.
        assert_eq!(risk_score(1, 3, 2, 1, false), Risk::Medium);
        // Wide fan-out + several untested → high.
        assert_eq!(risk_score(5, 12, 6, 3, true), Risk::High);
    }

    #[test]
    fn risk_is_monotonic_in_untested() {
        let base = risk_score(1, 0, 0, 0, false);
        let more = risk_score(1, 0, 0, 3, false);
        assert!(matches!(base, Risk::Low));
        assert!(matches!(more, Risk::Medium | Risk::High));
    }

    #[test]
    fn risk_is_deterministic() {
        assert_eq!(risk_score(3, 5, 2, 1, true), risk_score(3, 5, 2, 1, true));
    }

    // ─── compute_blast_radius + render ────────────────────────────────────────

    #[test]
    fn blast_radius_counts_distinct_callers_and_files() {
        let ch = vec![
            changed("c1", EntityKind::Function),
            changed("c2", EntityKind::Function),
        ];
        let mut map = BTreeMap::new();
        map.insert(
            "c1".to_string(),
            vec![
                caller("p1", "src/a.rs", "u1"),
                caller("p2", "src/b.rs", "u2"),
            ],
        );
        // c2 shares caller p1 (same file) and gets a test caller.
        map.insert(
            "c2".to_string(),
            vec![
                caller("p1", "src/a.rs", "u1"),
                caller("t1", "tests/it.rs", "test_x"),
            ],
        );
        let br = compute_blast_radius(&ch, &map);
        assert_eq!(br.changed, 2);
        assert_eq!(br.callers, 3); // p1, p2, t1 distinct
        assert_eq!(br.files, 3); // src/a.rs, src/b.rs, tests/it.rs
        assert_eq!(br.untested, 1); // c1 untested, c2 covered
        assert_eq!(
            br.render(),
            "2 changed · 3 callers/3 files · 1 untested · risk:medium"
        );
    }

    #[test]
    fn empty_blast_radius_is_low_risk() {
        let br = compute_blast_radius(&[], &BTreeMap::new());
        assert_eq!(br.changed, 0);
        assert_eq!(br.callers, 0);
        assert_eq!(br.risk, Risk::Low);
    }
}
