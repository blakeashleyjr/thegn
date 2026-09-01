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
        let Some(diff) = (|| {
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
    let recorded = recorded_gitlinks(loc).unwrap_or_default();
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

/// Parse git ls-files -u -z records into the two mode-160000 conflict tips.
pub(crate) fn parse_conflicts(output: &str, paths: &[String]) -> Vec<SubmoduleConflict> {
    let mut found: HashMap<String, (String, String)> = HashMap::new();
    for record in output.split('\0') {
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        if !paths.iter().any(|candidate| candidate == path) {
            continue;
        }
        let mut fields = meta.split_whitespace();
        if fields.next() != Some("160000") {
            continue;
        }
        let Some(sha) = fields.next() else { continue };
        let Some(stage) = fields.next() else { continue };
        let entry = found.entry(path.to_string()).or_default();
        match stage {
            "2" => entry.0 = sha.to_string(),
            "3" => entry.1 = sha.to_string(),
            _ => {}
        }
    }
    found
        .into_iter()
        .map(|(path, (ours_sha, theirs_sha))| SubmoduleConflict {
            path,
            ours_sha,
            theirs_sha,
        })
        .collect()
}

pub(crate) fn conflicts(loc: &GitLoc, paths: &[String]) -> Result<Vec<SubmoduleConflict>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let output = run(loc, &["ls-files", "-u", "-z", "--"])?;
    Ok(parse_conflicts(&output, paths))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn conflict_parser_uses_stage_two_and_three() {
        let out = "160000 aaaaaaa 2\tvendor/lib\0160000 bbbbbbb 3\tvendor/lib\0";
        let got = parse_conflicts(out, &["vendor/lib".into()]);
        assert_eq!(got[0].ours_sha, "aaaaaaa");
        assert_eq!(got[0].theirs_sha, "bbbbbbb");
    }
}
