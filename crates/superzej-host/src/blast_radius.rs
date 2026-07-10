//! Semantic blast-radius graph **builder** (items 313/316) — the I/O half that
//! feeds the pure [`superzej_core::semantic_graph`] logic and persists the graph
//! behind the [`SemanticStore`] seam.
//!
//! Runs entirely off the event loop (`spawn_blocking`): for each changed entity
//! in the active worktree's `git diff HEAD`, it asks the warm LSP client for
//! `textDocument/references`, resolves each caller location back to the entity
//! that encloses it (tree-sitter spans), and writes `caller → callee` edges.
//! The footer and the `blast_radius` MCP tool only ever *read* the persisted
//! graph. Incremental: a file whose `source_hash` is unchanged is skipped.
//!
//! Strictly additive: with `[lsp]` off / no server / an unserved language, no
//! edges are written and every reader falls back — see the module tests and the
//! `semantic-graph` capability spec.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use superzej_core::db::Db;
use superzej_core::remote::GitLoc;
use superzej_core::semantic::{self, Entity, EntitySummary, Lang};
use superzej_core::semantic_graph::{
    BlastRadius, CallerRef, ChangedEntity, compute_blast_radius, entity_id, is_test_entity,
};
use superzej_core::store::{SemEdgeRow, SemEntityRow, SemanticStore};
use superzej_svc::lsp::{Position, path_to_uri};

use termwiz::terminal::TerminalWaker;

use crate::lsp::LspInner;

/// Most changed files we process in one build (mirrors the hydration cap so a
/// sprawling change never balloons the LSP work).
const MAX_CHANGED_FILES: usize = 50;

/// A cheap, stable content hash for the source-changed skip check.
fn source_hash(src: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Debounce between graph builds — cheap because unchanged-source files are
/// skipped, so this only guards against churn during rapid edits.
const BUILD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(1500);

thread_local! {
    /// Last build time, kept here (not in `run.rs`) so the loop hook is one call
    /// with no extra loop-local state. The event loop is single-threaded.
    static LAST_BUILD: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// Loop-side trigger: rebuild the active worktree's graph off the event loop
/// when `should` (LSP enabled ∧ its diff refreshed), throttled. Kept out of
/// `run.rs` (god-file ratchet); the loop calls this in one statement.
pub(crate) fn maybe_spawn_build(
    should: bool,
    cwd: Option<std::path::PathBuf>,
    lsp: std::sync::Arc<LspInner>,
    waker: &TerminalWaker,
) {
    if !should {
        return;
    }
    if LAST_BUILD
        .get()
        .is_some_and(|t| t.elapsed() < BUILD_DEBOUNCE)
    {
        return;
    }
    let Some(cwd) = cwd else {
        return;
    };
    LAST_BUILD.set(Some(std::time::Instant::now()));
    spawn_graph_build(cwd, lsp, waker.clone());
}

/// Spawn an off-loop build of the worktree's blast-radius graph. Best-effort:
/// pulses the waker only when the graph changed (so the footer re-hydrates).
pub(crate) fn spawn_graph_build(
    root: std::path::PathBuf,
    lsp: std::sync::Arc<LspInner>,
    waker: TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        let Ok(db) = Db::open() else {
            return;
        };
        if build_graph(&root, &lsp, &db) {
            let _ = waker.wake();
        }
    });
}

/// A parsed source file: its content hash + tree-sitter entities (full 1-based
/// inclusive line spans, used to resolve a caller line to its enclosing entity).
struct FileParse {
    hash: String,
    entities: Vec<Entity>,
}

