//! The merge-queue section (the fold-actor): per-branch land/defer status from
//! the `merge_queue` cache, with the queue-management action keys. Reads
//! `model.panel.merge_queue` (populated from the `merge_queue` table each model
//! build, and patched in place by the live drain — see
//! `handlers::merge_queue`). Each queue row carries a cursor hit aligned with
//! `ui.cursor`; the action keys (`a/A/x/l/r/c/D`) are dispatched by the loop's
//! section-key arm to `handlers::merge_queue::section_key`, so the hint row
//! below can never drift from the dispatch.

use thegn_core::attention::MqStatus;
use thegn_core::remote::GitLoc;
use thegn_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, g, g2, hint_row, hue, list_detail, t, wrap_text,
};

/// A short host label for a queue row whose branch lives off this host, or
/// `None` for a local (same-store) branch. Derived from the row's `location`
/// descriptor (mirrored from `worktrees.location`): the ssh host, or the
/// provider's exec prefix head — so the reader can see, at a glance, which
/// queued branches sit on another machine (they get their tip bundle-fetched
/// into the target store at drain time; see `crate::merge_remote`).
fn host_label(location: &str) -> Option<String> {
    let loc = location.trim();
    if loc.is_empty() || loc == "local" {
        return None;
    }
    match GitLoc::from_db("", Some(loc)) {
        GitLoc::Local(_) => None,
        GitLoc::Remote { ssh, .. } => Some(ssh.host.clone()),
        GitLoc::Provider { control_prefix, .. } => control_prefix.first().cloned(),
    }
}

/// The hued glyph for a queue row's status string: the shared
/// [`thegn_core::attention::MqStatus::glyph`] vocabulary (also the sidebar
/// detail chip's), capability-degraded like the rest of the chrome. Unknown
/// statuses are flagged with the attention glyph.
pub(super) fn status_glyph(status: &str) -> Seg {
    use thegn_core::attention::MqStatus;
    let gl = crate::caps::active_glyphs();
    match MqStatus::parse(status) {
        Some(mq) => {
            let (glyph, h) = mq.glyph(gl);
            seg(hue(h), glyph)
        }
        None => seg(hue(Hue::Red), gl.attention),
    }
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let rows = &ctx.model.panel.merge_queue;
    let scope_tail = if crate::panel::scope::merge_all() {
        "merge queue empty (all projects)"
    } else {
        "merge queue empty (this project · g = all)"
    };
    if rows.is_empty() {
        let mut out = Vec::new();
        if let Some(notice) = claim_unavailable_notice(ctx.model.panel.merge_claim_error.as_deref())
        {
            out.push(notice);
        }
        out.push(PanelRow::plain(Line::segs(vec![seg(d(), scope_tail)])));
        out.push(mq_hint_row());
        return out;
    }
    if ctx.full() {
        return full_view(ctx, rows);
    }
    let mut out: Vec<PanelRow> = Vec::new();
    if crate::panel::scope::merge_all() {
        out.push(PanelRow::plain(Line::segs(vec![seg(
            g2(),
            "all projects (g = this project)",
        )])));
    }
    if let Some(notice) = claim_unavailable_notice(ctx.model.panel.merge_claim_error.as_deref()) {
        out.push(notice);
    }
    for (i, r) in rows.iter().enumerate() {
        let mut left = vec![status_glyph(&r.status), seg(d(), format!(" {}", r.branch))];
        // Off-host branches get a host chip so the reader sees which rows live on
        // another machine (their tips are bundle-fetched into the target store).
        if let Some(host) = host_label(&r.location) {
            left.push(seg(hue(Hue::Blue), format!(" @{host}")));
        }
        if let Some(view) = claim_for_row(ctx, &r.worktree)
            && let Some(chip) = row_claim_chip(Some(view), thegn_core::util::now_ms())
        {
            left.push(seg(g(), "  "));
            left.push(chip);
        }
        // Blocked rows carry the reason recorded on the row. The gate branch
        // used to hardcode "breaks build" here, ignoring `error_detail` — so the
        // panel could never show WHICH test failed, and an environment failure
        // read as a verdict about the code. Always prefer the recorded detail;
        // only the headline's first line is shown (the log tail is in the row).
        if let Some((reason, tone)) = held_reason(r) {
            left.push(seg(g(), "  "));
            left.push(seg(hue(tone), reason));
        }
        // A landed row under `on_landed = "expire"` is on a clock, so say so:
        // an expiry the reader cannot see is one they cannot act on before it
        // fires. The right column carries the countdown in place of the bare
        // "landed", which the ✓ glyph already conveys.
        let right = right_label(r);
        // Each queue row carries a `Row` hit so the enumerate index lines up
        // with `ui.cursor` and with `model.panel.merge_queue`.
        out.push(
            PanelRow::plain(Line::split(left, vec![seg(g2(), right)]))
                .with_hit(PanelHit::Row(Section::MergeQueue, i)),
        );
    }
    out.push(mq_hint_row());
    out
}

