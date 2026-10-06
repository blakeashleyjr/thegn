//! Gitlink/submodule reads and command assembly.
//!
//! The domain types live in thegn_core; this module is deliberately only the
//! provider-side adapter. Summaries are local-object-only and never fetch as a
//! side effect of rendering.

use anyhow::Result;
use std::collections::HashMap;

use thegn_core::remote::GitLoc;
use thegn_core::submodule::{
    AncestorFacts, SubmoduleConflict, SubmoduleDiff, SubmoduleDiffKind, SubmoduleDirection,
    SubmoduleState, SubmoduleSummary, classify_pointer, direction_from_ancestors,
    parse_submodule_status,
};

use super::{FileStatus, parse_status_porcelain, run, run_w};

/// Parse the mode-160000 entries from git diff --raw -z output.
///
/// With `-z`, Git terminates the header and pathname with NULs rather than
/// using the tab separator used by the human-readable form: `header\0path\0`.
/// Keep accepting the tab form for small fixture callers, but parse the real
/// wire format as pairs so live gitlink diffs cannot disappear.
pub(crate) fn parse_raw_diffs(output: &str) -> Vec<SubmoduleDiff> {
    let mut records = output.split('\0');
    let mut diffs = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let (header, path) = if let Some((header, path)) = record.split_once('\t') {
            (header, path)
        } else {
            let Some(path) = records.next() else { break };
            (record, path)
        };
        let is_nul_record = record.split_once('\t').is_none();
        let status = header.split_whitespace().nth(4).unwrap_or_default();
        let has_second_rename_path =
            is_nul_record && matches!(status.as_bytes().first(), Some(b'R' | b'C'));
        let diff = (|| {
            let mut fields = header.split_whitespace();
            let old_mode = fields.next()?.trim_start_matches(':');
            let new_mode = fields.next()?;
            let old_sha = fields.next()?;
            let new_sha = fields.next()?;
            let status = fields.next()?;
            if old_mode != "160000" && new_mode != "160000" {
                return None;
            }
            let kind = if old_sha.chars().all(|c| c == '0') {
                SubmoduleDiffKind::Added
            } else if new_sha.chars().all(|c| c == '0') {
                SubmoduleDiffKind::Deleted
            } else {
                // A raw rename can carry two paths. Gitlinks are still atomic;
                // retain the first path, which is the stable path used by the
                // ordinary diff join. The status is otherwise not interpreted.
                let _ = status;
                SubmoduleDiffKind::Changed
            };
            Some(SubmoduleDiff {
                path: path.to_string(),
                old_sha: old_sha.to_string(),
                new_sha: new_sha.to_string(),
                kind,
            })
        })();
        // A caller that supplies the non-`--no-renames` raw format has one
        // extra pathname for R/C records. The production command below pins
        // rename detection off, but consume that field here to keep parsing
        // subsequent records aligned in tests/alternate providers.
        if has_second_rename_path {
            let _ = records.next();
        }
        let Some(diff) = diff else { continue };
        diffs.push(diff);
    }
    diffs
}

fn has_gitmodules(loc: &GitLoc) -> bool {
    match loc {
        GitLoc::Local(path) => path.join(".gitmodules").is_file(),
        // A remote/provider checkout cannot be inspected with local fs APIs.
        // The committed root-level file is the useful lifecycle signal there.
        _ => super::run_status(loc, &["cat-file", "-e", "HEAD:.gitmodules"])
            .is_ok_and(|(exit, _)| exit == 0),
    }
}

fn status_marks_submodule(status: &[FileStatus], path: &str) -> (bool, bool) {
    let mut dirty = false;
    let mut untracked = false;
    for row in status {
        if !thegn_core::submodule::is_submodule_descendant(&row.path, path) {
            continue;
        }
        if row.unstaged == '?' || row.staged == '?' {
            untracked = true;
        }
        if matches!(row.staged, 'M' | 'm') || matches!(row.unstaged, 'M' | 'm') {
            dirty = true;
        }
    }
    (dirty, untracked)
}

