//! fff-search backend — the file-search engine for the palette + test-locate.
//!
//! Wraps [`fff_search`] (the Fabulous & Fast File Finder core) so the rest of
//! the host talks to a small, superzej-shaped surface instead of fff's stateful
//! `FilePicker`/LMDB API directly. Everything here is intended to be called from
//! `spawn_blocking` (fff scans + greps synchronously); nothing touches the event
//! loop.
//!
//! Design notes tying into superzej's invariants:
//!
//! * **Watcher-less pickers (0%-idle).** Pickers are built with `watch: false`
//!   and a synchronous scan (`new_with_shared_state` + `wait_for_scan`), so no
//!   fff filesystem-watcher thread persists after a search returns. The only
//!   background threads fff owns are its rayon pools, which park at 0% CPU when
//!   idle — same shape as tokio's blocking pool. Freshness comes from
//!   [`rebuild`] (the palette re-warms on Files-mode entry, mirroring the old
//!   `FileIndex::build` lifecycle).
//! * **git is the source of truth; the LMDB is a cache.** The frecency /
//!   query-history store lives beside the SQLite DB under
//!   `$XDG_STATE_HOME/superzej/fff/` and is strictly best-effort: if it fails to
//!   open (corruption, version skew) we fall back to `noop()` trackers and file
//!   ranking simply loses combo-boost — never an error on the user's path.
//! * **Substrate boundary.** fff (LMDB + vendored libgit2) lives only here in
//!   the host crate; the substrate-free core uses `neo_frizbee` directly for
//!   in-memory list fuzzing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use fff_search::file_picker::FilePicker;
use fff_search::frecency::FrecencyTracker;
use fff_search::query_tracker::QueryTracker;
use fff_search::{
    FFFMode, FilePickerOptions, FuzzySearchOptions, GrepMode, GrepSearchOptions, PaginationArgs,
    QueryParser, SharedFilePicker, SharedFrecency, SharedQueryTracker,
};

/// How long a synchronous scan may take before a search proceeds without it.
/// A cold scan of a very large tree can take a beat; the palette shows partial
/// (or empty) results and the next keystroke re-queries the now-warm picker.
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// Combo-boost: a query that has repeatedly opened a specific file lifts that
/// file on the next identical query. Mirrors fff's own defaults; 0 disables.
const COMBO_BOOST_MULTIPLIER: i32 = 1000;
const MIN_COMBO_COUNT: u32 = 1;

// ── Global state: per-root picker registry + shared LMDB trackers ─────────────

fn registry() -> &'static Mutex<HashMap<PathBuf, SharedFilePicker>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, SharedFilePicker>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-root cache of git-ignored relative paths (pass two of [`file_search`]).
/// The fff picker excludes ignored files, so they are gathered separately (once
/// per `rebuild`) and fuzzy-ranked in memory per keystroke.
fn ignored_registry() -> &'static Mutex<HashMap<PathBuf, Arc<Vec<String>>>> {
    static IGNORED: OnceLock<Mutex<HashMap<PathBuf, Arc<Vec<String>>>>> = OnceLock::new();
    IGNORED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cap on cached ignored paths per root — a heavy `node_modules`/`target` tree
/// can hold hundreds of thousands; bound memory and per-keystroke fuzzing cost.
const MAX_IGNORED: usize = 50_000;

