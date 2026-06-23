//! Loop-fetched documents the section bodies render from: the git section's
//! heat/velocity/log payload, the changes section's parsed side-by-side diff,
//! the rolling telemetry history, and the cached cheatsheet groups. Fetch
//! results are generation-tagged so strays from before a worktree switch die
//! on arrival.

use superzej_core::diff_sbs::SbsFile;

/// The git section's wide-view payload, computed off-loop on section entry
/// and cached per worktree by the event loop.
#[derive(Debug, Clone)]
pub struct GitDocs {
    /// `heat[week][weekday]` levels 0..=4, oldest week first, Mon=0.
    pub heat: Vec<[u8; 7]>,
    /// Weekly commit totals over the same window, oldest first.
    pub weekly: Vec<u32>,
    pub log: Vec<superzej_svc::git::LogRow>,
    /// Commits in the window (the VELOCITY headline).
    pub total: u32,
    pub head_branch: String,
}

/// One file's parsed side-by-side diff (the changes section's full view). An
/// empty `path` means the working tree had nothing to show.
#[derive(Debug, Clone)]
pub struct DiffDoc {
    pub path: String,
    pub file: SbsFile,
}

/// Off-loop panel-document fetch results, tagged with the loop's docs
/// generation so results from before a worktree switch die on arrival.
#[derive(Debug)]
pub enum DocsPayload {
    Git(GitDocs),
    Diff(DiffDoc),
}

/// Everything the loop feeds the section bodies outside the hydrated
/// [`super::PanelData`]. Lives on [`super::PanelUi`] (precedent: the banked
/// hunk previews) so the render path needs no extra parameters.
#[derive(Debug, Clone, Default)]
pub struct PanelDocs {
    /// Per-worktree git calendar/log payload; `None` until fetched.
    pub git: Option<GitDocs>,
    /// The selected file's parsed diff; `None` while a fetch is out.
    pub diff: Option<DiffDoc>,
    /// Rolling stats history feeding the telemetry graphs.
    pub telemetry: crate::telemetry::TelemetryHistory,
    /// Rolling event-loop self-profiler history (the Telemetry "Loop" sub-block).
    pub loop_perf: crate::telemetry::LoopPerfHistory,
    /// Cheatsheet groups from the effective keymap, refreshed on config
    /// reload (the keys section's content).
    pub cfg_keys: Vec<crate::keyhint::HintGroup>,
    /// Monotonic stats-tick counter driving the loading spinners.
    pub tick: u64,
}

/// The sha the git section's `y` copies: the HEAD row's, else the first
/// commit row's.
pub fn copy_target_sha(docs: &GitDocs) -> Option<String> {
    docs.log
        .iter()
        .find(|r| r.is_head())
        .or_else(|| docs.log.iter().find(|r| !r.sha.is_empty()))
        .map(|r| r.sha.clone())
}

// ---- flattened diff geometry (pure; unit-tested) --------------------------

/// Flattened row count of a parsed diff: each hunk = 1 header row + its rows.
pub fn diff_flat_len(file: &SbsFile) -> usize {
    file.hunks.iter().map(|h| 1 + h.rows.len()).sum()
}

/// Start offset of each hunk in the flattened row space.
pub fn diff_hunk_starts(file: &SbsFile) -> Vec<usize> {
    let mut at = 0;
    file.hunks
        .iter()
        .map(|h| {
            let start = at;
            at += 1 + h.rows.len();
            start
        })
        .collect()
}

/// The hunk containing flattened row `at` (for the `hunk n/m` readout).
pub fn diff_hunk_at(starts: &[usize], at: usize) -> usize {
    starts.iter().rposition(|&s| s <= at).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::diff_sbs::parse_unified;

    fn file() -> SbsFile {
        parse_unified(
            "@@ -1,2 +1,2 @@ fn demo()\n ctx\n-old\n+new\n@@ -10,1 +10,2 @@\n keep\n+added\n",
        )
    }

    #[test]
    fn flattened_geometry_maps_rows_to_hunks() {
        let f = file();
        let starts = diff_hunk_starts(&f);
        // Hunk 1: header + 2 rows → 3; hunk 2 starts at 3.
        assert_eq!(starts, vec![0, 3]);
        assert_eq!(diff_flat_len(&f), 6);
        assert_eq!(diff_hunk_at(&starts, 0), 0);
        assert_eq!(diff_hunk_at(&starts, 2), 0);
        assert_eq!(diff_hunk_at(&starts, 3), 1);
        assert_eq!(diff_hunk_at(&starts, 5), 1);
        // Empty diffs are zero-length, never panicking.
        let empty = SbsFile::default();
        assert_eq!(diff_flat_len(&empty), 0);
        assert!(diff_hunk_starts(&empty).is_empty());
        assert_eq!(diff_hunk_at(&[], 5), 0);
    }

    #[test]
    fn copy_target_prefers_the_head_row() {
        let row = |sha: &str, refs: &str| superzej_svc::git::LogRow {
            graph: "*".into(),
            sha: sha.into(),
            subject: "s".into(),
            refs: refs.into(),
        };
        let mut docs = GitDocs {
            heat: Vec::new(),
            weekly: Vec::new(),
            log: vec![row("aaa1111", ""), row("bbb2222", "HEAD -> main")],
            total: 2,
            head_branch: "main".into(),
        };
        assert_eq!(copy_target_sha(&docs), Some("bbb2222".into()));
        // Without a HEAD decoration the first real commit row wins.
        docs.log[1].refs = String::new();
        assert_eq!(copy_target_sha(&docs), Some("aaa1111".into()));
        docs.log.clear();
        assert_eq!(copy_target_sha(&docs), None);
    }
}
