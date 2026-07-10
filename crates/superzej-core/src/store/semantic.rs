//! The **semantic-graph** seam: the persistent inter-entity impact graph that
//! backs the blast-radius subsystem (items 313/316).
//!
//! Two tables — `sem_entity` (id → file/name/kind/span/source_hash) and
//! `sem_edge` (caller `src_id` → callee `dst_id`) — hold pure *derived* state:
//! a fresh DB rebuilds the graph from the fs-watcher, so there is no backfill on
//! upgrade and no source-of-truth coupling. The host graph builder writes it off
//! the event loop from LSP `references`; the footer and the `blast_radius` MCP
//! tool read it.
//!
//! Object-safe: every method takes `&self` and concrete arguments, so
//! `&dyn SemanticStore` works. [`crate::db::Db`] is the embedded-SQLite impl
//! ([`crate::db_semantic`]).

use anyhow::Result;

use crate::semantic::EntityKind;

/// A persisted entity row. `file` is the absolute worktree path; `id` is the
/// stable [`crate::semantic_graph::entity_id`]; the span is a 1-based inclusive
/// line range; `source_hash` is the file's source at parse time (the skip key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemEntityRow {
    pub id: String,
    pub file: String,
    pub name: String,
    pub kind: EntityKind,
    pub start_line: u32,
    pub end_line: u32,
    pub source_hash: String,
}

/// A persisted edge: `src_id` (caller) → `dst_id` (callee). `kind` is
/// `"ref" | "call" | "test"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemEdgeRow {
    pub src_id: String,
    pub dst_id: String,
    pub kind: String,
}

/// The persistent semantic entity graph.
pub trait SemanticStore {
    /// Replace all entity rows for a file (delete-then-insert), so entities that
    /// vanished from a re-parse are dropped. Used for the *changed* file.
    fn replace_file_entities(&self, file: &str, entities: &[SemEntityRow]) -> Result<()>;

    /// Insert-or-replace a single entity by id. Used for a *caller* entity that
    /// lives in a file we did not fully re-parse, so `callers_of` can return its
    /// file/name/kind.
    fn upsert_entity(&self, entity: &SemEntityRow) -> Result<()>;

    /// The stored `source_hash` for a file (any of its rows share it), or `None`
    /// if the file has no rows — the fast skip-on-unchanged check.
    fn file_source_hash(&self, file: &str) -> Result<Option<String>>;

    /// Replace the edges whose callee (`dst_id`) is in `dst_ids` with `edges`
    /// (delete-then-insert), so a re-parse rewrites only the changed callees'
    /// incoming edges.
    fn replace_edges_for_dsts(&self, dst_ids: &[String], edges: &[SemEdgeRow]) -> Result<()>;

    /// The caller entities that reach `dst_id` (join `sem_edge.dst_id` →
    /// `sem_entity` on `src_id`). Empty when the graph has no edges for it.
    fn callers_of(&self, dst_id: &str) -> Result<Vec<SemEntityRow>>;
}
