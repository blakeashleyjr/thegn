//! Worktree-wide semantic **entity index** builder — the I/O half that grows
//! the persistent `sem_entity` store from diff-scoped (the blast-radius builder)
//! to worktree-wide, so the repo map ([`thegn_core::repo_map`], `thegn map`, the
//! `semantic.map` MCP tool) has an index to render from.
//!
//! Runs entirely off the event loop (`spawn_blocking`, thread QoS `Background`):
//! it walks the worktree's **git file listing** (never raw `readdir`, so ignored
//! / vendored trees are skipped), parses each tree-sitter-served file with
//! [`thegn_core::semantic::parse_entities`], and writes the rows via
//! `replace_file_entities`. Capped by `[semantic] index_max_files` — an oversized
//! worktree yields an honestly *partial* index rather than unbounded work.
//! Incremental by construction: a file whose `source_hash` is unchanged is
//! skipped, and edges stay the blast-radius builder's job (no LSP here).
//!
//! The CLI/MCP surfaces call [`load_repo_map`] / [`crawl_worktree`] directly (it
//! is their own process's time); the compositor drives [`maybe_spawn_crawl`] off
//! the loop.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use thegn_core::db::Db;
use thegn_core::remote::GitLoc;
use thegn_core::repo_map::{MapEntity, RepoMap};
use thegn_core::semantic::{self, Lang};
use thegn_core::semantic_graph::entity_id;
use thegn_core::store::{SemEntityRow, SemanticStore};

use termwiz::terminal::TerminalWaker;