fn right_label(r: &thegn_core::db::MergeQueueRow) -> String {
    expiry_label(r).unwrap_or_else(|| crate::merge_sweep::safe_display(&r.status))
}

fn held_reason(r: &thegn_core::db::MergeQueueRow) -> Option<(String, Hue)> {
    let Some(status) = MqStatus::parse(&r.status) else {
        return Some((
            format!(
                "unrecognised queue status '{}'",
                crate::merge_sweep::safe_display(&r.status)
            ),
            Hue::Red,
        ));
    };
    if !status.is_held() {
        return None;
    }
    let reason = if let Some(d) = r.error_detail.as_deref().filter(|s| !s.is_empty()) {
        let reason = d.lines().next().unwrap_or(d);
        if status == MqStatus::AgentBlocked {
            format!("environment/policy blocked — {reason}")
        } else {
            reason.to_string()
        }
    } else {
        match r.conflict_paths.as_deref() {
            Some(p) if !p.is_empty() => p.replace('\n', ", "),
            _ if status == MqStatus::GateFailed => "breaks build".to_string(),
            _ if status == MqStatus::GateError => "gate could not run".to_string(),
            _ if status == MqStatus::AgentBlocked => {
                "agent blocked by environment or isolation policy".to_string()
            }
            _ => "conflict".to_string(),
        }
    };
    // Amber for an environment failure: nothing is wrong with the branch.
    let tone = if status.is_infrastructure() {
        Hue::Amber
    } else {
        Hue::Red
    };
    Some((reason, tone))
}

/// Full: queue list + a detail column for the cursor row — the one place the
/// FULL `error_detail` (gate log tail) and conflict-path list are readable
/// instead of clipped to the row's first line.
fn full_view(ctx: &SectionCtx, rows: &[thegn_core::db::MergeQueueRow]) -> Vec<PanelRow> {
    let cols = ctx.cols;
    let mut out: Vec<PanelRow> = Vec::new();
    let scope = if crate::panel::scope::merge_all() {
        "all projects (g = this project)"
    } else {
        "this project (g = all)"
    };
    out.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "MERGE QUEUE"),
        seg(g2(), format!(" · {} branches · {scope}", rows.len())),
    ])));
    out.push(super::rule());

    let cursor = ctx.ui.cursor.min(rows.len().saturating_sub(1));
    let list_w = 45_usize.min(cols / 2);
    let list_rows: Vec<(Vec<Seg>, Option<usize>)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let sel = if i == cursor { "▶ " } else { "  " };
            let mut l = vec![
                seg(if i == cursor { t() } else { g() }, sel),
                status_glyph(&r.status),
                seg(
                    if i == cursor { t() } else { d() },
                    format!(" {}", r.branch),
                ),
            ];
            if let Some(host) = host_label(&r.location) {
                l.push(seg(hue(Hue::Blue), format!(" @{host}")));
            }
            (l, Some(i))
        })
        .collect();

    let detail_w = cols.saturating_sub(list_w + 2);
    let r = &rows[cursor];
    let mut detail: Vec<Vec<Seg>> = vec![
        vec![
            status_glyph(&r.status),
            seg(
                t(),
                format!(" {}", crate::merge_sweep::safe_display(&r.status)),
            )
            .bold(),
        ],
        vec![
            seg(d(), r.branch.clone()),
            seg(g2(), " → "),
            seg(d(), r.target_branch.clone()),
        ],
    ];
    if let Some(host) = host_label(&r.location) {
        detail.push(vec![seg(g2(), "host  "), seg(hue(Hue::Blue), host)]);
    }
    if let Some((label, tone)) = claim_detail_line(
        claim_for_row(ctx, &r.worktree),
        ctx.model.panel.merge_claim_error.as_deref(),
        thegn_core::util::now_ms(),
    ) {
        detail.push(vec![seg(g2(), "claim  "), seg(hue(tone), label)]);
    }
    if r.agent_attempts > 0 {
        detail.push(vec![seg(
            g2(),
            format!("agent attempts  {}", r.agent_attempts),
        )]);
    }
    if let Some(paths) = r.conflict_paths.as_deref().filter(|s| !s.is_empty()) {
        detail.push(Vec::new());
        detail.push(vec![seg(hue(Hue::Red), "conflicts".to_string()).bold()]);
        for p in paths.lines() {
            detail.push(vec![seg(g(), "  "), seg(d(), p.to_string())]);
        }
    }
    if let Some(err) = r.error_detail.as_deref().filter(|s| !s.is_empty()) {
        detail.push(Vec::new());
        detail.push(vec![seg(g2(), "detail".to_string()).bold()]);
        // The whole recorded detail (gate log tail), line by line, wrapped —
        // the Normal row shows only the first line.
        for line in err.lines() {
            if line.is_empty() {
                detail.push(Vec::new());
            } else {
                for chunk in wrap_text(line, detail_w) {
                    detail.push(vec![seg(d(), chunk)]);
                }
            }
        }
    }

    out.extend(list_detail(
        Section::MergeQueue,
        &list_rows,
        &detail,
        cursor,
        list_w,
        ctx.rows.saturating_sub(out.len() + 1),
    ));
    out.push(mq_hint_row());
    out
}

