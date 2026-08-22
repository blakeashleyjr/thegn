//! Loop-fetched documents the section bodies render from: the git section's
//! heat/velocity/log payload, the rolling telemetry history, and the cached
//! cheatsheet groups. Fetch results are generation-tagged so strays from
//! before a worktree switch die on arrival.

/// The git section's wide-view payload, computed off-loop on section entry
/// and cached per worktree by the event loop.
#[derive(Debug, Clone)]
pub struct GitDocs {
    /// `heat[week][weekday]` levels 0..=4, oldest week first, Mon=0.
    pub heat: Vec<[u8; 7]>,
    /// Weekly commit totals over the same window, oldest first.
    pub weekly: Vec<u32>,
    pub log: Vec<thegn_svc::git::LogRow>,
    /// Commits in the window (the VELOCITY headline).
    pub total: u32,
    pub head_branch: String,
}

/// Off-loop panel-document fetch results, tagged with the loop's docs
/// generation so results from before a worktree switch die on arrival.
/// (The old `Diff` variant fed the changes section's side-by-side full view,
/// which the git-family Full frame superseded — removed with it.)
#[derive(Debug)]
pub enum DocsPayload {
    Git(GitDocs),
}

/// Everything the loop feeds the section bodies outside the hydrated
/// [`super::PanelData`]. Lives on [`super::PanelUi`] (precedent: the banked
/// hunk previews) so the render path needs no extra parameters.
///
/// Deliberately **not** `Clone`: `telemetry` retains an hour of samples across
/// ~18 series (~620 KiB), and this struct sits on the render path where an
/// accidental clone would be a per-frame memcpy. Nothing cloned it, so nothing
/// loses anything.
#[derive(Debug, Default)]
pub struct PanelDocs {
    /// Per-worktree git calendar/log payload; `None` until fetched.
    pub git: Option<GitDocs>,
    /// Rolling stats history feeding the telemetry graphs.
    pub telemetry: crate::telemetry::TelemetryHistory,
    /// Rolling event-loop self-profiler history (the Telemetry "Loop" sub-block).
    pub loop_perf: crate::telemetry::LoopPerfHistory,
    /// Cached pane-daemon registry row (PID / version / uptime / heartbeat),
    /// refreshed off-loop on the ticker. Feeds the far-right status chip + its
    /// modal.
    pub daemon: crate::chrome::DaemonStatus,
    /// The daemon's live session list, fetched over the control socket when the
    /// user opens the status modal (never on a timer — see
    /// `handlers::status::probe_sessions`).
    pub daemon_sessions: crate::detail::DaemonSessions,
    /// When the session list last landed (`None` until a probe answers) — the
    /// modal's "as of N ago" staleness marker.
    pub daemon_sessions_at: Option<std::time::Instant>,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copy_target_prefers_the_head_row() {
        let row = |sha: &str, refs: &str| thegn_svc::git::LogRow {
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