fn recorded_gitlinks(loc: &GitLoc) -> Result<HashMap<String, String>> {
    let out = run(loc, &["ls-files", "--stage", "-z", "--"])?;
    let mut result = HashMap::new();
    for record in out.split('\0') {
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let Some(mode) = fields.next() else { continue };
        let Some(sha) = fields.next() else { continue };
        if mode == "160000" {
            result.insert(path.to_string(), sha.to_string());
        }
    }
    Ok(result)
}

/// Read recursive submodule state plus ordinary status evidence in one
/// provider-side operation. An absent root .gitmodules is an empty state, not
/// an error, so ordinary git views remain usable.
pub(crate) fn states(loc: &GitLoc) -> Result<Vec<SubmoduleState>> {
    if !has_gitmodules(loc) {
        return Ok(Vec::new());
    }
    let status_output = run(loc, &["submodule", "status", "--recursive"])?;
    let ordinary_status = parse_status_porcelain(&run(
        loc,
        &["status", "--porcelain=v1", "-z", "--no-renames"],
    )?);
    // The recorded pointer is part of the state read. If it cannot be read,
    // fail the submodule field so the host can retain its last-known value;
    // treating the missing map as empty would misclassify every initialized
    // submodule as moved/dirty.
    let recorded = recorded_gitlinks(loc)?;
    let mut states = parse_submodule_status(&status_output)
        .map_err(|e| anyhow::anyhow!("parse submodule status: {e}"))?;
    for state in &mut states {
        if let Some(sha) = recorded.get(&state.path) {
            state.recorded_sha = sha.clone();
        }
        let (dirty, untracked) = status_marks_submodule(&ordinary_status, &state.path);
        state.dirty = dirty;
        state.untracked = untracked;
        if state.initialized && !state.checked_out_sha.is_empty() {
            state.pointer = classify_pointer(
                &state.recorded_sha,
                &state.checked_out_sha,
                true,
                state.pointer == thegn_core::submodule::SubmodulePointer::Conflict,
                AncestorFacts::default(),
            );
        }
    }
    Ok(states)
}

pub(crate) fn dirty_from_states(states: &[SubmoduleState]) -> bool {
    states.iter().any(|state| {
        state.dirty
            || state.untracked
            || !state.initialized
            || !matches!(
                state.pointer,
                thegn_core::submodule::SubmodulePointer::Clean
            )
    })
}

pub(crate) fn dirty_from_outputs(submodule_output: &str, ordinary_output: &str) -> Result<bool> {
    let ordinary = parse_status_porcelain(ordinary_output);
    let mut states = parse_submodule_status(submodule_output)
        .map_err(|e| anyhow::anyhow!("parse submodule status: {e}"))?;
    for state in &mut states {
        let (dirty, untracked) = status_marks_submodule(&ordinary, &state.path);
        state.dirty = dirty;
        state.untracked = untracked;
    }
    Ok(dirty_from_states(&states))
}

pub(crate) fn diffs(loc: &GitLoc, base: &str) -> Result<Vec<SubmoduleDiff>> {
    let out = run(
        loc,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            "--abbrev=64",
            base,
        ],
    )?;
    Ok(parse_raw_diffs(&out))
}

fn ancestor(loc: &GitLoc, older: &str, newer: &str) -> Result<Option<bool>> {
    let (exit, _) = super::run_status(loc, &["merge-base", "--is-ancestor", older, newer])?;
    match exit {
        0 => Ok(Some(true)),
        1 => Ok(Some(false)),
        _ => Ok(None),
    }
}