/// List the paths git ignores under `root` (relative, forward-slashed), bounded
/// at [`MAX_IGNORED`]. Empty off a git repo or on any error. Blocking.
fn compute_ignored(root: &Path) -> Vec<String> {
    // `--others --ignored --exclude-standard` = files present but git-ignored;
    // `-z` = NUL-separated so paths with odd bytes survive. This is the precise
    // complement of the fff (non-ignored) set.
    #[expect(clippy::disallowed_methods)] // blocking git; called from spawn_blocking
    let out = superzej_core::util::git_cmd(root)
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    out.stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .take(MAX_IGNORED)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Cached ignored-path list for `root`, computing (and caching) it on first use.
/// Blocking on first use per root.
fn ignored_for(root: &Path) -> Arc<Vec<String>> {
    if let Ok(reg) = ignored_registry().lock()
        && let Some(v) = reg.get(root)
    {
        return v.clone();
    }
    let v = Arc::new(compute_ignored(root));
    if let Ok(mut reg) = ignored_registry().lock() {
        reg.insert(root.to_path_buf(), v.clone());
    }
    v
}

fn fff_dir() -> PathBuf {
    superzej_core::util::xdg_state_home()
        .join("superzej")
        .join("fff")
}

/// Process-wide file-access frecency store (best-effort; `noop` on failure).
fn frecency() -> &'static SharedFrecency {
    static FRECENCY: OnceLock<SharedFrecency> = OnceLock::new();
    FRECENCY.get_or_init(|| {
        let shared = SharedFrecency::default();
        let _ = std::fs::create_dir_all(fff_dir());
        match FrecencyTracker::open(fff_dir().join("frecency")) {
            Ok(tracker) => {
                if shared.init(tracker).is_err() {
                    return SharedFrecency::noop();
                }
                shared
            }
            Err(e) => {
                tracing::debug!("fff frecency open failed, ranking without it: {e}");
                SharedFrecency::noop()
            }
        }
    })
}

/// Process-wide query→file combo-boost store (best-effort; `noop` on failure).
fn query_tracker() -> &'static SharedQueryTracker {
    static QUERIES: OnceLock<SharedQueryTracker> = OnceLock::new();
    QUERIES.get_or_init(|| {
        let shared = SharedQueryTracker::default();
        let _ = std::fs::create_dir_all(fff_dir());
        match QueryTracker::open(fff_dir().join("queries")) {
            Ok(tracker) => {
                if shared.init(tracker).is_err() {
                    return SharedQueryTracker::noop();
                }
                shared
            }
            Err(e) => {
                tracing::debug!("fff query tracker open failed, no combo-boost: {e}");
                SharedQueryTracker::noop()
            }
        }
    })
}

// ── Picker lifecycle ──────────────────────────────────────────────────────────

/// Build a fresh watcher-less picker for `root` and synchronously scan it.
/// Blocking — call from `spawn_blocking`.
fn build_picker(root: &Path) -> Option<SharedFilePicker> {
    let shared = SharedFilePicker::default();
    let opts = FilePickerOptions {
        base_path: root.to_string_lossy().into_owned(),
        mode: FFFMode::Neovim,
        // No persistent fs-watcher thread — see module docs (0%-idle invariant).
        watch: false,
        ..Default::default()
    };
    if let Err(e) = FilePicker::new_with_shared_state(shared.clone(), frecency().clone(), opts) {
        tracing::debug!("fff picker build failed for {}: {e}", root.display());
        return None;
    }
    // Wait for the initial scan so the first search sees a populated index.
    shared.wait_for_scan(SCAN_TIMEOUT);
    Some(shared)
}

/// Force a fresh scan of `root`, replacing any cached picker. Returns the warm
/// handle. Called when (re)entering Files mode — mirrors the old
/// `FileIndex::build` "rebuild on mode entry" lifecycle. Blocking.
pub fn rebuild(root: &Path) -> Option<SharedFilePicker> {
    let handle = build_picker(root)?;
    if let Ok(mut reg) = registry().lock() {
        reg.insert(root.to_path_buf(), handle.clone());
    }
    // Refresh the git-ignored set (pass two of `file_search`) on the same beat.
    let ignored = Arc::new(compute_ignored(root));
    if let Ok(mut reg) = ignored_registry().lock() {
        reg.insert(root.to_path_buf(), ignored);
    }
    Some(handle)
}

/// Get the cached picker for `root`, building (and caching) one if absent.
/// Blocking on first use per root.
fn picker_for(root: &Path) -> Option<SharedFilePicker> {
    if let Ok(reg) = registry().lock()
        && let Some(handle) = reg.get(root)
    {
        return Some(handle.clone());
    }
    rebuild(root)
}

// ── Searches (all blocking; call from spawn_blocking) ─────────────────────────

