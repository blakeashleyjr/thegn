//! The pipeline board's row model — a **pure** fold over the agent-dispatch
//! roster into the rows the monitor's Pipeline tab draws.
//!
//! Kept out of `monitor/build.rs` on purpose: the ordering rules (stage
//! grouping, parent→chunk indentation, orphan handling) are the part that can
//! be wrong in ways a screenshot won't show, so they live in a function with no
//! model, no clock and no DB — exhaustively unit-tested here.
//!
//! # Doctrine
//!
//! `[[pipeline.stages]]` encodes STRUCTURE, not judgment. This module only
//! *groups and labels* by stage; nothing here advances a stage, enforces a
//! concurrency limit or times a row out. Stage transitions are the supervising
//! agent's, written onto the roster through `dispatches.put` — the roster gains
//! columns, never transitions.

use thegn_core::issue::{AgentDispatch, AgentDispatchStatus};

/// The label a `NULL`-stage row is grouped under. Trailing, after every named
/// stage: a dispatch made outside a pipeline (the `D` key, a hand-run agent) is
/// still worth seeing, but it is not part of the org chart.
pub(crate) const UNSTAGED: &str = "unstaged";

/// One drawable board row.
///
/// Deliberately owns its strings: the overlay caches the row list at rebuild so
/// a key handler can resolve the cursor without re-borrowing the model, exactly
/// as the Containers tab caches `ContainerRowMeta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineRow {
    /// Roster row id — the board's identity for a row (never its index).
    pub id: i64,
    /// The stage this row is filed under; [`UNSTAGED`] for a `NULL` stage.
    pub stage: String,
    /// True on the first row of a stage group — the builder draws a group
    /// heading above it.
    pub group_head: bool,
    /// Nesting depth: 0 for a root dispatch, 1+ for a chunk row under its
    /// parent. Capped so a pathological parent chain can't indent off-screen.
    pub depth: u8,
    /// The roster parent, carried RAW rather than folded into [`Self::depth`].
    ///
    /// `depth` only ever records a parent inside the same stage group, so it
    /// cannot tell the board the one thing a left-to-right board needs to draw:
    /// whether this row's parent sits in the PREVIOUS stage column (an inbound
    /// edge) or in this one (a tree connector). Not re-derivable from anything
    /// else on the row.
    pub parent_id: Option<i64>,
    /// Dispatch stamp in unix milliseconds — what the stall predicate measures
    /// against. [`Self::age`] is its pre-formatted display twin; a string can't
    /// be compared to a `timeout_secs` budget.
    pub dispatched_at_ms: i64,
    /// Status glyph, straight from [`AgentDispatchStatus::glyph`] — the same
    /// vocabulary `thegn dispatch list` prints, so the board and the CLI can
    /// never disagree about what a row is doing.
    pub glyph: &'static str,
    pub status: AgentDispatchStatus,
    pub agent_name: String,
    /// Basename of the worktree path — the sidebar's own row identity.
    pub worktree: String,
    /// Full worktree path, for the jump action.
    pub worktree_path: String,
    /// The daemon session running this row, when it has one (phase 2 focuses
    /// the pane itself; today it only rides the jump request).
    pub session_id: Option<String>,
    /// Age since dispatch, pre-formatted (`3m`, `2h`, `4d`). Empty when the
    /// clock is behind the row (a skewed stamp reads as "just now", never as a
    /// negative age).
    pub age: String,
    pub issue_id: String,
}

/// Cap on rendered indentation. A malformed parent chain (or a cycle written by
/// a supervisor) must degrade to a flat-ish list, never to an unreadable one.
const MAX_DEPTH: u8 = 4;

/// The roster snapshot the board renders, plus the stage order it groups by.
///
/// One model field rather than two: the order is sampled with the rows (off the
/// loop, same door), so the board can never draw rows against a stale order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DispatchRoster {
    pub rows: Vec<AgentDispatch>,
    /// Stage names in configured order. Empty ⇒ [`ordered_rows`] falls back to
    /// alphabetical stage names.
    pub stage_order: Vec<String>,
    /// `Some` when the latest sample FAILED and `rows` is the last known-good
    /// roster rather than a fresh read (THE-271). `None` = fresh.
    pub stale: Option<RosterStale>,
}

/// Why a roster is being shown stale: the last sample failed to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RosterStale {
    /// Unix ms of the last successful sample; `None` if none ever succeeded.
    pub last_good_ms: Option<i64>,
    pub error: String,
}

