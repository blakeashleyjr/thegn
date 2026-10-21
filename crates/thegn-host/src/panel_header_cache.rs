//! Last-known-good cache for the diff/PR panel's git header.
//!
//! The header zone (branch, upstream divergence, merge-in-progress banner) is
//! rebuilt from live git reads on every hydration. Those reads can fail
//! transiently — a `.git` mutation racing the scan, or (the failure mode this
//! guards) file-descriptor exhaustion in a long multi-workspace session, which
//! makes every `gix::discover` + CLI fallback fail at once. Without a fallback
//! the header collapses to `branch = "—"` with no divergence and no banner — the
//! "git status glitches to -" symptom.
//!
//! This mirrors the sidebar-glyph pattern ([`crate::hydrate::glyph_cache`] +
//! [`crate::hydrate::merge_glyph_scan`]): keep the last successful value per
//! worktree path and, when a read errors, reuse it instead of a placeholder.
//! Only successful reads overwrite the cache, so a worktree whose git reads are
//! failing freezes at its last real header until they recover.
//! The status, log, and merge-incoming lists use a parallel per-worktree cache.
//! Failed list reads retain their last successful rows and expose unavailable flags.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use thegn_svc::git::{FileStatus, LogRow, MergeInfo};

/// One worktree's last-known-good header fields.
#[derive(Clone, Default)]
struct HeaderRow {
    branch: String,
    ahead_behind: Option<(usize, usize)>,
    merge: Option<MergeInfo>,
}

/// Process-global last-known-good header cache, keyed by worktree path. Mirrors
/// [`crate::hydrate::glyph_cache`]'s global-state shape so it needs no threading
/// through hydration's call sites; the `Mutex` covers overlapping hydrations.
fn cache() -> &'static Mutex<HashMap<String, HeaderRow>> {
    static CACHE: OnceLock<Mutex<HashMap<String, HeaderRow>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Default)]
struct ListRow {
    status: Vec<FileStatus>,
    log: Vec<LogRow>,
    incoming: std::collections::HashSet<String>,
}

fn list_cache() -> &'static Mutex<HashMap<String, ListRow>> {
    static CACHE: OnceLock<Mutex<HashMap<String, ListRow>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) struct MergedLists {
    pub status: Vec<FileStatus>,
    pub status_unavailable: bool,
    pub log: Vec<LogRow>,
    pub log_unavailable: bool,
    pub incoming: std::collections::HashSet<String>,
    pub incoming_unavailable: bool,
}

/// Adopt successful list reads and retain each prior list independently on error.
/// A log read omitted for this pass is deliberately empty and leaves its cache alone.
pub(crate) fn merge_lists(
    path: &str,
    status: Result<Vec<FileStatus>, ()>,
    log: Option<Result<Vec<LogRow>, ()>>,
    incoming: Result<std::collections::HashSet<String>, ()>,
) -> MergedLists {
    let mut map = list_cache().lock().unwrap_or_else(|e| e.into_inner());
    let prior = map.get(path).cloned().unwrap_or_default();
    let log_requested = log.is_some();
    let (status, status_unavailable) = match status {
        Ok(value) => (value, false),
        Err(()) => (prior.status.clone(), true),
    };
    let (log, log_unavailable) = match log {
        Some(Ok(value)) => (value, false),
        Some(Err(())) => (prior.log.clone(), true),
        None => (Vec::new(), false),
    };
    let (incoming, incoming_unavailable) = match incoming {
        Ok(value) => (value, false),
        Err(()) => (prior.incoming.clone(), true),
    };
    let row = map.entry(path.to_string()).or_default();
    if !status_unavailable {
        row.status.clone_from(&status);
    }
    if log_requested && !log_unavailable {
        row.log.clone_from(&log);
    }
    if !incoming_unavailable {
        row.incoming.clone_from(&incoming);
    }
    MergedLists {
        status,
        status_unavailable,
        log,
        log_unavailable,
        incoming,
        incoming_unavailable,
    }
}