/// A fuzzy path match: relative path + a display-order score (higher first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHit {
    pub path: String,
    pub score: u32,
}

/// Fuzzy file-path search over `root`, in two passes: tracked/non-ignored files
/// (fff, frecency- and combo-boost-weighted) rank first, then git-ignored files
/// surface after them (fuzzy-ranked). See [`merge_two_pass`]. Blocking.
pub fn file_search(root: &Path, query: &str, limit: usize) -> Vec<PathHit> {
    let pass1 = file_search_tracked(root, query, limit);
    // Empty query shows the tracked list only — ignored files would flood it.
    let pass2 = if query.is_empty() {
        Vec::new()
    } else {
        let ignored = ignored_for(root);
        let refs: Vec<&str> = ignored.iter().map(String::as_str).collect();
        fuzzy_rank(query, &refs)
            .into_iter()
            .map(|(i, score)| PathHit {
                path: ignored[i].clone(),
                score: u32::from(score),
            })
            .collect()
    };
    merge_two_pass(pass1, pass2, limit)
}

/// Concatenate the tracked pass ahead of the ignored pass: dedup ignored entries
/// already present in the tracked pass (tracked wins), cap at `limit`, then
/// re-stamp strictly-decreasing scores so every tracked hit outranks every
/// ignored hit — including at an equal fuzzy score — under any stable re-sort.
/// Pure over its inputs (unit-tested); each pass arrives already best-first.
pub fn merge_two_pass(pass1: Vec<PathHit>, pass2: Vec<PathHit>, limit: usize) -> Vec<PathHit> {
    let seen: std::collections::HashSet<&str> = pass1.iter().map(|h| h.path.as_str()).collect();
    let mut out = pass1.clone();
    for h in pass2 {
        if !seen.contains(h.path.as_str()) {
            out.push(h);
        }
    }
    out.truncate(limit);
    let n = out.len();
    for (i, h) in out.iter_mut().enumerate() {
        h.score = (n - i) as u32;
    }
    out
}

/// Tracked/non-ignored fuzzy file search (fff picker) — pass one of
/// [`file_search`]. Frecency- and combo-boost-weighted. Blocking.
fn file_search_tracked(root: &Path, query: &str, limit: usize) -> Vec<PathHit> {
    let Some(shared) = picker_for(root) else {
        return Vec::new();
    };
    let Ok(guard) = shared.read() else {
        return Vec::new();
    };
    let Some(picker) = guard.as_ref() else {
        return Vec::new();
    };

    let parsed = QueryParser::default().parse(query);
    let qt_guard = query_tracker().read().ok();
    let qt = qt_guard.as_ref().and_then(|g| g.as_ref());

    let result = picker.fuzzy_search(
        &parsed,
        qt,
        FuzzySearchOptions {
            max_threads: 0,
            combo_boost_score_multiplier: COMBO_BOOST_MULTIPLIER,
            min_combo_count: MIN_COMBO_COUNT,
            pagination: PaginationArgs { offset: 0, limit },
            ..Default::default()
        },
    );

    // Items arrive already ranked best-first; synthesize a strictly decreasing
    // score so any stable re-sort downstream preserves fff's order without us
    // coupling to fff's internal `Score` representation.
    let total = result.items.len();
    result
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| PathHit {
            path: item.relative_path(picker),
            score: (total - i) as u32,
        })
        .collect()
}

/// A content (grep) match, in superzej's shape.
pub struct ContentHit {
    pub path: String,
    pub line_no: u64,
    pub line_text: String,
}

/// Literal-substring content search over `root` (fff grep, PlainText/SIMD).
pub fn content_search(root: &Path, query: &str, limit: usize) -> Vec<ContentHit> {
    grep_collect(root, query, GrepMode::PlainText, limit, |m, path| {
        ContentHit {
            path,
            line_no: m.line_number,
            line_text: m.line_content.clone(),
        }
    })
}