/// One off-loop roster read, tagged with the request generation that issued
/// it. A failure is a distinct arm, never an empty-success roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RosterSample {
    pub generation: u64,
    /// Unix ms the read finished (display only; never used for ordering).
    pub at_ms: i64,
    pub result: Result<DispatchRoster, String>,
}

/// What [`RosterFence::accept`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Accepted {
    /// The model roster changed: repaint and refresh the board.
    pub changed: bool,
    /// A dirty signal arrived while the read was in flight: sample once more.
    pub follow_up: bool,
    /// `Some(n)` when this failure (the n-th in a row) should be logged.
    /// Rate-limited to powers of two so a stuck DB cannot flood the log.
    pub log_failure: Option<u32>,
}

/// Generation fence for roster samples (THE-260/271). Pure state, no I/O.
///
/// At most one read is in flight; dirty signals during a read coalesce into
/// one follow-up; a completion older than the newest applied one is dropped.
#[derive(Debug, Default)]
pub(crate) struct RosterFence {
    issued: u64,
    applied: u64,
    in_flight: bool,
    pending: bool,
    fails: u32,
    last_good_ms: Option<i64>,
}

impl RosterFence {
    /// Ask to start a read. `Some(generation)` = spawn it now. `None` = one is
    /// already in flight; `dirty` is remembered so a follow-up is guaranteed.
    pub fn request(&mut self, dirty: bool) -> Option<u64> {
        if self.in_flight {
            self.pending |= dirty;
            return None;
        }
        self.in_flight = true;
        self.issued += 1;
        Some(self.issued)
    }

    /// Consecutive failed samples; drives the retry backoff.
    pub fn failures(&self) -> u32 {
        self.fails
    }

    /// Cadence between samples after `failures` consecutive failures:
    /// doubling, capped at 32x. A pure function of the count, no timer.
    pub fn backoff(base: std::time::Duration, failures: u32) -> std::time::Duration {
        base * (1u32 << failures.min(5))
    }

    /// Fold a completed sample into `current`.
    pub fn accept(&mut self, current: &mut DispatchRoster, sample: RosterSample) -> Accepted {
        let mut out = Accepted::default();
        if sample.generation == self.issued {
            self.in_flight = false;
            out.follow_up = std::mem::take(&mut self.pending);
        }
        if sample.generation <= self.applied {
            return out;
        }
        self.applied = sample.generation;
        match sample.result {
            Ok(fresh) => {
                self.fails = 0;
                self.last_good_ms = Some(sample.at_ms);
                if *current != fresh {
                    *current = fresh;
                    out.changed = true;
                }
            }
            Err(error) => {
                self.fails = self.fails.saturating_add(1);
                if self.fails.is_power_of_two() {
                    out.log_failure = Some(self.fails);
                }
                let stale = Some(RosterStale {
                    last_good_ms: self.last_good_ms,
                    error,
                });
                if current.stale != stale {
                    current.stale = stale;
                    out.changed = true;
                }
            }
        }
        out
    }
}

impl DispatchRoster {
    /// Whether the Pipeline tab has anything to show: a roster row, or a
    /// configured pipeline that has simply not been run yet.
    pub fn is_present(&self) -> bool {
        !self.rows.is_empty() || !self.stage_order.is_empty()
    }
}

/// The configured stage order, from `[[pipeline.stages]]`.
///
/// Declaration order IS the board's column order (the org chart). Unnamed or
/// blank stages are skipped by `Pipeline::stage_names`, so a half-written
/// entry never opens a phantom column. An empty `[[pipeline.stages]]` yields an
/// empty order and the board falls back to alphabetical stage names (see
/// [`ordered_rows`]) — stable, just not the org chart's.
pub(crate) fn stage_order(cfg: &thegn_core::config::Config) -> Vec<String> {
    cfg.pipeline.stage_names()
}

/// The stage sequence both surfaces order by — [`ordered_rows`]' row groups and
/// the board's COLUMNS — so the two can never disagree about precedence.
///
/// 1. Stages named in `stage_order`, in that order (the org chart), skipping
///    blank names and duplicates.
/// 2. Stages `present` on the roster but absent from the config, by name — a
///    stage renamed in config must not make its live rows vanish.
/// 3. [`UNSTAGED`] last, when present.
///
/// `keep_empty` is the one difference between the two callers: the row fold
/// skips a configured stage with no rows (there is nothing to list), while the
/// board keeps it (a configured-but-empty stage must still be a visible column
/// — that is the org chart, and an invisible one reads as misconfiguration).
pub(crate) fn stage_sequence(
    present: &std::collections::BTreeSet<String>,
    stage_order: &[String],
    keep_empty: bool,
) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    for name in stage_order {
        let name = name.trim();
        if name.is_empty() || name == UNSTAGED {
            continue;
        }
        if (keep_empty || present.contains(name)) && !order.iter().any(|o| o == name) {
            order.push(name.to_string());
        }
    }
    // BTreeSet order — alphabetical, so an unconfigured stage lands somewhere
    // stable rather than wherever the roster happened to list it.
    for name in present {
        if name != UNSTAGED && !order.iter().any(|o| o == name) {
            order.push(name.clone());
        }
    }
    if present.contains(UNSTAGED) {
        order.push(UNSTAGED.to_string());
    }
    order
}

