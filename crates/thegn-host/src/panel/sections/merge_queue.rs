//! The merge-queue section (the fold-actor): per-branch land/defer status from
//! the `merge_queue` cache, with the queue-management action keys. Reads
//! `model.panel.merge_queue` (populated from the `merge_queue` table each model
//! build, and patched in place by the live drain — see
//! `handlers::merge_queue`). Each queue row carries a cursor hit aligned with
//! `ui.cursor`; the action keys (`a/A/x/l/r/c/D`) are dispatched by the loop's
//! section-key arm to `handlers::merge_queue::section_key`, so the hint row
//! below can never drift from the dispatch.

use thegn_core::remote::GitLoc;
use thegn_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, g, g2, hint_row, hue, t, two_col, wrap_text,
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
/// statuses render like `queued`.
pub(super) fn status_glyph(status: &str) -> Seg {
    use thegn_core::attention::MqStatus;
    let gl = crate::caps::active_glyphs();
    match MqStatus::parse(status) {
        Some(mq) => {
            let (glyph, h) = mq.glyph(gl);
            seg(hue(h), glyph)
        }
        None => seg(g(), gl.dot_hollow), // unknown ≈ queued
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
        return vec![
            PanelRow::plain(Line::segs(vec![seg(d(), scope_tail)])),
            mq_hint_row(),
        ];
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
    for (i, r) in rows.iter().enumerate() {
        let mut left = vec![status_glyph(&r.status), seg(d(), format!(" {}", r.branch))];
        // Off-host branches get a host chip so the reader sees which rows live on
        // another machine (their tips are bundle-fetched into the target store).
        if let Some(host) = host_label(&r.location) {
            left.push(seg(hue(Hue::Blue), format!(" @{host}")));
        }
        // Blocked rows carry the reason recorded on the row. The gate branch
        // used to hardcode "breaks build" here, ignoring `error_detail` — so the
        // panel could never show WHICH test failed, and an environment failure
        // read as a verdict about the code. Always prefer the recorded detail;
        // only the headline's first line is shown (the log tail is in the row).
        if matches!(
            r.status.as_str(),
            "deferred" | "gate_failed" | "gate_error" | "needs_human"
        ) {
            let reason = if let Some(d) = r.error_detail.as_deref().filter(|s| !s.is_empty()) {
                d.lines().next().unwrap_or(d).to_string()
            } else {
                match r.conflict_paths.as_deref() {
                    Some(p) if !p.is_empty() => p.replace('\n', ", "),
                    _ if r.status == "gate_failed" => "breaks build".to_string(),
                    _ if r.status == "gate_error" => "gate could not run".to_string(),
                    _ => "conflict".to_string(),
                }
            };
            // Amber for an environment failure: nothing is wrong with the branch.
            let tone = if r.status == "gate_error" {
                Hue::Amber
            } else {
                Hue::Red
            };
            left.push(seg(g(), "  "));
            left.push(seg(hue(tone), reason));
        }
        // A landed row under `on_landed = "expire"` is on a clock, so say so:
        // an expiry the reader cannot see is one they cannot act on before it
        // fires. The right column carries the countdown in place of the bare
        // "landed", which the ✓ glyph already conveys.
        let right = expiry_label(r).unwrap_or_else(|| r.status.clone());
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
    let list_rows: Vec<Vec<Seg>> = rows
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
            l
        })
        .collect();

    let detail_w = cols.saturating_sub(list_w + 2);
    let r = &rows[cursor];
    let mut detail: Vec<Vec<Seg>> = vec![
        vec![
            status_glyph(&r.status),
            seg(t(), format!(" {}", r.status)).bold(),
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

    let combined = two_col(&list_rows, &detail, list_w, 2);
    let n = rows.len();
    out.extend(combined.into_iter().enumerate().map(|(i, line)| {
        let row = PanelRow::plain(line);
        // Hits index the queue rows only, by visible index.
        if i < n {
            row.with_hit(PanelHit::Row(Section::MergeQueue, i))
        } else {
            row
        }
    }));
    out.push(mq_hint_row());
    out
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
}