/// Build (incrementally) the worktree's blast-radius graph from `git diff HEAD`.
/// Returns `true` if any edges were (re)written.
fn build_graph(root: &Path, lsp: &LspInner, db: &Db) -> bool {
    let loc = GitLoc::for_worktree(root);
    let root_s = root.to_string_lossy().into_owned();
    // Same sanitized flags the semantic footer uses so the patch parses cleanly.
    let Some(diff) = loc.git_out(&[
        "-c",
        "diff.noprefix=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-renames",
        "-U3",
        "HEAD",
    ]) else {
        return false;
    };
    let files = superzej_core::patch::parse_patch(&diff);
    if files.is_empty() || files.len() > MAX_CHANGED_FILES {
        return false;
    }

    // Per-file parse cache, keyed by absolute path (caller files are re-used
    // across changed callees).
    let mut cache: HashMap<String, FileParse> = HashMap::new();
    let mut changed_any = false;

    for f in &files {
        let Some(lang) = Lang::from_path(&f.new_path) else {
            continue;
        };
        let abs = root.join(&f.new_path);
        let abs_s = abs.to_string_lossy().into_owned();
        let Ok(src) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let hash = source_hash(&src);
        // Incremental skip: unchanged source ⇒ its edges are already current.
        if db.file_source_hash(&abs_s).ok().flatten().as_deref() == Some(hash.as_str()) {
            continue;
        }

        let file_entities = semantic::parse_entities(&src, lang);
        let changed = semantic::entities_for_diff(&src, lang, &f.hunks);

        // Persist this file's entities (drops entities that vanished) + hash.
        let rows = entity_rows(&root_s, &abs_s, &file_entities, &hash);
        let _ = db.replace_file_entities(&abs_s, &rows);
        // Seed the cache with the just-parsed data (avoids a re-read below).
        cache.insert(
            abs_s.clone(),
            FileParse {
                hash: hash.clone(),
                entities: file_entities.clone(),
            },
        );

        if changed.is_empty() {
            continue;
        }

        // A server is required for edges; without one, entities+hash are still
        // recorded above so the footer's intra-diff summary is unaffected.
        let Ok(client) = lsp.client(root, lang) else {
            continue;
        };
        let uri = path_to_uri(&abs_s);
        let _ = client.did_open(&uri, lang, &src);
        // Precise name (selectionRange) positions for the references query.
        let symbols = client.document_symbols(&uri).unwrap_or_default();

        let mut callee_ids: Vec<String> = Vec::new();
        let mut edges: Vec<SemEdgeRow> = Vec::new();
        let mut caller_upserts: Vec<SemEntityRow> = Vec::new();

        for ch in &changed {
            // The full-span entity for this change (same (name, start_line) key).
            let Some(ent) = file_entities
                .iter()
                .find(|e| e.name == ch.name && e.start_line == ch.start_line)
            else {
                continue;
            };
            let callee_id = entity_id(&root_s, &abs_s, &ent.name, ent.kind);
            callee_ids.push(callee_id.clone());
            // Query position = the symbol's name; fall back is to skip (a
            // keyword-position query yields no references for most servers).
            let Some(pos) = symbol_pos(&symbols, &ent.name, ent.start_line, ent.end_line) else {
                continue;
            };
            let Ok(locs) = client.references(&uri, pos) else {
                continue;
            };
            for l in locs {
                let cfile = l.path.clone();
                let cline = l.line_1based();
                // Skip self-references within the callee's own body.
                if cfile == abs_s && ent.contains(cline) {
                    continue;
                }
                ensure_parsed(&mut cache, &cfile);
                let Some(parse) = cache.get(&cfile) else {
                    continue;
                };
                // Innermost enclosing entity (last-by-start), mirroring the core
                // mapper's rule.
                let Some(caller) = parse.entities.iter().rev().find(|e| e.contains(cline)) else {
                    continue;
                };
                let caller_id = entity_id(&root_s, &cfile, &caller.name, caller.kind);
                let is_test = is_test_entity(&cfile, &caller.name);
                caller_upserts.push(SemEntityRow {
                    id: caller_id.clone(),
                    file: cfile.clone(),
                    name: caller.name.clone(),
                    kind: caller.kind,
                    start_line: caller.start_line,
                    end_line: caller.end_line,
                    source_hash: parse.hash.clone(),
                });
                edges.push(SemEdgeRow {
                    src_id: caller_id,
                    dst_id: callee_id.clone(),
                    kind: if is_test { "test" } else { "ref" }.to_string(),
                });
            }
        }

        for c in &caller_upserts {
            let _ = db.upsert_entity(c);
        }
        let _ = db.replace_edges_for_dsts(&callee_ids, &edges);
        changed_any = true;
    }

    changed_any
}

/// Ensure `abs` (an absolute path) is parsed into the cache. Best-effort: an
/// unreadable / unsupported-language file simply leaves no entry.
fn ensure_parsed(cache: &mut HashMap<String, FileParse>, abs: &str) {
    if cache.contains_key(abs) {
        return;
    }
    if let Some(lang) = Lang::from_path(abs)
        && let Ok(src) = std::fs::read_to_string(abs)
    {
        let entities = semantic::parse_entities(&src, lang);
        cache.insert(
            abs.to_string(),
            FileParse {
                hash: source_hash(&src),
                entities,
            },
        );
    }
}

/// Read the blast-radius for a diff's changed entities from the persisted graph
/// (no LSP at read time). Returns `None` — so the footer falls back to the
/// intra-diff summary — when the graph knows of no callers (LSP off, graph not
/// yet built, or the change genuinely has no dependents). Shared by the footer
/// (`build_panel`) and the `blast_radius` MCP tool's host-side callers.
pub(crate) fn read_blast(root: &Path, summary: &EntitySummary, db: &Db) -> Option<BlastRadius> {
    use std::collections::BTreeMap;
    let root_s = root.to_string_lossy().into_owned();
    let mut changed: Vec<ChangedEntity> = Vec::new();
    let mut callers_by: BTreeMap<String, Vec<CallerRef>> = BTreeMap::new();
    for (file, changes) in &summary.per_file {
        let abs = root.join(file).to_string_lossy().into_owned();
        for ch in changes {
            let id = entity_id(&root_s, &abs, &ch.name, ch.kind);
            let callers: Vec<CallerRef> = db
                .callers_of(&id)
                .unwrap_or_default()
                .into_iter()
                .map(|r| CallerRef {
                    id: r.id,
                    file: r.file,
                    name: r.name,
                    kind: r.kind,
                })
                .collect();
            if !callers.is_empty() {
                callers_by.insert(id.clone(), callers);
            }
            changed.push(ChangedEntity {
                id,
                name: ch.name.clone(),
                kind: ch.kind,
                touch: ch.touch,
            });
        }
    }
    let total_callers: usize = callers_by.values().map(Vec::len).sum();
    if total_callers == 0 {
        return None;
    }
    Some(compute_blast_radius(&changed, &callers_by))
}