/// Fold the roster into board rows.
///
/// Order, top to bottom:
///
/// 1. Stages named in `stage_order`, in that order (the org chart).
/// 2. Stages present on the roster but absent from the config, by name — a
///    stage renamed in config must not make its live rows vanish.
/// 3. `NULL`-stage rows last, under [`UNSTAGED`].
///
/// Within a stage: root rows oldest-first by `dispatched_at_ms` (the order they
/// were dispatched reads as the order work started), each followed immediately
/// by its chunk children, recursively, same ordering. A child whose parent is
/// not in *this stage group* is an orphan and renders as a root there — a pruned
/// or cross-stage parent must never make a row unreachable.
pub(crate) fn ordered_rows(
    dispatches: &[AgentDispatch],
    stage_order: &[String],
    now_ms: i64,
) -> Vec<PipelineRow> {
    // Group ids by stage label, preserving nothing about input order (the
    // per-group sort below is the only ordering that matters).
    let mut groups: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (ix, d) in dispatches.iter().enumerate() {
        let stage = d
            .stage
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(UNSTAGED)
            .to_string();
        groups.entry(stage).or_default().push(ix);
    }

    // Configured stages first (in config order, skipping ones with no rows),
    // then the rest alphabetically, then UNSTAGED — one shared helper with the
    // board's column order, so a row group and its column can never disagree.
    let present: std::collections::BTreeSet<String> = groups.keys().cloned().collect();
    let order = stage_sequence(&present, stage_order, false);

    let mut out: Vec<PipelineRow> = Vec::new();
    for stage in order {
        let Some(ixs) = groups.get(&stage) else {
            continue;
        };
        // Membership of THIS group, so a child whose parent lives in another
        // stage renders as a root here rather than disappearing.
        let member: std::collections::BTreeSet<i64> =
            ixs.iter().map(|&i| dispatches[i].id).collect();
        // parent id -> child indices, oldest-first.
        let mut children: std::collections::BTreeMap<i64, Vec<usize>> = Default::default();
        let mut roots: Vec<usize> = Vec::new();
        for &i in ixs {
            match dispatches[i].parent_id.filter(|p| member.contains(p)) {
                // A row that is its own parent is a cycle of one: treat it as a
                // root rather than recursing.
                Some(p) if p != dispatches[i].id => children.entry(p).or_default().push(i),
                _ => roots.push(i),
            }
        }
        let by_time = |a: &usize, b: &usize| {
            let (a, b) = (&dispatches[*a], &dispatches[*b]);
            (a.dispatched_at_ms, a.id).cmp(&(b.dispatched_at_ms, b.id))
        };
        roots.sort_by(by_time);
        for v in children.values_mut() {
            v.sort_by(by_time);
        }

        let head_at = out.len();
        // Iterative pre-order walk (an explicit stack, not recursion: the parent
        // graph is user data and a deep chain must not blow the stack). `seen`
        // breaks any cycle a supervisor manages to write.
        let mut seen: std::collections::BTreeSet<i64> = Default::default();
        let walk = |stack: &mut Vec<(usize, u8)>,
                    out: &mut Vec<PipelineRow>,
                    seen: &mut std::collections::BTreeSet<i64>| {
            while let Some((ix, depth)) = stack.pop() {
                let d = &dispatches[ix];
                if !seen.insert(d.id) {
                    continue;
                }
                out.push(row(d, &stage, depth.min(MAX_DEPTH), now_ms));
                if let Some(kids) = children.get(&d.id) {
                    for &k in kids.iter().rev() {
                        stack.push((k, depth.saturating_add(1)));
                    }
                }
            }
        };
        let mut stack: Vec<(usize, u8)> = roots.iter().rev().map(|&i| (i, 0u8)).collect();
        walk(&mut stack, &mut out, &mut seen);
        // Anything the walk could not reach belongs to a parent CYCLE (every
        // member has a parent inside the group, so the group has no root). A row
        // the board silently omits is worse than a mis-indented one, so each
        // unreached row is promoted to a root, oldest first, and its subtree
        // walked from there.
        let mut cyclic: Vec<usize> = ixs
            .iter()
            .copied()
            .filter(|&j| !seen.contains(&dispatches[j].id))
            .collect();
        if !cyclic.is_empty() {
            cyclic.sort_by(by_time);
            let mut stack: Vec<(usize, u8)> = cyclic.iter().rev().map(|&j| (j, 0u8)).collect();
            walk(&mut stack, &mut out, &mut seen);
        }
        if let Some(first) = out.get_mut(head_at) {
            first.group_head = true;
        }
    }
    out
}