/// Run a fff grep over `root` and map each match through `f`. Shared by content
/// search, the symbol sweep, and test-locate.
fn grep_collect<T>(
    root: &Path,
    query: &str,
    mode: GrepMode,
    limit: usize,
    mut f: impl FnMut(&fff_search::GrepMatch, String) -> T,
) -> Vec<T> {
    let Some(shared) = picker_for(root) else {
        return Vec::new();
    };
    let Ok(guard) = shared.read() else {
        return Vec::new();
    };
    let Some(picker) = guard.as_ref() else {
        return Vec::new();
    };

    let parsed = fff_search::parse_grep_query(query);
    let result = picker.grep(
        &parsed,
        &GrepSearchOptions {
            mode,
            page_limit: limit,
            ..Default::default()
        },
    );
    result
        .matches
        .iter()
        .map(|m| {
            let path = result
                .files
                .get(m.file_index)
                .map(|it| it.relative_path(picker))
                .unwrap_or_default();
            f(m, path)
        })
        .collect()
}

/// Regex grep over `root`, mapping each match to `(relative_path, line, line_text)`.
/// Used by the symbol sweep (the caller supplies the symbol regex).
pub fn regex_grep(root: &Path, pattern: &str, limit: usize) -> Vec<(String, u64, String)> {
    grep_collect(root, pattern, GrepMode::Regex, limit, |m, path| {
        (path, m.line_number, m.line_content.clone())
    })
}

/// Best-effort locate: return the first `(relative_path, 1-based line)` matching
/// any of `patterns` (regex). Replaces the old `rg`/`grep` subprocess.
pub fn locate(root: &Path, patterns: impl IntoIterator<Item = String>) -> Option<(String, usize)> {
    for pat in patterns {
        let hits = regex_grep(root, &pat, 1);
        if let Some((path, line, _)) = hits.into_iter().next() {
            return Some((path, line as usize));
        }
    }
    None
}

/// Source scan for test discovery: regex-grep `patterns` over `root`, keep only
/// files whose name matches one of `globs`, and emit rg-style `path:line:text`
/// lines (the format `task::parse_scan_output` consumes). Local worktrees only
/// — replaces the `rg`/`grep` discovery subprocess. Blocking.
pub fn scan(root: &Path, globs: &[&str], patterns: &[&str], limit: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for pat in patterns {
        for (rel, line_no, line) in regex_grep(root, pat, limit) {
            if !globs.is_empty() && !globs.iter().any(|g| glob_match(g, &rel)) {
                continue;
            }
            out.push_str(&rel);
            out.push(':');
            out.push_str(&line_no.to_string());
            out.push(':');
            out.push_str(&line);
            out.push('\n');
            count += 1;
            if count >= limit {
                return out;
            }
        }
    }
    out
}

/// Match a path's file name against a simple glob (`*` wildcard only — the test
/// discovery rulesets are all `*<suffix>` shapes). Anchors the non-`*` prefix
/// and suffix, requiring interior segments in order.
fn glob_match(glob: &str, path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let segs: Vec<&str> = glob.split('*').collect();
    if segs.len() == 1 {
        return glob == name;
    }
    let Some(rest) = name.strip_prefix(segs[0]) else {
        return false;
    };
    let last = segs[segs.len() - 1];
    let Some(mut mid) = rest.strip_suffix(last) else {
        return false;
    };
    for seg in &segs[1..segs.len() - 1] {
        if seg.is_empty() {
            continue;
        }
        match mid.find(seg) {
            Some(i) => mid = &mid[i + seg.len()..],
            None => return false,
        }
    }
    true
}

// ── Frecency recording ────────────────────────────────────────────────────────

/// Record that `rel_path` (relative to `root`) was opened for `query`, feeding
/// fff's frecency + combo-boost stores. Best-effort; never fails the open.
pub fn record_open(root: &Path, query: &str, rel_path: &str) {
    let abs = root.join(rel_path);
    if let Ok(guard) = frecency().read()
        && let Some(tracker) = guard.as_ref()
    {
        let _ = tracker.track_access(&abs);
    }
    if let Ok(mut guard) = query_tracker().write()
        && let Some(tracker) = guard.as_mut()
    {
        let _ = tracker.track_query_completion(query, root, &abs);
    }
}

// ── In-memory list fuzzing (nucleo replacement, via neo_frizbee) ──────────────