pub(crate) fn summary(
    loc: &GitLoc,
    old: &str,
    new: &str,
    limit: usize,
) -> Result<SubmoduleSummary> {
    if old == new {
        return Ok(SubmoduleSummary::bounded(
            SubmoduleDirection::Unknown,
            Vec::new(),
            limit,
            false,
        ));
    }
    let facts = AncestorFacts {
        old_is_ancestor: ancestor(loc, old, new)?,
        new_is_ancestor: ancestor(loc, new, old)?,
    };
    let direction = direction_from_ancestors(facts);
    if matches!(direction, SubmoduleDirection::Unknown) {
        return Ok(SubmoduleSummary::bounded(
            direction,
            Vec::new(),
            limit,
            true,
        ));
    }
    let (from, to) = if direction == SubmoduleDirection::Rewind {
        (new, old)
    } else {
        (old, new)
    };
    let cap = limit.min(SubmoduleSummary::DEFAULT_LIMIT).saturating_add(1);
    let cap_s = cap.to_string();
    let range = format!("{from}..{to}");
    let (exit, output) =
        super::run_status(loc, &["log", "--format=%h", "--max-count", &cap_s, &range])?;
    if exit != 0 {
        return Ok(SubmoduleSummary::bounded(
            direction,
            Vec::new(),
            limit,
            true,
        ));
    }
    Ok(SubmoduleSummary::bounded(
        direction,
        output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        limit,
        false,
    ))
}

pub(crate) fn init(loc: &GitLoc, recursive: bool) -> Result<()> {
    let args: Vec<&str> = if recursive {
        vec!["submodule", "update", "--init", "--recursive"]
    } else {
        vec!["submodule", "update", "--init"]
    };
    run_w(loc, &[], &args).map(|_| ())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    mode: String,
    sha: String,
}

/// Budgets for conflict enrichment. Exceeding any of them is an error, never a
/// silently truncated (partially trusted) conflict set.
const MAX_CONFLICT_PATHS: usize = 20_000;
const MAX_CONFLICT_PATH_BYTES: usize = 4 * 1024 * 1024;
/// Path bytes per `ls-tree` invocation, so argv length never depends on the
/// size of the conflict set (well under the platform ARG_MAX floor).
const CHUNK_PATH_BYTES: usize = 48 * 1024;
const CHUNK_MAX_PATHS: usize = 1024;
const ENRICH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

fn valid_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_mode(s: &str) -> bool {
    s.len() == 6 && s.bytes().all(|b| (b'0'..=b'7').contains(&b))
}

/// Parse `git ls-tree -z` output for a set of requested paths, all-or-error:
/// every record must be well formed (valid mode and object id), name a
/// requested path, and appear at most once. Requested paths with no record are
/// simply absent from the result (a valid tree lookup outcome).
fn parse_tree_entries(
    output: &str,
    requested: &std::collections::HashSet<&str>,
) -> Result<HashMap<String, TreeEntry>> {
    let mut found: HashMap<String, TreeEntry> = HashMap::new();
    for record in output.split('\0').filter(|record| !record.is_empty()) {
        let (meta, path) = record
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("git ls-tree returned a malformed record"))?;
        let mut fields = meta.split(' ');
        let (mode, _kind, sha) = match (fields.next(), fields.next(), fields.next(), fields.next())
        {
            (Some(m), Some(k), Some(s), None) => (m, k, s),
            _ => anyhow::bail!("git ls-tree record has the wrong field count"),
        };
        if !valid_mode(mode) || !valid_oid(sha) {
            anyhow::bail!("git ls-tree record has an invalid mode or object id");
        }
        if !requested.contains(path) {
            anyhow::bail!("git ls-tree returned an unrequested entry {path:?}");
        }
        let entry = TreeEntry {
            mode: mode.to_string(),
            sha: sha.to_string(),
        };
        if found.insert(path.to_string(), entry).is_some() {
            anyhow::bail!("git ls-tree returned duplicate records for {path:?}");
        }
    }
    Ok(found)
}

/// Resolve every path in `paths` against `tree` with chunked `ls-tree` calls
/// (a number of processes proportional to total path bytes / chunk size, not
/// to the path count).
fn tree_entries(
    loc: &GitLoc,
    tree: &str,
    paths: &[&str],
    deadline: std::time::Instant,
) -> Result<HashMap<String, TreeEntry>> {
    let mut all = HashMap::new();
    let mut start = 0;
    while start < paths.len() {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("submodule conflict enrichment exceeded its time budget");
        }
        let mut end = start;
        let mut bytes = 0usize;
        while end < paths.len()
            && end - start < CHUNK_MAX_PATHS
            && (end == start || bytes + paths[end].len() <= CHUNK_PATH_BYTES)
        {
            bytes += paths[end].len();
            end += 1;
        }
        let chunk = &paths[start..end];
        let mut args: Vec<&str> = vec!["ls-tree", "-z", tree, "--"];
        args.extend_from_slice(chunk);
        let output = run(loc, &args)?;
        let requested: std::collections::HashSet<&str> = chunk.iter().copied().collect();
        for (path, entry) in parse_tree_entries(&output, &requested)? {
            if all.insert(path.clone(), entry).is_some() {
                anyhow::bail!("git ls-tree returned duplicate records for {path:?}");
            }
        }
        start = end;
    }
    Ok(all)
}

