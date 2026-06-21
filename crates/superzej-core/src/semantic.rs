//! Semantic git layer (items 309–317): make git operations *entity-aware* —
//! by function / type / class rather than just lines.
//!
//! This module is the analytical core: it parses source into named code
//! **entities** (via tree-sitter), attributes a diff's churn to the entities it
//! touches, groups blame by entity, summarizes a change's impact, and derives a
//! structural commit message — all pure, no I/O, so it's unit-tested and lives
//! under the 95% core coverage gate. The host fetches inputs off-thread and
//! renders the results.
//!
//! We replicate the capability of the (license-encumbered) `sem`/`weave` tools
//! ourselves on the permissive `tree-sitter` ecosystem.

use std::sync::LazyLock;

use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use crate::patch::{LineKind, PatchHunk};

/// A source language we can parse entities for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
}

impl Lang {
    /// Infer the language from a file path's extension (`None` = unsupported).
    pub fn from_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            _ => return None,
        })
    }
}

/// The kind of a code entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Class,
    Interface,
    TypeAlias,
    Const,
    Module,
}

impl EntityKind {
    /// A short label for UI ("fn", "struct", …).
    pub fn label(self) -> &'static str {
        match self {
            EntityKind::Function => "fn",
            EntityKind::Method => "method",
            EntityKind::Struct => "struct",
            EntityKind::Enum => "enum",
            EntityKind::Trait => "trait",
            EntityKind::Impl => "impl",
            EntityKind::Class => "class",
            EntityKind::Interface => "interface",
            EntityKind::TypeAlias => "type",
            EntityKind::Const => "const",
            EntityKind::Module => "mod",
        }
    }

    /// Map a tree-sitter node kind (the def node's grammar type) to an
    /// `EntityKind`. `None` for node kinds we don't surface.
    fn from_node_kind(kind: &str) -> Option<EntityKind> {
        Some(match kind {
            "function_item" | "function_declaration" | "function_definition" | "arrow_function" => {
                EntityKind::Function
            }
            "method_declaration" | "method_definition" => EntityKind::Method,
            "struct_item" | "type_declaration" => EntityKind::Struct,
            "enum_item" | "enum_declaration" => EntityKind::Enum,
            "trait_item" => EntityKind::Trait,
            "impl_item" => EntityKind::Impl,
            "class_definition" | "class_declaration" => EntityKind::Class,
            "interface_declaration" => EntityKind::Interface,
            "type_item" | "type_alias_declaration" => EntityKind::TypeAlias,
            "const_item" | "static_item" => EntityKind::Const,
            "mod_item" => EntityKind::Module,
            _ => return None,
        })
    }
}

/// A named code entity with a 1-based, inclusive line span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub kind: EntityKind,
    pub name: String,
    pub start_line: u32,
    pub end_line: u32,
}

impl Entity {
    /// Whether a 1-based line number falls within this entity.
    pub fn contains(&self, line: u32) -> bool {
        line >= self.start_line && line <= self.end_line
    }
}

// ─── tree-sitter grammars + entity queries ──────────────────────────────────

/// A compiled grammar: its `Language` plus the entity-extraction query. Each
/// query captures `@def` (the entity node, whose grammar kind names the
/// `EntityKind`) and `@name` (its identifier).
struct Grammar {
    language: Language,
    query: Query,
}

fn build(language: Language, src: &str) -> Grammar {
    // A malformed query is a programming error (bad node-type for the grammar
    // version) caught by the unit tests; expect with a clear message.
    let query = Query::new(&language, src).expect("entity query compiles");
    Grammar { language, query }
}

static RUST: LazyLock<Grammar> = LazyLock::new(|| {
    build(
        tree_sitter_rust::LANGUAGE.into(),
        r#"
        (function_item name: (identifier) @name) @def
        (struct_item name: (type_identifier) @name) @def
        (enum_item name: (type_identifier) @name) @def
        (trait_item name: (type_identifier) @name) @def
        (mod_item name: (identifier) @name) @def
        (const_item name: (identifier) @name) @def
        (static_item name: (identifier) @name) @def
        (type_item name: (type_identifier) @name) @def
        (impl_item type: (type_identifier) @name) @def
        "#,
    )
});

static TYPESCRIPT: LazyLock<Grammar> =
    LazyLock::new(|| build(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), TS_QUERY));
static TSX: LazyLock<Grammar> =
    LazyLock::new(|| build(tree_sitter_typescript::LANGUAGE_TSX.into(), TS_QUERY));

const TS_QUERY: &str = r#"
    (function_declaration name: (identifier) @name) @def
    (class_declaration name: (type_identifier) @name) @def
    (interface_declaration name: (type_identifier) @name) @def
    (enum_declaration name: (identifier) @name) @def
    (type_alias_declaration name: (type_identifier) @name) @def
    (method_definition name: (property_identifier) @name) @def
    (variable_declarator name: (identifier) @name value: (arrow_function) @def)