/// Merge freshly-read header fields against the last-known-good row for `path`.
///
/// Each field's input is `Ok(v)` when the live read succeeded (adopt `v` and
/// refresh the cache) or `Err(())` when it failed (reuse the prior cached
/// value). A genuine `Ok(None)` — e.g. no upstream configured, or no merge in
/// progress — is a real "absent" state and clears the field, distinct from a
/// failed read. The first-ever failure with no prior falls back to `"—"` /
/// `None`, matching the old behavior. Modelled with `()` as the error so the
/// helper stays free of the git backend's error type, exactly like
/// [`crate::hydrate::merge_glyph_scan`].
pub(crate) fn merge_header(
    path: &str,
    branch: Result<String, ()>,
    ahead_behind: Result<Option<(usize, usize)>, ()>,
    merge: Result<Option<MergeInfo>, ()>,
) -> (String, Option<(usize, usize)>, Option<MergeInfo>) {
    let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
    let prior = map.get(path).cloned().unwrap_or_default();

    let branch = match branch {
        Ok(b) => b,
        Err(()) if !prior.branch.is_empty() => prior.branch.clone(),
        Err(()) => "—".to_string(),
    };
    let ahead_behind = match ahead_behind {
        Ok(v) => v,
        Err(()) => prior.ahead_behind,
    };
    let merge = match merge {
        Ok(v) => v,
        Err(()) => prior.merge.clone(),
    };

    map.insert(
        path.to_string(),
        HeaderRow {
            branch: branch.clone(),
            ahead_behind,
            merge: merge.clone(),
        },
    );
    (branch, ahead_behind, merge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_svc::git::MergeKind;

    fn merging(onto: &str) -> MergeInfo {
        MergeInfo {
            kind: MergeKind::Merge,
            onto: onto.to_string(),
        }
    }

    #[test]
    fn successful_read_is_adopted_and_cached() {
        let p = "/wt/adopt";
        let (b, ab, m) = merge_header(
            p,
            Ok("feature".into()),
            Ok(Some((2, 1))),
            Ok(Some(merging("main"))),
        );
        assert_eq!(b, "feature");
        assert_eq!(ab, Some((2, 1)));
        assert_eq!(m.map(|m| m.onto), Some("main".to_string()));
    }

    #[test]
    fn transient_failure_reuses_last_known_good() {
        let p = "/wt/retain";
        // Prime the cache with a good read.
        merge_header(p, Ok("main".into()), Ok(Some((0, 3))), Ok(None));
        // A failing tick must not blank the header — it reuses the prior values.
        let (b, ab, m) = merge_header(p, Err(()), Err(()), Err(()));
        assert_eq!(b, "main", "branch must not collapse to em-dash");
        assert_eq!(ab, Some((0, 3)));
        assert!(m.is_none());
    }

    #[test]
    fn first_failure_with_no_prior_falls_back_to_dash() {
        let (b, ab, m) = merge_header("/wt/cold", Err(()), Err(()), Err(()));
        assert_eq!(b, "—");
        assert_eq!(ab, None);
        assert!(m.is_none());
    }

    #[test]
    fn ok_none_is_a_real_absent_state_not_a_failure() {
        let p = "/wt/clear";
        // Establish divergence + a live merge.
        merge_header(p, Ok("x".into()), Ok(Some((1, 1))), Ok(Some(merging("y"))));
        // A later *successful* read with no upstream / no merge clears them —
        // this is real, distinct from a failed read reusing the prior.
        let (_b, ab, m) = merge_header(p, Ok("x".into()), Ok(None), Ok(None));
        assert_eq!(ab, None);
        assert!(m.is_none());
    }

    #[test]
    fn per_field_independence_mixes_fresh_and_prior() {
        let p = "/wt/mixed";
        merge_header(p, Ok("main".into()), Ok(Some((5, 0))), Ok(None));
        // Branch reads fine, but divergence read fails: keep the prior arrows,
        // adopt the fresh branch.
        let (b, ab, _m) = merge_header(p, Ok("main".into()), Err(()), Ok(None));
        assert_eq!(b, "main");
        assert_eq!(ab, Some((5, 0)));
    }

    fn status(path: &str) -> FileStatus {
        FileStatus {
            path: path.to_string(),
            staged: ' ',
            unstaged: 'M',
        }
    }

    fn log_row(subject: &str) -> LogRow {
        LogRow {
            graph: "* ".to_string(),
            sha: "abc1234".to_string(),
            subject: subject.to_string(),
            refs: String::new(),
        }
    }

    #[test]
    fn lists_status_failure_keeps_last_known_rows() {
        let p = "lists_status_failure_keeps_last_known_rows";
        let first = merge_lists(
            p,
            Ok(vec![status("src/a.rs"), status("src/b.rs")]),
            None,
            Ok(Default::default()),
        );
        assert!(!first.status_unavailable);

        let failed = merge_lists(p, Err(()), None, Ok(Default::default()));
        assert_eq!(
            failed
                .status
                .iter()
                .map(|row| row.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/b.rs"]
        );
        assert!(failed.status_unavailable);

        let clean = merge_lists(p, Ok(Vec::new()), None, Ok(Default::default()));
        assert!(clean.status.is_empty());
        assert!(!clean.status_unavailable);
    }

    #[test]
    fn lists_first_failure_is_empty_and_flagged() {
        let failed = merge_lists(
            "lists_first_failure_is_empty_and_flagged",
            Err(()),
            None,
            Err(()),
        );
        assert!(failed.status.is_empty());
        assert!(failed.status_unavailable);
        assert!(failed.incoming.is_empty());
        assert!(failed.incoming_unavailable);
    }

    #[test]
    fn lists_log_not_requested_keeps_cache_untouched() {
        let p = "lists_log_not_requested_keeps_cache_untouched";
        let first = merge_lists(
            p,
            Ok(Vec::new()),
            Some(Ok(vec![log_row("known")])),
            Ok(Default::default()),
        );
        assert_eq!(first.log.len(), 1);

        let skipped = merge_lists(p, Ok(Vec::new()), None, Ok(Default::default()));
        assert!(skipped.log.is_empty());
        assert!(!skipped.log_unavailable);

        let failed = merge_lists(p, Ok(Vec::new()), Some(Err(())), Ok(Default::default()));
        assert_eq!(failed.log, vec![log_row("known")]);
        assert!(failed.log_unavailable);
    }

    #[test]
    fn lists_incoming_failure_reuses_prior() {
        let p = "lists_incoming_failure_reuses_prior";
        let mut incoming = std::collections::HashSet::new();
        incoming.insert("src/incoming.rs".to_string());
        let first = merge_lists(p, Ok(Vec::new()), None, Ok(incoming));
        assert!(!first.incoming_unavailable);

        let failed = merge_lists(p, Ok(Vec::new()), None, Err(()));
        assert!(failed.incoming.contains("src/incoming.rs"));
        assert!(failed.incoming_unavailable);
    }
}