fn claim_for_row<'a>(
    ctx: &SectionCtx<'a>,
    worktree: &str,
) -> Option<&'a thegn_core::store::MergeClaimView> {
    ctx.model
        .panel
        .merge_claims
        .iter()
        .find(|view| view.worktree == worktree)
}

fn claim_live(view: &thegn_core::store::MergeClaimView, now_ms: i64) -> bool {
    view.phase != "unclaimed" && view.lease_until_ms.is_some_and(|until| until > now_ms)
}

fn row_claim_chip(view: Option<&thegn_core::store::MergeClaimView>, now_ms: i64) -> Option<Seg> {
    view.filter(|view| claim_live(view, now_ms))
        .map(|_| seg(hue(Hue::Blue), "claimed"))
}

fn claim_label(
    view: Option<&thegn_core::store::MergeClaimView>,
    now_ms: i64,
) -> Option<(String, Hue)> {
    let view = view.filter(|view| view.phase != "unclaimed")?;
    let owner = view
        .owner_hint
        .as_deref()
        .filter(|hint| !hint.is_empty())
        .map(crate::merge_sweep::safe_display)
        .unwrap_or_else(|| "unknown owner".to_owned());
    let phase = crate::merge_sweep::safe_display(&view.phase);
    match view.lease_until_ms {
        Some(until) if until > now_ms => Some((
            format!(
                "claimed by {owner} · {phase} · lease {}s left",
                until.saturating_sub(now_ms) / 1000
            ),
            Hue::Blue,
        )),
        Some(until) => Some((
            format!(
                "claim by {owner} expired {}s ago · recoverable",
                now_ms.saturating_sub(until) / 1000
            ),
            Hue::Amber,
        )),
        None => Some((
            format!("claim by {owner} · {phase} · lease unavailable"),
            Hue::Amber,
        )),
    }
}

fn claim_detail_line(
    view: Option<&thegn_core::store::MergeClaimView>,
    error: Option<&str>,
    now_ms: i64,
) -> Option<(String, Hue)> {
    if let Some(error) = error {
        return Some((
            format!(
                "claim state unavailable: {}",
                crate::merge_sweep::safe_display(error)
            ),
            Hue::Amber,
        ));
    }
    claim_label(view, now_ms)
}

fn claim_unavailable_notice(error: Option<&str>) -> Option<PanelRow> {
    error.map(|_| {
        PanelRow::plain(Line::segs(vec![seg(
            hue(Hue::Amber),
            "claim state unavailable",
        )]))
    })
}