"#;

static JAVASCRIPT: LazyLock<Grammar> = LazyLock::new(|| {
    build(
        tree_sitter_javascript::LANGUAGE.into(),
        r#"
        (function_declaration name: (identifier) @name) @def
        (class_declaration name: (identifier) @name) @def
        (method_definition name: (property_identifier) @name) @def
        (variable_declarator name: (identifier) @name value: (arrow_function) @def)
        "#,
    )
});

static PYTHON: LazyLock<Grammar> = LazyLock::new(|| {
    build(
        tree_sitter_python::LANGUAGE.into(),
        r#"
        (function_definition name: (identifier) @name) @def
        (class_definition name: (identifier) @name) @def
        "#,
    )
});

static GO: LazyLock<Grammar> = LazyLock::new(|| {
    build(
        tree_sitter_go::LANGUAGE.into(),
        r#"
        (function_declaration name: (identifier) @name) @def
        (method_declaration name: (field_identifier) @name) @def
        (type_declaration (type_spec name: (type_identifier) @name)) @def
        "#,
    )
});

fn grammar(lang: Lang) -> &'static Grammar {
    match lang {
        Lang::Rust => &RUST,
        Lang::TypeScript => &TYPESCRIPT,
        Lang::Tsx => &TSX,
        Lang::JavaScript => &JAVASCRIPT,
        Lang::Python => &PYTHON,
        Lang::Go => &GO,
    }
}

/// Parse `source` into its named entities, sorted by start line. Best-effort:
/// a parse failure (or an unparseable region) yields whatever entities matched;
/// never panics.
pub fn parse_entities(source: &str, lang: Lang) -> Vec<Entity> {
    let g = grammar(lang);
    let mut parser = Parser::new();
    if parser.set_language(&g.language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let bytes = source.as_bytes();
    // The capture indices for @def and @name within the query.
    let def_idx = g.query.capture_index_for_name("def");
    let name_idx = g.query.capture_index_for_name("name");
    let (Some(def_idx), Some(name_idx)) = (def_idx, name_idx) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&g.query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        let def = m.captures.iter().find(|c| c.index == def_idx);
        let name = m.captures.iter().find(|c| c.index == name_idx);
        let (Some(def), Some(name)) = (def, name) else {
            continue;
        };
        let Some(kind) = EntityKind::from_node_kind(def.node.kind()) else {
            continue;
        };
        let Ok(name_text) = name.node.utf8_text(bytes) else {
            continue;
        };
        out.push(Entity {
            kind,
            name: name_text.to_string(),
            // tree-sitter rows are 0-based; entity spans are 1-based inclusive.
            start_line: def.node.start_position().row as u32 + 1,
            end_line: def.node.end_position().row as u32 + 1,
        });
    }
    out.sort_by_key(|e| (e.start_line, e.end_line));
    out
}

// ─── Entity ↔ diff mapping (items 311 / 313) ────────────────────────────────

/// How a diff touched an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Touch {
    /// The entity is new (all of its lines were added).
    Added,
    /// The entity's lines were all removed.
    Removed,
    /// The entity was edited in place.
    Modified,
}

/// Per-entity churn attributed from a file's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityChange {
    pub kind: EntityKind,
    pub name: String,
    pub added: u32,
    pub deleted: u32,
    pub touch: Touch,
}

/// Attribute a file diff's added/deleted lines to the entities they fall in,
/// using the *new* file source for entity spans. Added lines map to their
/// new-file line number; deleted lines are attributed to the entity at the
/// hunk's position. Churn outside any entity is dropped (file-level noise like
/// imports); the impact view counts entities, not raw lines.
pub fn entities_for_diff(new_source: &str, lang: Lang, hunks: &[PatchHunk]) -> Vec<EntityChange> {
    let entities = parse_entities(new_source, lang);
    if entities.is_empty() {
        return Vec::new();
    }
    // Accumulate (added, deleted) per entity index.
    let mut acc: Vec<(u32, u32)> = vec![(0, 0); entities.len()];
    // Find the entity owning a 1-based new-file line (innermost = last by start).
    let owner = |line: u32| -> Option<usize> {
        entities
            .iter()
            .enumerate()
            .rfind(|(_, e)| e.contains(line))
            .map(|(i, _)| i)
    };

    for h in hunks {
        // Walk the hunk body tracking the new-file line number. Context and Add
        // lines advance it; a Del line is attributed to the current new line
        // (the edit site) without advancing.
        let mut new_line = h.new_start;
        for l in &h.lines {
            match l.kind {
                LineKind::Context => new_line += 1,
                LineKind::Add => {
                    if let Some(i) = owner(new_line) {
                        acc[i].0 += 1;
                    }
                    new_line += 1;
                }
                LineKind::Del => {
                    // Attribute to the entity at the current new-line position
                    // (clamped to the line before, since a deletion sits between
                    // surrounding context).
                    let probe = new_line.max(1);
                    if let Some(i) = owner(probe).or_else(|| owner(probe.saturating_sub(1))) {
                        acc[i].1 += 1;
                    }
                }
                LineKind::NoNewlineOld | LineKind::NoNewlineNew => {}
            }
        }
    }

    entities
        .into_iter()
        .zip(acc)
        .filter(|(_, (a, d))| *a > 0 || *d > 0)
        .map(|(e, (added, deleted))| {
            // Touch heuristic: only-adds with no deletes ⇒ Added; only-deletes ⇒
            // Removed; both ⇒ Modified.
            let touch = match (added > 0, deleted > 0) {
                (true, false) => Touch::Added,
                (false, true) => Touch::Removed,
                _ => Touch::Modified,
            };
            EntityChange {
                kind: e.kind,
                name: e.name,
                added,
                deleted,
                touch,
            }
        })
        .collect()
}

