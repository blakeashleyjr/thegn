//! Semantic blast-radius graph **builder** (items 313/316) — the I/O half that
//! feeds the pure [`thegn_core::semantic_graph`] logic and persists the graph
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

use thegn_core::db::Db;
use thegn_core::remote::GitLoc;
use thegn_core::semantic::{self, Entity, EntitySummary, Lang};
use thegn_core::semantic_graph::{
    BlastRadius, CallerRef, ChangedEntity, compute_blast_radius, entity_id, is_test_entity,
};
use thegn_core::store::{SemEdgeRow, SemEntityRow, SemanticStore};
use thegn_svc::lsp::{CancelToken, LspError, Position, path_to_uri};

use termwiz::terminal::TerminalWaker;

use crate::lsp::LspInner;

/// Most changed files we process in one build (mirrors the hydration cap so a
/// sprawling change never balloons the LSP work).
const MAX_CHANGED_FILES: usize = 50;

type BlastInput = (
    Vec<ChangedEntity>,
    std::collections::BTreeMap<String, Vec<CallerRef>>,
);

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

/// The change print at which each worktree's graph last finished building. The
/// build is a pure function of the tree (`git diff HEAD` + file contents, with
/// unchanged sources skipped), so an unchanged print needs no new build; the
/// loop's per-tick trigger used to fork `git diff HEAD` every 5 s regardless
/// (THE-718). A path with no live watcher never skips.
static BUILT: crate::diff_watch::Seen = crate::diff_watch::Seen::new();
static BUILD_CANCEL: std::sync::Mutex<Option<(std::path::PathBuf, CancelToken)>> =
    std::sync::Mutex::new(None);
static BUILD_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn supersede_build(root: &Path) -> Option<(CancelToken, u64)> {
    let mut current = BUILD_CANCEL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((previous_root, previous)) = current.as_ref()
        && previous_root == root
        && !previous.is_cancelled()
    {
        return None;
    }
    if let Some((_, previous)) = current.as_ref() {
        previous.cancel();
    }
    let token = CancelToken::new();
    let generation = BUILD_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    *current = Some((root.to_path_buf(), token.clone()));
    Some((token, generation))
}

fn finish_build(root: &Path, generation: u64) {
    let mut current = BUILD_CANCEL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current.as_ref().is_some_and(|(current_root, _)| {
        current_root == root
            && BUILD_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
                == generation.wrapping_add(1)
    }) {
        *current = None;
    }
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
    let (print, unchanged) = BUILT.check(&cwd);
    if unchanged {
        return;
    }
    LAST_BUILD.set(Some(std::time::Instant::now()));
    spawn_graph_build(cwd, lsp, waker.clone(), print);
}

/// Spawn an off-loop build of the worktree's blast-radius graph. Best-effort:
/// pulses the waker only when the graph changed (so the footer re-hydrates).
pub(crate) fn spawn_graph_build(
    root: std::path::PathBuf,
    lsp: std::sync::Arc<LspInner>,
    waker: TerminalWaker,
    print: Option<crate::diff_watch::WatchPrint>,
) {
    let Some((cancel, generation)) = supersede_build(&root) else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        if let Ok(db) = Db::open() {
            // `None` = the diff could not be read: not a completed pass, so it is
            // not recorded and the next tick retries.
            if let Some(changed) = build_graph(&root, &lsp, &db, &cancel) {
                BUILT.mark(&root, print);
                if changed {
                    let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
                }
            }
        }
        finish_build(&root, generation);
    });
}

fn invalidate_file_hash(db: &Db, root: &str, file: &str, entities: &[Entity]) {
    let rows = entity_rows(root, file, entities, "");
    if let Err(error) = db.replace_file_entities(file, &rows) {
        tracing::debug!(target: "thegn::blast_radius", %error, file, "failed to invalidate cancelled blast-radius file hash");
    }
}

fn invalidate_cancelled_file(
    db: &Db,
    cancel: &CancelToken,
    root: &str,
    file: &str,
    entities: &[Entity],
) -> bool {
    if !cancel.is_cancelled() {
        return false;
    }
    invalidate_file_hash(db, root, file, entities);
    true
}

/// A parsed source file: its content hash + tree-sitter entities (full 1-based
/// inclusive line spans, used to resolve a caller line to its enclosing entity).
struct FileParse {
    hash: String,
    entities: Vec<Entity>,
}

