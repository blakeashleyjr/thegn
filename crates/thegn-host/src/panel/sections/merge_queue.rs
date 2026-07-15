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

use super::{PanelHit, PanelRow, Section, SectionCtx, d, g, g2, hint_row, hue};

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
    if rows.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(d(), "merge queue empty")])),
            mq_hint_row(),
        ];
    }
    let mut out: Vec<PanelRow> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let mut left = vec![status_glyph(&r.status), seg(d(), format!(" {}", r.branch))];
        // Off-host branches get a host chip so the reader sees which rows live on
        // another machine (their tips are bundle-fetched into the target store).
        if let Some(host) = host_label(&r.location) {
            left.push(seg(hue(Hue::Blue), format!(" @{host}")));
        }
        // Blocked rows carry the reason: the conflicting paths, "breaks build"
        // for a gate failure, or the recorded detail when an agent gave up.
        if r.status == "deferred" || r.status == "gate_failed" || r.status == "needs_human" {
            let reason = if r.status == "gate_failed" {
                "breaks build".to_string()
            } else if let Some(d) = r.error_detail.as_deref().filter(|s| !s.is_empty()) {
                d.replace('\n', ", ")
            } else {
                match r.conflict_paths.as_deref() {
                    Some(p) if !p.is_empty() => p.replace('\n', ", "),
                    _ => "conflict".to_string(),
                }
            };
            left.push(seg(g(), "  "));
            left.push(seg(hue(Hue::Red), reason));
        }
        // Each queue row carries a `Row` hit so the enumerate index lines up
        // with `ui.cursor` and with `model.panel.merge_queue`.
        out.push(
            PanelRow::plain(Line::split(left, vec![seg(g2(), r.status.clone())]))
                .with_hit(PanelHit::Row(Section::MergeQueue, i)),
        );
    }
    out.push(mq_hint_row());
    out
}

/// The per-section key hints (the same keys the event loop dispatches to
/// `handlers::merge_queue::section_key`, so they can't drift).
fn mq_hint_row() -> PanelRow {
    hint_row(&[
        ("a/A", "add"),
        ("x", "rm"),
        ("l", "land"),
        ("r", "retry"),
        ("c", "clear ✓"),
        ("D", "drain"),
    ])
}