/// Rank `haystacks` against `needle`, returning `(original_index, score)` pairs
/// best-first. Non-matching entries are dropped. Empty `needle` returns every
/// index in input order with score 0 (so callers can show the full list).
pub fn fuzzy_rank(needle: &str, haystacks: &[&str]) -> Vec<(usize, u16)> {
    if needle.is_empty() {
        return (0..haystacks.len()).map(|i| (i, 0)).collect();
    }
    let mut matches = neo_frizbee::match_list(needle, haystacks, &neo_frizbee::Config::default());
    // `Match: Ord` is (score desc, index asc) — best-first.
    matches.sort_unstable();
    matches
        .into_iter()
        .map(|m| (m.index as usize, m.score))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_suffix_and_interior() {
        assert!(glob_match("*_test.go", "pkg/math_test.go"));
        assert!(glob_match("*_test.go", "math_test.go"));
        assert!(!glob_match("*_test.go", "src/math.go"));
        assert!(glob_match("*.test.ts", "a/b/foo.test.ts"));
        assert!(!glob_match("*.test.ts", "foo.ts"));
        assert!(glob_match("*Tests.swift", "MyThingTests.swift"));
        // interior wildcard + windows separator
        assert!(glob_match("foo*bar", "dir\\fooXYbar"));
        // no wildcard → exact filename
        assert!(glob_match("Makefile", "sub/Makefile"));
        assert!(!glob_match("Makefile", "sub/Makefile.in"));
    }

    #[test]
    fn fuzzy_rank_empty_needle_is_identity() {
        let hay = ["alpha", "beta", "gamma"];
        let ranked = fuzzy_rank("", &hay);
        assert_eq!(
            ranked.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn fuzzy_rank_orders_matches_and_drops_non_matches() {
        let hay = ["src/main.rs", "src/lib.rs", "README.md"];
        let ranked = fuzzy_rank("librs", &hay);
        // "src/lib.rs" must rank first; "README.md" (no subsequence) is dropped.
        assert_eq!(ranked.first().map(|(i, _)| *i), Some(1));
        assert!(ranked.iter().all(|(i, _)| *i != 2));
    }

    #[test]
    fn fuzzy_rank_is_case_insensitive() {
        let hay = ["Cargo.toml", "unrelated"];
        let ranked = fuzzy_rank("cargo", &hay);
        assert_eq!(ranked.first().map(|(i, _)| *i), Some(0));
    }

    fn hit(path: &str, score: u32) -> PathHit {
        PathHit {
            path: path.to_string(),
            score,
        }
    }

    #[test]
    fn two_pass_tracked_outranks_ignored_at_equal_score() {
        // A tracked and an ignored file with the SAME fuzzy score: tracked first.
        let out = merge_two_pass(vec![hit("src/app.rs", 5)], vec![hit("dist/app.js", 5)], 10);
        assert_eq!(
            out.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["src/app.rs", "dist/app.js"]
        );
        assert!(
            out[0].score > out[1].score,
            "tracked re-stamped above ignored"
        );
    }

    #[test]
    fn two_pass_ignored_surfaces_when_no_tracked_matches() {
        let out = merge_two_pass(vec![], vec![hit("node_modules/x.js", 3)], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "node_modules/x.js");
    }

    #[test]
    fn two_pass_no_ignored_leaves_tracked_untouched() {
        let out = merge_two_pass(vec![hit("a", 9), hit("b", 8)], vec![], 10);
        assert_eq!(
            out.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn two_pass_dedups_ignored_already_in_tracked() {
        // If a path shows up in both passes, the tracked copy wins (appears once).
        let out = merge_two_pass(vec![hit("shared", 5)], vec![hit("shared", 9)], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "shared");
    }

    #[test]
    fn two_pass_caps_at_limit_tracked_first() {
        let out = merge_two_pass(vec![hit("t1", 2), hit("t2", 1)], vec![hit("i1", 9)], 2);
        assert_eq!(
            out.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["t1", "t2"],
            "limit fills from the tracked pass first"
        );
    }
}