/// A cheap, stable content hash for the source-changed skip check — must match
/// the blast-radius builder's so the two share the `source_hash` skip on the
/// same files.
fn source_hash(src: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The result of one crawl pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CrawlOutcome {
    /// Any file's entity rows were (re)written this pass (⇒ pulse the waker).
    pub changed: bool,
    /// Tree-sitter-served files present in the git listing (before the cap).
    pub ts_files: usize,
    /// Files actually parsed this pass (≤ the cap).
    pub indexed: usize,
    /// The listing exceeded the cap: the index is honestly partial.
    pub partial: bool,
}

/// The git-listed, tree-sitter-served files under `root`, as paths relative to
/// it. Tracked files only (`git ls-files`), so `.gitignore`d / vendored trees
/// are excluded by construction — the "walk the git listing, never readdir"
/// contract. NUL-separated so paths with spaces need no unquoting.
fn git_ts_files(root: &Path) -> Vec<PathBuf> {
    let loc = GitLoc::for_worktree(root);
    let Some(out) = loc.git_out(&["ls-files", "-z"]) else {
        return Vec::new();
    };
    out.split('\0')
        .filter(|s| !s.is_empty())
        .filter(|rel| Lang::from_path(rel).is_some())
        .map(PathBuf::from)
        .collect()
}

/// Crawl `root`'s git-listed tree-sitter files into the entity index, capped at
/// `cap` files. Idempotent: unchanged files (by `source_hash`) are skipped, so a
/// re-crawl is cheap and safe to run inline or repeatedly. Best-effort — an
/// unreadable file is skipped, never fatal.
pub fn crawl_worktree(root: &Path, cap: usize, db: &Db) -> CrawlOutcome {
    let cap = cap.max(1);
    let root_s = root.to_string_lossy().into_owned();
    let files = git_ts_files(root);
    let ts_files = files.len();
    let partial = ts_files > cap;

    let mut outcome = CrawlOutcome {
        changed: false,
        ts_files,
        indexed: 0,
        partial,
    };

    for rel in files.into_iter().take(cap) {
        let Some(lang) = Lang::from_path(&rel.to_string_lossy()) else {
            continue;
        };
        let abs = root.join(&rel);
        let abs_s = abs.to_string_lossy().into_owned();
        let Ok(src) = std::fs::read_to_string(&abs) else {
            continue;
        };
        outcome.indexed += 1;
        let hash = source_hash(&src);
        // Incremental skip: unchanged source ⇒ its rows are already current
        // (whether written here or by the blast-radius builder — same hash).
        if db.file_source_hash(&abs_s).ok().flatten().as_deref() == Some(hash.as_str()) {
            continue;
        }
        let entities = semantic::parse_entities(&src, lang);
        let rows: Vec<SemEntityRow> = entities
            .iter()
            .map(|e| SemEntityRow {
                id: entity_id(&root_s, &abs_s, &e.name, e.kind),
                file: abs_s.clone(),
                name: e.name.clone(),
                kind: e.kind,
                start_line: e.start_line,
                end_line: e.end_line,
                source_hash: hash.clone(),
            })
            .collect();
        // best-effort: the index is derived state (a fresh DB rebuilds it).
        if db.replace_file_entities(&abs_s, &rows).is_ok() {
            outcome.changed = true;
        }
    }
    outcome
}

/// The read side: build the ranked repo map for `root` from the index, running
/// a capped inline crawl first when the index is empty (the CLI/MCP process's
/// own time — a compositor would have crawled already). `file_filter` narrows to
/// one file's outline (path relative to `root`).
///
/// Returns the [`RepoMap`] plus whether the worktree has **any**
/// tree-sitter-served files at all, so a caller can say "no indexable files"
/// distinctly from "empty map".
pub struct MapLoad {
    pub map: RepoMap,
    /// The worktree has ≥1 tree-sitter-served file in its git listing.
    pub has_ts_files: bool,
}

pub fn load_repo_map(root: &Path, cap: usize, db: &Db, file_filter: Option<&str>) -> MapLoad {
    let root_s = root.to_string_lossy().into_owned();

    // Inline first-use crawl when the index is empty for this worktree.
    let mut rows = db.entities_under(&root_s).unwrap_or_default();
    let mut ts_files = git_ts_files(root).len();
    if rows.is_empty() && ts_files > 0 {
        let outcome = crawl_worktree(root, cap, db);
        ts_files = outcome.ts_files;
        rows = db.entities_under(&root_s).unwrap_or_default();
    }
    // Partial iff the listing outran the cap (independent of empty-entity files).
    let partial = ts_files > cap.max(1);

    // Degrees for the ranking signal, indexed by entity id.
    let degrees: std::collections::HashMap<String, u32> = db
        .caller_degrees()
        .unwrap_or_default()
        .into_iter()
        .collect();

    let root_prefix = format!("{root_s}/");
    let want_file = file_filter.map(|f| f.trim_start_matches("./").to_string());

    let mut entities: Vec<MapEntity> = Vec::new();
    for r in rows {
        // Relativize the absolute store path for display.
        let rel = r
            .file
            .strip_prefix(&root_prefix)
            .unwrap_or(&r.file)
            .to_string();
        if let Some(want) = &want_file
            && &rel != want
        {
            continue;
        }
        entities.push(MapEntity {
            kind: r.kind,
            name: r.name,
            file: rel,
            line: r.start_line,
            degree: degrees.get(&r.id).copied().unwrap_or(0),
        });
    }

    MapLoad {
        map: RepoMap::new(entities, partial),
        has_ts_files: ts_files > 0,
    }
}

// ── Loop-side trigger ────────────────────────────────────────────────────────

/// Debounce between full crawls of the same root — cheap because unchanged
/// files are skipped, so this only guards rapid successive refreshes. Longer
/// than the blast-radius builder's (1.5s): a worktree-wide walk is heavier than
/// re-parsing the diff's files, and committed source changes far less often.
const CRAWL_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(20);

thread_local! {
    /// Per-root last-crawl time (the loop is single-threaded). A root absent
    /// here has never been crawled this session ⇒ crawl immediately (first
    /// open); present-and-recent ⇒ skip (debounce). Kept here, not in `run.rs`,
    /// so the loop hook is one call.
    static LAST_CRAWL: std::cell::RefCell<std::collections::HashMap<PathBuf, std::time::Instant>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Whether the active root is due for a crawl: never crawled this session, or
/// its debounce elapsed. Records the decision (marks it crawled now) so the same
/// tick's repeat refreshes don't re-trigger.
fn due(root: &Path) -> bool {
    LAST_CRAWL.with(|m| {
        let mut m = m.borrow_mut();
        let now = std::time::Instant::now();
        match m.get(root) {
            Some(t) if now.duration_since(*t) < CRAWL_DEBOUNCE => false,
            _ => {
                m.insert(root.to_path_buf(), now);
                true
            }
        }
    })
}

/// Loop-side trigger: crawl the active worktree's entity index off the event
/// loop when `should` (the index is enabled ∧ the model refreshed), throttled
/// per root. Kept out of `run.rs` (god-file ratchet); the loop calls this in one
/// statement. Pulses the waker only when the index actually changed (so the
/// focused file's fallback outline re-hydrates) — never on an idle no-op, so the
/// 0%-idle contract holds.
pub(crate) fn maybe_spawn_crawl(
    should: bool,
    cwd: Option<PathBuf>,
    cap: usize,
    waker: &TerminalWaker,
) {
    if !should {
        return;
    }
    let Some(cwd) = cwd else {
        return;
    };
    if !due(&cwd) {
        return;
    }
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        let Ok(db) = Db::open() else {
            return;
        };
        if crawl_worktree(&cwd, cap, &db).changed {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Query the entity index for symbols whose name contains `query`
/// (case-insensitive), for the LSP-less symbol-search fallback. Returns up to
/// `max` `(relative_path, 1-based line, name, kind_label)` tuples. Pure DB read;
/// the caller runs it off the loop.
pub fn index_symbol_matches(
    root: &Path,
    query: &str,
    max: usize,
    db: &Db,
) -> Vec<(String, u64, String, &'static str)> {
    let root_s = root.to_string_lossy().into_owned();
    let root_prefix = format!("{root_s}/");
    let needle = query.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut seen: HashSet<(String, u32)> = HashSet::new();
    for r in db.entities_under(&root_s).unwrap_or_default() {
        if out.len() >= max {
            break;
        }
        if !needle.is_empty() && !r.name.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        let rel = r
            .file
            .strip_prefix(&root_prefix)
            .unwrap_or(&r.file)
            .to_string();
        if seen.insert((rel.clone(), r.start_line)) {
            out.push((rel, r.start_line as u64, r.name, r.kind.label()));
        }
    }
    out
}