/// The right-column countdown for a landed row under `on_landed = "expire"`:
/// `✓ 6d` while the grace period runs, `✓ due` once it is up and the row is
/// waiting on the next sweep.
///
/// `None` for anything that is not a landed row on a clock, so the caller falls
/// back to the plain status text. Reads the mirrored TTL rather than a `Config`
/// — the section builder has neither, and a zero there means "no expiry", which
/// correctly yields no countdown.
fn expiry_label(r: &thegn_core::db::MergeQueueRow) -> Option<String> {
    if r.status != "landed" {
        return None;
    }
    let ttl = crate::panel::scope::merged_ttl_secs();
    if ttl == 0 {
        return None;
    }
    let gl = crate::caps::active_glyphs();
    Some(
        match thegn_core::merge_sweep::remaining_secs(r.updated_at, thegn_core::util::now(), ttl) {
            Some(left) => format!(
                "{} {}",
                gl.check,
                thegn_core::merge_sweep::humanize_remaining(left)
            ),
            None => format!("{} due", gl.check),
        },
    )
}

/// The per-section key hints (the same keys the event loop dispatches to
/// `handlers::merge_queue::section_key`, so they can't drift).
fn mq_hint_row() -> PanelRow {
    hint_row(&[
        ("a/A", "add"),
        ("x", "remove"),
        ("l", "land"),
        ("r", "retry"),
        // Under `on_landed = "expire"` this also sweeps the merged worktrees, so
        // the hint says "sweep", not "clear rows" — the visible effect is the
        // worktrees leaving the sidebar, not a row count changing.
        ("c", "sweep ✓"),
        ("D", "drain"),
    ])
}

#[cfg(test)]
mod expiry_tests {
    use super::*;
    use thegn_core::store::{MergeRetryOutcome, MergeStatusFields, WorktreeAuxStore};

    fn row(status: &str, updated_at: i64) -> thegn_core::db::MergeQueueRow {
        thegn_core::db::MergeQueueRow {
            worktree: "/wt/a".into(),
            branch: "feat".into(),
            target_branch: "main".into(),
            status: status.into(),
            queued_at: updated_at,
            updated_at,
            result_oid: None,
            conflict_paths: None,
            error_detail: None,
            location: String::new(),
            agent_attempts: 0,
        }
    }

    /// Only a landed row is on a clock; everything else keeps its status text.
    #[test]
    fn only_landed_rows_get_a_countdown() {
        crate::panel::scope::set_merged_ttl_secs(7 * 24 * 3600);
        let now = thegn_core::util::now();
        for s in ["queued", "deferred", "gate_failed", "ready", "needs_human"] {
            assert_eq!(expiry_label(&row(s, now)), None, "{s}");
        }
        assert!(expiry_label(&row("landed", now)).is_some());
    }

    /// A zero mirror means no grace period at all (`move`/`remove`/ttl 0), so
    /// the row must not claim to be expiring.
    #[test]
    fn no_countdown_without_a_grace_period() {
        crate::panel::scope::set_merged_ttl_secs(0);
        assert_eq!(expiry_label(&row("landed", thegn_core::util::now())), None);
    }

    #[test]
    fn the_countdown_reads_down_then_says_due() {
        let ttl = 7 * 24 * 3600u64;
        crate::panel::scope::set_merged_ttl_secs(ttl);
        let now = thegn_core::util::now();
        // Just landed ⇒ the full window, in days.
        let fresh = expiry_label(&row("landed", now)).unwrap();
        assert!(fresh.ends_with("7d"), "{fresh}");
        // Past the window ⇒ waiting on the next sweep.
        let old = expiry_label(&row("landed", now - ttl as i64 - 1)).unwrap();
        assert!(old.ends_with("due"), "{old}");
    }

    #[test]
    fn held_reason_uses_the_complete_typed_status_table() {
        for status in MqStatus::ALL {
            assert_eq!(
                held_reason(&row(status.as_str(), 1)).is_some(),
                status.is_held(),
                "reason column for {}",
                status.as_str()
            );
        }
        assert_eq!(
            held_reason(&row("unknown-status", 1)),
            Some((
                "unrecognised queue status 'unknown-status'".into(),
                Hue::Red
            ))
        );
    }

    #[test]
    fn unknown_status_is_flagged_with_its_raw_value() {
        let (reason, tone) = held_reason(&row("future_status", 1)).unwrap();
        assert_eq!(tone, Hue::Red);
        assert!(reason.contains("future_status"));
    }

    #[test]
    fn right_column_status_is_sanitized() {
        let label = right_label(&row("evil\u{1b}[2J\u{202e}", 1));
        assert!(!label.contains('\u{1b}'));
        assert!(!label.contains('\u{202e}'));
        assert!(label.starts_with("evil"));
    }