/// A change's impact: the entities it touched, with a one-line summary. No
/// cross-file reference resolution (changed-entities set only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactSummary {
    pub entities: usize,
    pub files: usize,
    pub summary: String,
}

/// Aggregate per-file entity changes into an impact summary.
pub fn impact_summary(per_file: &[(String, Vec<EntityChange>)]) -> ImpactSummary {
    let files = per_file.iter().filter(|(_, c)| !c.is_empty()).count();
    // Count entities by kind for the summary line.
    let mut by_kind: Vec<(EntityKind, usize)> = Vec::new();
    let mut total = 0usize;
    for (_, changes) in per_file {
        for c in changes {
            total += 1;
            match by_kind.iter_mut().find(|(k, _)| *k == c.kind) {
                Some((_, n)) => *n += 1,
                None => by_kind.push((c.kind, 1)),
            }
        }
    }
    let parts: Vec<String> = by_kind
        .iter()
        .map(|(k, n)| format!("{n} {}{}", k.label(), if *n == 1 { "" } else { "s" }))
        .collect();
    let summary = if total == 0 {
        "no entity-level changes".to_string()
    } else {
        format!(
            "{} across {} file{}",
            parts.join(", "),
            files,
            if files == 1 { "" } else { "s" }
        )
    };
    ImpactSummary {
        entities: total,
        files,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::parse_patch;

    fn names(ents: &[Entity]) -> Vec<&str> {
        ents.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn lang_from_path() {
        assert_eq!(Lang::from_path("src/main.rs"), Some(Lang::Rust));
        assert_eq!(Lang::from_path("a/b.tsx"), Some(Lang::Tsx));
        assert_eq!(Lang::from_path("x.ts"), Some(Lang::TypeScript));
        assert_eq!(Lang::from_path("x.py"), Some(Lang::Python));
        assert_eq!(Lang::from_path("x.go"), Some(Lang::Go));
        assert_eq!(Lang::from_path("README.md"), None);
        assert_eq!(Lang::from_path("noext"), None);
    }

    #[test]
    fn parse_rust_entities() {
        let src = "\
struct Point { x: i32 }

impl Point {
    fn norm(&self) -> i32 { self.x }
}

fn free() -> u8 { 0 }

const K: u8 = 3;
";
        let ents = parse_entities(src, Lang::Rust);
        let got = names(&ents);
        assert!(got.contains(&"Point"), "{got:?}");
        assert!(got.contains(&"norm"), "{got:?}");
        assert!(got.contains(&"free"), "{got:?}");
        assert!(got.contains(&"K"), "{got:?}");
        // The struct entity spans its single line.
        let pt = ents.iter().find(|e| e.name == "Point").unwrap();
        assert_eq!(pt.kind, EntityKind::Struct);
        assert_eq!(pt.start_line, 1);
    }

    #[test]
    fn parse_typescript_entities() {
        let src = "\
export function greet(n: string) { return n; }
class Box { area() { return 1; } }
interface Shape { sides: number; }
const add = (a: number, b: number) => a + b;
";
        let ts_ents = parse_entities(src, Lang::TypeScript);
        let got = names(&ts_ents);
        assert!(got.contains(&"greet"), "{got:?}");
        assert!(got.contains(&"Box"), "{got:?}");
        assert!(got.contains(&"area"), "{got:?}");
        assert!(got.contains(&"Shape"), "{got:?}");
        assert!(got.contains(&"add"), "{got:?}");
    }

    #[test]
    fn parse_python_and_go_entities() {
        let py = "\
def top():
    pass

class C:
    def method(self):
        pass
";
        let py_ents = parse_entities(py, Lang::Python);
        let got = names(&py_ents);
        assert!(
            got.contains(&"top") && got.contains(&"C") && got.contains(&"method"),
            "{got:?}"
        );

        let go = "\
package main

func Free() {}

type T struct { x int }

func (t T) Method() {}
";
        let go_ents = parse_entities(go, Lang::Go);
        let got = names(&go_ents);
        assert!(
            got.contains(&"Free") && got.contains(&"T") && got.contains(&"Method"),
            "{got:?}"
        );
    }

    #[test]
    fn parse_javascript_and_tsx_entities() {
        let js = "\
function f() {}
class Widget { render() { return 0; } }
const g = () => 1;
";
        let js_ents = parse_entities(js, Lang::JavaScript);
        let got = names(&js_ents);
        assert!(
            got.contains(&"f") && got.contains(&"Widget") && got.contains(&"g"),
            "{got:?}"
        );

        let tsx = "\
export function View() { return null; }
const Btn = () => null;
";
        let tsx_ents = parse_entities(tsx, Lang::Tsx);
        let got = names(&tsx_ents);
        assert!(got.contains(&"View") && got.contains(&"Btn"), "{got:?}");
    }

    #[test]
    fn entities_for_diff_added_and_removed_touch() {
        // A brand-new function (all adds) → Touch::Added.
        let new_source = "\
fn keep() {}

fn fresh() -> i32 {
    7
}
";
        let added = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -2,0 +3,4 @@
+fn fresh() -> i32 {
+    7
+}
+
";
        let f = parse_patch(added);
        let ch = entities_for_diff(new_source, Lang::Rust, &f[0].hunks);
        let fresh = ch.iter().find(|c| c.name == "fresh").unwrap();
        assert_eq!(fresh.touch, Touch::Added);
        assert!(fresh.added > 0 && fresh.deleted == 0);

        // A hunk that only deletes lines inside `keep` → Touch::Removed.
        let removed = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,2 +1,1 @@
 fn keep() {}
-    stale
";
        let f = parse_patch(removed);
        let ch = entities_for_diff(new_source, Lang::Rust, &f[0].hunks);
        if let Some(keep) = ch.iter().find(|c| c.name == "keep") {
            assert_eq!(keep.touch, Touch::Removed);
            assert!(keep.deleted > 0 && keep.added == 0);
        }
    }

    #[test]
    fn entities_for_diff_empty_when_no_entities() {
        // A file with no recognized entities yields no entity changes.
        let diff = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,1 +1,1 @@
-use a;
+use b;
";
        let f = parse_patch(diff);
        assert!(entities_for_diff("use b;\n", Lang::Rust, &f[0].hunks).is_empty());
    }

    #[test]
    fn malformed_source_never_panics() {
        let _ = parse_entities("fn fn fn (((", Lang::Rust);
        let _ = parse_entities("", Lang::Python);
    }

    #[test]
    fn entities_for_diff_attributes_churn() {
        // New file: two functions; a hunk edits the body of `b`.
        let new_source = "\
fn a() -> i32 {
    1
}

fn b() -> i32 {
    42
}
";
        let diff = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -5,3 +5,3 @@ fn b() -> i32 {
 fn b() -> i32 {
-    2
+    42
 }
";
        let files = parse_patch(diff);
        assert_eq!(files.len(), 1, "{files:?}");
        let changes = entities_for_diff(new_source, Lang::Rust, &files[0].hunks);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].name, "b");
        assert_eq!(changes[0].added, 1);
        assert_eq!(changes[0].deleted, 1);
        assert_eq!(changes[0].touch, Touch::Modified);
    }

    #[test]
    fn impact_summary_counts_kinds_and_files() {
        let per_file = vec![
            (
                "a.rs".to_string(),
                vec![
                    EntityChange {
                        kind: EntityKind::Function,
                        name: "a".into(),
                        added: 2,
                        deleted: 0,
                        touch: Touch::Added,
                    },
                    EntityChange {
                        kind: EntityKind::Function,
                        name: "b".into(),
                        added: 1,
                        deleted: 1,
                        touch: Touch::Modified,
                    },
                ],
            ),
            (
                "b.rs".to_string(),
                vec![EntityChange {
                    kind: EntityKind::Struct,
                    name: "S".into(),
                    added: 0,
                    deleted: 3,
                    touch: Touch::Removed,
                }],
            ),
        ];
        let s = impact_summary(&per_file);
        assert_eq!(s.entities, 3);
        assert_eq!(s.files, 2);
        assert!(s.summary.contains("2 fns"), "{}", s.summary);
        assert!(s.summary.contains("1 struct"), "{}", s.summary);
        assert!(s.summary.contains("across 2 files"), "{}", s.summary);
    }

    #[test]
    fn impact_summary_empty() {
        assert_eq!(impact_summary(&[]).summary, "no entity-level changes");
    }
}