fn row(d: &AgentDispatch, stage: &str, depth: u8, now_ms: i64) -> PipelineRow {
    PipelineRow {
        id: d.id,
        stage: stage.to_string(),
        group_head: false,
        depth,
        parent_id: d.parent_id,
        dispatched_at_ms: d.dispatched_at_ms,
        glyph: d.status.glyph(),
        status: d.status,
        agent_name: d.agent_name.clone(),
        worktree: thegn_core::util::basename(&d.worktree_path).to_string(),
        worktree_path: d.worktree_path.clone(),
        session_id: d.session_id.clone(),
        age: fmt_age_ms(now_ms.saturating_sub(d.dispatched_at_ms)),
        issue_id: d.issue_id.clone(),
    }
}

/// `4s` / `3m` / `2h` / `5d`. A negative span (clock skew, a stamp from the
/// future) reads as `0s` rather than as a nonsense age.
pub(crate) fn fmt_age_ms(delta_ms: i64) -> String {
    let s = delta_ms.max(0) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// The per-worktree stage badge the sidebar shows: the stage of each worktree's
/// most recent **active** roster row.
///
/// Active rather than most-recent-of-any: a finished row's stage is history, and
/// a badge that keeps naming last week's `review` on an idle worktree is worse
/// than none. Pure, so the hydration thread's fold and its test share one rule.
pub(crate) fn stage_badges(
    dispatches: &[AgentDispatch],
) -> std::collections::BTreeMap<String, String> {
    let mut out: std::collections::BTreeMap<String, ((i64, i64), String)> = Default::default();
    for d in dispatches {
        if !d.status.is_active() || d.worktree_path.is_empty() {
            continue;
        }
        let Some(stage) = d.stage.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let key = (d.dispatched_at_ms, d.id);
        match out.get_mut(&d.worktree_path) {
            Some(e) if e.0 >= key => {}
            Some(e) => *e = (key, stage.to_string()),
            None => {
                out.insert(d.worktree_path.clone(), (key, stage.to_string()));
            }
        }
    }
    out.into_iter().map(|(k, (_, s))| (k, s)).collect()
}

/// Worktrees whose most recent **active** roster row is parked on a human,
/// mapped to that row's dispatch time in unix **seconds** — the shape
/// [`thegn_core::attention::AttentionInputs::stage_blocked_since`] wants.
pub(crate) fn stage_blocked(
    dispatches: &[AgentDispatch],
) -> std::collections::BTreeMap<String, i64> {
    let mut out: std::collections::BTreeMap<String, i64> = Default::default();
    for d in dispatches {
        if d.status != AgentDispatchStatus::WaitingHuman || d.worktree_path.is_empty() {
            continue;
        }
        let at = d.dispatched_at_ms / 1000;
        let e = out.entry(d.worktree_path.clone()).or_insert(at);
        *e = (*e).max(at);
    }
    out
}

/// The roster changed under the loop (a pane exit stamped a row, a
/// `dispatches.put` landed). Set off-loop; the loop clears it and re-samples.
///
/// A flag rather than a channel send from the exit path: the pane-exit handler
/// runs inside `spawn_blocking` with no refresh sender in reach, and the exit
/// has already dirtied the frame — so this needs no wake source of its own,
/// which is exactly the 0%-idle contract.
fn roster_dirty() -> &'static std::sync::atomic::AtomicBool {
    static F: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    &F
}