    #[test]
    fn persisted_agent_blocked_row_keeps_its_reason_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.db");
        let db = thegn_core::db::Db::open_at(&path).unwrap();
        db.enqueue_merge("/wt/a", "feat", "main").unwrap();
        db.replace_merge_status(
            "/wt/a",
            "agent_blocked",
            &MergeStatusFields {
                error_detail: Some("sandbox floor not met".into()),
                ..Default::default()
            },
        )
        .unwrap();
        drop(db);

        let db = thegn_core::db::Db::open_at(&path).unwrap();
        let rows = db.list_merge_queue().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            held_reason(&rows[0]),
            Some((
                "environment/policy blocked — sandbox floor not met".into(),
                Hue::Amber
            ))
        );
    }

    #[test]
    fn agent_blocked_retry_rearms_with_attempts_unspent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.db");
        let db = thegn_core::db::Db::open_at(&path).unwrap();
        db.enqueue_merge("/wt/a", "feat", "main").unwrap();
        db.replace_merge_status(
            "/wt/a",
            "agent_blocked",
            &MergeStatusFields {
                error_detail: Some("sandbox floor not met".into()),
                ..Default::default()
            },
        )
        .unwrap();
        drop(db);

        let db = thegn_core::db::Db::open_at(&path).unwrap();
        let rows = db.list_merge_queue().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "agent_blocked");
        db.set_merge_agent_attempts("/wt/a", 2).unwrap();
        assert_eq!(
            db.retry_held_merge_entry("/wt/a").unwrap(),
            MergeRetryOutcome::Rearmed
        );
        let rows = db.list_merge_queue().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "queued");
        assert_eq!(rows[0].agent_attempts, 0);
    }
}

#[cfg(test)]
mod claim_tests {
    use super::*;

    fn claim(
        phase: &str,
        lease_until_ms: Option<i64>,
        owner_hint: Option<&str>,
    ) -> thegn_core::store::MergeClaimView {
        thegn_core::store::MergeClaimView {
            worktree: "/wt/a".to_owned(),
            generation: 1,
            phase: phase.to_owned(),
            lease_until_ms,
            owner_hint: owner_hint.map(str::to_owned),
        }
    }

    #[test]
    fn claim_label_covers_unclaimed_live_expired_and_missing_hint() {
        let now = 10_000;
        assert_eq!(
            claim_label(Some(&claim("unclaimed", None, None)), now),
            None
        );

        let live = claim("claimed_no_child", Some(now + 60_000), Some("thegn pid 7"));
        let (label, tone) = claim_label(Some(&live), now).unwrap();
        assert_eq!(tone, Hue::Blue);
        assert!(label.contains("claimed by thegn pid 7"), "{label}");
        assert!(label.contains("60s left"), "{label}");

        let expired = claim("settled", Some(now - 5_000), Some("drain-a"));
        let (label, tone) = claim_label(Some(&expired), now).unwrap();
        assert_eq!(tone, Hue::Amber);
        assert!(label.contains("expired 5s ago"), "{label}");
        assert!(label.contains("recoverable"), "{label}");

        let missing_hint = claim("claimed_no_child", None, None);
        let (label, _) = claim_label(Some(&missing_hint), now).unwrap();
        assert!(label.contains("unknown owner"), "{label}");
        assert!(label.contains("lease unavailable"), "{label}");
    }

    #[test]
    fn full_view_says_claim_state_unavailable_on_a_read_error() {
        let line = claim_detail_line(None, Some("boom"), 10_000).unwrap();
        assert_eq!(line.1, Hue::Amber);
        assert!(
            line.0.contains("claim state unavailable: boom"),
            "{}",
            line.0
        );
        assert!(!line.0.contains("claimed by"), "{}", line.0);
    }

    #[test]
    fn unavailable_claim_state_has_one_panel_notice_without_row_chip_text() {
        let notice = claim_unavailable_notice(Some("boom")).unwrap();
        let Line::Segs(segments) = notice.line else {
            panic!("claim notice should be a text row");
        };
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "claim state unavailable");
        assert!(claim_unavailable_notice(None).is_none());
    }

    #[test]
    fn normal_row_marks_a_live_claim() {
        let live = claim("claimed_no_child", Some(70_000), Some("test-drain"));
        let chip = row_claim_chip(Some(&live), 10_000).unwrap();
        assert_eq!(chip.text, "claimed");
    }
}