/// Build (incrementally) the worktree's blast-radius graph from `git diff HEAD`.
/// Returns `Some(true)` if any edges were (re)written, `Some(false)` for a
/// completed pass that wrote none, and `None` when the diff could not be read.
fn build_graph(root: &Path, lsp: &LspInner, db: &Db, cancel: &CancelToken) -> Option<bool> {
    build_graph_with_file_persist_hook(root, lsp, db, cancel, || {})
}

fn build_graph_with_file_persist_hook(
    root: &Path,
    lsp: &LspInner,
    db: &Db,
    cancel: &CancelToken,
    mut after_file_persist: impl FnMut(),
) -> Option<bool> {
    let loc = GitLoc::for_worktree(root);
    let root_s = root.to_string_lossy().into_owned();
    let diff = head_diff_text(&loc)?;
    let files = thegn_core::patch::parse_patch(&diff);
    if files.is_empty() || files.len() > MAX_CHANGED_FILES {
        return Some(false);
    }

    // Per-file parse cache, keyed by absolute path (caller files are re-used
    // across changed callees).
    let mut cache: HashMap<String, FileParse> = HashMap::new();
    let mut changed_any = false;

    for f in &files {
        if cancel.is_cancelled() {
            return None;
        }
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
        let _ = db.replace_file_entities(&abs_s, &rows); // best-effort: cache write: the symbol graph is derived state; the DB is a cache
        after_file_persist();
        if invalidate_cancelled_file(db, cancel, &root_s, &abs_s, &file_entities) {
            return None;
        }
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
        // recorded above so the footer's intra-diff summary is unaffected. The
        // semantic graph is tree-sitter-tier (the `Lang::from_path` guard above),
        // so the registry key here always maps to a grammar too.
        let Some(key) = lsp.resolve_key(&f.new_path) else {
            continue;
        };
        let client = match lsp.client_with_cancel(root, &key, Some(cancel)) {
            Ok(client) => client,
            Err(LspError::Cancelled) => {
                invalidate_cancelled_file(db, cancel, &root_s, &abs_s, &file_entities);
                return None;
            }
            Err(LspError::Overloaded) => {
                tracing::debug!(target: "thegn::lsp", file = %f.new_path, "blast radius skipped an overloaded LSP file");
                continue;
            }
            Err(_) => continue,
        };
        let uri = path_to_uri(&abs_s);
        let _ = client.did_open(&uri, &src); // best-effort: LSP did_open is advisory: a missed open just means the server discovers the file itself
        // Precise name (selectionRange) positions for the references query.
        let symbols = match client.document_symbols_with(&uri, cancel) {
            Ok(symbols) => symbols,
            Err(LspError::Cancelled) => {
                invalidate_cancelled_file(db, cancel, &root_s, &abs_s, &file_entities);
                return None;
            }
            Err(LspError::Overloaded) => {
                tracing::debug!(target: "thegn::lsp", file = %f.new_path, "blast radius skipped an overloaded LSP file");
                continue;
            }
            Err(_) => continue,
        };

        let mut callee_ids: Vec<String> = Vec::new();
        let mut edges: Vec<SemEdgeRow> = Vec::new();
        let mut caller_upserts: Vec<SemEntityRow> = Vec::new();
        let mut file_overloaded = false;

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
            if cancel.is_cancelled() {
                invalidate_cancelled_file(db, cancel, &root_s, &abs_s, &file_entities);
                return None;
            }
            let locs = match client.references_with(&uri, pos, cancel) {
                Ok(locs) => locs,
                Err(LspError::Cancelled) => {
                    invalidate_cancelled_file(db, cancel, &root_s, &abs_s, &file_entities);
                    return None;
                }
                Err(LspError::Overloaded) => {
                    file_overloaded = true;
                    continue;
                }
                Err(_) => continue,
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

        if file_overloaded {
            tracing::debug!(target: "thegn::lsp", file = %f.new_path, "blast radius skipped overloaded references in an LSP file");
        }

        for c in &caller_upserts {
            let _ = db.upsert_entity(c); // best-effort: cache write: the symbol graph is derived state; the DB is a cache
        }
        let _ = db.replace_edges_for_dsts(&callee_ids, &edges); // best-effort: cache write: the symbol graph is derived state; the DB is a cache
        changed_any = true;
    }

    Some(changed_any)
}

/// Read the semantic graph's diff. A failing diff is an empty completed pass
/// only when HEAD is unborn; other failures must remain retryable.
fn head_diff_text(loc: &GitLoc) -> Option<String> {
    // Same sanitized flags the semantic footer uses so the patch parses cleanly.
    // off-loop: this runs inside `spawn_graph_build`'s blocking task.
    let out = crate::bounded_git_probe::capture(
        loc.git_command(&[
            "-c",
            "diff.noprefix=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            "-U3",
            "HEAD",
        ]),
        None,
        "blast-radius diff",
    );
    if let Ok(stdout) = out {
        return Some(String::from_utf8_lossy(&stdout).trim().to_string());
    }

    let head = crate::bounded_git_probe::capture_capability(
        loc.git_command(&["rev-parse", "--verify", "-q", "HEAD^{commit}"]),
        std::time::Duration::from_secs(5),
        16 * 1024,
    )
    .ok()?;
    if head.status.success() {
        None
    } else {
        Some(String::new())
    }
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
    let (changed, callers_by) = match collect_changed_and_callers(root, summary, |id| {
        db.callers_of(id)
    }) {
        Ok(result) => result,
        Err(error) => {
            tracing::debug!(target: "thegn::blast_radius", %error, "semantic caller lookup failed");
            return None;
        }
    };
    let total_callers: usize = callers_by.values().map(Vec::len).sum();
    if total_callers == 0 {
        return None;
    }
    Some(compute_blast_radius(&changed, &callers_by))
}

/// Collect a diff summary's changed entities and each one's persisted callers
/// (no LSP at read time) — the shared input for both the aggregate
/// [`read_blast`] and the detailed [`blast_report_for_worktree`].
fn collect_changed_and_callers(
    root: &Path,
    summary: &EntitySummary,
    mut caller_lookup: impl FnMut(&str) -> anyhow::Result<Vec<SemEntityRow>>,
) -> anyhow::Result<BlastInput> {
    use std::collections::BTreeMap;
    let root_s = root.to_string_lossy().into_owned();
    let mut changed: Vec<ChangedEntity> = Vec::new();
    let mut callers_by: BTreeMap<String, Vec<CallerRef>> = BTreeMap::new();
    for (file, changes) in &summary.per_file {
        let abs = root.join(file).to_string_lossy().into_owned();
        for ch in changes {
            let id = entity_id(&root_s, &abs, &ch.name, ch.kind);
            let callers: Vec<CallerRef> = caller_lookup(&id)
                .map_err(|error| anyhow::anyhow!("semantic caller lookup failed: {error}"))?
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
    Ok((changed, callers_by))
}

/// The detailed, serializable blast-radius report for a worktree's current
/// changes — the host side of the `semantic.blast_radius` MCP tool. Computes the
/// entity summary from `git diff HEAD`, resolves each changed entity's callers
/// from the persisted graph, and returns the
/// [`BlastReport`](thegn_core::semantic_graph::BlastReport). `None` when the
/// graph contributes nothing (no changes with resolvable callers) — the tool
/// renders that as a clear "graph unavailable" result, per the spec.
pub(crate) fn blast_report_for_worktree(
    root: &Path,
    db: &Db,
) -> Option<thegn_core::semantic_graph::BlastReport> {
    use thegn_core::semantic_graph::compute_blast_report;
    let loc = GitLoc::for_worktree(root);
    let entries = crate::git_handle::get().diff_files(&loc, "HEAD").ok()?;
    let summary = crate::hydrate_semantic::compute_entity_summary(&loc, &entries)?;
    let (changed, callers_by) = match collect_changed_and_callers(root, &summary, |id| {
        db.callers_of(id)
    }) {
        Ok(result) => result,
        Err(error) => {
            tracing::debug!(target: "thegn::blast_radius", %error, "semantic caller lookup failed");
            return None;
        }
    };
    let total_callers: usize = callers_by.values().map(Vec::len).sum();
    if total_callers == 0 {
        return None;
    }
    Some(compute_blast_report(&changed, &callers_by))
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
    symbols: &[thegn_svc::lsp::SymbolInfo],
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
    fn a_same_root_build_does_not_cancel_an_in_flight_build() {
        let root = Path::new("/wt/same-root");
        let (previous, generation) = supersede_build(root).unwrap();
        assert!(supersede_build(root).is_none());
        assert!(!previous.is_cancelled());
        finish_build(root, generation);
    }

    #[test]
    fn a_different_root_build_cancels_the_previous_token() {
        let previous_root = Path::new("/wt/previous-root");
        let current_root = Path::new("/wt/current-root");
        let (previous, _) = supersede_build(previous_root).unwrap();
        let (current, generation) = supersede_build(current_root).unwrap();
        assert!(previous.is_cancelled());
        assert!(!current.is_cancelled());
        finish_build(current_root, generation);
    }

    #[test]
    fn build_graph_with_a_cancelled_token_returns_none() {
        fn git(root: &Path, args: &[&str]) {
            let mut command = thegn_core::util::git_cmd(root);
            command
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null");
            crate::bounded_git_probe::capture(command, None, "blast-radius cancellation fixture")
                .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
        }

        let repo = tempfile::Builder::new()
            .prefix("thegn-blast-cancel-")
            .tempdir()
            .unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        git(
            repo.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        let source = repo.path().join("src/lib.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "pub fn target() {}\n").unwrap();
        git(repo.path(), &["add", "src/lib.rs"]);
        git(
            repo.path(),
            &["-c", "commit.gpgsign=false", "commit", "-qm", "seed"],
        );
        std::fs::write(&source, "pub fn target() { target_inner(); }\n").unwrap();

        let config = thegn_core::config::Config {
            lsp: thegn_core::config::LspConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let supervisor = crate::lsp::LspSupervisor::from_config(&config);
        let lsp = supervisor.handle();
        let db = Db::open_memory().unwrap();
        let cancelled = CancelToken::new();
        cancelled.cancel();
        assert!(build_graph(repo.path(), &lsp, &db, &cancelled).is_none());

        let cancelled_after_persist = CancelToken::new();
        assert!(
            build_graph_with_file_persist_hook(
                repo.path(),
                &lsp,
                &db,
                &cancelled_after_persist,
                || cancelled_after_persist.cancel(),
            )
            .is_none()
        );
        let abs_source = source.to_string_lossy().into_owned();
        let current_hash = source_hash(&std::fs::read_to_string(&source).unwrap());
        assert_ne!(
            db.file_source_hash(&abs_source).unwrap().as_deref(),
            Some(current_hash.as_str())
        );

        let live = CancelToken::new();
        assert!(build_graph(repo.path(), &lsp, &db, &live).is_some());
    }

    #[test]
    fn source_hash_is_stable_and_sensitive() {
        assert_eq!(source_hash("abc"), source_hash("abc"));
        assert_ne!(source_hash("abc"), source_hash("abd"));
    }

    #[test]
    fn read_blast_falls_back_when_graph_empty_then_enriches() {
        use thegn_core::semantic::{EntityChange, EntityKind, EntitySummary, Touch};
        use thegn_core::store::{SemEdgeRow, SemEntityRow};

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
    fn caller_lookup_error_aborts_the_whole_blast_input() {
        use thegn_core::semantic::{EntityChange, EntityKind, EntitySummary, Touch};

        let summary = EntitySummary::new(vec![(
            "src/lib.rs".to_string(),
            ["first", "second"]
                .into_iter()
                .map(|name| EntityChange {
                    kind: EntityKind::Function,
                    name: name.to_string(),
                    added: 1,
                    deleted: 0,
                    touch: Touch::Modified,
                    start_line: 1,
                })
                .collect(),
        )]);
        let mut calls = 0;
        let result = collect_changed_and_callers(Path::new("/wt"), &summary, |_| {
            calls += 1;
            if calls == 2 {
                anyhow::bail!("injected lookup failure");
            }
            Ok(Vec::new())
        });

        assert!(
            result.is_err(),
            "a later DB error must abort the full report"
        );
        assert_eq!(calls, 2, "both changed entities must reach the lookup seam");
    }

    #[test]
    fn head_diff_only_treats_an_unborn_head_as_an_empty_pass() {
        fn git(root: &Path, args: &[&str]) {
            let mut cmd = thegn_core::util::git_cmd(root);
            cmd.args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null");
            crate::bounded_git_probe::capture(cmd, None, "blast-radius test fixture")
                .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
        }

        let unborn = tempfile::tempdir().unwrap();
        git(unborn.path(), &["init", "-q"]);
        let loc = GitLoc::Local(unborn.path().to_path_buf());
        assert_eq!(head_diff_text(&loc).as_deref(), Some(""));

        let committed = tempfile::tempdir().unwrap();
        git(committed.path(), &["init", "-q"]);
        git(committed.path(), &["config", "user.name", "Test"]);
        git(
            committed.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(committed.path().join("tracked.txt"), "tracked\n").unwrap();
        git(committed.path(), &["add", "tracked.txt"]);
        git(
            committed.path(),
            &["-c", "commit.gpgsign=false", "commit", "-qm", "seed"],
        );
        std::fs::write(committed.path().join(".git/index"), b"not an index").unwrap();
        let loc = GitLoc::Local(committed.path().to_path_buf());
        assert_eq!(head_diff_text(&loc), None);
    }

    #[test]
    fn symbol_pos_matches_name_within_span() {
        use thegn_svc::lsp::{Location, Position, Range, SymbolInfo, SymbolKind};
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