/// Mark the roster stale (see [`roster_dirty`]).
pub(crate) fn mark_roster_dirty() {
    roster_dirty().store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Claim the stale flag: `true` at most once per change.
pub(crate) fn take_roster_dirty() -> bool {
    roster_dirty().swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Last roster fingerprint seen by the hydration thread. `0` = never seen (a
/// genuinely-empty roster hashes to something else, so the first pass always
/// registers).
fn roster_fingerprint() -> &'static std::sync::atomic::AtomicU64 {
    static F: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    &F
}

/// Notice a roster the hydration thread already holds, and mark it stale if it
/// moved since the last pass.
///
/// This is what lets a dispatch written by ANOTHER process (`thegn dispatch
/// put`, a supervising agent through the control API) reach a board that is
/// currently shut — without which the board's tab, hidden until a row exists,
/// could never discover that one now does. It costs one hash over rows already
/// in memory: no I/O, no allocation, no wake source.
pub(crate) fn note_roster(dispatches: &[AgentDispatch]) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    dispatches.len().hash(&mut h);
    for d in dispatches {
        d.id.hash(&mut h);
        d.status.as_str().hash(&mut h);
        d.stage.hash(&mut h);
        d.parent_id.hash(&mut h);
        d.dispatched_at_ms.hash(&mut h);
        d.worktree_path.hash(&mut h);
    }
    // `1` is reserved as "never seen": a real hash colliding with it just
    // re-samples once, which is harmless.
    let fp = h.finish().max(1);
    if roster_fingerprint().swap(fp, std::sync::atomic::Ordering::Relaxed) != fp {
        mark_roster_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: i64, stage: Option<&str>, parent: Option<i64>, at: i64) -> AgentDispatch {
        AgentDispatch {
            id,
            issue_id: format!("THE-{id}"),
            worktree_path: format!("/wt/w{id}"),
            agent_name: format!("a{id}"),
            dispatched_at_ms: at,
            status: AgentDispatchStatus::Running,
            stage: stage.map(str::to_string),
            parent_id: parent,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
            report: None,
            exit_code: None,
            exited_at_ms: None,
        }
    }

    fn ids(rows: &[PipelineRow]) -> Vec<i64> {
        rows.iter().map(|r| r.id).collect()
    }

    #[test]
    fn a_row_dispatched_now_reads_as_seconds_old() {
        // Regression: `put_agent_dispatch` wrote `util::now()` (SECONDS) into a
        // column every reader treats as milliseconds, so a just-dispatched row
        // rendered ~20671d old. Drive the real clock end to end.
        let now_ms = thegn_core::util::now_ms();
        let mut fresh = d(1, Some("code"), None, now_ms);
        fresh.status = AgentDispatchStatus::Running;
        let rows = ordered_rows(&[fresh], &["code".into()], now_ms);
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].age.ends_with('s'),
            "a row dispatched right now must read in seconds, got {:?}",
            rows[0].age
        );
    }

    #[test]
    fn empty_roster_yields_no_rows() {
        assert!(ordered_rows(&[], &[], 0).is_empty());
        assert!(ordered_rows(&[], &["code".into()], 0).is_empty());
    }

    #[test]
    fn config_order_wins_then_unknown_stages_by_name_then_unstaged() {
        let rows = ordered_rows(
            &[
                d(1, Some("review"), None, 10),
                d(2, None, None, 20),
                d(3, Some("zeta"), None, 30),
                d(4, Some("architect"), None, 40),
                d(5, Some("alpha"), None, 50),
            ],
            &["architect".into(), "code".into(), "review".into()],
            0,
        );
        // architect, review (config order; `code` has no rows and is skipped),
        // then alpha, zeta (unknown, by name), then the NULL-stage row.
        assert_eq!(ids(&rows), vec![4, 1, 5, 3, 2]);
        assert_eq!(rows[4].stage, UNSTAGED);
        // Every group's first row is a heading anchor, and nothing else is.
        assert_eq!(
            rows.iter().map(|r| r.group_head).collect::<Vec<_>>(),
            vec![true, true, true, true, true]
        );
    }

    #[test]
    fn stage_sequence_keeps_empty_configured_stages_only_for_the_board() {
        let present: std::collections::BTreeSet<String> =
            ["review".into(), "zeta".into(), UNSTAGED.into()]
                .into_iter()
                .collect();
        let cfg = ["architect".into(), "code".into(), "review".into()];
        // The row fold drops the two configured stages with no rows…
        assert_eq!(
            stage_sequence(&present, &cfg, false),
            vec!["review", "zeta", UNSTAGED]
        );
        // …the board keeps them, in declaration order, ahead of the unknown one.
        assert_eq!(
            stage_sequence(&present, &cfg, true),
            vec!["architect", "code", "review", "zeta", UNSTAGED]
        );
    }

    #[test]
    fn stage_sequence_ignores_blanks_duplicates_and_a_configured_unstaged() {
        let present: std::collections::BTreeSet<String> = ["code".into()].into_iter().collect();
        let cfg = [
            "  ".into(),
            "code".into(),
            "code".into(),
            UNSTAGED.into(),
            String::new(),
        ];
        assert_eq!(stage_sequence(&present, &cfg, true), vec!["code"]);
    }

    #[test]
    fn rows_within_a_stage_are_oldest_first_and_only_the_first_heads_the_group() {
        let rows = ordered_rows(
            &[
                d(1, Some("code"), None, 300),
                d(2, Some("code"), None, 100),
                d(3, Some("code"), None, 200),
            ],
            &["code".into()],
            0,
        );
        assert_eq!(ids(&rows), vec![2, 3, 1]);
        assert_eq!(
            rows.iter().map(|r| r.group_head).collect::<Vec<_>>(),
            vec![true, false, false]
        );
    }

    #[test]
    fn children_indent_under_their_parent_in_dispatch_order() {
        let rows = ordered_rows(
            &[
                d(1, Some("code"), None, 100),
                d(3, Some("code"), Some(1), 300),
                d(2, Some("code"), Some(1), 200),
                d(4, Some("code"), Some(2), 250),
                d(5, Some("code"), None, 900),
            ],
            &["code".into()],
            0,
        );
        // 1 → (2 → 4) → 3, then the later root 5.
        assert_eq!(ids(&rows), vec![1, 2, 4, 3, 5]);
        assert_eq!(
            rows.iter().map(|r| r.depth).collect::<Vec<_>>(),
            vec![0, 1, 2, 1, 0]
        );
    }

    #[test]
    fn an_orphan_parent_id_renders_the_row_as_a_root() {
        // Parent pruned entirely, parent in another stage, and a self-parent
        // cycle — all three must still render the row.
        let rows = ordered_rows(
            &[
                d(1, Some("architect"), None, 100),
                d(2, Some("code"), Some(999), 200),
                d(3, Some("code"), Some(1), 300),
                d(4, Some("code"), Some(4), 400),
            ],
            &["architect".into(), "code".into()],
            0,
        );
        assert_eq!(ids(&rows), vec![1, 2, 3, 4]);
        assert_eq!(
            rows.iter().map(|r| r.depth).collect::<Vec<_>>(),
            vec![0, 0, 0, 0],
            "a parent outside the group is not an indent anchor"
        );
    }

    #[test]
    fn a_parent_cycle_terminates_and_lists_every_row_once() {
        // 1 → 2 → 1: neither has a root, so nothing is reachable by the walk;
        // the rows must still all appear exactly once rather than vanish or
        // spin.
        let rows = ordered_rows(
            &[
                {
                    let mut r = d(1, Some("code"), Some(2), 100);
                    r.status = AgentDispatchStatus::WaitingHuman;
                    r
                },
                d(2, Some("code"), Some(1), 200),
            ],
            &["code".into()],
            0,
        );
        assert_eq!(ids(&rows), vec![1, 2]);
    }

    #[test]
    fn indent_is_capped_so_a_deep_chain_stays_readable() {
        let mut rows_in = vec![d(1, Some("code"), None, 1)];
        for i in 2..=10 {
            rows_in.push(d(i, Some("code"), Some(i - 1), i));
        }
        let rows = ordered_rows(&rows_in, &["code".into()], 0);
        assert_eq!(rows.len(), 10);
        assert!(rows.iter().all(|r| r.depth <= MAX_DEPTH));
        assert_eq!(rows.last().unwrap().depth, MAX_DEPTH);
    }

    #[test]
    fn blank_and_whitespace_stages_group_as_unstaged() {
        let rows = ordered_rows(
            &[d(1, Some("   "), None, 10), d(2, Some(""), None, 20)],
            &[],
            0,
        );
        assert!(rows.iter().all(|r| r.stage == UNSTAGED));
        assert_eq!(ids(&rows), vec![1, 2]);
    }

    #[test]
    fn with_no_configured_order_stages_fall_back_to_alphabetical() {
        let rows = ordered_rows(
            &[
                d(1, Some("review"), None, 10),
                d(2, Some("architect"), None, 20),
                d(3, Some("code"), None, 30),
            ],
            &[],
            0,
        );
        assert_eq!(ids(&rows), vec![2, 3, 1]);
    }

    #[test]
    fn row_fields_carry_glyph_basename_and_age() {
        let mut src = d(7, Some("code"), None, 1_000);
        src.status = AgentDispatchStatus::WaitingHuman;
        src.worktree_path = "/home/u/code/app/feat-x".into();
        src.session_id = Some("s-1".into());
        let rows = ordered_rows(&[src], &[], 1_000 + 125_000);
        let r = &rows[0];
        assert_eq!(r.glyph, AgentDispatchStatus::WaitingHuman.glyph());
        assert_eq!(r.worktree, "feat-x");
        assert_eq!(r.worktree_path, "/home/u/code/app/feat-x");
        assert_eq!(r.session_id.as_deref(), Some("s-1"));
        assert_eq!(r.age, "2m");
        assert_eq!(r.issue_id, "THE-7");
        // The two raw facts the board's edge/stall rules need. `parent_id` is
        // carried even when the parent is outside this stage group (here: no
        // parent at all), because "which column is the parent in" is the
        // board's question, not the fold's.
        assert_eq!(r.parent_id, None);
        assert_eq!(r.dispatched_at_ms, 1_000);
    }

    #[test]
    fn age_formats_across_units_and_never_goes_negative() {
        assert_eq!(fmt_age_ms(-5_000), "0s");
        assert_eq!(fmt_age_ms(4_000), "4s");
        assert_eq!(fmt_age_ms(59_999), "59s");
        assert_eq!(fmt_age_ms(60_000), "1m");
        assert_eq!(fmt_age_ms(3_600_000), "1h");
        assert_eq!(fmt_age_ms(86_400_000 * 5), "5d");
    }

    #[test]
    fn stage_badges_take_the_newest_active_row_per_worktree() {
        let mut a = d(1, Some("architect"), None, 100);
        a.worktree_path = "/wt/x".into();
        let mut b = d(2, Some("code"), None, 200);
        b.worktree_path = "/wt/x".into();
        // A newer TERMINAL row must not win — a finished stage is history.
        let mut c = d(3, Some("review"), None, 300);
        c.worktree_path = "/wt/x".into();
        c.status = AgentDispatchStatus::Done;
        // A stage-less active row contributes no badge at all.
        let mut e = d(4, None, None, 400);
        e.worktree_path = "/wt/y".into();
        let m = stage_badges(&[a, b, c, e]);
        assert_eq!(m.get("/wt/x").map(String::as_str), Some("code"));
        assert!(!m.contains_key("/wt/y"));
    }

    #[test]
    fn stage_blocked_reports_only_waiting_human_rows_in_seconds() {
        let mut a = d(1, Some("code"), None, 5_000);
        a.worktree_path = "/wt/x".into();
        a.status = AgentDispatchStatus::WaitingHuman;
        let mut b = d(2, Some("code"), None, 9_000);
        b.worktree_path = "/wt/x".into();
        b.status = AgentDispatchStatus::WaitingHuman;
        let mut c = d(3, Some("code"), None, 100_000);
        c.worktree_path = "/wt/y".into();
        let m = stage_blocked(&[a, b, c]);
        assert_eq!(m.get("/wt/x").copied(), Some(9));
        assert!(!m.contains_key("/wt/y"), "only waiting_human rows block");
    }

    #[test]
    fn roster_presence_gates_the_tab() {
        assert!(!DispatchRoster::default().is_present());
        assert!(
            DispatchRoster {
                rows: vec![],
                stage_order: vec!["code".into()],
                stale: None,
            }
            .is_present(),
            "a configured but never-run pipeline still earns the tab"
        );
        assert!(
            DispatchRoster {
                rows: vec![d(1, None, None, 0)],
                stage_order: vec![],
                stale: None,
            }
            .is_present()
        );
    }

    fn roster_of(ids: &[i64]) -> DispatchRoster {
        DispatchRoster {
            rows: ids.iter().map(|&i| d(i, Some("code"), None, 0)).collect(),
            ..Default::default()
        }
    }

    fn ok(generation: u64, ids: &[i64]) -> RosterSample {
        RosterSample {
            generation,
            at_ms: generation as i64 * 1000,
            result: Ok(roster_of(ids)),
        }
    }

    fn err(generation: u64) -> RosterSample {
        RosterSample {
            generation,
            at_ms: generation as i64 * 1000,
            result: Err("database is locked".into()),
        }
    }

    #[test]
    fn one_read_in_flight_and_dirty_requests_coalesce_into_one_follow_up() {
        let mut f = RosterFence::default();
        let mut cur = DispatchRoster::default();
        let g1 = f.request(false).expect("first request starts a read");
        assert_eq!(f.request(true), None);
        assert_eq!(f.request(true), None);
        let a = f.accept(&mut cur, ok(g1, &[1]));
        assert!(a.changed && a.follow_up);
        let g2 = f.request(false).expect("follow-up read");
        assert!(g2 > g1);
        assert!(!f.accept(&mut cur, ok(g2, &[1])).follow_up);
    }

    #[test]
    fn a_cadence_request_during_a_read_does_not_queue_a_follow_up() {
        let mut f = RosterFence::default();
        let mut cur = DispatchRoster::default();
        let g = f.request(false).unwrap();
        assert_eq!(f.request(false), None);
        assert!(!f.accept(&mut cur, ok(g, &[])).follow_up);
    }

    #[test]
    fn an_older_sample_never_overwrites_a_newer_one() {
        let mut f = RosterFence::default();
        let mut cur = DispatchRoster::default();
        let _g1 = f.request(false).unwrap();
        // Force a newer generation to be applied first, as if the first read
        // stalled and a later one finished ahead of it.
        f.in_flight = false;
        let g2 = f.request(false).unwrap();
        assert!(f.accept(&mut cur, ok(g2, &[1, 2])).changed);
        let late = f.accept(&mut cur, ok(1, &[9]));
        assert!(!late.changed);
        assert_eq!(cur, roster_of(&[1, 2]));
        // An older FAILURE must not mark the newer roster stale either.
        assert!(!f.accept(&mut cur, err(1)).changed);
        assert_eq!(cur.stale, None);
        assert_eq!(f.failures(), 0);
    }

    #[test]
    fn failure_keeps_last_good_rows_marked_stale_and_recovery_clears_it() {
        let mut f = RosterFence::default();
        let mut cur = DispatchRoster::default();
        let g = f.request(false).unwrap();
        f.accept(&mut cur, ok(g, &[1, 2]));
        let g = f.request(false).unwrap();
        let a = f.accept(&mut cur, err(g));
        assert!(a.changed);
        assert_eq!(cur.rows.len(), 2, "never an empty board on failure");
        let st = cur.stale.clone().expect("marked stale");
        assert_eq!(st.last_good_ms, Some(1000));
        assert_eq!(st.error, "database is locked");
        let g = f.request(false).unwrap();
        assert!(f.accept(&mut cur, ok(g, &[1, 2])).changed);
        assert_eq!(cur.stale, None);
        assert_eq!(f.failures(), 0);
    }

    #[test]
    fn a_failure_before_any_success_is_stale_with_no_age_and_empty_success_clears() {
        let mut f = RosterFence::default();
        let mut cur = DispatchRoster::default();
        let g = f.request(false).unwrap();
        f.accept(&mut cur, err(g));
        assert_eq!(cur.stale.as_ref().unwrap().last_good_ms, None);
        let g = f.request(false).unwrap();
        f.accept(&mut cur, ok(g, &[1]));
        let g = f.request(false).unwrap();
        f.accept(&mut cur, ok(g, &[]));
        assert!(
            cur.rows.is_empty(),
            "a confirmed empty read clears the roster"
        );
        assert_eq!(cur.stale, None);
    }

    #[test]
    fn repeated_failures_log_on_powers_of_two_and_back_off_to_a_cap() {
        let mut f = RosterFence::default();
        let mut cur = roster_of(&[1]);
        let mut logged = Vec::new();
        let mut changed = 0;
        for _ in 0..9 {
            let g = f.request(false).unwrap();
            let a = f.accept(&mut cur, err(g));
            changed += a.changed as u32;
            logged.extend(a.log_failure);
        }
        assert_eq!(logged, vec![1, 2, 4, 8]);
        assert_eq!(changed, 1, "an unchanged stale marker does not repaint");
        let base = std::time::Duration::from_secs(2);
        assert_eq!(RosterFence::backoff(base, 0), base);
        assert_eq!(RosterFence::backoff(base, 3), base * 8);
        assert_eq!(RosterFence::backoff(base, 99), base * 32);
    }

    /// The two tests below share the process-global staleness flag and
    /// fingerprint. nextest runs a process per test, but `cargo test` does not
    /// — serialize them so the suite is honest under either runner.
    fn global_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn note_roster_marks_dirty_only_when_the_roster_moved() {
        let _g = global_lock();
        let a = d(1, Some("code"), None, 100);
        note_roster(std::slice::from_ref(&a));
        take_roster_dirty(); // consume whatever the first sighting raised
        // Same roster again: nothing moved.
        note_roster(std::slice::from_ref(&a));
        assert!(!take_roster_dirty());
        // A status change moves it…
        let mut b = a.clone();
        b.status = AgentDispatchStatus::Done;
        note_roster(std::slice::from_ref(&b));
        assert!(take_roster_dirty());
        // …and so does a new row.
        note_roster(&[b.clone(), d(2, Some("code"), None, 200)]);
        assert!(take_roster_dirty());
    }

    #[test]
    fn roster_dirty_flag_is_claim_once() {
        let _g = global_lock();
        // Independent of ambient state: clear, set, claim, claim again.
        take_roster_dirty();
        assert!(!take_roster_dirty());
        mark_roster_dirty();
        assert!(take_roster_dirty());
        assert!(!take_roster_dirty());
    }
}