/// Build `SemEntityRow`s for a file's entities.
fn entity_rows(repo: &str, abs_file: &str, entities: &[Entity], hash: &str) -> Vec<SemEntityRow> {
    entities
        .iter()
        .map(|e| SemEntityRow {
            id: entity_id(repo, abs_file, &e.name, e.kind),
            file: abs_file.to_string(),
            name: e.name.clone(),
            kind: e.kind,
            start_line: e.start_line,
            end_line: e.end_line,
            source_hash: hash.to_string(),
        })
        .collect()
}

/// The name-identifier position for `name`, chosen from the document symbols
/// whose selection line falls within `[start_line, end_line]` (1-based).
fn symbol_pos(
    symbols: &[superzej_svc::lsp::SymbolInfo],
    name: &str,
    start_line: u32,
    end_line: u32,
) -> Option<Position> {
    symbols
        .iter()
        .filter(|s| s.name == name)
        .map(|s| s.location.range.start)
        .find(|p| {
            let l = p.line.saturating_add(1);
            l >= start_line && l <= end_line
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_hash_is_stable_and_sensitive() {
        assert_eq!(source_hash("abc"), source_hash("abc"));
        assert_ne!(source_hash("abc"), source_hash("abd"));
    }

    #[test]
    fn read_blast_falls_back_when_graph_empty_then_enriches() {
        use superzej_core::semantic::{EntityChange, EntityKind, EntitySummary, Touch};
        use superzej_core::store::{SemEdgeRow, SemEntityRow};

        let db = Db::open_memory().unwrap();
        let root = Path::new("/wt");
        let summary = EntitySummary::new(vec![(
            "src/lib.rs".to_string(),
            vec![EntityChange {
                kind: EntityKind::Function,
                name: "target".to_string(),
                added: 1,
                deleted: 0,
                touch: Touch::Modified,
                start_line: 10,
            }],
        )]);

        // Empty graph → no callers → None (footer keeps its intra-diff summary).
        assert!(read_blast(root, &summary, &db).is_none());

        // Seed a caller edge for the changed entity, then it enriches.
        let abs = root.join("src/lib.rs").to_string_lossy().into_owned();
        let callee = entity_id("/wt", &abs, "target", EntityKind::Function);
        let caller = entity_id("/wt", "/wt/src/use.rs", "user", EntityKind::Function);
        db.upsert_entity(&SemEntityRow {
            id: caller.clone(),
            file: "/wt/src/use.rs".to_string(),
            name: "user".to_string(),
            kind: EntityKind::Function,
            start_line: 1,
            end_line: 5,
            source_hash: "h".to_string(),
        })
        .unwrap();
        db.replace_edges_for_dsts(
            std::slice::from_ref(&callee),
            &[SemEdgeRow {
                src_id: caller,
                dst_id: callee.clone(),
                kind: "ref".to_string(),
            }],
        )
        .unwrap();

        let br = read_blast(root, &summary, &db).expect("blast present");
        assert_eq!(br.changed, 1);
        assert_eq!(br.callers, 1);
        assert_eq!(br.untested, 1); // no test caller
    }

    #[test]
    fn symbol_pos_matches_name_within_span() {
        use superzej_svc::lsp::{Location, Position, Range, SymbolInfo, SymbolKind};
        let sym = |name: &str, line: u32, ch: u32| SymbolInfo {
            name: name.to_string(),
            kind: SymbolKind::Function,
            location: Location {
                path: "f.rs".into(),
                range: Range {
                    start: Position {
                        line,
                        character: ch,
                    },
                    end: Position {
                        line,
                        character: ch,
                    },
                },
            },
            container: None,
        };
        let syms = vec![sym("foo", 4, 3), sym("bar", 20, 3)];
        // foo's name is on 0-based line 4 → 1-based 5, within [3, 10].
        let p = symbol_pos(&syms, "foo", 3, 10).unwrap();
        assert_eq!(p.line, 4);
        assert_eq!(p.character, 3);
        // No symbol for a name outside any span.
        assert!(symbol_pos(&syms, "bar", 1, 5).is_none());
        assert!(symbol_pos(&syms, "missing", 1, 100).is_none());
    }
}