/// Resolve mode-160000 conflict tips from the merge input trees, not the
/// current index. `merge-tree --write-tree` operates entirely in the object
/// database and therefore does not create the unmerged index records that
/// `git ls-files -u` would require. `base` is read when supplied so an
/// unavailable explicit merge input fails closed just like ours/theirs.
///
/// Lookups are batched per tree and bounded by path-count, path-byte and
/// wall-time budgets; exceeding any budget is an error (fail closed).
pub(crate) fn conflicts(
    loc: &GitLoc,
    paths: &[String],
    base: Option<&str>,
    ours: &str,
    theirs: &str,
) -> Result<Vec<SubmoduleConflict>> {
    if paths.len() > MAX_CONFLICT_PATHS {
        anyhow::bail!(
            "too many conflict paths for submodule enrichment: {} > {MAX_CONFLICT_PATHS}",
            paths.len()
        );
    }
    let total_bytes: usize = paths.iter().map(String::len).sum();
    if total_bytes > MAX_CONFLICT_PATH_BYTES {
        anyhow::bail!(
            "conflict paths too large for submodule enrichment: {total_bytes} > {MAX_CONFLICT_PATH_BYTES} bytes"
        );
    }
    let deadline = std::time::Instant::now() + ENRICH_DEADLINE;
    let mut unique: Vec<&str> = Vec::with_capacity(paths.len());
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        if seen.insert(path.as_str()) {
            unique.push(path.as_str());
        }
    }
    // The base lookup only validates availability of an explicit merge input.
    if let Some(tree) = base {
        tree_entries(loc, tree, &unique, deadline)?;
    }
    let ours_entries = tree_entries(loc, ours, &unique, deadline)?;
    let theirs_entries = tree_entries(loc, theirs, &unique, deadline)?;

    let mut conflicts = Vec::new();
    for path in paths {
        let ours_entry = ours_entries.get(path);
        let theirs_entry = theirs_entries.get(path);
        // A path is a gitlink conflict when either merge side carries the
        // atomic gitlink mode.
        if ours_entry.is_none_or(|entry| entry.mode != "160000")
            && theirs_entry.is_none_or(|entry| entry.mode != "160000")
        {
            continue;
        }
        conflicts.push(SubmoduleConflict {
            path: path.clone(),
            ours_sha: ours_entry.map(|e| e.sha.clone()).unwrap_or_default(),
            theirs_sha: theirs_entry.map(|e| e.sha.clone()).unwrap_or_default(),
        });
    }
    Ok(conflicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::PlumbingOps;
    use crate::git::testutil::{TestRepo, git_in};

    #[test]
    fn raw_parser_keeps_only_gitlinks_and_classifies_add_delete() {
        let out = ":100644 160000 0000000 bbbbbbb A\tvendor/lib\0:160000 0000000 aaaaaaa 0000000 D\tvendor/old\0:100644 100644 aaaaaaa bbbbbbb M\ttext\0";
        let diffs = parse_raw_diffs(out);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].kind, SubmoduleDiffKind::Added);
        assert_eq!(diffs[1].kind, SubmoduleDiffKind::Deleted);
    }

    #[test]
    fn raw_parser_accepts_git_real_nul_header_path_pairs() {
        let out = ":100644 160000 0000000 bbbbbbb A\0vendor/lib\0:160000 0000000 aaaaaaa 0000000 D\0vendor/old\0";
        let diffs = parse_raw_diffs(out);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].path, "vendor/lib");
        assert_eq!(diffs[0].kind, SubmoduleDiffKind::Added);
        assert_eq!(diffs[1].path, "vendor/old");
        assert_eq!(diffs[1].kind, SubmoduleDiffKind::Deleted);
    }

    #[test]
    fn tree_parser_is_all_or_error() {
        let sha = "a".repeat(40);
        let req: std::collections::HashSet<&str> = ["vendor/lib", "b"].into_iter().collect();
        let ok = format!("160000 commit {sha}\tvendor/lib\0");
        let got = parse_tree_entries(&ok, &req).unwrap();
        assert_eq!(got["vendor/lib"].mode, "160000");
        assert_eq!(got["vendor/lib"].sha, sha);
        assert!(parse_tree_entries("", &req).unwrap().is_empty());
        assert!(parse_tree_entries("not-a-tree-record\0", &req).is_err());
        // unrequested, duplicate, bad mode, bad oid, bad field count
        assert!(parse_tree_entries(&format!("160000 commit {sha}\tother\0"), &req).is_err());
        assert!(parse_tree_entries(&format!("{ok}{ok}"), &req).is_err());
        assert!(parse_tree_entries(&format!("16x000 commit {sha}\tb\0"), &req).is_err());
        assert!(parse_tree_entries("160000 commit aaaaaaa\tb\0", &req).is_err());
        assert!(parse_tree_entries(&format!("160000 commit {sha} x\tb\0"), &req).is_err());
    }

    #[test]
    fn conflicts_refuse_over_budget_path_sets() {
        let repo = TestRepo::new("submodule-conflict-budget");
        let many: Vec<String> = (0..=MAX_CONFLICT_PATHS).map(|i| format!("p{i}")).collect();
        assert!(conflicts(&repo.loc(), &many, None, "HEAD", "HEAD").is_err());
        let huge = vec!["x".repeat(MAX_CONFLICT_PATH_BYTES + 1)];
        assert!(conflicts(&repo.loc(), &huge, None, "HEAD", "HEAD").is_err());
    }

    #[test]
    fn conflicts_read_divergent_gitlinks_from_merge_input_trees() {
        let repo = TestRepo::new("submodule-conflict-trees");
        git_in(&repo.dir, &["config", "user.name", "t"]);
        git_in(&repo.dir, &["config", "user.email", "t@e"]);
        git_in(&repo.dir, &["config", "commit.gpgsign", "false"]);
        repo.commit_file("base.txt", "base\n", "base");
        let base = repo.head();

        // Existing commit objects serve as valid gitlink targets. The
        // superproject conflict below deliberately has no unmerged index.
        git_in(&repo.dir, &["checkout", "-q", "-b", "target-one"]);
        repo.commit_file("one.txt", "one\n", "target one");
        let one = repo.head();
        git_in(&repo.dir, &["checkout", "-q", &base]);
        git_in(&repo.dir, &["checkout", "-q", "-b", "target-two"]);
        repo.commit_file("two.txt", "two\n", "target two");
        let two = repo.head();

        git_in(&repo.dir, &["checkout", "-q", &base]);
        git_in(&repo.dir, &["checkout", "-q", "-b", "ours"]);
        git_in(
            &repo.dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{one},vendor/lib"),
            ],
        );
        git_in(&repo.dir, &["commit", "-q", "-m", "ours pointer"]);
        let ours = repo.head();

        git_in(&repo.dir, &["checkout", "-q", &base]);
        git_in(&repo.dir, &["checkout", "-q", "-b", "theirs"]);
        git_in(
            &repo.dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{two},vendor/lib"),
            ],
        );
        git_in(&repo.dir, &["commit", "-q", "-m", "theirs pointer"]);
        let theirs = repo.head();

        let paths = match crate::git::CliGit
            .merge_tree(&repo.loc(), &ours, &theirs)
            .unwrap()
        {
            crate::git::MergeTreeOutcome::Conflict { paths, .. } => paths,
            other => panic!("expected a gitlink conflict, got {other:?}"),
        };
        let got = conflicts(&repo.loc(), &paths, Some(&base), &ours, &theirs).unwrap();
        assert_eq!(
            got,
            [SubmoduleConflict {
                path: "vendor/lib".into(),
                ours_sha: one,
                theirs_sha: two,
            }]
        );
    }
}
