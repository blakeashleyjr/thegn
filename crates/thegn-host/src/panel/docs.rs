//! Loop-fetched documents the section bodies render from: the git section's
//! velocity/log payload, the rolling telemetry history, and the cached
//! cheatsheet groups. Fetch results are generation-tagged so strays from
//! before a worktree switch die on arrival.

/// The git section's wide-view payload, computed off-loop on section entry
/// and cached per worktree by the event loop.
#[derive(Debug, Clone)]
pub struct GitDocs {
    /// Weekly commit totals over the same window, oldest first.
    pub weekly: Vec<u32>,
    pub log: Vec<thegn_svc::git::LogRow>,
    /// Commits in the window (the VELOCITY headline).
    pub total: u32,
    /// A git history fetch failure, safe for one-line terminal display.
    pub error: Option<String>,
}

/// Build the git section's cached payload while preserving whichever fetch
/// completed successfully. A failed half remains visible as an error so a
/// later section sync can retry it.
pub fn git_docs_from(
    epochs: anyhow::Result<Vec<i64>>,
    log: anyhow::Result<Vec<thegn_svc::git::LogRow>>,
    now: i64,
    weeks: usize,
) -> GitDocs {
    let (epochs, epochs_error) = match epochs {
        Ok(epochs) => (epochs, None),
        Err(error) => (Vec::new(), Some(format!("commit calendar: {error}"))),
    };
    let (log, log_error) = match log {
        Ok(log) => (log, None),
        Err(error) => (Vec::new(), Some(format!("commit log: {error}"))),
    };
    let error = match (epochs_error, log_error) {
        (None, None) => None,
        (Some(reason), None) | (None, Some(reason)) => Some(reason),
        (Some(calendar), Some(log)) => Some(format!("{calendar}; {log}")),
    }
    .map(|reason| {
        thegn_core::pipeline_report::display_cell(reason.lines().next().unwrap_or(&reason))
            .chars()
            .take(120)
            .collect()
    });

    GitDocs {
        weekly: thegn_core::gitviz::weekly_counts(&epochs, now, weeks),
        total: epochs.len() as u32,
        log,
        error,
    }
}

/// Failed payloads remain renderable, but the next section sync should retry.
pub fn needs_git_fetch(cached: Option<&GitDocs>) -> bool {
    cached.is_none_or(|docs| docs.error.is_some())
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
    /// Resolved calendar settings, world clocks, and cached events. Filled on
    /// demand when the date/clock popup opens — never on a timer.
    pub calendar: crate::calendar_docs::CalendarDocs,
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
            weekly: Vec::new(),
            log: vec![row("aaa1111", ""), row("bbb2222", "HEAD -> main")],
            total: 2,
            error: None,
        };
        assert_eq!(copy_target_sha(&docs), Some("bbb2222".into()));
        // Without a HEAD decoration the first real commit row wins.
        docs.log[1].refs = String::new();
        assert_eq!(copy_target_sha(&docs), Some("aaa1111".into()));
        docs.log.clear();
        assert_eq!(copy_target_sha(&docs), None);
    }

    #[test]
    fn failed_log_is_not_an_empty_history() {
        let docs = git_docs_from(
            Err(anyhow::anyhow!("git log failed")),
            Err(anyhow::anyhow!("git graph failed")),
            1_700_000_000,
            36,
        );

        assert!(docs.error.is_some());
        assert_eq!(docs.total, 0);
    }

    #[test]
    fn partial_failure_keeps_the_good_half() {
        let row = thegn_svc::git::LogRow {
            graph: "*".into(),
            sha: "abc1234".into(),
            subject: "commit".into(),
            refs: String::new(),
        };
        let docs = git_docs_from(
            Err(anyhow::anyhow!("calendar failed")),
            Ok(vec![row]),
            0,
            36,
        );

        assert!(docs.error.is_some());
        assert_eq!(docs.log.len(), 1);
        assert_eq!(docs.total, 0);
    }

    #[test]
    fn reason_is_one_line_sanitized_and_bounded() {
        let docs = git_docs_from(
            Err(anyhow::anyhow!("bad\n\x1b[31m{}", "x".repeat(500))),
            Ok(Vec::new()),
            0,
            36,
        );
        let reason = docs.error.expect("failed fetch has a reason");

        assert!(!reason.contains('\n'));
        assert!(!reason.contains('\u{1b}'));
        assert!(reason.chars().count() <= 120);
    }

    #[test]
    fn failed_payload_is_refetched() {
        let failed = git_docs_from(Err(anyhow::anyhow!("git failed")), Ok(Vec::new()), 0, 36);
        let ok = git_docs_from(Ok(Vec::new()), Ok(Vec::new()), 0, 36);

        assert!(needs_git_fetch(Some(&failed)));
        assert!(!needs_git_fetch(Some(&ok)));
        assert!(needs_git_fetch(None));
    }
}
